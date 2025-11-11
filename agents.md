# AGENTS.md

This file provides guidance for assistants working in this repo. Follow these rules carefully to avoid breaking workflows or performance.

## Mission
- Keep behavior identical unless the user explicitly requests changes.
- Prefer non-destructive edits. Do not reset the repo or delete data.
- Comment sparsely and only when necessary for clarity.

## Project Overview
- Python import scripts build a local OpenLibrary SQLite database from dumps under `data/dumps/` into `data/database/openlibrary.sqlite3`:
  - `scripts/import/import_authors_sqlite.py`
  - `scripts/import/import_works_sqlite.py`
- Rust cleanup utility (`scripts/cleanup`) normalizes/merges author folders under a given `--root`, produces `data/authors.csv`, matches authors against the SQLite DB, then consolidates every folder that shares the same `author_id` (or a probable ID above the configured threshold).
- Rust sorter `sortbook` lives in `scripts/sort/` and moves files from `input/<ext>/` into the `output/` buckets using the local DB.
- Logs live under `logs/`, including state (`sortbook_state.jsonl`) and copy failure logs (`sortbook_copy_failures.jsonl`).

## Key Constraints
- LLM prompt must remain in French. Do NOT translate or alter its content.
- Resume-by-default: sorter skips files already marked successful in `logs/sortbook_state.jsonl`.
- Copy failures: never stop the run. Log to `logs/sortbook_copy_failures.jsonl` and continue.
- Input/output locations are part of public docs; if you move anything, update README accordingly.

## Performance
- SQLite queries for title matching must use `GLOB` to benefit from indexes; avoid `LIKE`.
- The `works` table is very large (tens of millions). Always ensure lookups are indexable and bounded with LIMITs.

## Paths and Layout
- Input: `input/<ext>/` (e.g., `input/epub/`).
- Database: `data/database/openlibrary.sqlite3`.
- Outputs: `output/sorted_books/`, `output/fail_author/`, `output/fail_title/`.
  - Additional utility: `scripts/cleanup-filenames` normalizes book filenames inside author folders. Default root `output/sorted_book`.
  - Online resolver: `scripts/author-alias-online` fetches author aliases from Wikidata and can move/merge folders when enabled.
- Cleanup-generated CSV: `data/authors.csv` (location referenced in public docs).
- Logs: `logs/sortbook.log`, `logs/sortbook_state.jsonl`, `logs/sortbook_copy_failures.jsonl`.

## Cleanup Crate Notes
- Location: Cargo crate under `scripts/cleanup`.
- Responsibilities: (1) rename/normalize/merge author folders under the provided `--root`, (2) regenerate `data/authors.csv` with `author_id`, `author_name_db`, and `probable_author_multi`, (3) merge folders that share the same confirmed author_id (or a probable ID above the threshold).
- Build/Run:
  ```
  cargo run --manifest-path scripts/cleanup/Cargo.toml -- \
    --root output/sorted_books \
    --db data/database/openlibrary.sqlite3 \
    --csv data/authors.csv \
    [--min-files N] [--probable-threshold 0.90] [--dry-run]
  ```
- Recommended order: run the `scripts/sort` binary first (to populate `output/sorted_books/`), then execute `cleanup` on that output. The two tools remain independent if another directory needs to be processed.
- Defaults align with the sorter output tree: `--root output/sorted_books`, `--csv data/authors.csv`.

## Filename Cleanup Notes
- Location: Cargo crate under `scripts/cleanup-filenames`.
- Responsibility: normalize book filenames per subfolder, deduplicate by normalized key (prefer accented variant, else largest by size), and rename to Title with only the first letter capitalized. Removes losers when not in dry-run.
- Build/Run:
  - `cargo build --manifest-path scripts/cleanup-filenames/Cargo.toml`
  - `cargo run --manifest-path scripts/cleanup-filenames/Cargo.toml -- [--root <path>] [--exts csv] [--dry-run true|false] [--verbose]`
- Defaults:
  - `--root output/sorted_book`
  - `--dry-run true` (use `--dry-run false` to apply)
- Parallelization: processes author directories in parallel; console output order is not guaranteed.

## Online Author Alias Notes
- Location: Cargo crate under `scripts/author-alias-online`.
- Responsibility: for each author folder name, query Wikidata, score the best candidate using normalized/inverted forms, and optionally move/merge to a canonical "Last, First" folder (accents removed). Writes a CSV proof when not in dry-run.
- Build/Run:
  - `cargo build --manifest-path scripts/author-alias-online/Cargo.toml`
  - `cargo run --manifest-path scripts/author-alias-online/Cargo.toml -- [--root <path>] [--prefer-lang en|fr] [--timeout N] [--limit N] [--dry-run true|false] [--verbose]`
