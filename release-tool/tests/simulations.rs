//! Simulations du pipeline de publication — port Rust de l'ancien
//! `scripts/release-tests.ps1` (retiré avec le reste du PowerShell).
//!
//! Aucun secret, aucun réseau, aucune publication réelle : `cargo publish`,
//! le statut crates.io et la pause d'indexation passent par des
//! implémentations fabriquées ([`FakeOps`]) et les étapes du gate par
//! [`FakePipeline`] — l'équivalent des surcharges de fonctions de l'ancien
//! harnais PowerShell. Les scénarios CLI (cohérence du tag, CHANGELOG)
//! lancent le vrai binaire via `CARGO_BIN_EXE_release-tool` avec
//! `GITHUB_REF`/`GITHUB_ACTIONS` posés, sur des fichiers CHANGELOG jetables.
//!
//! Cas couverts — ceux exigés par l'audit du pipeline :
//!   - SemVer : toutes les combinaisons numériques (`0.10.10`...), injections
//!     refusées ;
//!   - cohérence des versions : racine = tag = input = exigence macros exacte
//!     = section CHANGELOG présente ;
//!   - tag incohérent ou mal formé : refus avant tout test et toute
//!     publication ;
//!   - réseau indisponible : arrêt SANS publication (une erreur réseau n'est
//!     jamais une preuve d'absence) ;
//!   - « already uploaded » : traité comme déjà publié ; toute autre erreur
//!     de `cargo publish` est remontée telle quelle ;
//!   - tests rouges : le gate s'arrête à la première étape en échec ;
//!   - contenu des paquets : licences MIT/Apache exigées dans les deux paquets.

use std::collections::HashMap;
use std::process::{Command, Stdio};

use release_tool::{
    Gate, RegistryOps, Status, VersionFacts, assert_package_contents, assert_version_coherence,
    is_semver, publish_crate, publish_flow, verify_gates,
};

// ---------------------------------------------------------------------------
// Implémentations fabriquées — les frontières impures sont injectées.
// ---------------------------------------------------------------------------

/// Statuts crates.io scénarisés (dernier répété) + réponses de
/// `cargo publish` (dépilées) + compteurs séparés (sondes ≠ publications).
#[derive(Default)]
struct FakeOps {
    statuts: Vec<Status>,
    statut_appels: usize,
    sorties_publish: Vec<(i32, String)>,
    sommeils: usize,
    publie_appels: usize,
}

impl RegistryOps for FakeOps {
    fn crates_io_status(&mut self, _crate_name: &str, _version: &str) -> Status {
        let index = self.statut_appels.min(self.statuts.len().saturating_sub(1));
        let statut = self
            .statuts
            .get(index)
            .copied()
            .unwrap_or(Status::Inconclusive);
        self.statut_appels += 1;
        statut
    }

    fn run_cargo_publish(&mut self, _crate_name: &str, _dry_run: bool) -> (i32, String) {
        self.publie_appels += 1;
        self.sorties_publish
            .pop()
            .unwrap_or_else(|| (1, String::new()))
    }

    fn sleep(&mut self, _secs: u64) {
        self.sommeils += 1;
    }
}

/// Verdicts d'étapes du gate scénarisés (dernier répété) + listes de paquets
/// fabriquées.
#[derive(Default)]
struct FakePipeline {
    verdicts: Vec<Result<(), i32>>,
    paquets: HashMap<String, Vec<String>>,
    etapes_lancees: usize,
}

impl release_tool::Pipeline for FakePipeline {
    fn run_step(&mut self, _gate: &Gate) -> Result<(), i32> {
        let index = self
            .etapes_lancees
            .min(self.verdicts.len().saturating_sub(1));
        let verdict = self.verdicts.get(index).copied().unwrap_or(Ok(()));
        self.etapes_lancees += 1;
        verdict
    }

    fn cargo_package_list(&mut self, crate_name: &str) -> Result<Vec<String>, String> {
        self.paquets
            .get(crate_name)
            .cloned()
            .ok_or_else(|| format!("paquet {crate_name} non simulé"))
    }
}

fn faits(racine: &str, macros: &str, exigence: &str) -> VersionFacts {
    VersionFacts {
        root_version: racine.to_string(),
        macros_version: macros.to_string(),
        macros_req: exigence.to_string(),
    }
}

