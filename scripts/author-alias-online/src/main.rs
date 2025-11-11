use anyhow::{anyhow, Context, Result};
use clap::Parser;
use regex::Regex;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

#[derive(Parser, Debug)]
#[command(name = "author-alias-online", about = "Resolve author aliases online (Wikidata) and export CSV")] 
struct Cli {
    /// Root directory that contains author folders (one level deep)
    #[arg(long, default_value = "output/sorted_book")]
    root: PathBuf,

    /// Output CSV path
    #[arg(long, default_value = "data/online_aliases.csv")]
    out_csv: PathBuf,

    /// Prefer label language (en|fr). Falls back to whatever is available.
    #[arg(long, default_value = "en")]
    prefer_lang: String,

    /// Max number of authors to query (0 = all)
    #[arg(long, default_value_t = 0)]
    limit: usize,

    /// HTTP timeout in seconds
    #[arg(long, default_value_t = 5)]
    timeout: u64,

    /// Dry-run: show actions without writing CSV or moving files
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    dry_run: bool,

    /// Verbose actions (prints renames/moves)
    #[arg(long, default_value_t = false)]
    verbose: bool,
}

#[derive(Debug, Deserialize)]
struct WikidataSearchResponse {
    search: Vec<WikidataSearchItem>,
}

#[derive(Debug, Deserialize)]
struct WikidataSearchItem {
    id: String,
    label: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WikidataEntityResponse {
    entities: std::collections::HashMap<String, WikidataEntity>,
}

#[derive(Debug, Deserialize)]
struct WikidataEntity {
    claims: Option<std::collections::HashMap<String, Vec<WikidataClaim>>>,
    labels: Option<std::collections::HashMap<String, WikidataLabel>>, // fallback
}

#[derive(Debug, Deserialize)]
struct WikidataLabel { value: String }

#[derive(Debug, Deserialize)]
struct WikidataClaim { mainsnak: WikidataSnak }

#[derive(Debug, Deserialize)]
struct WikidataSnak { datavalue: Option<WikidataValue> }

#[derive(Debug, Deserialize)]
struct WikidataValue { value: serde_json::Value }

fn main() -> Result<()> {
    let cli = Cli::parse();
    if !cli.root.exists() {
        return Err(anyhow!("Root directory {:?} does not exist", cli.root));
    }
    let authors = list_author_dirs(&cli.root)?;
    if authors.is_empty() {
        println!("No author directories under {:?}", cli.root);
        return Ok(());
    }
    let mut writer = if cli.dry_run {
        None
    } else {
        let mut w = csv::WriterBuilder::new()
            .has_headers(true)
            .from_path(&cli.out_csv)
            .with_context(|| format!("Open output CSV {:?}", &cli.out_csv))?;
        w.write_record(["author_local", "wikidata_id", "label", "description", "score", "source"]) ? ;
        Some(w)
    };

    let mut count = 0usize;
    let mut resolved = 0usize;
    for name in authors {
        if cli.limit > 0 && count >= cli.limit {
            break;
        }
        let query = normalize_query(&name);
        match wikidata_search(&query, &cli.prefer_lang, cli.timeout) {
            Some((id, label, desc, score)) => {
                if let Some(w) = writer.as_mut() {
                    w.write_record([name.as_str(), id.as_str(), label.as_str(), desc.as_str(), &format!("{score:.2}"), "wikidata"]) ? ;
                }
                // Determine target folder now for display
                let (first, last) = enrich_first_last_with_wikidata(&id, &label, cli.timeout)
                    .unwrap_or_else(|| pick_first_last(&name, &label));
                let target_display = format_author_dir(&first, &last);
                println!("OK  {:<40} -> {} ({}) [score={:.2}] — {} | target: {}", name, id, label, score, truncate(&desc, 80), target_display);
                // Apply move if not dry-run
                if let Err(e) = maybe_move_author_folder(&cli, &name, &label) {
                    eprintln!("WARN move '{}': {}", name, e);
                }
                resolved += 1;
            }
            None => {
                if let Some(w) = writer.as_mut() {
                    w.write_record([name.as_str(), "", "", "", "0.00", ""]) ? ;
                }
                println!("MISS {:<40}", name);
            }
        }
        count += 1;
    }
    if let Some(mut w) = writer { w.flush()?; println!("Done. Wrote {:?}", cli.out_csv); }
    println!("Summary: processed {}, resolved {}", count, resolved);
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { return s.to_string(); }
    let mut out = s.chars().take(max).collect::<String>();
    out.push_str("…");
    out
}

fn list_author_dirs(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name().to_string_lossy().trim().to_string();
            if !name.is_empty() {
                names.push(name);
            }
        }
    }
    names.sort();
    Ok(names)
}

