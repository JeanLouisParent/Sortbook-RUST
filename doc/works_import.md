# works_import.py — Technical Doc

Script: `scripts/import/import_works_sqlite.py`

Purpose
- Imports `ol_dump_works.txt` directly into the final DB, deduplicating by `title_normalized` and tracking alternates.

CLI
- `--db` (str, default `../../data/database/openlibrary.sqlite3`): target DB.
- `--dump` (str, default `../../data/dumps/ol_dump_works.txt`): works dump path.
- `--batch`/`-b` (int, default 100_000): batch insert size.
- `--commit-interval`/`-c` (int, default 1_000_000): commit frequency; `0` = single final commit.
- `--force`/`-f` (flag): drop and recreate `works` table.
- `--vacuum`/`-v` (flag): VACUUM at end.
- `--verbose` (flag): extra logs.

Core Functions
- `normalize_text(text: str) -> str` (lines ~36-51):
  - Lowercase, strip accents (NFD), remove non `[a-z0-9\s-]`, collapse spaces.
- `parse_line(line: str)` (lines ~54-98):
  - Parses JSON payload from 5th TSV column and returns `(work_id, title, title_norm, author_id)`.
  - Derives first available author id if present.
- `configure_connection(conn)` (lines ~121-145):
  - Applies SQLite pragmas suitable for large bulk inserts (WAL, large cache, mmap, etc.).
- `ensure_schema(conn, force)` (see creation at lines ~190-214 in file):
  - Creates `works` table with:
    - `work_id TEXT UNIQUE`
    - `title TEXT`
    - `title_normalized TEXT PRIMARY KEY`
    - `author_id TEXT`
    - `alternate_id TEXT`
- `create_indexes(conn)` (lines ~219-224):
  - Ensures index `idx_works_author_id(author_id)` exists.
- `flush_batch(conn, batch)` (lines ~241-246):
  - Executes UPSERT batch using `INSERT_SQL`.
- `import_works(...)` (lines ~249-344):
  - Orchestrates the streaming import, batching, periodic commits, WAL checkpoints, final indexes, optional vacuum.

SQL Details
- `INSERT_SQL` (lines ~226-239):
  - On conflict of `title_normalized`, merges `alternate_id` CSV (idempotent) and fills `author_id` if previously empty.

Important Lines
- Normalization: 36-51
- Parse line: 54-98
- Pragmas: 121-145
- Table creation: ~198-214
- UPSERT: 226-239
- Import loop: 283-344

DB Effects
- With `--force`, drops `works` before import; otherwise upserts.
- Index on `author_id` enables later joins/filtering.

Notes
- Purges residual `-wal`/`-shm` files before opening the DB to avoid space bloat.
- Uses `tqdm` progress bar labelled "🚚 Import works".

