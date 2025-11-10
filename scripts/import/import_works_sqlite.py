#!/usr/bin/env python3
"""
Sequential OpenLibrary works import without intermediate partial databases.

Usage is harmonized with authors importer:
  - --db: target SQLite database path
  - --dump: works dump file path
  - --verbose: extra logs (kept lightweight)
  - plus existing knobs: --batch, --commit-interval, --force, --vacuum

Flow:
  - Stream-read the dump
  - Insert directly into the final database
  - Deduplicate via UNIQUE on title_normalized (as PRIMARY KEY)

The first work met for a normalized title becomes the reference; subsequent
works for that title are appended into alternate_id.
"""

import argparse
import os
import re
import sqlite3
import time
import unicodedata
from typing import List, Optional, Tuple

try:  # pragma: no cover — optional fast JSON
    import ujson as json
except ImportError:  # pragma: no cover — fallback
    import json

from tqdm import tqdm


# === Defaults ===
DB_PATH: str = "../../data/database/openlibrary.sqlite3"
DUMP_FILE: str = "../../data/dumps/ol_dump_works.txt"
BATCH_SIZE: int = 100_000


# === Utilities ===
def normalize_text(text: str) -> str:
    """Lowercase string, strip accents/punctuation; collapse spaces.

    Preserves original normalization logic.
    """
    if not text:
        return ""
    text = text.lower()
    text = unicodedata.normalize("NFD", text)
    text = "".join(c for c in text if unicodedata.category(c) != "Mn")
    text = re.sub(r"[^a-z0-9\s-]", "", text)
    text = re.sub(r"\s+", " ", text).strip()
    return text


def parse_line(line: str) -> Optional[Tuple[str, str, str, str]]:
    """Parse one TSV line and extract (work_id, title, title_norm, author_id).

    - Expects JSON payload in 5th column
    - Derives first available author id when present
    - Returns None for malformed or incomplete items
    """
    parts = line.strip().split("\t", 4)
    if len(parts) < 5:
        return None
    try:
        data = json.loads(parts[4])
    except Exception:
        return None

    work_id = data.get("key", "").replace("/works/", "")
    title = data.get("title", "").strip()
    if not work_id or not title:
        return None

    title_norm = normalize_text(title)
    if not title_norm:
        return None

    authors = data.get("authors") or []
    author_id = ""
    if isinstance(authors, list):
        for entry in authors:
            key = None
            if isinstance(entry, dict):
                if isinstance(entry.get("author"), dict):
                    key = entry["author"].get("key")
                elif "key" in entry:
                    key = entry.get("key")
            elif isinstance(entry, str):
                key = entry
            if key:
                author_id = key.replace("/authors/", "")
                break

    return work_id, title, title_norm, author_id


def count_lines(file_path: str) -> int:
    """Return total line count of a text file (streaming)."""
    total = 0
    with open(file_path, "r", encoding="utf-8", errors="ignore") as handle:
        for _ in handle:
            total += 1
    return total


def configure_connection(conn: sqlite3.Connection) -> None:
    """Tune SQLite pragmas for large imports. Behavior unchanged."""
    pragmas = [
        "PRAGMA page_size = 32768;",
        "PRAGMA journal_mode = WAL;",
        "PRAGMA synchronous = NORMAL;",
        "PRAGMA temp_store = MEMORY;",
        "PRAGMA cache_size = -2000000;",  # ~2 GB of in-memory cache
        "PRAGMA mmap_size = 17179869184;",  # ~16 GB mmap window
        "PRAGMA wal_autocheckpoint = 20000;",
        "PRAGMA locking_mode = EXCLUSIVE;",
        "PRAGMA foreign_keys = OFF;",
    ]
    cursor = conn.cursor()
    for pragma in pragmas:
        cursor.execute(pragma)
    cursor.close()


def ensure_schema(conn: sqlite3.Connection, force: bool) -> None:
    """Create (or recreate) table works as needed. Behavior unchanged."""
    cur = conn.cursor()
    if force:
        cur.execute("DROP TABLE IF EXISTS works;")

    cur.execute(
        """
        CREATE TABLE IF NOT EXISTS works (
            work_id TEXT UNIQUE,
            title TEXT,
            title_normalized TEXT PRIMARY KEY,
            author_id TEXT,
            alternate_id TEXT
        );
        """
    )
    cur.close()


def create_indexes(conn: sqlite3.Connection) -> None:
    """Create secondary indexes (idempotent)."""
    cur = conn.cursor()
    cur.execute("CREATE INDEX IF NOT EXISTS idx_works_author_id ON works(author_id);")
    cur.close()


