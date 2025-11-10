#!/usr/bin/env python3
"""
Authors import script for OpenLibrary dumps.

Usage is now harmonized with the works importer:
  - --db: target SQLite database path
  - --dump: authors dump file path
  - --verbose: print extra progress logs

Behavior is unchanged: rebuilds authors table then imports data.
"""

from __future__ import annotations

import json
import os
import re
import sqlite3
import unicodedata
from collections import defaultdict
import argparse
from typing import DefaultDict, List, Tuple

# Paths configuration
DB_PATH: str = "../../data/database/openlibrary.sqlite3"
DUMP_FILE: str = "../../data/dumps/ol_dump_authors.txt"
TABLE_NAME: str = "authors"

# Ensure target directory for the database exists (non-destructive)
os.makedirs(os.path.dirname(DB_PATH), exist_ok=True)


# === Utilities ===
def normalize_name(name: str) -> str:
    """Return a lowercase, accent-stripped, punctuation-free author name.

    - Lowercase
    - Remove accents (NFD, drop Mn category)
    - Keep only [a-z0-9\s-]
    - Collapse multiple spaces
    """
    if not name:
        return ""
    # Lowercase
    name = name.lower()
    # Strip accents
    name = unicodedata.normalize("NFD", name)
    name = "".join(c for c in name if unicodedata.category(c) != "Mn")
    # Remove punctuation/special chars
    name = re.sub(r"[^a-z0-9\s-]", "", name)
    # Collapse whitespace
    name = re.sub(r"\s+", " ", name).strip()
    return name


# === Schema (re)build ===
def rebuild_table(conn: sqlite3.Connection) -> None:
    """Drop-and-create authors table. Preserves original behavior."""
    c = conn.cursor()
    c.execute(f"DROP TABLE IF EXISTS {TABLE_NAME}")
    c.execute(
        f"""
        CREATE TABLE {TABLE_NAME} (
            author_id TEXT PRIMARY KEY,
            name TEXT,
            name_normalized TEXT,
            alternate_id TEXT
        )
        """
    )
    conn.commit()


# === Main import ===
def import_authors(db_path: str, dump_file: str, verbose: bool = False) -> None:
    """Rebuild authors table and import from the dump file.

    - Reads dump lines, parses JSON payload in 5th TSV column
    - Groups authors by normalized name
    - First author id becomes the primary id, others go to alternate_id
    - Recreates index for fast lookups on name_normalized
    """
    conn = sqlite3.connect(db_path)
    rebuild_table(conn)
    c = conn.cursor()

    print(f"Reading dump: {dump_file}")
    authors_by_name: DefaultDict[str, List[Tuple[str, str]]] = defaultdict(list)

    # Stream read the dump
    with open(dump_file, "r", encoding="utf-8") as f:
        for i, line in enumerate(f, start=1):
            parts = line.strip().split("\t", 4)
            if len(parts) < 5:
                continue
            try:
                data = json.loads(parts[4])
                author_id = (data.get("key", "") or "").replace("/authors/", "")
                name = (data.get("name", "") or "").strip().lower()
                name_normalized = normalize_name(name)

                if not author_id or not name:
                    continue

                authors_by_name[name_normalized].append((author_id, name))
            except Exception as e:
                # Skip malformed lines, keep behavior identical (log + continue)
                if verbose:
                    print(f"⚠️ Skipping line {i} ({e})")
                continue

    print(f"{len(authors_by_name)} unique author names processed")

    # Batch insert with de-duplication by normalized name
    batch: List[Tuple[str, str, str, str]] = []
    for name_norm, entries in authors_by_name.items():
        # First author becomes the primary key for this normalized name
        author_id, name = entries[0]
        if len(entries) > 1:
            alternate_ids = ",".join(aid for aid, _ in entries[1:])
        else:
            alternate_ids = ""
        batch.append((author_id, name, name_norm, alternate_ids))

    c.executemany(
        f"""
        INSERT INTO {TABLE_NAME} (author_id, name, name_normalized, alternate_id)
        VALUES (?, ?, ?, ?)
        """,
        batch,
    )
    conn.commit()

    # Index for fast lookups (kept as-is; no IF NOT EXISTS to preserve behavior)
    c.execute(f"CREATE INDEX idx_name_norm ON {TABLE_NAME}(name_normalized)")
    conn.commit()
    conn.close()

    print(f"✅ Import complete: inserted {len(batch)} authors into {db_path}")


# === CLI ===
def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Import OpenLibrary authors into SQLite (rebuilds table).")
    parser.add_argument("--db", default=DB_PATH, help="Target SQLite database path")
    parser.add_argument("--dump", default=DUMP_FILE, help="Authors dump file path")
    parser.add_argument("--verbose", action="store_true", help="Print extra progress logs")
    return parser.parse_args()


# === Entrypoint ===
if __name__ == "__main__":
    args = parse_args()
    import_authors(db_path=args.db, dump_file=args.dump, verbose=args.verbose)
