//! CLI de l'outil de publication — `release-tool <semver|check|verify|publish>`.
//!
//! Portage direct de l'ancien `scripts/release.ps1` : mêmes sous-commandes,
//! mêmes codes de sortie (0 succès, 1 échec de contrôle, 2 usage invalide),
//! même ligne `version=<x.y.z>` en sortie de `check` (capturée par les
//! workflows dans `$GITHUB_OUTPUT`). La logique vit dans la bibliothèque
//! ([`release_tool`]) ; ce binaire ne fait que décoder les arguments et
//! imprimer — aucun contrôle ici, tous dans la lib testée par les
//! simulations.

use std::process::Command;
use std::process::ExitCode;

use release_tool::{
    EXIT_CONTROLE, EXIT_OK, EXIT_USAGE, LiveOps, LiveRunner, assert_version_coherence,
    expected_tag_version, is_semver, publish_flow, verify_gates, version_facts,
};

const USAGE: &str = "usage : release-tool <semver|check|verify|publish> [--version <x.y.z>] [--input-version <v>] [--changelog-path <fichier>] [--dry-run]";

/// Échec de contrôle : message imprimé puis exit 1 (port de `Stop-Release`).
fn stop_release(message: &str) -> ExitCode {
    println!("[release] !!! {message}");
    ExitCode::from(EXIT_CONTROLE as u8)
}

/// `cargo metadata --no-deps --format-version 1` — la source des versions.
fn cargo_metadata_facts() -> Result<release_tool::VersionFacts, String> {
    let sortie = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|e| format!("cargo metadata en échec : {e}"))?;
    if !sortie.status.success() {
        return Err(
            "cargo metadata en échec - versions invérifiables, rien n'est publié".to_string(),
        );
    }
    version_facts(&String::from_utf8_lossy(&sortie.stdout))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        println!("{USAGE}");
        return ExitCode::from(EXIT_USAGE as u8);
    };

    // Décodage des options communes — sans clap : cinq drapeaux au total.
    let mut version = None;
    let mut input_version = None;
    let mut changelog_path = "CHANGELOG.md".to_string();
    let mut dry_run = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--version" | "--input-version" | "--changelog-path" => {
                let Some(valeur) = args.get(i + 1) else {
                    println!("{} : valeur attendue après {}", args[i], args[i]);
                    println!("{USAGE}");
                    return ExitCode::from(EXIT_USAGE as u8);
                };
                match args[i].as_str() {
                    "--version" => version = Some(valeur.clone()),
                    "--input-version" => input_version = Some(valeur.clone()),
                    _ => changelog_path = valeur.clone(),
                }
                i += 2;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            autre => {
                println!("argument inconnu : '{autre}'");
                println!("{USAGE}");
                return ExitCode::from(EXIT_USAGE as u8);
            }
        }
    }

    match command.as_str() {
        "semver" => {
            let Some(valeur) = version.as_deref() else {
                println!("usage : release-tool semver --version <x.y.z>");
                return ExitCode::from(EXIT_USAGE as u8);
            };
            if is_semver(valeur) {
                println!("[semver] OK : {valeur}");
                return ExitCode::from(EXIT_OK as u8);
            }
            println!(
                "[semver] refusé : '{valeur}' (regex semver.org complète - 0.10.10 et préreleases acceptés, zéros initiaux et injection refusés)"
            );
            ExitCode::from(EXIT_CONTROLE as u8)
        }
        "check" => {
            // Cohérence AVANT toute publication : tag dérivé de GITHUB_REF
            // (jamais recopié en argument), input optionnel du break-glass,
            // exigence macros exacte, section CHANGELOG présente.
            let facts = match cargo_metadata_facts() {
                Ok(facts) => facts,
                Err(message) => return stop_release(&message),
            };
            let contenu = match std::fs::read_to_string(&changelog_path) {
                Ok(contenu) => contenu,
                Err(_) => {
                    return stop_release(&format!("CHANGELOG introuvable : {changelog_path}"));
                }
            };
            let github_actions = std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
            let tag = match expected_tag_version(
                std::env::var("GITHUB_REF").ok().as_deref(),
                github_actions,
            ) {
                Ok(tag) => tag,
                Err(message) => return stop_release(&message),
            };
            if let Err(message) = assert_version_coherence(
                &facts,
                &facts.root_version,
                tag.as_deref(),
                input_version.as_deref(),
                &contenu,
                &changelog_path,
            ) {
                return stop_release(&message);
            }
            println!(
                "[check] cohérence vérifiée : racine={}, litemcp-macros={} (exigence '{}'), CHANGELOG '## [{}]' présent",
                facts.root_version, facts.macros_version, facts.macros_req, facts.root_version
            );
            println!("version={}", facts.root_version);
            ExitCode::from(EXIT_OK as u8)
        }
        "verify" => {
            if let Err(message) = verify_gates(&mut LiveRunner) {
                return stop_release(&message);
            }
            ExitCode::from(EXIT_OK as u8)
        }
        "publish" => {
            let Some(publish_version) = version.as_deref() else {
                println!("usage : release-tool publish --version <x.y.z> [--dry-run]");
                return ExitCode::from(EXIT_USAGE as u8);
            };
            let facts = match cargo_metadata_facts() {
                Ok(facts) => facts,
                Err(message) => return stop_release(&message),
            };
            if let Err(message) = publish_flow(publish_version, dry_run, &facts, &mut LiveOps) {
                return stop_release(&message);
            }
            ExitCode::from(EXIT_OK as u8)
        }
        autre => {
            println!(
                "commande inconnue : '{autre}' - attendues : semver | check | verify | publish"
            );
            ExitCode::from(EXIT_USAGE as u8)
        }
    }
}