fn paquets_conformes() -> HashMap<String, Vec<String>> {
    HashMap::from([
        (
            "litemcp-macros".to_string(),
            vec![
                "Cargo.toml".to_string(),
                "src/lib.rs".to_string(),
                "LICENSE-APACHE".to_string(),
                "LICENSE-MIT".to_string(),
            ],
        ),
        (
            "litemcp".to_string(),
            vec![
                "Cargo.toml".to_string(),
                "README.md".to_string(),
                "LICENSE-APACHE".to_string(),
                "LICENSE-MIT".to_string(),
            ],
        ),
    ])
}

// ---------------------------------------------------------------------------
// 1. SemVer : toutes les combinaisons numériques, injections refusées.
// ---------------------------------------------------------------------------

#[test]
fn la_matrice_semver_accepte_les_combinaisons_numeriques_et_refuse_l_injection() {
    const MATRICE: [(&str, bool); 18] = [
        ("0.10.10", true),
        ("0.10.11", true),
        ("10.10.10", true),
        ("0.0.0", true),
        ("1.0.0-rc.1", true),
        ("1.0.0-alpha.1+build.7", true),
        ("0.10.10+20261110", true),
        ("v0.10.10", false),
        ("0.10", false),
        ("0.10.10.", false),
        ("00.10.10", false),
        ("0.010.10", false),
        ("0.10.010", false),
        ("0.10.10-", false),
        ("0.10.10+", false),
        ("0.10.10-rc..1", false),
        ("0.10.10; rm -rf /", false),
        ("../0.10.10", false),
    ];
    for (valeur, attendu) in MATRICE {
        assert_eq!(
            is_semver(valeur),
            attendu,
            "SemVer '{valeur}' : attendu {attendu}"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Cohérence des versions : tag, macros, CHANGELOG (faits fabriqués).
// ---------------------------------------------------------------------------

#[test]
fn la_coherence_nominale_accepte_racine_tag_input_macros_changelog() {
    let facts = faits("0.10.10", "0.10.10", "^0.10.10");
    let changelog = "# Changelog\n\n## [0.10.10] - 2026-11-10\n\n- correctif\n";
    assert_eq!(
        assert_version_coherence(
            &facts,
            "0.10.10",
            Some("0.10.10"),
            Some("0.10.10"),
            changelog,
            "CHANGELOG.md"
        ),
        Ok(()),
        "0.10.10 accepté de bout en bout (racine = tag = input = macros = CHANGELOG)"
    );
}

#[test]
fn des_macros_desalignees_de_l_exigence_racine_refusent_avant_publication() {
    let facts = faits("0.10.10", "0.4.0", "^0.3.0");
    let erreur = assert_version_coherence(
        &facts,
        "0.10.10",
        None,
        None,
        "## [0.10.10]",
        "CHANGELOG.md",
    )
    .expect_err("macros désalignées : refus attendu");
    assert!(
        erreur.contains("litemcp-macros 0.4.0"),
        "message attendu : {erreur}"
    );
}

#[test]
fn une_exigence_de_version_non_exacte_refuse() {
    let facts = faits("0.10.10", "0.10.10", "^0.10");
    let erreur = assert_version_coherence(
        &facts,
        "0.10.10",
        None,
        None,
        "## [0.10.10]",
        "CHANGELOG.md",
    )
    .expect_err("exigence ^0.10 : refus attendu");
    assert!(
        erreur.contains("version exacte"),
        "message attendu : {erreur}"
    );
}

// ---------------------------------------------------------------------------
// 3. Tag : dérivé de GITHUB_REF par le binaire — incohérent ou mal formé
//    ⇒ refus. Ces scénarios lancent le VRAI binaire sur le VRAI workspace.
// ---------------------------------------------------------------------------

/// Exécute `release-tool check` avec un environnement contrôlé ; renvoie
/// (code de sortie, sortie combinée). Les variables GitHub du parent sont
/// retirées pour qu'un harnais lancé depuis un run de release ne biaise pas
/// les scénarios (port de `New-ChildStartInfo` de l'ancien harnais).
fn lance_check(changelog: &str, github_ref: Option<&str>, input: Option<&str>) -> (i32, String) {
    let mut commande = Command::new(env!("CARGO_BIN_EXE_release-tool"));
    commande
        .arg("check")
        .arg("--changelog-path")
        .arg(changelog)
        .env_remove("GITHUB_REF")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("CARGO_REGISTRY_TOKEN")
        .env_remove("CARGO_REGISTRIES_CRATES_IO_TOKEN")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(réf) = github_ref {
        commande.env("GITHUB_REF", réf);
        commande.env("GITHUB_ACTIONS", "true");
    }
    if let Some(valeur) = input {
        commande.arg("--input-version").arg(valeur);
    }
    let sortie = commande.output().expect("le binaire doit s'exécuter");
    (
        sortie.status.code().unwrap_or(1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&sortie.stdout),
            String::from_utf8_lossy(&sortie.stderr)
        ),
    )
}

/// Écrit un CHANGELOG jetable dans le temporaire du système.
fn changelog_jetable(nom: &str, corps: &str) -> std::path::PathBuf {
    let chemin = std::env::temp_dir().join(format!("{}-{nom}.md", std::process::id()));
    std::fs::write(&chemin, corps).expect("CHANGELOG jetable écrit");
    chemin
}

/// Corps CHANGELOG minimal pour une version : section `## [X.Y.Z]` présente
/// (le contrôle exige le préfixe de ligne, comme l'ancien regex `(?m)^## \[V\]`).
fn changelog_section(version: &str) -> String {
    format!("# Changelog\n\n## [Unreleased]\n\n## [{version}] - 2026-11-10\n\n- correctif\n")
}

/// Version du manifeste racine, lue par `cargo metadata` (comme le binaire).
fn version_racine_reelle() -> String {
    let sortie = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .expect("cargo metadata doit s'exécuter");
    let document: serde_json::Value =
        serde_json::from_slice(&sortie.stdout).expect("cargo metadata JSON");
    document["packages"]
        .as_array()
        .and_then(|packages| {
            packages
                .iter()
                .find(|p| p["name"] == "litemcp")
                .and_then(|p| p["version"].as_str())
        })
        .expect("paquet litemcp dans cargo metadata")
        .to_string()
}

#[test]
fn le_tag_egale_le_manifeste_la_coherence_accepte_et_publie_la_version_en_sortie() {
    let version = version_racine_reelle();
    let chemin = changelog_jetable("coherence-ok", &changelog_section(&version));
    let refer = format!("refs/tags/v{version}");
    let (code, sortie) = lance_check(
        chemin.to_string_lossy().as_ref(),
        Some(refer.as_str()),
        None,
    );
    let _ = std::fs::remove_file(&chemin);
    assert_eq!(code, 0, "tag = manifeste : cohérence acceptée : {sortie}");
    assert!(
        sortie.contains(&format!("version={version}")),
        "la version publiée doit sortir en dernière ligne : {sortie}"
    );
}

#[test]
fn un_tag_incoherent_refuse_avant_tout_test_et_toute_publication() {
    let chemin = changelog_jetable("incoherent", &changelog_section("0.10.10"));
    let (code, sortie) = lance_check(
        chemin.to_string_lossy().as_ref(),
        Some("refs/tags/v0.0.1"),
        None,
    );
    let _ = std::fs::remove_file(&chemin);
    assert_eq!(code, 1, "tag incohérent : refus attendu : {sortie}");
    assert!(
        sortie.contains("version du manifeste racine"),
        "message attendu : {sortie}"
    );
}

#[test]
fn un_tag_v_sans_numero_refuse_jamais_ignore_en_silence() {
    let chemin = changelog_jetable("tag-v-nu", &changelog_section("0.10.10"));
    let (code, sortie) = lance_check(chemin.to_string_lossy().as_ref(), Some("refs/tags/v"), None);
    let _ = std::fs::remove_file(&chemin);
    assert_eq!(code, 1, "tag `v` sans numéro : refus attendu : {sortie}");
    assert!(
        sortie.contains("non conforme"),
        "message attendu : {sortie}"
    );
}

#[test]
fn un_tag_sans_prefixe_v_refuse() {
    let chemin = changelog_jetable("tag-sans-v", &changelog_section("0.10.10"));
    let (code, sortie) = lance_check(
        chemin.to_string_lossy().as_ref(),
        Some("refs/tags/0.10.10"),
        None,
    );
    let _ = std::fs::remove_file(&chemin);
    assert_eq!(code, 1, "tag sans préfixe `v` : refus attendu : {sortie}");
    assert!(
        sortie.contains("non conforme"),
        "message attendu : {sortie}"
    );
}

#[test]
fn une_version_saisie_incoherente_break_glass_refuse() {
    let chemin = changelog_jetable("break-glass", &changelog_section("0.10.10"));
    let (code, sortie) = lance_check(
        chemin.to_string_lossy().as_ref(),
        Some("refs/heads/main"),
        Some("9.9.9"),
    );
    let _ = std::fs::remove_file(&chemin);
    assert_eq!(code, 1, "input incohérent : refus attendu : {sortie}");
    assert!(
        sortie.contains("version du manifeste racine"),
        "message attendu : {sortie}"
    );
}

#[test]
fn une_reference_ci_illisible_refuse_tag_inverifiable() {
    let chemin = changelog_jetable("ref-absente", &changelog_section("0.10.10"));
    let mut commande = Command::new(env!("CARGO_BIN_EXE_release-tool"));
    commande
        .arg("check")
        .arg("--changelog-path")
        .arg(&chemin)
        .env("GITHUB_ACTIONS", "true")
        .env_remove("GITHUB_REF")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let sortie = commande.output().expect("le binaire doit s'exécuter");
    let _ = std::fs::remove_file(&chemin);
    let texte = format!(
        "{}{}",
        String::from_utf8_lossy(&sortie.stdout),
        String::from_utf8_lossy(&sortie.stderr)
    );
    assert_eq!(
        sortie.status.code(),
        Some(1),
        "GITHUB_REF absent sous Actions : refus attendu : {texte}"
    );
    assert!(
        texte.contains("GITHUB_REF absent"),
        "message attendu : {texte}"
    );
}

#[test]
fn une_section_changelog_absente_refuse() {
    // Le tag est celui du manifeste RÉEL : la cohérence tag/manifeste passe,
    // c'est bien la section CHANGELOG absente qui est refusée.
    let version = version_racine_reelle();
    let chemin = changelog_jetable("sans-section", "# Changelog\n\n## [Unreleased]\n\n- rien\n");
    let refer = format!("refs/tags/v{version}");
    let (code, sortie) = lance_check(
        chemin.to_string_lossy().as_ref(),
        Some(refer.as_str()),
        None,
    );
    let _ = std::fs::remove_file(&chemin);
    assert_eq!(
        code, 1,
        "section CHANGELOG absente : refus attendu : {sortie}"
    );
    assert!(sortie.contains("absente"), "message attendu : {sortie}");
}

// ---------------------------------------------------------------------------
// 4. Publication : déjà publiée, réseau injoignable, réponses de cargo.
// ---------------------------------------------------------------------------

#[test]
fn une_version_deja_sur_crates_io_est_ignoree_sans_appeler_cargo() {
    let mut ops = FakeOps {
        statuts: vec![Status::Published],
        ..Default::default()
    };
    assert_eq!(
        publish_crate("litemcp", "0.10.10", false, &mut ops),
        Ok(()),
        "déjà en ligne : publication ignorée"
    );
    assert_eq!(
        ops.publie_appels, 0,
        "cargo publish ne doit JAMAIS être appelé"
    );
}

#[test]
fn un_reseau_indisponible_arrete_sans_publication() {
    let mut ops = FakeOps {
        statuts: vec![Status::Inconclusive],
        sorties_publish: vec![(101, "cargo ne doit pas être appelé".to_string())],
        ..Default::default()
    };
    let erreur = publish_crate("litemcp", "0.10.10", false, &mut ops)
        .expect_err("statut invérifiable : abandon attendu");
    assert!(
        erreur.contains("n'est pas une preuve d'absence"),
        "message attendu : {erreur}"
    );
    assert_eq!(
        ops.publie_appels, 0,
        "aucune publication : cargo ne doit pas être appelé"
    );
}

#[test]
fn un_already_uploaded_est_traite_comme_deja_publie() {
    let mut ops = FakeOps {
        statuts: vec![Status::Absent],
        sorties_publish: vec![(
            101,
            "error: crate litemcp@0.10.10 already uploaded".to_string(),
        )],
        ..Default::default()
    };
    assert_eq!(
        publish_crate("litemcp", "0.10.10", false, &mut ops),
        Ok(()),
        "« already uploaded » : traité comme publié (idempotence)"
    );
    assert_eq!(ops.publie_appels, 1);
}

#[test]
fn une_erreur_cargo_publish_reelle_est_remontee_telle_quelle() {
    let mut ops = FakeOps {
        statuts: vec![Status::Absent],
        sorties_publish: vec![(
            101,
            "error: failed to get `litemcp` as a dependency of package (network failure)"
                .to_string(),
        )],
        ..Default::default()
    };
    let erreur = publish_crate("litemcp", "0.10.10", false, &mut ops)
        .expect_err("erreur réelle : remontée attendue");
    assert!(
        erreur.contains("complaisante"),
        "aucune interprétation complaisante : {erreur}"
    );
}

#[test]
fn une_publication_nominale_attend_puis_confirme_l_indexation() {
    let mut ops = FakeOps {
        // Sonde initiale : absente (publication partie) ; 1re sonde d'attente :
        // encore absente (une pause) ; 2e : indexée.
        statuts: vec![Status::Absent, Status::Absent, Status::Published],
        sorties_publish: vec![(0, String::new())],
        ..Default::default()
    };
    assert_eq!(
        publish_crate("litemcp-macros", "0.4.0", false, &mut ops),
        Ok(()),
        "absente + publication réussie : indexation attendue puis confirmée"
    );
    assert_eq!(
        ops.statut_appels, 3,
        "la sonde initiale et les deux sondes d'attente"
    );
    assert_eq!(ops.sommeils, 1, "une pause entre les deux sondes d'attente");
}

#[test]
fn la_publication_d_une_version_differente_du_manifeste_refuse() {
    let mut ops = FakeOps {
        statuts: vec![Status::Published],
        ..Default::default()
    };
    let facts = faits("0.10.10", "0.4.0", "^0.4.0");
    let erreur = publish_flow("0.0.1", false, &facts, &mut ops)
        .expect_err("version ≠ manifeste racine : refus local attendu");
    assert!(
        erreur.contains("version du manifeste racine"),
        "message attendu : {erreur}"
    );
}

// ---------------------------------------------------------------------------
// 5. Gate : une étape rouge arrête tout (tests rouges).
// ---------------------------------------------------------------------------

#[test]
fn des_tests_rouges_rompent_le_gate_avant_la_suite() {
    let mut pipeline = FakePipeline {
        verdicts: vec![Err(1)],
        ..Default::default()
    };
    let erreur = verify_gates(&mut pipeline).expect_err("étape rouge : gate rompu attendu");
    assert!(erreur.contains("gate rompu"), "message attendu : {erreur}");
    assert_eq!(
        pipeline.etapes_lancees, 1,
        "le gate s'arrête à la première étape en échec : rien ne court après"
    );
}

#[test]
fn un_gate_tout_vert_passe_le_contenu_des_paquets() {
    let mut pipeline = FakePipeline {
        verdicts: vec![Ok(())],
        paquets: paquets_conformes(),
        ..Default::default()
    };
    assert_eq!(
        verify_gates(&mut pipeline),
        Ok(()),
        "toutes les étapes vertes + paquets conformes : gate vert"
    );
}

// ---------------------------------------------------------------------------
// 6. Contenu des paquets : licences MIT/Apache exigées.
// ---------------------------------------------------------------------------

#[test]
fn un_paquet_macros_sans_licence_mit_rompt_le_gate() {
    let mut pipeline = FakePipeline {
        paquets: HashMap::from([(
            "litemcp-macros".to_string(),
            vec![
                "Cargo.toml".to_string(),
                "src/lib.rs".to_string(),
                "LICENSE-APACHE".to_string(),
            ],
        )]),
        ..Default::default()
    };
    let erreur = assert_package_contents(&mut pipeline)
        .expect_err("LICENSE-MIT absente du paquet macros : refus attendu");
    assert!(
        erreur.contains("LICENSE-MIT absente"),
        "message attendu : {erreur}"
    );
}

#[test]
fn les_deux_paquets_portent_les_licences_et_le_readme() {
    let mut pipeline = FakePipeline {
        paquets: paquets_conformes(),
        ..Default::default()
    };
    assert_eq!(
        assert_package_contents(&mut pipeline),
        Ok(()),
        "les deux paquets portent MIT/Apache (README en plus pour litemcp) : gate vert"
    );
}
