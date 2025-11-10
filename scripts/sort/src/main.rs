use anyhow::{anyhow, Context, Result};
use clap::{ArgAction, Parser};
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, info, warn};
use regex::Regex;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use simplelog::{ConfigBuilder, LevelFilter, WriteLogger};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};
use walkdir::WalkDir;

// Input root (by type under this folder, e.g., input/epub, input/pdf)
const RAW_DIR: &str = "input";
// Output folders in snake_case (English)
const SORTED_DIR: &str = "output/sorted_books";
const FAIL_AUTHOR_DIR: &str = "output/fail_author";
const FAIL_TITLE_DIR: &str = "output/fail_title";
// Copy failures are logged to a dedicated JSONL file instead of moving files.
const COPY_FAIL_LOG: &str = "sortbook_copy_failures.jsonl";
// Ollama model name used for LLM classification (French-focused). Change here if needed.
const OLLAMA_MODEL: &str = "mistral:7b";

#[derive(Parser, Debug)]
#[command(
    name = "sortbook",
    version,
    about = "Sort ebooks using OpenLibrary + LLM assistance"
)]
struct Cli {
    /// File extension to process (e.g., epub, mobi, azw3)
    #[arg(short, long)]
    ext: String,

    /// Maximum number of files to process (0 = unlimited)
    #[arg(short, long, default_value_t = 0)]
    limit: usize,

    /// Active les logs debug
    #[arg(long, action = ArgAction::SetTrue)]
    debug: bool,

    /// Purge outputs (sorted_books, fail_author, fail_title, logs)
    #[arg(long, action = ArgAction::SetTrue)]
    purge: bool,

    /// Project root directory (default: current project root)
    #[arg(long, default_value = ".")]
    root: String,

    /// Matching mode (strict|normal|full)
    #[arg(long, default_value = "full")]
    mode: String,

    /// Number of authors to preload as hints for LLM (0 = disabled)
    #[arg(long, default_value_t = 2000)]
    author_hints: usize,

    /// Explicit log file path (enables file logging). Ignored if empty.
    #[arg(long, default_value = "")]
    log_file: String,

