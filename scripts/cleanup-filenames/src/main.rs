use anyhow::{Context, Result};
use clap::Parser;
use regex::Regex;
use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;
use unidecode::unidecode;
use rayon::prelude::*;

#[derive(Parser, Debug)]
#[command(name = "cleanup-filenames", about = "Normalise les noms de fichiers de livres par dossier d'auteur")] 
struct Cli {
    /// Racine où se trouvent les dossiers auteurs (par défaut output/sorted_book)
    #[arg(long, default_value = "output/sorted_book")] 
    root: PathBuf,

    /// Extensions à considérer (séparées par des virgules). Vide = toutes.
    #[arg(long, default_value = "")] 
    exts: String,

    /// Ne fait aucun changement, affiche seulement (true/false)
    #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
    dry_run: bool,

    /// Affichage détaillé
    #[arg(long, default_value_t = false)]
    verbose: bool,
}

#[derive(Debug, Clone)]
struct FileEntry {
    path: PathBuf,
    size: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let allowed_exts = parse_exts(&cli.exts);

    ensure_dir(&cli.root)?;

    // On parcourt uniquement les dossiers de premier niveau (auteurs)
    // Collecte des dossiers auteurs
    let author_dirs: Vec<PathBuf> = fs::read_dir(&cli.root)
        .with_context(|| format!("Lecture du dossier {:?}", &cli.root))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();

    // Traitement en parallèle par dossier d'auteur
    let total_files: usize = author_dirs
        .par_iter()
        .map(|dir| process_author_dir(dir, &allowed_exts, cli.dry_run, cli.verbose).unwrap_or(0))
        .sum();

    println!("Terminé. Total fichiers traités: {}", total_files);
    Ok(())
}

fn ensure_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path).with_context(|| format!("Création du dossier {:?}", path))?;
    }
    Ok(())
}

fn parse_exts(exts: &str) -> Option<Vec<String>> {
    let trimmed = exts.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(
            trimmed
                .split(',')
                .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
        )
    }
}

fn process_author_dir(dir: &Path, allowed_exts: &Option<Vec<String>>, dry_run: bool, verbose: bool) -> Result<usize> {
    // Collecte des fichiers directement dans ce dossier et dans ses sous-dossiers
    // Traitement par sous-dossiers: on traite chaque sous-dossier indépendamment
    let mut count = 0usize;

    // Traiter les fichiers à la racine du dossier auteur comme un groupe séparé
    count += process_one_group(dir, allowed_exts, dry_run, verbose)?;

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            count += process_one_group(&path, allowed_exts, dry_run, verbose)?;
        }
    }

    // Affichage par auteur
    let author_name = dir.file_name().and_then(OsStr::to_str).unwrap_or("<inconnu>");
    println!("Auteur: {} — fichiers traités: {}", author_name, count);
    Ok(count)
}