fn maybe_move_author_folder(cli: &Cli, local_name: &str, canonical_label: &str) -> Result<()> {
    // Build destination in canonical format: "Last, First" without accents, safe for filesystem
    // Try to fetch explicit given/family names from Wikidata when possible
    let (first, last) = pick_first_last(local_name, canonical_label);
    let target_display = format_author_dir(&first, &last);
    let safe_target = sanitize_dir_name(&target_display);
    if safe_target.is_empty() { return Ok(()); }
    let src = cli.root.join(local_name);
    let dst = cli.root.join(&safe_target);
    if !src.exists() { return Ok(()); }
    if src == dst { return Ok(()); }
    if cli.verbose {
        println!("MOVE {} -> {} (from label: {})", src.display(), dst.display(), canonical_label);
    }
    if cli.dry_run { return Ok(()); }
    if dst.exists() {
        // merge: move files then remove empty src
        merge_dirs(&src, &dst)?;
        fs::remove_dir_all(&src).ok();
    } else {
        match fs::rename(&src, &dst) {
            Ok(_) => {}
            Err(_) => {
                fs::create_dir_all(&dst)?;
                merge_dirs(&src, &dst)?;
                fs::remove_dir_all(&src).ok();
            }
        }
    }
    Ok(())
}

fn sanitize_dir_name(name: &str) -> String {
    let s = name.trim();
    if s.is_empty() { return String::new(); }
    let mut out = String::new();
    for ch in s.chars() {
        if ch == '/' || ch == '\\' || ch == ':' || ch == '|' || ch == '"' || ch == '<' || ch == '>' || ch == '*' || ch == '?' { out.push('_'); }
        else { out.push(ch); }
    }
    out
}

fn strip_accents(s: &str) -> String { s.nfkd().filter(|c| !is_combining_mark(*c)).collect() }

