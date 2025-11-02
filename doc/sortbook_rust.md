# sortbook (Rust) — Technical Doc

Crate: `scripts/sort/` — main file `scripts/sort/src/main.rs`

Purpose
- Scan `input/<ext>/`, classify files using an LLM + OpenLibrary-backed SQLite DB, and copy them into `output/` buckets. Resume across runs and never abort on copy errors.

CLI (struct `Cli`)
- `--ext <str>`: required. Extension/folder to process (e.g., `epub`).
- `--limit <n>`: optional. Max files; `0` = no limit.
- `--debug`: optional. Enable debug logging; may write to file.
- `--purge`: optional. Purge outputs and logs before start.
- `--root <path>`: optional. Resolve project resources; default now `.`.
- `--mode <strict|normal|full|full-normal|full-raw>`: default `full`.
- `--author-hints <n>`: default `2000`; `0` disables hints.
- `--log-file <path>`: optional. If set, write logs to this file.
- `--no-ol-meta`: optional. Do not write OpenLibrary metadata back into files.

Constants
- Paths:
  - `RAW_DIR = "input"`
  - `SORTED_DIR = "output/sorted_books"`
  - `FAIL_AUTHOR_DIR = "output/fail_author"`
  - `FAIL_TITLE_DIR = "output/fail_title"`
  - `COPY_FAIL_LOG = "sortbook_copy_failures.jsonl"`
- Model:
  - `OLLAMA_MODEL = "mistral:7b"` — change here to switch model.

Key Functions
- `normalize_text(&str) -> String` (lines ~60-74): lowercase, strip accents, keep `[A-Za-z0-9\s-]`, collapse spaces.
- `extract_first_json_object(&str)` (lines ~76-103): defensive JSON recovery from noisy LLM outputs.
- `call_ollama_mistral(prompt)` (lines ~105-164): spawn `ollama run OLLAMA_MODEL`, JSON-parse answer into `LlmGuess`.
- `build_llm_prompt(base, author_hints)` (lines ~166-186): prefix strict JSON contract + optional author list, then append base prompt.
- `open_db(root)` (lines ~188-193): open `data/database/openlibrary.sqlite3` under `--root`.
- `find_work_in_db(conn, title_norm)` (lines ~195-205): exact match on `works.title_normalized`.
- `find_author_by_name_norm(conn, name_norm)` (lines ~213-224): lookup in `authors.name_normalized`, parse alternates CSV.
- `find_work_by_title_and_author(conn, title_norm, candidate_ids)` (lines ~226-257): confirm a title against specific author ids/alternates.
- `find_work_strict_like(conn, title_original, title_norm)` (lines ~259-309 + 311-334): GLOB prefix, GLOB containment on `title_normalized`, fallback `lower(title) GLOB`, then exact.
- `ensure_dirs(root)` (lines ~344-353): create output buckets.
- `run()` main flow (lines ~355-...): parse args, init logging, scan input, resume state, per-file loop, LLM call, matching by mode, copying, state/log writes.

Per-file Flow (high level)
1. Skip if in resume set (prior success).
2. Build French `prompt_base` (lines ~424-440) and combine with `build_llm_prompt`.
3. Query LLM via Ollama; parse `LlmGuess{title,title_normalized,author_firstname,author_lastname}`.
4. Normalize `title` as needed; choose strategy based on `--mode`.
5. Probe DB with `find_work_strict_like`; optionally confirm author via `find_author_by_name_norm` and `find_work_by_title_and_author`.
6. On success: compute `Author, Firstname/Title/` path, copy file; optionally write metadata unless `--no-ol-meta`.
7. On failure: copy to `fail_author` or `fail_title` as appropriate.
8. On copy error: append JSON line to `logs/sortbook_copy_failures.jsonl` and continue.
9. Append JSON line to `logs/sortbook_state.jsonl` to record outcome.

Important Lines
- Model constant: near top — `const OLLAMA_MODEL: &str = "mistral:7b";`
- Prompt literal: around lines ~426-440.
- GLOB query usage: 277-309 and 317-334.
- Resume handling: input scan and seen_ok set creation in `run()`; state file path `logs/sortbook_state.jsonl`.
- Copy-failure log file: constant `COPY_FAIL_LOG` and writing sites.

Notes
- Queries are designed to hit indexes; avoid changing to `LIKE`.
- Keep LIMITs low in exploratory queries to avoid wide scans.