fn process_one_group(group_dir: &Path, allowed_exts: &Option<Vec<String>>, dry_run: bool, verbose: bool) -> Result<usize> {
    // Map de baseNameNormalisé -> meilleure entrée retenue
    use std::collections::HashMap;
    let mut best_by_norm: HashMap<String, FileEntry> = HashMap::new();
    let mut originals_by_norm: HashMap<String, Vec<FileEntry>> = HashMap::new();

    let mut local_count = 0usize;

    for entry in fs::read_dir(group_dir).unwrap_or_else(|_| fs::read_dir(group_dir).unwrap()) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.is_file() {
            if let Some(exts) = allowed_exts {
                let ext = path.extension().and_then(OsStr::to_str).map(|s| s.to_ascii_lowercase());
                if ext.is_none() || !exts.contains(&ext.unwrap()) {
                    continue;
                }
            }

            let file_name = match path.file_stem().and_then(OsStr::to_str) {
                Some(s) => s,
                None => continue,
            };
            let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");

            let norm_key = normalize_basename_for_group(file_name);
            let size = file_size(&path).unwrap_or(0);
            let entry_obj = FileEntry { path: path.clone(), size };

            originals_by_norm.entry(norm_key.clone()).or_default().push(entry_obj.clone());

            match best_by_norm.get_mut(&norm_key) {
                None => {
                    best_by_norm.insert(norm_key, entry_obj);
                }
                Some(best) => {
                    // Règle: garder le plus lourd, mais si l’un contient des accents et l’autre non, on préfère celui avec accents
                    let best_has_accents = has_accents(best.path.file_stem().and_then(OsStr::to_str).unwrap_or(""));
                    let cand_has_accents = has_accents(file_name);

                    let ordering = best.size.cmp(&size);

                    let choose_candidate = if best_has_accents && !cand_has_accents {
                        // garder best
                        false
                    } else if !best_has_accents && cand_has_accents {
                        // préférer accents
                        true
                    } else {
                        // Sinon, choisir le plus lourd
                        match ordering {
                            Ordering::Less => true,
                            _ => false,
                        }
                    };

                    if choose_candidate {
                        *best = FileEntry { path: path.clone(), size };
                    }
                }
            }

            let _ = ext; // keep ext captured for completeness
        }
    }

    // Appliquer renommages et suppressions des doublons dans le groupe
    for (norm, best) in best_by_norm.iter() {
        // Nouveau nom: capitale sur la première lettre uniquement
        let target_stem = capitalize_first(norm.to_string());
        let ext = best.path.extension().and_then(OsStr::to_str).unwrap_or("");
        let target_name = if ext.is_empty() { target_stem.clone() } else { format!("{}.{}", target_stem, ext) };
        let target_path = best.path.parent().unwrap_or(Path::new(".")).join(&target_name);

        // Renommer le meilleur si nécessaire
        if best.path.file_name().and_then(OsStr::to_str) != Some(target_name.as_str()) {
            if verbose {
                println!("RENOM -> {:?}  =>  {:?}", best.path.file_name().unwrap_or(OsStr::new("")), target_name);
            }
            if !dry_run {
                fs::rename(&best.path, &target_path).with_context(|| format!("Rename {:?} -> {:?}", &best.path, &target_path))?;
            }
        }

        // Supprimer les autres doublons (conserver le meilleur)
        if let Some(all) = originals_by_norm.get(norm) {
            for other in all {
                if other.path != best.path {
                    if verbose {
                        println!("SUPPR -> {:?}", other.path.file_name().unwrap_or(OsStr::new("")));
                    }
                    if !dry_run {
                        // On envoie à la corbeille? Spécifié: garder le fichier le plus lourd donc on supprime les autres.
                        // Sécurité: supprimer via fs::remove_file
                        if let Err(e) = fs::remove_file(&other.path) {
                            eprintln!("Erreur suppression {:?}: {}", &other.path, e);
                        }
                    }
                }
            }
        }

        local_count += 1;
    }

    Ok(local_count)
}

fn file_size(path: &Path) -> io::Result<u64> {
    Ok(fs::metadata(path)?.len())
}

fn has_accents(s: &str) -> bool {
    // True si s contient des caractères non-ASCII une fois normalisés NFC
    let nfc = s.nfc().collect::<String>();
    nfc.chars().any(|c| !c.is_ascii())
}

fn normalize_basename_for_group(name: &str) -> String {
    // Objectif: normaliser de façon robuste, supprimer ponctuation superflue, espaces multiples, conserver accents pour préférence
    // mais clé de groupement sans accents pour rapprocher variantes.
    let lower = name.trim().to_lowercase();

    // Remplace séparateurs communs par espace
    let sep_re = Regex::new(r"[\s_\-]+").unwrap();
    let tmp = sep_re.replace_all(&lower, " ");

    // Supprime ponctuation
    let punct_re = Regex::new(r"[^\p{L}\p{N} ]+").unwrap();
    let tmp = punct_re.replace_all(&tmp, "");

    // Dé-accentuation pour la clé de groupement
    let deaccent = unidecode(&tmp);

    // Compacte espaces
    let compact = deaccent.split_whitespace().collect::<Vec<_>>().join(" ");
    compact
}

fn capitalize_first(s: String) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => s,
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}