fn capitalize_words(value: &str) -> String {
    value
        .split_whitespace()
        .map(|token| {
            let mut chars = token.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_author_dir(first: &str, last: &str) -> String {
    let first = strip_accents(first).trim().to_string();
    let last = strip_accents(last).trim().to_string();
    let first = capitalize_words(&first);
    let last = capitalize_words(&last);
    if !last.is_empty() && !first.is_empty() {
        format!("{}, {}", last, first)
    } else if !last.is_empty() {
        last
    } else {
        first
    }
}

fn pick_first_last(local_name: &str, label: &str) -> (String, String) {
    // Prefer parsing the local name if already in "Last, First"; otherwise derive from label
    if let Some(idx) = local_name.find(',') {
        let last = local_name[..idx].trim();
        let first = local_name[idx + 1..].trim();
        if !last.is_empty() && !first.is_empty() {
            return (first.to_string(), last.to_string());
        }
    }
    // Fallback: parse label as "First Last [Middle...]" -> take last token as last name
    let tokens: Vec<&str> = label.split_whitespace().collect();
    if tokens.len() >= 2 {
        let last = tokens.last().unwrap().to_string();
        let first = tokens[..tokens.len() - 1].join(" ");
        (first, last)
    } else {
        (label.to_string(), String::new())
    }
}

fn enrich_first_last_with_wikidata(qid: &str, fallback_label: &str, timeout: u64) -> Option<(String, String)> {
    // qid like "Q42". Fetch P735 (given name) and P734 (family name)
    if !qid.starts_with('Q') { return None; }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout))
        .user_agent("author-alias-online/0.1")
        .build().ok()?;
    let url = "https://www.wikidata.org/w/api.php";
    let resp = client.get(url).query(&[
        ("action", "wbgetentities"),
        ("ids", qid),
        ("format", "json"),
        ("languages", "en|fr"),
        ("props", "claims|labels"),
    ]).send().ok()?;
    if !resp.status().is_success() { return None; }
    let data: WikidataEntityResponse = resp.json().ok()?;
    let entity = data.entities.get(qid)?;
    let claims = entity.claims.as_ref()?;
    let mut given = None;
    let mut family = None;
    if let Some(items) = claims.get("P735") { // given name
        if let Some(val) = extract_label_from_claim(items, &client, timeout) { given = Some(val); }
    }
    if let Some(items) = claims.get("P734") { // family name
        if let Some(val) = extract_label_from_claim(items, &client, timeout) { family = Some(val); }
    }
    match (given, family) {
        (Some(g), Some(f)) => Some((g, f)),
        _ => {
            // fallback to heuristic from label
            Some(pick_first_last(fallback_label, fallback_label).0)
                .and_then(|_| Some(pick_first_last(fallback_label, fallback_label)))
        }
    }
}

fn extract_label_from_claim(claims: &Vec<WikidataClaim>, client: &reqwest::blocking::Client, timeout: u64) -> Option<String> {
    // claim mainsnak datavalue.value.entity-type/id structure
    for c in claims {
        if let Some(val) = &c.mainsnak.datavalue {
            if let Some(id) = val.value.get("id").and_then(|v| v.as_str()) {
                // fetch a label for this id in en or fr
                let url = "https://www.wikidata.org/w/api.php";
                let resp = client.get(url).query(&[
                    ("action", "wbgetentities"),
                    ("ids", id),
                    ("format", "json"),
                    ("languages", "en|fr"),
                    ("props", "labels"),
                ]).send().ok()?;
                if !resp.status().is_success() { continue; }
                let data: WikidataEntityResponse = resp.json().ok()?;
                if let Some(ent) = data.entities.get(id) {
                    if let Some(labels) = &ent.labels {
                        if let Some(l) = labels.get("en").or_else(|| labels.get("fr")) { return Some(l.value.clone()); }
                    }
                }
            }
        }
    }
    None
}

fn merge_dirs(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let p = entry.path();
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&target)?;
            merge_dirs(&p, &target)?;
        } else {
            if target.exists() {
                // keep larger file
                let src_len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let dst_len = fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                if src_len > dst_len { fs::rename(&p, &target).or_else(|_| { fs::copy(&p, &target).map(|_| ()) })?; }
                else { fs::remove_file(&p).ok(); }
            } else {
                fs::rename(&p, &target).or_else(|_| { fs::copy(&p, &target).map(|_| ()) })?;
            }
        }
    }
    Ok(())
}

fn normalize_query(name: &str) -> String {
    // Remove brackets/parentheses, collapse whitespace, prefer "Last, First" -> "First Last"
    let bracket_re = Regex::new(r"\[[^\]]+\]").unwrap();
    let paren_re = Regex::new(r"\([^\)]+\)").unwrap();
    let mut s = bracket_re.replace_all(name, "").to_string();
    s = paren_re.replace_all(&s, "").to_string();
    s = s.nfkd().collect::<String>();
    s = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c.is_whitespace() || c == ',' { c } else { ' ' })
        .collect::<String>();
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(idx) = s.find(',') {
        let last = s[..idx].trim();
        let first = s[idx + 1..].trim();
        if !first.is_empty() && !last.is_empty() {
            return format!("{} {}", first, last);
        }
    }
    s
}

