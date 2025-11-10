# cleanup (Rust) — Technical Doc

Crate: `scripts/cleanup/` — main file `scripts/cleanup/src/main.rs`

Purpose
- Single-pass pipeline for author folders: normalize/rename directories, merge duplicates, generate `data/authors.csv`, match names against the OpenLibrary database, and consolidate every folder that shares an `author_id`. Recommended order: run the sorter (`scripts/sort`) first to populate `output/sorted_books/`, then execute `cleanup` on that tree. Both binaries remain independent, so `cleanup` can target any directory of author folders if needed.

CLI (struct `Cli`)
- `--root <path>`: directory that contains author folders (one level deep). Default `output/sorted_books`.
- `--db <path>`: OpenLibrary SQLite file, default `data/database/openlibrary.sqlite3`.
- `--csv <path>`: generated CSV path, default `data/authors.csv`.
- `--min-files <n>`: minimum number of files a folder must contain before it participates in `author_id` merges. Default `0`.
- `--probable-threshold <f64>`: minimum score for reusing a `probable_author_multi` suggestion (sequence score preferred, otherwise average). Default `0.90`.
- `--dry-run`: log planned renames/merges without touching the filesystem.

Constants
- `DEFAULT_DB`, `DEFAULT_CSV`: default paths.
- `PROBABLE_MIN_SCORE = 0.90`, `NEIGHBOR_LIMIT = 25`: scoring baseline and SQLite neighbor window.
- `INVALID_FILENAME_CHARS`, `WINDOWS_RESERVED`: characters/names replaced during sanitization.
- `SCORER_KEYS = ["seq","token","prefix","suffix","ngram","lenratio"]`: order used when serializing suggestion scores.

High-Level Flow (`run`)
1. Validate `--root` exists and log whether we run in dry-run mode.
2. `normalize_directories`: rename each folder via `normalize_author_display` (convert to `Last, First`, strip accents/punctuation) and merge immediately if the sanitized target already exists (`merge_directories` + `move_or_keep_larger`).
3. `collect_author_dirs`: list one-level directories, normalize their names, and build `AuthorEntry` structs.
4. `match_and_fill`: open SQLite, build normalized variants (`generate_candidates` → `normalize_name`), try an exact match on `authors.name_normalized`, otherwise compute a suggestion with `suggest_author`.
5. `write_authors_csv`: ensure the destination directory exists, write the header `author,author_id,author_name_db,probable_author_multi`, serialize suggestions using the legacy pipe-delimited format.
6. `merge_by_author_id`: group entries sharing the same confirmed or probable ID (subject to the configured threshold), filter by `--min-files`, pick the best destination via `alignment_score` + file count, and merge every other directory into it.

Normalization / Initial Merge
- `normalize_author_display`: strip accents (Unicode NFKD), replace dashes/underscores by spaces, reshape into `Last, First` when possible, and handle all-caps names by lowercasing before capitalization.
- `sanitize_component`: replace invalid characters with `_`, trim trailing dots/spaces, avoid reserved Windows names (`con`, `nul`, etc.).
- `rename_with_case_handling`: perform case-insensitive renames safely by using an intermediate temporary name when required.
- `merge_directories`: walk the source tree with `WalkDir`, sanitize each relative component, create directories, then delegate file moves to `move_or_keep_larger`; delete the source once empty.

SQLite Matching
- `normalized_variants` aggregates:
  - `strip_enclosures` (regex `BRACKET_RE`, `PAREN_RE`) to drop bracketed or parenthetical content.
  - `remove_numeric_tokens` to remove numeric-only tokens.
  - `reorder_initials` to push one-letter initials after full tokens when both exist.
  - Comma swaps (`"Last, First"` → `"First Last"`).
- Exact-match cache: `HashMap<String, Option<(author_id,name)>>` to avoid repeating queries.
- `suggest_author`:
  - `fetch_neighbor_candidates` runs two `name_normalized` queries (`>=` ascending, `<` descending) each limited to 25 rows, then caches the combined list.
  - Computes six metrics (`sequence_ratio`, `token_overlap_score`, `prefix_score`, `suffix_score`, `bigram_dice_score`, `length_ratio_score`), clamps them to `[0,1]`, averages them, and keeps the best candidate above 0.65 (early-exits when ≥ 0.85).
  - Serializes the winner via `format_probable_value`, e.g. `OL123|Jane Doe|avg:0.91|seq:0.95|token:0.80|...`.

CSV
- Always regenerated from the latest scan; no dependency on a pre-existing file.
- Layout matches the historical CSV sample, now written to `data/authors.csv` by default.

Merge by `author_id`
- Group confirmed IDs or probable IDs (score ≥ `--probable-threshold`). `entry_best_probable_display` supplies a fallback display name when none exists in the DB.
- `alignment_score` compares the directory name with the DB/probable name via normalized sequence ratios, also trying the `"Last First"` permutation.
- Candidates are sorted by alignment score (desc), file count (desc), and folder name; the first entry is the destination and every other folder is merged into it ( honoring `--dry-run` ).

Notes
- This binary replaces the old helper scripts (`match_authors.py`, `merge_author_dirs.py`, `merge_books.py`, `normalize_names.py`); do not reintroduce them.
- Always perform a dry run before applying changes on a large corpus.
- The generated CSV is referenced in user documentation as `data/authors.csv`; change the flag only if the documentation is updated accordingly.
