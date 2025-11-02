# authors_import.py — Technical Doc

Script: `scripts/import/import_authors_sqlite.py`

Purpose
- Rebuilds the `authors` table from `ol_dump_authors.txt`, deduplicating by a normalized author name.

CLI
- `--db` (str, default `../../data/database/openlibrary.sqlite3`): target DB path.
- `--dump` (str, default `../../data/dumps/ol_dump_authors.txt`): authors dump path.
- `--verbose` (flag): extra logs for skipped/malformed lines.

Core Functions
- `normalize_name(name: str) -> str` (lines ~31-53):
  - Lowercases, strips accents (NFD, drop Mn), removes non `[a-z0-9\s-]`, collapses whitespace.
  - Used to generate `name_normalized` for deduplication and indexing.
- `rebuild_table(conn)` (lines ~56-78):
  - Drops and recreates `authors(author_id TEXT PRIMARY KEY, name TEXT, name_normalized TEXT, alternate_id TEXT)`.
- `import_authors(db_path, dump_file, verbose)` (lines ~81-148):
  - Streams the dump, parses JSON in the 5th TSV column.
  - Extracts `author_id` from `/authors/<id>` and `name`.
  - Groups by `name_normalized`; the first id becomes the primary id, the rest join `alternate_id` (CSV).
  - Inserts all rows, then creates index `idx_name_norm(name_normalized)`.

Important Lines
- Normalization: 31-53
- Table rebuild: 56-78
- Grouping and insert loop: 103-138
- Index creation: 141-147

DB Effects
- Drops and recreates `authors` each run (preserves historical behavior).
- Creates `idx_name_norm` for fast lookups by normalized name.

Notes
- Skips malformed lines; if `--verbose`, prints the reason with the line number.
- Ensures DB directory exists; does not remove pre-existing DB files.