    /// Disable OpenLibrary metadata writes in strict mode
    #[arg(long, action = ArgAction::SetTrue)]
    no_ol_meta: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct LlmGuess {
    title: Option<String>,
    title_normalized: Option<String>,
    author_firstname: Option<String>,
    author_lastname: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OlDoc {
    key: Option<String>,
    title: Option<String>,
    authors: Option<Vec<OlAuthorRef>>,
}

#[derive(Debug, Deserialize)]
struct OlAuthorRef {
    key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OlSearch {
    docs: Vec<OlSearchDoc>,
}

#[derive(Debug, Deserialize)]
struct OlSearchDoc {
    key: String,
}

fn normalize_text(s: &str) -> String {
    let s = s.to_lowercase();
    let mut out = String::with_capacity(s.len());
    for ch in s.nfd() {
        if is_combining_mark(ch) {
            continue;
        }
        if ch.is_ascii_alphanumeric() || ch.is_whitespace() || ch == '-' {
            out.push(ch);
        }
    }
    let re_ws = Regex::new(r"\s+").unwrap();
    re_ws.replace_all(&out.trim(), " ").to_string()
}

fn extract_first_json_object(s: &str) -> Option<&str> {
    // naive brace matcher to extract first top-level {...}
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut start = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'{' {
            if depth == 0 {
                start = Some(i);
            }
            depth += 1;
        } else if b == b'}' {
            if depth > 0 {
                depth -= 1;
            }
            if depth == 0 {
                if let Some(st) = start {
                    return s.get(st..=i);
                }
            }
        }
    }
    None
}

async fn call_ollama_mistral(prompt: &str) -> Result<LlmGuess> {
    // Utilise `ollama run mistral:7b` en mode non interactif
    let mut cmd = Command::new("ollama");
    cmd.arg("run")
        .arg(OLLAMA_MODEL)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = cmd.spawn().context("lancement ollama")?;
    {
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("stdin ollama"))?;
        stdin.write_all(prompt.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        return Err(anyhow!("ollama execution failed"));
    }
    let txt = String::from_utf8_lossy(&out.stdout);
    // Essayer parse direct puis extraction du premier objet JSON si bruit
    let guess: LlmGuess = match serde_json::from_str(&txt) {
        Ok(g) => g,
        Err(_) => {
            if let Some(obj) = extract_first_json_object(&txt) {
                serde_json::from_str(obj).context("failed to parse LLM response (object slice)")?
            } else {
                return Err(anyhow!("LLM response was not valid JSON"));
            }
        }
    };
    Ok(guess)
}

fn load_author_hints(conn: &Connection, max: usize) -> Result<Vec<String>> {
    if max == 0 {
        return Ok(Vec::new());
    }
    let mut stmt =
        conn.prepare("SELECT name FROM authors WHERE name IS NOT NULL AND name <> '' LIMIT ?1")?;
    let iter = stmt.query_map(params![max as i64], |row| row.get::<_, String>(0))?;
    let mut list = Vec::new();
    let mut seen = HashSet::new();
    for name in iter {
        let n = name?;
        let norm = normalize_text(&n);
        if seen.insert(norm) {
            list.push(n);
        }
        if list.len() >= max {
            break;
        }
    }
    Ok(list)
}

fn build_llm_prompt(base: &str, author_hints: &[String]) -> String {
    if author_hints.is_empty() {
        return base.to_string();
    }
    let mut prompt = String::with_capacity(base.len() + author_hints.len() * 16);
    prompt.push_str("Tu dois répondre STRICTEMENT en JSON avec les clés: ");
    prompt.push_str(
        "{\"title\", \"title_normalized\", \"author_firstname\", \"author_lastname\"}.\n",
    );
    prompt.push_str("Si possible, choisis l'auteur parmi la liste partielle suivante.\n");
    prompt.push_str("Auteurs connus (partiel): ");
    for (i, a) in author_hints.iter().enumerate() {
        if i > 0 {
            prompt.push_str("; ");
        }
        prompt.push_str(a);
        if prompt.len() > 40_000 {
            break;
        }
    }
    prompt.push_str("\n\n");
    prompt.push_str(base);
    prompt
}

fn open_db(root: &Path) -> Result<Connection> {
    // Database now under data/database
    let db = root
        .join("data")
        .join("database")
        .join("openlibrary.sqlite3");
    Ok(Connection::open(db)?)
}

fn find_work_in_db(
    conn: &Connection,
    title_norm: &str,
) -> Result<Option<(String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT work_id, title, author_id FROM works WHERE title_normalized = ?1 LIMIT 1",
    )?;
    let mut rows = stmt.query(params![title_norm])?;
    if let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let title: String = row.get(1)?;
        let author_id: String = row.get(2)?;
        return Ok(Some((id, title, author_id)));
    }
    Ok(None)
}

fn normalize_name(first: &str, last: &str) -> String {
    normalize_text(&format!("{first} {last}"))
}

fn find_author_by_name_norm(
    conn: &Connection,
    name_norm: &str,
) -> Result<Option<(String, Vec<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT author_id, alternate_id FROM authors WHERE name_normalized = ?1 LIMIT 1",
    )?;
    let mut rows = stmt.query(params![name_norm])?;
    if let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let alternates: String = row.get(1).unwrap_or_default();
        let vec = if alternates.is_empty() {
            vec![]
        } else {
            alternates
                .split(',')
                .map(|s| s.trim().to_string())
                .collect()
        };
        return Ok(Some((id, vec)));
    }
    Ok(None)
}

fn find_work_by_title_and_author(
    conn: &Connection,
    title_norm: &str,
    candidate_ids: &[String],
) -> Result<Option<(String, String, String)>> {
    if candidate_ids.is_empty() {
        return Ok(None);
    }
    // try direct title match first
    if let Some(hit) = find_work_in_db(conn, title_norm)? {
        return Ok(Some(hit));
    }
    // otherwise, filter by author ids or alternates CSV
    let mut stmt = conn.prepare(
        "SELECT work_id, title, author_id, alternate_id FROM works WHERE title_normalized = ?1",
    )?;
    let mut rows = stmt.query(params![title_norm])?;
    while let Some(row) = rows.next()? {
        let work_id: String = row.get(0)?;
        let title: String = row.get(1)?;
        let author_id: String = row.get(2).unwrap_or_default();
        let alternates: String = row.get(3).unwrap_or_default();
        let mut ok = false;
        if candidate_ids.iter().any(|c| c == &author_id) {
            ok = true;
        }
        if !ok && !alternates.is_empty() {
            let csv = format!(",{},", alternates);
            for c in candidate_ids {
                let needle = format!(",{},", c);
                if csv.contains(&needle) {
                    ok = true;
                    break;
                }
            }
        }
        if ok {
            return Ok(Some((work_id, title, author_id)));
        }
    }
    Ok(None)
}