INSERT_SQL = """
    INSERT INTO works (work_id, title, title_normalized, author_id, alternate_id)
    VALUES (?, ?, ?, ?, '')
    ON CONFLICT(title_normalized) DO UPDATE SET
        alternate_id = CASE
            WHEN works.work_id = excluded.work_id THEN works.alternate_id
            WHEN works.alternate_id IS NULL OR works.alternate_id = '' THEN excluded.work_id
            WHEN instr(',' || works.alternate_id || ',', ',' || excluded.work_id || ',') > 0 THEN works.alternate_id
            ELSE works.alternate_id || ',' || excluded.work_id
        END,
        author_id = CASE
            WHEN (works.author_id IS NULL OR works.author_id = '') AND excluded.author_id <> '' THEN excluded.author_id
            ELSE works.author_id
        END;
"""


def flush_batch(conn: sqlite3.Connection, batch: List[Tuple[str, str, str, str]]) -> None:
    """Execute batched INSERT/UPSERTs when batch is non-empty."""
    if not batch:
        return
    conn.executemany(INSERT_SQL, batch)


def import_works(
    db_path: str,
    batch_size: int,
    force: bool,
    vacuum: bool,
    commit_interval: int,
    dump_file: str = DUMP_FILE,
    verbose: bool = False,
) -> None:
    """Import works dump into SQLite with dedup on title_normalized.

    - Purges residual WAL/SHM files before opening the DB
    - Streams the dump and inserts in batches
    - Periodic commit + WAL checkpoint (TRUNCATE) based on --commit-interval
    - Creates indexes at the end and optional VACUUM
    """
    t0 = time.time()
    os.makedirs(os.path.dirname(db_path) or ".", exist_ok=True)

    if not os.path.exists(dump_file):
        raise FileNotFoundError(f"Dump not found: {dump_file}")

    # Remove any leftover WAL/SHM files before opening the database
    base_prefix = db_path
    for suffix in ("-wal", "-shm"):
        try:
            os.remove(f"{base_prefix}{suffix}")
        except FileNotFoundError:
            pass
        except OSError as exc:
            print(f"⚠️ Unable to delete {base_prefix}{suffix} ({exc})")

    conn = sqlite3.connect(db_path)
    try:
        configure_connection(conn)
        ensure_schema(conn, force)

        if force:
            conn.commit()

        total_lines = count_lines(dump_file)
        print(f"📏 Dump {dump_file}: {total_lines:,} lines")

        processed = 0
        batch: List[Tuple[str, str, str, str]] = []
        rows_since_commit = 0

        conn.execute("BEGIN;")

        with open(dump_file, "r", encoding="utf-8") as handle, \
                tqdm(total=total_lines, desc="🚚 Import works", unit="line", unit_scale=False) as progress:
            for line in handle:
                progress.update(1)
                item = parse_line(line)
                if not item:
                    continue
                batch.append(item)
                processed += 1
                if len(batch) >= batch_size:
                    flush_batch(conn, batch)
                    rows_since_commit += len(batch)
                    batch.clear()

                if commit_interval and rows_since_commit >= commit_interval:
                    conn.commit()
                    conn.execute("PRAGMA wal_checkpoint(TRUNCATE);")
                    conn.execute("BEGIN;")
                    rows_since_commit = 0

            if batch:
                flush_batch(conn, batch)
                rows_since_commit += len(batch)
                batch.clear()

        conn.commit()
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE);")
        rows_since_commit = 0

        create_indexes(conn)
        conn.commit()
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE);")

        if vacuum:
            conn.execute("VACUUM;")
            conn.execute("PRAGMA wal_checkpoint(TRUNCATE);")
        else:
            conn.execute("PRAGMA wal_checkpoint(TRUNCATE);")

    finally:
        conn.close()

    duration = time.time() - t0
    print(f"✅ Import complete: {processed} works in {duration:.2f}s → {db_path}")


# === CLI ===
def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Sequential OpenLibrary works import (no temporary databases)."
    )
    parser.add_argument("--db", default=DB_PATH, help="Target SQLite database path")
    parser.add_argument("--batch", "-b", type=int, default=BATCH_SIZE, help="Number of rows per INSERT batch")
    parser.add_argument("--force", "-f", action="store_true", help="Drop and recreate the works table")
    parser.add_argument("--vacuum", "-v", action="store_true", help="Run VACUUM after the import finishes")
    parser.add_argument(
        "--commit-interval",
        "-c",
        type=int,
        default=1_000_000,
        help="Rows inserted before issuing a COMMIT (0 = single final commit)",
    )
    parser.add_argument("--dump", default=DUMP_FILE, help="Works dump file path")
    parser.add_argument("--verbose", action="store_true", help="Print extra progress logs")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    import_works(
        db_path=args.db,
        batch_size=args.batch,
        force=args.force,
        vacuum=args.vacuum,
        commit_interval=max(args.commit_interval, 0),
        dump_file=args.dump,
        verbose=args.verbose,
    )


if __name__ == "__main__":
    main()
