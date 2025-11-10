use anyhow::{Context, Result, anyhow};
use clap::Parser;
use once_cell::sync::Lazy;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;
use walkdir::WalkDir;

const INVALID_FILENAME_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
const WINDOWS_RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

const DEFAULT_DB: &str = "data/database/openlibrary.sqlite3";
const DEFAULT_CSV: &str = "output/authors.csv";
const PROBABLE_MIN_SCORE: f64 = 0.90;
const NEIGHBOR_LIMIT: i64 = 25;

const SCORER_KEYS: [&str; 6] = ["seq", "token", "prefix", "suffix", "ngram", "lenratio"];

static BRACKET_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[[^\]]+\]").unwrap());
static PAREN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\([^\)]+\)").unwrap());

#[derive(Parser, Debug)]
#[command(name = "cleanup", version)]
struct Cli {
    /// Root directory containing author folders.
    #[arg(long)]
    root: PathBuf,

    /// OpenLibrary SQLite database path.
    #[arg(long, default_value = DEFAULT_DB)]
    db: PathBuf,

    /// Destination CSV (matches the historical sample layout).
    #[arg(long = "csv", default_value = DEFAULT_CSV)]
    csv_path: PathBuf,

    /// Minimum number of files required before merging a folder into an author_id group.
    #[arg(long, default_value_t = 0)]
    min_files: usize,

    /// Threshold used to accept a probable author_id suggestion.
    #[arg(long, default_value_t = PROBABLE_MIN_SCORE)]
    probable_threshold: f64,

    /// Dry-run mode: log actions without touching the filesystem.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Clone)]
struct AuthorEntry {
    name: String,
    path: PathBuf,
    author_id: Option<String>,
    author_name_db: Option<String>,
    probable: Option<Suggestion>,
}

#[derive(Debug, Clone)]
struct Suggestion {
    author_id: String,
    display_name: String,
    avg_score: f64,
    seq_score: Option<f64>,
    per_metric: BTreeMap<String, f64>,
}

#[derive(Debug, Clone)]
struct CandidateRow {
    author_id: String,
    name: String,
    name_normalized: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if !cli.root.exists() {
        return Err(anyhow!("Root directory {:?} does not exist.", cli.root));
    }
    run(cli)
}

fn run(cli: Cli) -> Result<()> {
    println!(
        "Running cleanup {}on {:?}",
        if cli.dry_run { "(dry-run) " } else { "" },
        cli.root
    );
    normalize_directories(&cli.root, cli.dry_run)?;
    let mut authors = collect_author_dirs(&cli.root)?;
    if authors.is_empty() {
        println!("No author directories detected, aborting.");
        return Ok(());
    }
    match_and_fill(&cli.db, &mut authors)?;
    write_authors_csv(&cli.csv_path, &authors)?;
    merge_by_author_id(&cli, &authors)?;
    println!("Done. CSV written to {:?}.", cli.csv_path);
    Ok(())
}

fn normalize_directories(root: &Path, dry_run: bool) -> Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(root)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect();
    entries.sort();

    for original in entries {
        if !original.exists() {
            continue;
        }
        let name = original
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let normalized_display = normalize_author_display(&name);
        let sanitized = sanitize_component(&normalized_display);
        if sanitized.is_empty() {
            continue;
        }
        let target = root.join(&sanitized);
        if same_path(&original, &target) {
            continue;
        }
        if target.exists() {
            println!("Merging {} into {}", original.display(), target.display());
            merge_directories(&original, &target, dry_run)?;
        } else if dry_run {
            println!(
                "[DRY-RUN] rename {} -> {}",
                original.display(),
                target.display()
            );
        } else {
            rename_with_case_handling(&original, &target)?;
        }
    }
    Ok(())
}

fn collect_author_dirs(root: &Path) -> Result<Vec<AuthorEntry>> {
    let mut authors = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().trim().to_string();
        if name.is_empty() {
            continue;
        }
        authors.push(AuthorEntry {
            name,
            path: entry.path(),
            author_id: None,
            author_name_db: None,
            probable: None,
        });
    }
    authors.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(authors)
}