fn find_work_strict_like(
    conn: &Connection,
    title_original: &str,
    title_norm: &str,
) -> Result<Option<(String, String, String)>> {
    // Fast strategy first: prefix query on title_normalized
    let tn = title_norm.trim();
    if !tn.is_empty() {
        let prefix_len = tn.len().min(15);
        let prefix = &tn[..prefix_len];
        let glob_prefix = format!("{}*", prefix);
        let mut stmt0 = conn.prepare(
            "SELECT work_id, title, author_id FROM works WHERE title_normalized GLOB ?1 LIMIT 5",
        )?;
        let mut rows0 = stmt0.query(params![glob_prefix])?;
        if let Some(row) = rows0.next()? {
            let id: String = row.get(0)?;
            let title: String = row.get(1)?;
            let author_id: String = row.get(2)?;
            return Ok(Some((id, title, author_id)));
        }
    }

    // Ensuite: GLOB sur title_normalized (containment)
    if !tn.is_empty() {
        let glob_norm = format!("{}*", tn);
        let mut stmt1 = conn.prepare(
            "SELECT work_id, title, author_id FROM works WHERE title_normalized GLOB ?1 LIMIT 5",
        )?;
        let mut rows1 = stmt1.query(params![glob_norm])?;
        if let Some(row) = rows1.next()? {
            let id: String = row.get(0)?;
            let title: String = row.get(1)?;
            let author_id: String = row.get(2)?;
            return Ok(Some((id, title, author_id)));
        }
    }

    // Final attempt: GLOB on lower(title) (expensive). Limited to 1 row.
    let glob_pat = format!("{}*", title_original.to_lowercase());
    let mut stmt2 = conn.prepare(
        "SELECT work_id, title, author_id FROM works WHERE lower(title) GLOB ?1 LIMIT 1",
    )?;
    let mut rows2 = stmt2.query(params![glob_pat])?;
    if let Some(row) = rows2.next()? {
        let id: String = row.get(0)?;
        let title: String = row.get(1)?;
        let author_id: String = row.get(2)?;
        return Ok(Some((id, title, author_id)));
    }

    // Fallback exact sur title_normalized
    find_work_in_db(conn, title_norm)
}
async fn fetch_openlibrary_work_meta(work_id: &str) -> Result<OlDoc> {
    let url = format!("https://openlibrary.org/works/{work_id}.json");
    let resp = reqwest::get(&url).await?.error_for_status()?;
    let doc: OlDoc = resp.json().await?;
    Ok(doc)
}

fn format_author_dir(first: &str, last: &str) -> String {
    format!("{last}, {first}")
}

// Ensure all needed output directories exist (sorted + failure buckets).
fn ensure_dirs(root: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let base_sorted = root.join(SORTED_DIR);
    let fail_author = root.join(FAIL_AUTHOR_DIR);
    let fail_title = root.join(FAIL_TITLE_DIR);
    fs::create_dir_all(&base_sorted)?;
    fs::create_dir_all(&fail_author)?;
    fs::create_dir_all(&fail_title)?;
    Ok((base_sorted, fail_author, fail_title))
}