fn wikidata_search(query: &str, prefer_lang: &str, timeout: u64) -> Option<(String, String, String, f64)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout))
        .user_agent("author-alias-online/0.1")
        .build()
        .ok()?;
    let url = "https://www.wikidata.org/w/api.php";
    let resp = client
        .get(url)
        .query(&[
            ("action", "wbsearchentities"),
            ("search", query),
            ("format", "json"),
            ("type", "item"),
            ("language", prefer_lang),
            ("limit", "10"),
        ])
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: WikidataSearchResponse = resp.json().ok()?;
    // Scoring: normalize query and candidate labels; try both First Last and Last, First forms
    let q = normalize_for_score(query);
    let mut best: Option<(String, String, String, f64)> = None;
    for item in data.search.into_iter() {
        let label = item.label.unwrap_or_default();
        let desc = item.description.unwrap_or_default();
        let d = desc.to_ascii_lowercase();
        // normalize label as First Last and as Last, First
        let label_fl = normalize_for_score(&label);
        let inv_raw = invert_first_last(&label);
        let label_lf = inv_raw.as_ref().map(|s| normalize_for_score(s));
        let mut score = 0.0;
        // role bonus
        if d.contains("writer") || d.contains("author") || d.contains("novelist") || d.contains("poet") || d.contains("écrivain") {
            score += 0.5;
        }
        // exact token equality after normalization yields full score
        if label_fl == q || label_lf.as_deref() == Some(&q) {
            score = 1.0;
        }
        // choose best overlap against query (F1-like instead of Jaccard)
        let ov_fl = token_overlap_f1(&q, &label_fl);
        let ov_lf = label_lf.as_deref().map(|s| token_overlap_f1(&q, s)).unwrap_or(0.0);
        let ov = ov_fl.max(ov_lf);
        // Exact or near-exact after inversion gets full score
        if let Some(lf_norm) = &label_lf {
            if lf_norm == &q { score = 1.0; }
        }
        if score < 1.0 {
            let mut partial = 0.5 * ov;
            if ov >= 0.90 { partial = 0.5; }
            score += partial;
        }
        if let Some((_, _, _, best_score)) = &best { if *best_score >= score { continue; } }
        best = Some((item.id, label, desc, score));
    }
    best
}

fn token_overlap(a: &str, b: &str) -> f64 {
    let at: std::collections::HashSet<_> = a.split_whitespace().collect();
    let bt: std::collections::HashSet<_> = b.split_whitespace().collect();
    if at.is_empty() || bt.is_empty() { return 0.0; }
    let inter = at.intersection(&bt).count() as f64;
    let uni = at.union(&bt).count() as f64;
    if uni == 0.0 { 0.0 } else { inter/uni }
}

fn token_overlap_f1(a: &str, b: &str) -> f64 {
    let a_tokens: Vec<&str> = a.split_whitespace().collect();
    let b_tokens: Vec<&str> = b.split_whitespace().collect();
    if a_tokens.is_empty() || b_tokens.is_empty() { return 0.0; }
    let a_set: std::collections::HashSet<&str> = a_tokens.iter().copied().collect();
    let b_set: std::collections::HashSet<&str> = b_tokens.iter().copied().collect();
    let inter = a_set.intersection(&b_set).count() as f64;
    let prec = if b_set.is_empty() { 0.0 } else { inter / b_set.len() as f64 };
    let rec = if a_set.is_empty() { 0.0 } else { inter / a_set.len() as f64 };
    if prec + rec == 0.0 { 0.0 } else { 2.0 * prec * rec / (prec + rec) }
}

fn normalize_for_score(s: &str) -> String {
    let stripped = strip_accents(s);
    stripped
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn invert_first_last(label: &str) -> Option<String> {
    // Try to interpret label as "First Last [Middle...]" and invert to "Last, First"
    let tokens: Vec<&str> = label.split_whitespace().collect();
    if tokens.len() >= 2 {
        let last = tokens.last().unwrap();
        let first = tokens[..tokens.len() - 1].join(" ");
        Some(format!("{}, {}", last, first))
    } else { None }
}
