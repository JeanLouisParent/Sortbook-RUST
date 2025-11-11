# author-alias-online (Rust) — Technical Doc

Overview
- Purpose: Resolve author aliases from Wikidata for local author folders, compute a robust score against the local name, and optionally move/merge to a canonical folder. Writes a CSV as evidence when not in dry-run.
- Scope: Operates under a root containing author folders (one level deep). Safe by default (dry-run).

Location
- Crate: `scripts/author-alias-online/`
- Binary: `author-alias-online`

CLI
- Build: `cargo build --manifest-path scripts/author-alias-online/Cargo.toml`
- Run:
  - Dry-run (no changes): `cargo run --manifest-path scripts/author-alias-online/Cargo.toml -- --dry-run true --verbose`
  - Apply (score > 0.90 only) + CSV: `cargo run --manifest-path scripts/author-alias-online/Cargo.toml -- --dry-run false --verbose`
- Options:
  - `--root <path>` (default `output/sorted_book`)
  - `--out-csv <path>` (default `data/online_aliases.csv`; written only if `--dry-run false`)
  - `--prefer-lang en|fr` (default `en`)
  - `--timeout <secs>` (default 5)
  - `--limit <n>` (default 0 = all)
  - `--dry-run true|false` (default true)
  - `--verbose`

Behavior
- Queries Wikidata search (`wbsearchentities`) and scores candidates by:
  - Normalizing local query and labels (accents removed, lowercased, punctuation stripped, spaces squashed)
  - Testing both “First Last” and inverted “Last, First” forms
  - Exact normalized match after inversion → score 1.00
  - Token F1 overlap, with near-exact saturation (≥0.90)
  - Small role bonus (+0.1) if description indicates author-like roles
- Enrichment: attempts to fetch given name (P735) and family name (P734) for better “Last, First” splitting; falls back to heuristic otherwise.
- Moves/merges only when `--dry-run false` and score > 0.90. Duplicate files keep the largest.
- Target folder naming: normalized “Last, First” without accents, filesystem-safe.
- Console output prints OK/MISS per author, with QID, label, score, description snippet, and computed target folder.

Notes
- Network failures and timeouts are non-fatal; the tool records a MISS and continues.
- This tool is independent of the offline cleanup and the sorter. Integrate at your discretion.