- Defaults/Rules:
  - `--root output/sorted_book`
  - `--dry-run true` by default; no changes unless set to false
  - Moves/merges only when score > 0.90
  - Duplicate files: keeps the largest
  - Target folder format: normalized "Last, First" without accents
  - Scoring: exact/inverted normalized match → 1.0; token F1 overlap; small role bonus (+0.1) if description indicates author-like roles
- Safety: Network use is explicit; failures/timeouts do not abort processing.
- Always use `--dry-run` before applying destructive changes; without it, moves/renames happen for real.
- This binary replaces the legacy Python scripts `match_authors.py`, `merge_author_dirs.py`, `merge_books.py`, `normalize_names.py` (do not reintroduce them).

## Rust Sorter Notes
 - Location: Cargo crate under `scripts/sort/`. Binary name: `sortbook`.
 - Build: `cargo build --manifest-path scripts/sort/Cargo.toml`.
 - Run (recommended defaults):
   - `cargo run --manifest-path scripts/sort/Cargo.toml -- --root ../.. --ext epub --mode full --author-hints 0`
 - Input/Output assumptions (resolved from `--root`):
   - Input scanned in `input/<ext>/` (e.g., `input/epub/`).
   - Outputs in `output/sorted_books/`, `output/fail_author/`, `output/fail_title/`.
   - DB at `data/database/openlibrary.sqlite3`.
   - Logs at `logs/` including `sortbook_state.jsonl` (resume) and `sortbook_copy_failures.jsonl`.
 - CLI flags (from `Cli` in `scripts/sort/src/main.rs`):
   - `--ext <str>` (required), `--limit <n>`, `--debug`, `--purge`, `--root <path>`, `--mode <strict|normal|full|full-normal|full-raw>` (default: `full`), `--author-hints <n>`, `--log-file <path>`, `--no-ol-meta`.
 - LLM model selection:
   - Default constant: `const OLLAMA_MODEL: &str = "mistral:7b";` Change here and rebuild.
 - Prompt:
   - French prompt literal in `prompt_base`. Do NOT translate or alter its content.
 - Matching behavior highlights:
   - Normalization via `normalize_text`.
   - Title-first probing in `find_work_strict_like` using `GLOB` on `works.title_normalized` (prefix → containment), fallback to `lower(title) GLOB`, then exact.
   - Optional author confirmation via `find_author_by_name_norm` and `find_work_by_title_and_author`.
 - Resilience:
   - Resume-by-default from `logs/sortbook_state.jsonl` (skip prior successes).
   - Copy failures are logged to `logs/sortbook_copy_failures.jsonl` and do not abort.

## Python Import Scripts
- Default CLI options exist for `--db`, `--dump`, `--verbose`. Paths resolve from repo root in README examples.
- Authors script rebuilds `authors` and index `idx_name_norm`.
- Works script supports `--force`, batching and commit intervals, and WAL/SHM cleanup at start.

## Safety and Approvals
- Never delete user data or auto-purge directories unless invoked via `--purge`.
- When editing, keep diffs minimal and focused on the task.
- If adding features, guard with flags and default to current behavior.

## Git and Housekeeping
- A `.gitignore` exists to ignore `input/`, `output/`, `logs/`, `data/`, and `scripts/sort/target/`.
- Do not add license headers unless requested.
- Keep code style consistent; avoid drive-by refactors.

## Handy Checks
- Confirm resume filter accepts success modes: `strict`, `normal`, `full-normal`, `full-raw`.
- Validate paths after changes with quick `rg` searches before patching README.
- When touching SQL, run `EXPLAIN QUERY PLAN` locally if possible to keep index usage.
## Repository Map (What Each File/Dir Is For)
- `README.md`: User-facing documentation (setup, prerequisites, DB import, usage, CLI, prompt, behavior overview).
- `agents.md`: This file — agent-facing runbook and constraints.
- `scripts/import/import_authors_sqlite.py`: Build `authors` table from `ol_dump_authors.txt`, normalize names, create `idx_name_norm`.
- `scripts/import/import_works_sqlite.py`: Import `works` from `ol_dump_works.txt`, batching/commit control, UPSERT, optional `VACUUM`.
- `scripts/sort/`: Rust crate for `sortbook`.
  - `scripts/sort/Cargo.toml`: Crate manifest.
  - `scripts/sort/src/main.rs`: Entire CLI implementation (args, normalization, DB queries, LLM, copy, resume).
