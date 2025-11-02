# sortbook2

An end-to-end toolkit to sort very large French eBook libraries using OpenLibrary metadata, a local SQLite database, and a fast Rust sorter. I created this project because I needed to organize about 250,000 French EPUBs into a clean author/title hierarchy reliably and quickly.

The repo contains:
- Python import scripts to build a local OpenLibrary SQLite DB from dumps.
- A Rust CLI sorter that reads EPUB/PDF files, extracts metadata, matches against the DB, and organizes files into output folders.
- A predictable folder layout so you can drop files under `input/`, build the DB under `data/database/`, and get sorted results under `output/`.

## Folder Structure
- `scripts/`
  - `import/`
    - `import_authors_sqlite.py` — build the `authors` table from the authors dump.
    - `import_works_sqlite.py` — sequential import of `works` with de-duplication and SQLite performance tweaks.
  - `sort/` — Rust crate for the `sortbook` binary.
- `data/`
  - `dumps/` — place OpenLibrary dumps here (e.g., `ol_dump_works.txt`, `ol_dump_authors.txt`).
  - `database/` — generated SQLite databases (`openlibrary.sqlite3`, etc.).
- `input/` — per-type input folders (e.g., `input/epub`, `input/pdf`).
- `output/` — sorter outputs (`sorted_books`, `fail_author`, `fail_title`).
- `logs/` — logs and state files (resume markers, copy failures, sorter logs).

## Introduction
Sorting hundreds of thousands of eBooks is hard, especially when metadata is messy or incomplete. This project builds a local OpenLibrary index and uses a Rust CLI to classify and copy files by matching their metadata against that index. It aims to be:
- Fast: SQLite with appropriate indexes and optimized queries (using `GLOB` instead of `LIKE`) keeps lookups quick on 10M+ rows.
- Practical: Resume support skips already processed files; copy failures are logged without stopping the run.
- Deterministic: A normalized title/author pipeline and predictable output layout.

The primary use case is French-language eBooks, and the built-in LLM prompt is intentionally in French.

## Prerequisites

### 1) Download OpenLibrary Dumps
Download and place these files under `data/dumps/`:
- `ol_dump_authors.txt` (OpenLibrary authors dump)
- `ol_dump_works.txt` (OpenLibrary works dump)

Ensure you have sufficient disk space; the dumps plus the resulting SQLite DB can require tens of gigabytes.

### 2) Install Tooling
You need Python, Rust, and Ollama. Below are convenience scripts you can run per platform. Adjust to your environment as needed.

Windows (PowerShell):
```
# Python 3.10+ (winget) and Rust
winget install -e --id Python.Python.3.10
winget install -e --id Rustlang.Rustup

# Ollama
winget install -e --id Ollama.Ollama

# Python libs
python -m pip install --upgrade pip
python -m pip install tqdm ujson
```

macOS (bash/zsh with Homebrew):
```
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
brew install python rust

# Ollama
brew install ollama

python3 -m pip install --upgrade pip
python3 -m pip install tqdm ujson
```

Linux (Debian/Ubuntu):
```
sudo apt update
sudo apt install -y python3 python3-pip build-essential pkg-config libssl-dev
curl https://sh.rustup.rs -sSf | sh -s -- -y

# Ollama
curl -fsSL https://ollama.com/install.sh | sh

python3 -m pip install --upgrade pip
python3 -m pip install tqdm ujson
```

Notes:
- The sorter does not require Ollama to run; the embedded prompt remains in French if you use an LLM.
- If building on Apple Silicon, ensure toolchains are ARM-compatible.

## Build the SQLite DB from Dumps

Run from the repository root.

Import authors:
```
python3 scripts/import/import_authors_sqlite.py \
  --db data/database/openlibrary.sqlite3 \
  --dump data/dumps/ol_dump_authors.txt \
  --verbose
```

Import works (can take a long time; tune batching to your machine):
```
python3 scripts/import/import_works_sqlite.py \
  --db data/database/openlibrary.sqlite3 \
  --dump data/dumps/ol_dump_works.txt \
  --force \
  --batch 5000 \
  --commit-interval 50000 \
  --vacuum \
  --verbose
```

Schema overview:
- `authors(author_id TEXT PRIMARY KEY, name TEXT, name_normalized TEXT, alternate_id TEXT)`
  - Index: `idx_name_norm(name_normalized)`
- `works(work_id TEXT UNIQUE, title TEXT, title_normalized TEXT PRIMARY KEY, author_id TEXT, alternate_id TEXT)`
  - Index: `idx_works_author_id(author_id)`

## How To Use (Rust Sorter)

Build and run with default options. Place your EPUBs under `input/epub/`.

Default run (recommended):
```
cargo run --manifest-path scripts/sort/Cargo.toml -- \
  --root ../.. \
  --ext epub \
  --mode full \
  --author-hints 0
```

