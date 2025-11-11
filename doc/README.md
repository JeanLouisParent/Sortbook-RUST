# Technical Documentation

- cleanup (Rust): `doc/cleanup_rust.md` — Author folder normalization/merge and CSV generation.
- sortbook (Rust): `doc/sortbook_rust.md` — File sorter using the OpenLibrary DB and LLM hints.
- works import (Python): `doc/works_import.md` — Build the `works` table.
- authors import (Python): `doc/authors_import.md` — Build the `authors` table.
- cleanup-filenames (Rust): `doc/cleanup_filenames_rust.md` — Normalize and deduplicate book filenames within each author folder.
- author-alias-online (Rust): `doc/author_alias_online.md` — Resolve aliases via Wikidata, score matches, and optionally move/merge folders (CSV proof).
This folder contains technical, implementation‑level documentation for the Python import scripts and the Rust sorter. It is intended for developers and agents who need to understand the precise flow, functions, and key lines.

- authors_import.md — details for `scripts/import/import_authors_sqlite.py`
- works_import.md — details for `scripts/import/import_works_sqlite.py`
- sortbook_rust.md — details for `scripts/sort/src/main.rs`
- cleanup_rust.md — details for `scripts/cleanup/src/main.rs`