fn match_and_fill(db_path: &Path, authors: &mut [AuthorEntry]) -> Result<()> {
    let mut cache: HashMap<String, Option<(String, String)>> = HashMap::new();
    let mut neighbor_cache: HashMap<String, Vec<CandidateRow>> = HashMap::new();
    let connection = Connection::open(db_path)
        .with_context(|| format!("Impossible d'ouvrir la base {:?}", db_path))?;

    for entry in authors.iter_mut() {
        let variants = normalized_variants(&entry.name);
        let mut matched = None;
        for variant in &variants {
            if let Some(cached) = cache.get(variant) {
                if let Some(hit) = cached {
                    matched = Some(hit.clone());
                    break;
                }
                continue;
            }
            let result: Option<(String, String)> = connection
                .query_row(
                    "SELECT author_id, name FROM authors WHERE name_normalized = ?1 LIMIT 1",
                    [variant],
                    |row| {
                        let id: String = row.get(0)?;
                        let name: String = row.get(1)?;
                        Ok((id, name))
                    },
                )
                .optional()?;
            cache.insert(variant.clone(), result.clone());
            if let Some(hit) = result {
                matched = Some(hit);
                break;
            }
        }

        if let Some((id, db_name)) = matched {
            entry.author_id = Some(id);
            entry.author_name_db = Some(db_name);
            continue;
        }

        if let Some(suggestion) = suggest_author(&connection, &variants, &mut neighbor_cache)? {
            entry.probable = Some(suggestion);
        }
    }
    Ok(())
}