Model recommendation:
- Ollama model: `mistral:7b` (default in code) for strong French metadata handling.
- To change the model, edit the constant in `scripts/sort/src/main.rs`:
  - `const OLLAMA_MODEL: &str = "mistral:7b";`
  - Replace with your preferred model (e.g., `mixtral:8x7b`), then rebuild.

Key arguments:
- `--root <path>`: Project root used to resolve `input/`, `output/`, `logs/`, and `data/database/`. Default examples use `../..` when running inside the crate.
- `--ext <epub|pdf|...>`: Input subfolder under `input/<ext>/`. Default examples use `epub`.
- `--mode <strict|normal|full|full-normal|full-raw>`: Matching mode. `full` is the recommended balanced mode. Resume skips files that previously succeeded in any success mode (`strict`, `normal`, `full-normal`, `full-raw`).
- `--author-hints <0|1>`: Whether to use detected author hints from filenames. Default in examples is `0`.
- Resume behavior: The sorter reads `logs/sortbook_state.jsonl` and skips already successful files. Failures are retried on the next run.
- Copy failures: Files that cannot be copied are logged to `logs/sortbook_copy_failures.jsonl`, and the run continues.

Metadata writing:
- By default, the sorter writes resolved author/title metadata back into files when appropriate.
- Use `--no-ol-meta` to disable writing OpenLibrary-based metadata; files are still classified and copied.

### Full CLI Reference

All CLI options supported by `sortbook`:
- `-e, --ext <string>`
  - Required. File extension/folder to process (e.g., `epub`, `pdf`, `azw3`). Files are read from `input/<ext>/` under `--root`.
- `-l, --limit <number>`
  - Optional. Maximum number of files to process. `0` means no limit. Default: `0`.
- `--debug`
  - Optional. Enables verbose debug logging to console or file (see `--log-file`).
- `--purge`
  - Optional. Cleans `output/sorted_books`, `output/fail_author`, `output/fail_title`, and `logs/` before starting.
- `--root <path>`
  - Optional. Project root. Resolves `input/`, `output/`, `logs/`, and `data/database/`. Default: `.` when running from repo root, `../..` in examples when running inside the crate.
- `--mode <strict|normal|full|full-normal|full-raw>`
  - Optional. Matching strategy. Default: `full`.
- `--author-hints <number>`
  - Optional. Number of author names to preload from the DB and include in the French prompt to guide the LLM. `0` disables hints. Default: `2000`.
- `--log-file <path>`
  - Optional. If set, logs are written to this file. If empty, logs go to console unless `--debug` initializes file logging to `logs/sortbook.log`.
- `--no-ol-meta`
  - Optional. Do not write OpenLibrary-based metadata back into files. Sorting/copying still proceed.

Input and outputs:
- Put files in `input/<ext>/` (e.g., `input/epub`).
- Sorted files land in `output/sorted_books/` under `Author/Title/` folder structure.
- Unmatched/missing-author files go to `output/fail_author/` and unknown-title cases to `output/fail_title/`.

## LLM Prompt (French; do not modify)

The built-in prompt used for classification is intentionally in French to better handle francophone books. Do not translate or change it. This is the exact prompt assembled in code (with author hints optionally prefixed):

```
Réponds UNIQUEMENT en JSON compact sans texte hors JSON.
{
  "title": string|null,
  "title_normalized": string|null,
  "author_firstname": string|null,
  "author_lastname": string|null
}
Règles:
- favoris le titre français si probable
- si incertain -> null
- n'ajoute pas d'explication
Nom de fichier: {filename}
```

Note: The application UI/logs/comments are in English; only the prompt remains in French by design.

## How It Works (Behavior Overview)

- Title-first lookup: The sorter gets a candidate title from the LLM (French prompt) and normalizes it.
- Fast DB probing: It tries indexed `GLOB` patterns on `works.title_normalized` (prefix then containment), then a lower(title) fallback, then exact normalized match.
- Author confirmation: When a guess includes author names, it normalizes them and looks up `authors.name_normalized`. If multiple author IDs exist (including alternates), it filters `works` by those IDs.
- Modes: `strict`, `normal`, `full`, `full-normal`, `full-raw` change how much evidence is required from title vs author and whether raw/normalized matches are accepted.
- Outputs: On a match, files are copied to `output/sorted_books/<Author>/<Title>/`. Otherwise they go to `fail_author` or `fail_title`. Copy errors are logged and the run continues.
- Resume: Successful outcomes are logged to `logs/sortbook_state.jsonl` and are skipped on subsequent runs; failed ones are retried.

Performance note:
- Using `GLOB` on normalized columns allows SQLite to leverage indexes, reducing lookup time from seconds to milliseconds on large tables.

## Contributing / Support
- Check `agents.md` for project-specific guidelines.
- If OpenLibrary dump formats change, adjust the import scripts accordingly.
- Issues and PRs that improve reliability, performance, or documentation are welcome.