async fn run() -> Result<()> {
    let args = Cli::parse();
    // Configure logging: in --debug, write detailed logs to file under --root/sortbook.log
    // Initialize the logger AFTER purge to avoid deleting the freshly created file
    let mut deferred_log_path: Option<PathBuf> = None;

    let root = PathBuf::from(&args.root);
    debug!(
        "root resolved to: {:?}",
        fs::canonicalize(&root).unwrap_or(root.clone())
    );

    if args.purge {
        for p in [
            SORTED_DIR,
            FAIL_AUTHOR_DIR,
            FAIL_TITLE_DIR,
            "logs/sortbook.log",
        ] {
            let path = root.join(p);
            if path.exists() {
                let _ = fs::remove_dir_all(&path).or_else(|_| fs::remove_file(&path));
            }
        }
        let state = root.join("logs/sortbook_state.jsonl");
        if state.exists() {
            let _ = fs::remove_file(&state);
        }
        info!("Purge done, starting sorting...");
    }

    // Set up the logger here (after purge) to avoid wiping the file
    if args.debug || !args.log_file.is_empty() {
        let log_path = if !args.log_file.is_empty() {
            PathBuf::from(&args.log_file)
        } else {
            root.join("logs").join("sortbook.log")
        };
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::File::create(&log_path) {
            Ok(file) => {
                let cfg = ConfigBuilder::new()
                    .set_time_format_rfc3339()
                    .set_target_level(LevelFilter::Off)
                    .build();
                if let Err(e) = WriteLogger::init(LevelFilter::Debug, cfg, file) {
                    eprintln!("[warn] file logger init failed: {e}");
                } else {
                    println!("[debug] logs → {:?}", log_path);
                }
            }
            Err(e) => {
                eprintln!("[warn] cannot create log file {:?}: {e}", log_path);
                env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
                    .init();
            }
        }
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    }

    let (sorted_dir, fail_author_dir, fail_title_dir) = ensure_dirs(&root)?;
    debug!("sorted_dir: {:?}", &sorted_dir);
    debug!("fail_author_dir: {:?}", &fail_author_dir);
    debug!("fail_title_dir: {:?}", &fail_title_dir);

    let livres_bruts = root.join(RAW_DIR).join(&args.ext);
    debug!("scanning input dir: {:?}", &livres_bruts);
    if !livres_bruts.exists() {
        return Err(anyhow!("Input folder not found: {:?}", livres_bruts));
    }

    let mut files: Vec<PathBuf> = vec![];
    for entry in WalkDir::new(&livres_bruts).max_depth(1) {
        let entry = entry?;
        if entry.file_type().is_file() {
            files.push(entry.path().to_path_buf());
        }
    }
    debug!("found {} files before limit", files.len());
    if args.limit > 0 {
        files.truncate(args.limit);
    }
    debug!("processing up to {} files", files.len());
    let pb = ProgressBar::new(files.len() as u64);
    // Main progress bar (global progress)
    pb.set_style(
        ProgressStyle::with_template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {msg}")
            .unwrap(),
    );
    // No spinner: keep one line per file in console

    let conn = open_db(&root)?;
    // Load author hints once to guide the LLM
    let author_hints = load_author_hints(&conn, args.author_hints).unwrap_or_default();

    // Build a set of already successfully processed files to enable resume-by-default.
    // Only count success modes so that failed ones are retried automatically.
    let state_path = root.join("logs").join("sortbook_state.jsonl");
    let mut seen_ok: HashSet<String> = HashSet::new();
    if state_path.exists() {
        if let Ok(content) = fs::read_to_string(&state_path) {
            for line in content.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    let mode_ok = v
                        .get("mode")
                        .and_then(|m| m.as_str())
                        .map(|m| matches!(m, "strict" | "full-normal" | "full-brut" | "normal"))
                        .unwrap_or(false);
                    if mode_ok {
                        if let Some(p) = v.get("path").and_then(|x| x.as_str()) {
                            seen_ok.insert(p.to_string());
                        }
                    }
                }
            }
        }
    }
    let mut state_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&state_path)?;
    // Dedicated JSONL log for copy failures
    let mut copy_fail_log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("logs").join(COPY_FAIL_LOG))?;

    for (idx, file) in files.iter().enumerate() {
        let filename = file.file_name().unwrap().to_string_lossy().to_string();
        // Persistent display: current file and mode
        let mode = args.mode.to_lowercase();
        println!("→ File #{idx} [{mode}]: {}", filename);
        debug!("processing file {} -> {:?}", idx, file);
        let t_file_start = Instant::now();
        // Skip file if already processed successfully in a previous run
        let canon = match fs::canonicalize(&file) {
            Ok(p) => p.display().to_string(),
            Err(_) => file.display().to_string(),
        };
        if seen_ok.contains(&canon) {
            pb.inc(1);
            pb.set_message(format!("#{idx} already processed"));
            continue;
        }
        let prompt_base = format!(
            r#"Réponds UNIQUEMENT en JSON compact sans texte hors JSON.
{{
  "title": string|null,
  "title_normalized": string|null,
  "author_firstname": string|null,
  "author_lastname": string|null
}}
Règles:
- favoris le titre français si probable
- si incertain -> null
- n'ajoute pas d'explication
Nom de fichier: {filename}
"#
        );
        let prompt = build_llm_prompt(&prompt_base, &author_hints);
        let t_llm_start = Instant::now();
        let guess = match call_ollama_mistral(&prompt).await {
            Ok(g) => {
                debug!("LLM guess: {:?}", g);
                g
            }
            Err(e) => {
                warn!("LLM failure: {e}");
                LlmGuess {
                    title: None,
                    title_normalized: None,
                    author_firstname: None,
                    author_lastname: None,
                }
            }
        };
        debug!("timing llm: {} ms", t_llm_start.elapsed().as_millis());

        let title = guess.title.as_deref();
        // fallback normalization if title_normalized is missing
        let title_norm = title.map(normalize_text).unwrap_or_default();

        // Skip if already processed
        let canon = fs::canonicalize(&file)
            .unwrap_or(file.clone())
            .to_string_lossy()
            .to_string();
        if seen_ok.contains(&canon) {
            pb.inc(1);
            pb.set_message(format!("#{idx} already processed"));
            continue;
        }

        // Modes
        let mode = args.mode.to_lowercase();
        if mode == "normal" {
            // Mode normal: on s'appuie uniquement sur l'auteur
            let (mut first, mut last) = (String::new(), String::new());
            if let (Some(f), Some(l)) = (
                guess.author_firstname.as_deref(),
                guess.author_lastname.as_deref(),
            ) {
                first = f.to_string();
                last = l.to_string();
            }
            let mut ok = false;
            if !first.is_empty() && !last.is_empty() {
                let norm1 = normalize_name(&first, &last);
                ok = find_author_by_name_norm(&conn, &norm1)?.is_some();
                if !ok {
                    let norm2 = normalize_name(&last, &first);
                    ok = find_author_by_name_norm(&conn, &norm2)?.is_some();
                    if ok {
                        std::mem::swap(&mut first, &mut last);
                    }
                }
            }

            if ok {
                let out_dir = sorted_dir.join(format_author_dir(&first, &last));
                fs::create_dir_all(&out_dir).ok();
                let dest_path = out_dir.join(&filename);
                // Copy with failure tolerance: if copy fails, log to COPY_FAIL_LOG and continue (no move).
                if let Err(e) = fs::copy(&file, &dest_path) {
                    warn!("copy failed: {} -> {:?} ({})", file.display(), dest_path, e);
                    let rec = serde_json::json!({
                        "path": canon,
                        "context": "normal",
                        "error": e.to_string(),
                        "ts": chrono::Utc::now().to_rfc3339()
                    });
                    use std::io::Write;
                    writeln!(copy_fail_log, "{}", rec.to_string())?;
                    copy_fail_log.flush()?;
                    pb.inc(1);
                    pb.set_message(format!("#{idx} copy failure"));
                    continue;
                }
                // save state
                let rec = serde_json::json!({"path": canon, "mode": "normal", "ts": chrono::Utc::now().to_rfc3339()});
                use std::io::Write;
                writeln!(state_file, "{}", rec.to_string())?;
                state_file.flush()?;
                pb.inc(1);
                pb.set_message(format!("#{idx} OK (normal)"));
                continue;
            } else {
                let dest = fail_author_dir.join(filename.clone());
                fs::copy(&file, &dest).ok();
                let rec = serde_json::json!({"path": canon, "mode": "normal-fail", "ts": chrono::Utc::now().to_rfc3339()});
                use std::io::Write;
                writeln!(state_file, "{}", rec.to_string())?;
                state_file.flush()?;
                pb.inc(1);
                pb.set_message(format!("#{idx} fail author (normal)"));
                continue;
            }
        }

        if title_norm.is_empty() {
            // No title -> fail title
            let dest = fail_title_dir.join(filename.clone());
            fs::copy(&file, &dest).ok();
            let rec = serde_json::json!({"path": canon, "mode": "strict-fail-title", "ts": chrono::Utc::now().to_rfc3339()});
            use std::io::Write;
            writeln!(state_file, "{}", rec.to_string())?;
            state_file.flush()?;
            pb.inc(1);
            pb.set_message(format!("#{idx} fail title"));
            continue;
        }

        // Recherche DB: d'abord LIKE sur titre original, puis fallback sur title_normalized
        // Strict mode: keep detailed timings for each attempt
        let original_title = title.unwrap_or("");
        let mut db_hit;
        let t_strict_all = Instant::now();
        db_hit = find_work_strict_like(&conn, original_title, &title_norm)?;
        debug!(
            "timing strict-all: {} ms",
            t_strict_all.elapsed().as_millis()
        );
        debug!("DB hit by title_norm: {} -> {:?}", &title_norm, &db_hit);
        if db_hit.is_none() {
            // essayer par auteur si l'IA en propose un
            let t_strict_author = Instant::now();
            if let (Some(f), Some(l)) = (
                guess.author_firstname.as_deref(),
                guess.author_lastname.as_deref(),
            ) {
                let author_norm = normalize_name(f, l);
                debug!("trying author match: {} {} (norm={})", f, l, author_norm);
                if let Some((aid, alts)) = find_author_by_name_norm(&conn, &author_norm)? {
                    debug!("author match -> id={} alternates={:?}", aid, alts);
                    let mut ids = vec![aid];
                    ids.extend(alts);
                    db_hit = find_work_by_title_and_author(&conn, &title_norm, &ids)?;
                    debug!("DB hit by title+author: {:?}", &db_hit);
                }
            }
            debug!(
                "timing strict-author: {} ms",
                t_strict_author.elapsed().as_millis()
            );
        }

        // When title matches, verify that the LLM author (if any) is consistent
        if let Some((_, _, ref wauthor_id)) = db_hit {
            let t_author_consistency = Instant::now();
            if let (Some(f), Some(l)) = (
                guess.author_firstname.as_deref(),
                guess.author_lastname.as_deref(),
            ) {
                let author_norm = normalize_name(f, l);
                if let Some((aid, mut alts)) = find_author_by_name_norm(&conn, &author_norm)? {
                    alts.push(aid);
                    if !(wauthor_id.is_empty() || alts.iter().any(|x| x == wauthor_id)) {
                        debug!(
                            "strict: title OK but author mismatch: {} vs {:?}",
                            author_norm, wauthor_id
                        );
                        db_hit = None;
                    }
                }
            }
            debug!(
                "timing strict-consistency: {} ms",
                t_author_consistency.elapsed().as_millis()
            );
        }

        // In mode full, if strict path fails, fall back to normal workflow
        if db_hit.is_none() && mode == "full" {
            let (mut first, mut last) = (String::new(), String::new());
            let t_normal = Instant::now();
            if let (Some(f), Some(l)) = (
                guess.author_firstname.as_deref(),
                guess.author_lastname.as_deref(),
            ) {
                first = f.to_string();
                last = l.to_string();
            }
            let mut ok = false;
            if !first.is_empty() && !last.is_empty() {
                let norm1 = normalize_name(&first, &last);
                ok = find_author_by_name_norm(&conn, &norm1)?.is_some();
                if !ok {
                    let norm2 = normalize_name(&last, &first);
                    ok = find_author_by_name_norm(&conn, &norm2)?.is_some();
                    if ok {
                        std::mem::swap(&mut first, &mut last);
                    }
                }
            }
            if !ok {
                let dest = fail_author_dir.join(filename.clone());
                fs::copy(&file, &dest).ok();
                let rec = serde_json::json!({"path": canon, "mode": "full-fail", "ts": chrono::Utc::now().to_rfc3339()});
                use std::io::Write;
                writeln!(state_file, "{}", rec.to_string())?;
                state_file.flush()?;
                pb.inc(1);
                pb.set_message(format!("#{idx} fail (full)"));
                continue;
            } else {
                let out_dir = sorted_dir.join(format_author_dir(&first, &last));
                fs::create_dir_all(&out_dir).ok();
                let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("bin");
                let final_title = title.unwrap_or(&filename).to_string();
                let dest_path =
                    out_dir.join(format!("{} - {} {}.{}", final_title, first, last, ext));
                let t_copy = Instant::now();
                if let Err(e) = fs::copy(&file, &dest_path) {
                    warn!("copy failed: {} -> {:?} ({})", file.display(), dest_path, e);
                    let rec = serde_json::json!({
                        "path": canon,
                        "context": "full-normal",
                        "error": e.to_string(),
                        "ts": chrono::Utc::now().to_rfc3339()
                    });
                    use std::io::Write;
                    writeln!(copy_fail_log, "{}", rec.to_string())?;
                    copy_fail_log.flush()?;
                    pb.inc(1);
                    pb.set_message(format!("#{idx} copy failure"));
                    continue;
                }
                debug!("timing copy: {} ms", t_copy.elapsed().as_millis());
                if which::which("ebook-meta").is_ok() {
                    let _ = Command::new("ebook-meta")
                        .arg(&dest_path)
                        .arg("--title")
                        .arg(&final_title)
                        .arg("--authors")
                        .arg(format!("{} {}", first, last))
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .await;
                }
                let rec = serde_json::json!({"path": canon, "mode": "full-normal", "ts": chrono::Utc::now().to_rfc3339()});
                use std::io::Write;
                writeln!(state_file, "{}", rec.to_string())?;
                state_file.flush()?;
                pb.inc(1);
                pb.set_message(format!("#{idx} OK (full→normal)"));
                debug!("timing normal: {} ms", t_normal.elapsed().as_millis());
                continue;
            }
        }

        if db_hit.is_none() {
            // In full mode, try a "raw" fallback without LLM: deduce author from filename tokens
            if mode == "full" {
                let t_brut = Instant::now();
                let fname_norm = normalize_text(&filename);
                let tokens: Vec<&str> = fname_norm.split(' ').filter(|t| !t.is_empty()).collect();
                let mut brute_ok = false;
                let mut bf = String::new();
                let mut bl = String::new();
                // Try all token pairs like (first, last)
                'outer: for i in 0..tokens.len() {
                    for j in (i + 1)..tokens.len() {
                        let f = tokens[i];
                        let l = tokens[j];
                        let norm = normalize_name(f, l);
                        if find_author_by_name_norm(&conn, &norm)?.is_some() {
                            let low = fname_norm.as_str();
                            if low.contains(f) && low.contains(l) {
                                brute_ok = true;
                                bf = f.to_string();
                                bl = l.to_string();
                                break 'outer;
                            }
                        }
                    }
                }
                if brute_ok {
                    let out_dir = sorted_dir.join(format_author_dir(&bf, &bl));
                    fs::create_dir_all(&out_dir).ok();
                    let dest_path = out_dir.join(&filename);
                    let t_copy = Instant::now();
                    if let Err(e) = fs::copy(&file, &dest_path) {
                        warn!("copy failed: {} -> {:?} ({})", file.display(), dest_path, e);
                        let rec = serde_json::json!({
                            "path": canon,
                            "context": "full-raw",
                            "error": e.to_string(),
                            "ts": chrono::Utc::now().to_rfc3339()
                        });
                        use std::io::Write;
                        writeln!(copy_fail_log, "{}", rec.to_string())?;
                        copy_fail_log.flush()?;
                        pb.inc(1);
                        pb.set_message(format!("#{idx} copy failure"));
                        continue;
                    }
                    debug!("timing copy: {} ms", t_copy.elapsed().as_millis());
                    let rec = serde_json::json!({"path": canon, "mode": "full-raw", "ts": chrono::Utc::now().to_rfc3339()});
                    use std::io::Write;
                    writeln!(state_file, "{}", rec.to_string())?;
                    state_file.flush()?;
                    pb.inc(1);
                    pb.set_message(format!("#{idx} OK (full→raw)"));
                    debug!("timing raw: {} ms", t_brut.elapsed().as_millis());
                    continue;
                }
            }
            let dest = fail_author_dir.join(filename.clone());
            fs::copy(&file, &dest).ok();
            let rec = serde_json::json!({"path": canon, "mode": "strict-fail", "ts": chrono::Utc::now().to_rfc3339()});
            use std::io::Write;
            writeln!(state_file, "{}", rec.to_string())?;
            state_file.flush()?;
            pb.inc(1);
            pb.set_message(format!("#{idx} unknown DB"));
            continue;
        }
        let (work_id, db_title, db_author_id) = db_hit.unwrap();

        // Retrieve OpenLibrary metadata (optional)
        let meta_title_owned: String;
        let meta_title = if args.no_ol_meta {
            db_title.as_str()
        } else {
            let t_ol = Instant::now();
            let title_str = match fetch_openlibrary_work_meta(&work_id).await {
                Ok(doc) => {
                    if let Some(t) = doc.title {
                        meta_title_owned = t;
                        meta_title_owned.as_str()
                    } else {
                        db_title.as_str()
                    }
                }
                Err(_) => db_title.as_str(),
            };
            debug!("timing openlibrary: {} ms", t_ol.elapsed().as_millis());
            title_str
        };

        // Construire auteur/titre finaux
        let (first, last) = match (
            guess.author_firstname.as_deref(),
            guess.author_lastname.as_deref(),
        ) {
            (Some(f), Some(l)) if !f.is_empty() && !l.is_empty() => (f.to_string(), l.to_string()),
            _ => {
                // fallback: auteur inconnu
                (String::from(""), String::from(""))
            }
        };

        if first.is_empty() || last.is_empty() {
            // missing author
            let dest = fail_author_dir.join(filename.clone());
            fs::copy(&file, &dest).ok();
            let rec = serde_json::json!({"path": canon, "mode": "strict-fail-author", "ts": chrono::Utc::now().to_rfc3339()});
            use std::io::Write;
            writeln!(state_file, "{}", rec.to_string())?;
            state_file.flush()?;
            pb.inc(1);
            pb.set_message(format!("#{idx} fail author"));
            continue;
        }

        let out_dir = sorted_dir.join(format_author_dir(&first, &last));
        fs::create_dir_all(&out_dir).ok();
        let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("bin");
        let final_title = meta_title;
        let dest_path = out_dir.join(format!("{} - {} {}.{}", final_title, first, last, ext));
        let t_copy = Instant::now();
        if let Err(e) = fs::copy(&file, &dest_path) {
            warn!("copy failed: {} -> {:?} ({})", file.display(), dest_path, e);
            let rec = serde_json::json!({
                "path": canon,
                "context": "strict",
                "error": e.to_string(),
                "ts": chrono::Utc::now().to_rfc3339()
            });
            use std::io::Write;
            writeln!(copy_fail_log, "{}", rec.to_string())?;
            copy_fail_log.flush()?;
            pb.inc(1);
            pb.set_message(format!("#{idx} copy failure"));
            continue;
        }
        debug!("timing copy: {} ms", t_copy.elapsed().as_millis());

        // Overwrite metadata via calibre (ebook-meta)
        if which::which("ebook-meta").is_ok() {
            let t_meta = Instant::now();
            let _ = Command::new("ebook-meta")
                .arg(&dest_path)
                .arg("--title")
                .arg(final_title)
                .arg("--authors")
                .arg(format!("{} {}", first, last))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
            debug!("timing ebook-meta: {} ms", t_meta.elapsed().as_millis());
        } else {
            debug!("ebook-meta not found; metadata not overwritten");
        }

        pb.inc(1);
        // record state
        let rec = serde_json::json!({"path": canon, "mode": "strict", "ts": chrono::Utc::now().to_rfc3339(), "work_id": work_id});
        use std::io::Write;
        writeln!(state_file, "{}", rec.to_string())?;
        state_file.flush()?;
        pb.set_message(format!("#{idx} OK {}", work_id));
        debug!("timing file: {} ms", t_file_start.elapsed().as_millis());
    }

    pb.finish_with_message("Done");
    // fin
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    run().await
}