fn write_authors_csv(path: &Path, authors: &[AuthorEntry]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut writer = csv::WriterBuilder::new()
        .has_headers(true)
        .quote_style(csv::QuoteStyle::Necessary)
        .from_path(path)?;
    writer.write_record([
        "author",
        "author_id",
        "author_name_db",
        "probable_author_multi",
    ])?;
    for entry in authors {
        let probable_value = entry
            .probable
            .as_ref()
            .map(format_probable_value)
            .unwrap_or_default();
        writer.write_record([
            entry.name.as_str(),
            entry.author_id.as_deref().unwrap_or(""),
            entry.author_name_db.as_deref().unwrap_or(""),
            probable_value.as_str(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

fn merge_by_author_id(cli: &Cli, authors: &[AuthorEntry]) -> Result<()> {
    let mut grouped: HashMap<String, Vec<&AuthorEntry>> = HashMap::new();
    for entry in authors {
        let mut effective = entry.author_id.clone();
        if effective.is_none() {
            if let Some(probable) = &entry.probable {
                let score = probable.seq_score.unwrap_or(probable.avg_score);
                if score >= cli.probable_threshold {
                    effective = Some(probable.author_id.clone());
                }
            }
        }
        if let Some(id) = effective {
            grouped.entry(id).or_default().push(entry);
        }
    }

    for (author_id, dirs) in grouped {
        if dirs.len() < 2 {
            continue;
        }
        let mut candidates = Vec::new();
        for entry in dirs {
            if !entry.path.exists() {
                continue;
            }
            let file_count = count_files(&entry.path)?;
            if file_count < cli.min_files {
                continue;
            }
            candidates.push((entry, file_count));
        }
        if candidates.len() < 2 {
            continue;
        }
        let db_name = candidates
            .iter()
            .find_map(|(entry, _)| entry.author_name_db.clone())
            .or_else(|| entry_best_probable_display(&candidates));

        let db_name_ref = db_name.as_deref();
        candidates.sort_by(|(a_entry, a_count), (b_entry, b_count)| {
            let score_a = alignment_score(&a_entry.path, db_name_ref);
            let score_b = alignment_score(&b_entry.path, db_name_ref);
            score_b
                .partial_cmp(&score_a)
                .unwrap_or(Ordering::Equal)
                .then_with(|| b_count.cmp(a_count))
                .then_with(|| a_entry.name.cmp(&b_entry.name))
        });
        let destination = candidates.first().unwrap().0.path.clone();
        println!(
            "Merging author_id {} into {}",
            author_id,
            destination.display()
        );
        for (entry, _) in candidates.iter().skip(1) {
            println!("  - {} -> {}", entry.path.display(), destination.display());
            merge_directories(&entry.path, &destination, cli.dry_run)?;
        }
    }
    Ok(())
}

fn entry_best_probable_display(candidates: &[(&AuthorEntry, usize)]) -> Option<String> {
    for (entry, _) in candidates {
        if let Some(probable) = &entry.probable {
            return Some(probable.display_name.clone());
        }
    }
    None
}

fn alignment_score(path: &Path, db_name: Option<&str>) -> f64 {
    let db_name = match db_name {
        Some(value) if !value.trim().is_empty() => value,
        _ => return 0.0,
    };
    let dir_name = path
        .file_name()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    let dir_norm = normalize_for_compare(&dir_name);
    let db_norm = normalize_for_compare(db_name);
    if dir_norm.is_empty() || db_norm.is_empty() {
        return 0.0;
    }
    let mut variants = vec![db_norm.clone()];
    let parts: Vec<_> = db_norm.split_whitespace().collect();
    if parts.len() >= 2 {
        let mut rotated = parts.clone();
        rotated.rotate_right(1);
        variants.push(rotated.join(" "));
    }
    variants
        .into_iter()
        .map(|variant| sequence_ratio(&dir_norm, &variant))
        .fold(0.0, f64::max)
}

fn normalize_for_compare(value: &str) -> String {
    let stripped = strip_accents(value);
    stripped
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn rename_with_case_handling(src: &Path, dst: &Path) -> Result<()> {
    if same_path(src, dst) {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let src_lower = src.to_string_lossy().to_lowercase();
    let dst_lower = dst.to_string_lossy().to_lowercase();
    if src_lower == dst_lower {
        let mut temp = dst.with_extension("__tmp_case__");
        let mut index = 1;
        while temp.exists() {
            temp = dst.with_extension(format!("__tmp_case__{}", index));
            index += 1;
        }
        fs::rename(src, &temp)?;
        fs::rename(temp, dst)?;
    } else {
        fs::rename(src, dst)?;
    }
    Ok(())
}

fn merge_directories(src: &Path, dst: &Path, dry_run: bool) -> Result<()> {
    if same_path(src, dst) {
        return Ok(());
    }
    if dry_run {
        println!("[DRY-RUN] merge {} -> {}", src.display(), dst.display());
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in WalkDir::new(src).min_depth(1) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let sanitized_rel = sanitize_relative_path(rel);
        let target = dst.join(&sanitized_rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if entry.file_type().is_file() {
            fs::create_dir_all(
                target
                    .parent()
                    .ok_or_else(|| anyhow!("Invalid path {:?}", target))?,
            )?;
            move_or_keep_larger(entry.path(), &target)?;
        }
    }
    fs::remove_dir_all(src)?;
    Ok(())
}

fn sanitize_relative_path(rel: &Path) -> PathBuf {
    let mut cleaned = PathBuf::new();
    for component in rel.components() {
        let os: OsString = component.as_os_str().into();
        let part = os.to_string_lossy();
        cleaned.push(sanitize_component(&part));
    }
    cleaned
}

fn move_or_keep_larger(src: &Path, dst: &Path) -> Result<()> {
    if !dst.exists() {
        move_file(src, dst)?;
        return Ok(());
    }
    let src_size = src.metadata().map(|m| m.len()).unwrap_or(0);
    let dst_size = dst.metadata().map(|m| m.len()).unwrap_or(0);
    if src_size > dst_size {
        let tmp = dst.with_extension("old_to_delete");
        if tmp.exists() {
            fs::remove_file(&tmp).ok();
        }
        fs::rename(dst, &tmp).ok();
        move_file(src, dst)?;
        fs::remove_file(tmp).ok();
    } else {
        fs::remove_file(src).ok();
    }
    Ok(())
}

fn move_file(src: &Path, dst: &Path) -> Result<()> {
    match fs::rename(src, dst) {
        Ok(_) => Ok(()),
        Err(err) => {
            if err.kind() == std::io::ErrorKind::NotFound {
                return Ok(());
            }
            fs::copy(src, dst)?;
            fs::remove_file(src).ok();
            Ok(())
        }
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(left), Ok(right)) => left == right,
        _ => a == b,
    }
}

fn strip_accents(value: &str) -> String {
    value
        .nfkd()
        .filter(|ch| !is_combining_mark(*ch))
        .collect::<String>()
}

fn sanitize_component(value: &str) -> String {
    let mut cleaned = value
        .trim_matches(|ch: char| ch == '.' || ch.is_whitespace())
        .to_string();
    if cleaned.is_empty() {
        return "_".into();
    }
    cleaned = cleaned
        .chars()
        .map(|ch| {
            if INVALID_FILENAME_CHARS.contains(&ch) {
                '_'
            } else {
                ch
            }
        })
        .collect();
    cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        return "_".into();
    }
    let lowered = cleaned.trim().trim_matches('.').to_ascii_lowercase();
    if WINDOWS_RESERVED.contains(&lowered.as_str()) {
        return format!("_{}", cleaned);
    }
    cleaned
}

fn normalize_author_display(name: &str) -> String {
    if name.trim().is_empty() {
        return "_".into();
    }
    let mut stripped = strip_accents(name);
    stripped = stripped
        .replace('’', "'")
        .replace('`', "'")
        .replace('´', "'")
        .replace('_', " ")
        .replace('-', " ");
    stripped = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    let letters: String = stripped
        .chars()
        .filter(|ch| ch.is_ascii_alphabetic())
        .collect();
    let mut base = stripped.clone();
    if !letters.is_empty() && letters.chars().all(|ch| ch.is_ascii_uppercase()) {
        base = stripped.to_lowercase();
    }

    let (first, last) = if let Some(idx) = base.find(',') {
        let last = base[..idx].trim().to_string();
        let first = base[idx + 1..].trim().to_string();
        (first, last)
    } else {
        let tokens: Vec<_> = base.split_whitespace().collect();
        if tokens.len() >= 2 {
            let last = tokens.last().unwrap().to_string();
            let first = tokens[..tokens.len() - 1].join(" ");
            (first, last)
        } else {
            (
                tokens.get(0).map(|s| s.to_string()).unwrap_or_default(),
                String::new(),
            )
        }
    };

    let first_cap = capitalize_words(&first);
    let last_cap = capitalize_words(&last);
    if !last_cap.is_empty() {
        let mut value = format!("{}, {}", last_cap, first_cap)
            .trim()
            .trim_matches(',')
            .to_string();
        if value.is_empty() {
            value = last_cap;
        }
        value
    } else {
        first_cap
    }
}

fn capitalize_words(value: &str) -> String {
    value
        .split_whitespace()
        .map(|token| {
            let mut chars = token.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalized_variants(name: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut variants = Vec::new();
    for candidate in generate_candidates(name) {
        let normalized = normalize_name(&candidate);
        if normalized.is_empty() || !seen.insert(normalized.clone()) {
            continue;
        }
        variants.push(normalized);
    }
    variants
}

fn normalize_name(value: &str) -> String {
    let stripped = strip_accents(value);
    let lowered = stripped.to_lowercase();
    let filtered = lowered
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                ' '
            }
        })
        .collect::<String>();
    filtered.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn generate_candidates(name: &str) -> Vec<String> {
    let mut results = Vec::new();
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return results;
    }
    let mut base = vec![trimmed.to_string()];
    let stripped = strip_enclosures(trimmed);
    if stripped != trimmed {
        base.push(stripped.clone());
    }
    let digits_removed = remove_numeric_tokens(&stripped);
    if !digits_removed.is_empty() && !base.contains(&digits_removed) {
        base.push(digits_removed.clone());
    }
    let reordered = reorder_initials(&digits_removed)
        .or_else(|| reorder_initials(&stripped))
        .unwrap_or_default();
    if !reordered.is_empty() && !base.contains(&reordered) {
        base.push(reordered);
    }

    let mut seen = HashSet::new();
    for candidate in base {
        if seen.insert(candidate.clone()) {
            results.push(candidate.clone());
        }
        if candidate.contains(',') {
            if let Some((left, right)) = candidate.split_once(',') {
                let swapped = format!("{} {}", right.trim(), left.trim())
                    .trim()
                    .to_string();
                if !swapped.is_empty() && seen.insert(swapped.clone()) {
                    results.push(swapped);
                }
            }
        }
    }
    results
}

fn strip_enclosures(value: &str) -> String {
    let step = BRACKET_RE.replace_all(value, " ");
    PAREN_RE.replace_all(&step, " ").to_string()
}

fn remove_numeric_tokens(value: &str) -> String {
    value
        .split_whitespace()
        .filter(|token| !token.chars().all(|ch| ch.is_ascii_digit()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn reorder_initials(value: &str) -> Option<String> {
    let tokens: Vec<_> = value.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    let initials: Vec<_> = tokens.iter().filter(|t| t.len() == 1).cloned().collect();
    let others: Vec<_> = tokens.iter().filter(|t| t.len() > 1).cloned().collect();
    if initials.is_empty() || others.is_empty() {
        return None;
    }
    let mut combined: Vec<_> = others;
    combined.extend(initials);
    Some(combined.join(" "))
}

fn suggest_author(
    connection: &Connection,
    variants: &[String],
    cache: &mut HashMap<String, Vec<CandidateRow>>,
) -> Result<Option<Suggestion>> {
    let mut best: Option<Suggestion> = None;
    let mut best_avg = 0.0;
    for normalized in variants {
        let candidates = fetch_neighbor_candidates(connection, normalized, cache)?;
        for candidate in &candidates {
            let mut per_metric = BTreeMap::new();
            per_metric.insert(
                "seq".into(),
                clamp(sequence_ratio(normalized, &candidate.name_normalized)),
            );
            per_metric.insert(
                "token".into(),
                clamp(token_overlap_score(normalized, &candidate.name_normalized)),
            );
            per_metric.insert(
                "prefix".into(),
                clamp(prefix_score(normalized, &candidate.name_normalized)),
            );
            per_metric.insert(
                "suffix".into(),
                clamp(suffix_score(normalized, &candidate.name_normalized)),
            );
            per_metric.insert(
                "ngram".into(),
                clamp(bigram_dice_score(normalized, &candidate.name_normalized)),
            );
            per_metric.insert(
                "lenratio".into(),
                clamp(length_ratio_score(normalized, &candidate.name_normalized)),
            );
            let avg = per_metric.values().sum::<f64>() / per_metric.len() as f64;
            if avg > best_avg {
                best_avg = avg;
                best = Some(Suggestion {
                    author_id: candidate.author_id.clone(),
                    display_name: candidate.name.clone(),
                    avg_score: avg,
                    seq_score: per_metric.get("seq").copied(),
                    per_metric,
                });
            }
        }
        if best_avg >= 0.85 {
            break;
        }
    }
    if let Some(suggestion) = best {
        if best_avg >= 0.65 {
            return Ok(Some(suggestion));
        }
    }
    Ok(None)
}

fn fetch_neighbor_candidates(
    connection: &Connection,
    normalized: &str,
    cache: &mut HashMap<String, Vec<CandidateRow>>,
) -> Result<Vec<CandidateRow>> {
    if let Some(cached) = cache.get(normalized) {
        return Ok(cached.clone());
    }
    let mut rows = Vec::new();
    {
        let mut stmt = connection.prepare(
            "SELECT author_id, name, name_normalized
             FROM authors
             WHERE name_normalized >= ?1
             ORDER BY name_normalized
             LIMIT ?2",
        )?;
        let results = stmt.query_map(params![normalized, NEIGHBOR_LIMIT], |row| {
            Ok(CandidateRow {
                author_id: row.get(0)?,
                name: row.get(1)?,
                name_normalized: row.get(2)?,
            })
        })?;
        for result in results {
            rows.push(result?);
        }
    }
    {
        let mut stmt = connection.prepare(
            "SELECT author_id, name, name_normalized
             FROM authors
             WHERE name_normalized < ?1
             ORDER BY name_normalized DESC
             LIMIT ?2",
        )?;
        let results = stmt.query_map(params![normalized, NEIGHBOR_LIMIT], |row| {
            Ok(CandidateRow {
                author_id: row.get(0)?,
                name: row.get(1)?,
                name_normalized: row.get(2)?,
            })
        })?;
        for result in results {
            rows.push(result?);
        }
    }
    cache.insert(normalized.to_string(), rows.clone());
    Ok(rows)
}

fn count_files(path: &Path) -> Result<usize> {
    Ok(WalkDir::new(path)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .count())
}

fn format_probable_value(suggestion: &Suggestion) -> String {
    let mut parts = vec![
        suggestion.author_id.clone(),
        suggestion.display_name.clone(),
        format!("avg:{:.2}", suggestion.avg_score),
    ];
    for key in SCORER_KEYS {
        if let Some(score) = suggestion.per_metric.get(key) {
            parts.push(format!("{key}:{score:.2}"));
        }
    }
    parts.join("|")
}

fn clamp(value: f64) -> f64 {
    if value < 0.0 {
        0.0
    } else if value > 1.0 {
        1.0
    } else {
        value
    }
}

fn sequence_ratio(a: &str, b: &str) -> f64 {
    let lcs = lcs_length(a.as_bytes(), b.as_bytes());
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    (2.0 * lcs as f64) / (a.len() as f64 + b.len() as f64)
}

fn lcs_length(a: &[u8], b: &[u8]) -> usize {
    let mut prev = vec![0usize; b.len() + 1];
    let mut curr = vec![0usize; b.len() + 1];
    for &byte_a in a {
        for (j, &byte_b) in b.iter().enumerate() {
            curr[j + 1] = if byte_a == byte_b {
                prev[j] + 1
            } else {
                prev[j + 1].max(curr[j])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

fn token_overlap_score(a: &str, b: &str) -> f64 {
    let a_tokens: HashSet<_> = a.split_whitespace().collect();
    let b_tokens: HashSet<_> = b.split_whitespace().collect();
    if a_tokens.is_empty() || b_tokens.is_empty() {
        return 0.0;
    }
    let inter = a_tokens.intersection(&b_tokens).count() as f64;
    let union = a_tokens.union(&b_tokens).count() as f64;
    if union == 0.0 { 0.0 } else { inter / union }
}

fn prefix_score(a: &str, b: &str) -> f64 {
    let max_len = a.len().max(b.len());
    if max_len == 0 {
        return 0.0;
    }
    let mut count = 0;
    for (ch_a, ch_b) in a.chars().zip(b.chars()) {
        if ch_a == ch_b {
            count += 1;
        } else {
            break;
        }
    }
    count as f64 / max_len as f64
}

fn suffix_score(a: &str, b: &str) -> f64 {
    let a_rev: String = a.chars().rev().collect();
    let b_rev: String = b.chars().rev().collect();
    prefix_score(&a_rev, &b_rev)
}

fn bigram_dice_score(a: &str, b: &str) -> f64 {
    fn grams(value: &str) -> HashSet<String> {
        let chars: Vec<char> = value.chars().collect();
        if chars.len() < 2 {
            return chars.into_iter().map(|c| c.to_string()).collect();
        }
        (0..chars.len() - 1)
            .map(|i| format!("{}{}", chars[i], chars[i + 1]))
            .collect()
    }
    let set_a = grams(a);
    let set_b = grams(b);
    if set_a.is_empty() || set_b.is_empty() {
        return 0.0;
    }
    let intersection = set_a.intersection(&set_b).count() as f64;
    (2.0 * intersection) / (set_a.len() as f64 + set_b.len() as f64)
}

fn length_ratio_score(a: &str, b: &str) -> f64 {
    let max_len = a.len().max(b.len());
    if max_len == 0 {
        return 0.0;
    }
    1.0 - (a.len().abs_diff(b.len()) as f64 / max_len as f64)
}