- `data/dumps/`: Place OpenLibrary dumps here (authors, works).
- `data/database/`: SQLite DBs generated by import scripts (`openlibrary.sqlite3`).
- `input/`: Put files to sort under `input/<ext>/` (e.g., `input/epub`).
- `output/`: Sorted results and failure buckets.
  - `output/sorted_books/`: Canonical `Author, Firstname/Title/` structure.
  - `output/fail_author/`: Missing/uncertain author.
  - `output/fail_title/`: Missing/uncertain title.
- `logs/`: Runtime logs and state.
  - `logs/sortbook.log`: Debug/file logs when enabled.
  - `logs/sortbook_state.jsonl`: Success/attempt records used for resume-by-default.
  - `logs/sortbook_copy_failures.jsonl`: Copy errors; do not halt processing.

## Data Flow (Dumps → DB → Sorter → Outputs)
1. Download OpenLibrary dumps to `data/dumps/`.
2. Build the DB with the Python scripts (authors first, then works) into `data/database/openlibrary.sqlite3`.
3. Run the Rust sorter (`scripts/sort/`) with Cargo. It scans `input/<ext>/`, queries SQLite, classifies, copies to `output/`, and writes logs/state in `logs/`.

## SQLite Schema At-a-Glance
- `authors(author_id TEXT PRIMARY KEY, name TEXT, name_normalized TEXT, alternate_id TEXT)`
  - Index: `idx_name_norm(name_normalized)`
- `works(work_id TEXT UNIQUE, title TEXT, title_normalized TEXT PRIMARY KEY, author_id TEXT, alternate_id TEXT)`
  - Index: `idx_works_author_id(author_id)`

## Rust Landmarks (scripts/sort/src/main.rs)
- CLI definition: struct `Cli` with flags `--ext`, `--limit`, `--debug`, `--purge`, `--root`, `--mode`, `--author-hints`, `--log-file`, `--no-ol-meta`.
- LLM model: in `call_ollama_mistral`, `cmd.arg("run").arg("mistral:7b")`. Change here if needed.
- LLM prompt (French): `prompt_base` literal. Do NOT translate/alter content.
- Hints: `build_llm_prompt` prefixes strict JSON instructions and an optional author list.
- Normalization: `normalize_text` lowercases, strips accents/punctuation and squashes whitespace.
- Matching fast path: `find_work_strict_like` uses indexed `GLOB` on `works.title_normalized` (prefix → containment), fallback `lower(title) GLOB`, then exact normalized.
- Author checks: `find_author_by_name_norm` and `find_work_by_title_and_author` use `authors` and `works` (including alternates) to confirm candidates.
- Copy/output: `ensure_dirs`, `format_author_dir`; copy failures go to `sortbook_copy_failures.jsonl` and do not stop the run.
- Resume: JSONL state read early; successful items skipped; failures retried.

## Python Scripts Details
- `import_authors_sqlite.py`
  - CLI: `--db`, `--dump`, `--verbose`
  - Rebuilds `authors` from dump; normalizes names; creates `idx_name_norm`.
- `import_works_sqlite.py`
  - CLI: `--db`, `--dump`, `--force`, `--batch`, `--commit-interval`, `--vacuum`, `--verbose`
  - Sequential import with batching and UPSERT; index on `author_id`; WAL/SHM cleanup at start.

## Do / Don’t
Do:
- Keep behavior identical unless explicitly asked.
- Preserve directory layout and update README if paths change.
- Use `GLOB` for lookups; keep queries bounded with `LIMIT`.
- Log copy failures and continue.
- Maintain resume semantics (skip only successful outcomes).

Don’t:
- Translate or modify the French LLM prompt.
- Remove resume or make copy failures fatal.
- Replace `GLOB` with `LIKE` in queries.
- Purge outputs/logs unless `--purge` is used.

## Safe Edit Zones
- Documentation (`README.md`, `agents.md`).
- LLM model name in `call_ollama_mistral`.
- Adding CLI flags that default to current behavior.
- Extra logging (keep default noise low).

## Ask Before Editing
- Table schemas or indexes.
- Directory structure or default paths.
- Prompt content or matching mode semantics.
- Resume criteria or state file format.
