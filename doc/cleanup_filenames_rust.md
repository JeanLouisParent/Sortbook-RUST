# cleanup-filenames (Rust) — Technical Doc

Overview
- Purpose: Normalize book filenames within author directories, deduplicate variants, and enforce a simple capitalization rule for titles.
- Scope: Operates under a root containing author folders. Each subfolder is processed independently (including files at the author root as one separate group).
- Safety: Dry-run by default; no changes unless `--dry-run false` is provided.

Location
- Crate: `scripts/cleanup-filenames/`
- Binary: `cleanup-filenames`

Defaults
- `--root output/sorted_book`
- `--dry-run true`
- Processes author folders in parallel (Rayon).

CLI
- Build: `cargo build --manifest-path scripts/cleanup-filenames/Cargo.toml`
- Run:
  - `cargo run --manifest-path scripts/cleanup-filenames/Cargo.toml`
  - Options:
    - `--root <path>`: change root directory (default `output/sorted_book`).
    - `--exts <csv>`: filter by extensions (e.g., `epub,pdf`). Empty = all.
    - `--dry-run <true|false>`: apply or simulate changes (default true).
    - `--verbose`: log renames/deletions.

Behavior
- Grouping key: lowercased, punctuation removed, spaces squashed, de-accented version of the filename stem.
- Selection per group: prefer a variant that contains accents; otherwise keep the largest file by size.
- Renaming rule: only capitalize the first letter of the final title; keep original extension.
- Duplicates: remove all non-selected files when not in dry-run.
- Reporting: prints one line per author with the number of processed files.

Notes
- Output order is non-deterministic due to parallel execution.
- This utility is independent from the sorter and the author cleanup tools.

