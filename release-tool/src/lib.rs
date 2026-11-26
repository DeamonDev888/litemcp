//! Outil de publication LiteMCP — port Rust de l'ancien `scripts/release.ps1`
//! (retiré du dépôt : le dépôt ne contient plus que du Rust).
//!
//! Sous-commandes (contrat identique à l'ancien script PowerShell) :
//!   semver   — valide une version contre la grammaire SemVer officielle
//!              (semver.org, réécrite sans crate : toutes les combinaisons
//!              numériques `0.10.10` incluses, zéros initiaux et injections
//!              refusés) ;
//!   check    — cohérence AVANT toute publication, dérivée de `cargo
//!              metadata` : version racine = tag (dérivé de `GITHUB_REF`,
//!              jamais passé en argument) = input optionnel, version
//!              `litemcp-macros` = exigence exacte de la dépendance racine,
//!              section `## [X.Y.Z]` présente dans le CHANGELOG. Dernière
//!              ligne de sortie : `version=<x.y.z>` (les workflows la
//!              capturent dans `$GITHUB_OUTPUT`) ;
//!   verify   — gate qualité : simulations du pipeline (les tests de cette
//!              crate), fmt, clippy `-D warnings` (all-features ET sans
//!              features par défaut), tests all-features, configuration par
//!              défaut ET sans features par défaut, chaque feature isolée
//!              (check + test), exemples, doc rustdoc STRICTE, contenu des
//!              deux paquets — chaque étape court-circuite tout le pipeline ;
//!   publish  — publication crates.io IDEMPOTENTE : `litemcp-macros` d'abord
//!              puis `litemcp`, avec distinction stricte entre « déjà
//!              indexée » (crates.io le confirme) et toute autre erreur
//!              (réseau, auth, validation) qui est RÉELLE — une erreur réseau
//!              n'est JAMAIS interprétée comme une preuve d'absence.
//!
//! Codes de sortie : 0 succès, 1 échec de contrôle, 2 usage invalide.
//!
//! ## Injection pour les simulations
//!
//! Les frontières impures (cargo, curl/crates.io, pause) passent par les
//! traits [`Pipeline`] et [`RegistryOps`] : la production les instancie avec
//! [`LiveRunner`]/[`LiveOps`], les simulations (tests de cette crate) avec
//! des implémentations fabriquées — l'équivalent Rust des surcharges de
//! fonctions de l'ancien harnais PowerShell.

use std::process::Command;

/// Code de sortie : succès.
pub const EXIT_OK: i32 = 0;
/// Code de sortie : échec de contrôle (gate rompu, cohérence refusée).
pub const EXIT_CONTROLE: i32 = 1;
/// Code de sortie : usage invalide (commande ou argument manquant).
pub const EXIT_USAGE: i32 = 2;

/// Agent pour l'API crates.io (politeness policy de crates.io).
pub const REGISTRY_USER_AGENT: &str = "litemcp-release (https://github.com/DeamonDev888/litemcp)";

// ---------------------------------------------------------------------------
// SemVer — grammaire officielle semver.org, sans crate.
// ---------------------------------------------------------------------------

/// Identifiant numérique : que des chiffres, sans zéro initial
/// (`0` lui-même accepté — la grammaire l'exige pour `0.0.0`).
fn identifiant_numerique(v: &str) -> bool {
    !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) && (v.len() == 1 || !v.starts_with('0'))
}

/// Identifiant alphanumérique : `[0-9A-Za-z-]+`, non vide.
fn identifiant_alphanumerique(v: &str) -> bool {
    !v.is_empty()
        && v.bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_alphabetic() || b == b'-')
}

/// Valide une version contre la grammaire SemVer complète (semver.org) :
/// `majeure.mineure.correctif` sans zéro initial, prérelease optionnelle
/// (identifiants numériques SANS zéro initial ou alphanumériques) et
/// métadonnée de build optionnelle (alphanumériques, zéros initiaux admis).
///
/// Remplace la regex officielle de l'ancien script : même grammaire, mêmes
/// refus (`v0.10.10`, `0.10`, zéros initiaux, prérelease vide, injection).
pub fn is_semver(value: &str) -> bool {
    let (coeur_pre, build) = match value.split_once('+') {
        Some((c, b)) => (c, Some(b)),
        None => (value, None),
    };
    if let Some(build) = build {
        if build
            .split('.')
            .any(|ident| !identifiant_alphanumerique(ident))
        {
            return false;
        }
    }
    let (coeur, pre) = match coeur_pre.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (coeur_pre, None),
    };
    let mut parties = coeur.split('.');
    let (Some(majeure), Some(mineure), Some(correctif), None) = (
        parties.next(),
        parties.next(),
        parties.next(),
        parties.next(),
    ) else {
        return false;
    };
    for partie in [majeure, mineure, correctif] {
        if !identifiant_numerique(partie) {
            return false;
        }
    }
    if let Some(pre) = pre {
        for ident in pre.split('.') {
            let ok = if ident.bytes().all(|b| b.is_ascii_digit()) {
                identifiant_numerique(ident)
            } else {
                identifiant_alphanumerique(ident)
            };
            if !ok {
                return false;
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Cohérence des versions — dérivée de cargo metadata.
// ---------------------------------------------------------------------------

/// Faits de version extraits de `cargo metadata --no-deps --format-version 1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionFacts {
    /// Version du paquet racine `litemcp`.
    pub root_version: String,
    /// Version du paquet `litemcp-macros`.
    pub macros_version: String,
    /// Exigence de version de la dépendance `litemcp-macros` du manifeste
    /// racine (normalisée par cargo : `^X.Y.Z` par défaut).
    pub macros_req: String,
}

/// Extrait les faits de version d'une sortie `cargo metadata` (JSON).
///
/// Une page absente ou un champ absent est une erreur : le contrôle ne
/// devine jamais une version.
pub fn version_facts(metadata_json: &str) -> Result<VersionFacts, String> {
    let document: serde_json::Value = serde_json::from_str(metadata_json)
        .map_err(|e| format!("cargo metadata illisible : {e}"))?;
    let packages = document
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "cargo metadata sans paquet — versions invérifiables".to_string())?;
    let racine = package_version(packages, "litemcp")
        .ok_or_else(|| "paquet litemcp absent de cargo metadata".to_string())?;
    let macros = package_version(packages, "litemcp-macros")
        .ok_or_else(|| "paquet litemcp-macros absent de cargo metadata".to_string())?;
    let exigence = packages
        .iter()
        .find(|p| p.get("name").and_then(serde_json::Value::as_str) == Some("litemcp"))
        .and_then(|p| p.get("dependencies"))
        .and_then(serde_json::Value::as_array)
        .and_then(|deps| {
            deps.iter().find(|d| {
                d.get("name").and_then(serde_json::Value::as_str) == Some("litemcp-macros")
            })
        })
        .and_then(|d| d.get("req"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            "la dépendance litemcp-macros est absente du manifeste racine".to_string()
        })?;
    Ok(VersionFacts {
        root_version: racine,
        macros_version: macros,
        macros_req: exigence.to_string(),
    })
}

fn package_version(packages: &[serde_json::Value], name: &str) -> Option<String> {
    packages
        .iter()
        .find(|p| p.get("name").and_then(serde_json::Value::as_str) == Some(name))
        .and_then(|p| p.get("version"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Exige une exigence de version EXACTE : `^`/`=`/rien devant, puis un SemVer
/// complet égal à la version des macros — aucune plage (`^0.4`, `*`,
/// virgules) n'est tolérée (crates.io refuserait une dépendance versionnée
/// non publiée, après avoir publié les macros).
fn exigence_exacte(req: &str, macros_version: &str) -> Result<(), String> {
    let epuree = req.trim();
    let epinglee = epuree.trim_start_matches(['^', '=', '~']);
    if !is_semver(epinglee) {
        return Err(format!(
            "l'exigence litemcp-macros '{req}' n'est pas une version exacte (SemVer complet) - alignez-la sur la version des macros"
        ));
    }
    if epinglee != macros_version {
        return Err(format!(
            "litemcp-macros {macros_version} ≠ exigence racine '{epinglee}' - alignez les deux avant de publier"
        ));
    }
    Ok(())
}

/// Les quatre contrôles de cohérence, dans l'ordre de l'ancien script :
/// 1. version SemVer complète ;
/// 2. tag poussé et/ou input manuel égaux à la version du manifeste racine ;
/// 3. exigence `litemcp-macros` exacte et alignée ;
/// 4. section `## [X.Y.Z]` présente dans le CHANGELOG.
pub fn assert_version_coherence(
    facts: &VersionFacts,
    publish_version: &str,
    tag_version: Option<&str>,
    input_version: Option<&str>,
    changelog_text: &str,
    changelog_path: &str,
) -> Result<(), String> {
    if !is_semver(publish_version) {
        return Err(format!(
            "version '{publish_version}' non SemVer (regex semver.org complète) - abandon"
        ));
    }
    for (nom, valeur) in [("tag", tag_version), ("input", input_version)] {
        if let Some(valeur) = valeur {
            if valeur != publish_version {
                return Err(format!(
                    "{nom} '{valeur}' ≠ version du manifeste racine '{publish_version}' - abandon avant toute publication"
                ));
            }
        }
    }
    exigence_exacte(&facts.macros_req, &facts.macros_version)?;
    if changelog_text.is_empty() {
        return Err(format!("CHANGELOG introuvable ou vide ({changelog_path})"));
    }
    let cible = format!("## [{publish_version}]");
    if !changelog_text
        .lines()
        .any(|ligne| ligne.starts_with(&cible))
    {
        return Err(format!(
            "section '{cible}' absente de {changelog_path} - coupez la section [Unreleased] avant de publier"
        ));
    }
    Ok(())
}

/// Tag attendu, dérivé de `GITHUB_REF` — jamais passé en argument : un tag
/// présent ne peut pas être ignoré en silence, et un tag non `vX.Y.Z` est
/// refusé. Sous GitHub Actions sans `GITHUB_REF` lisible, le contrôle échoue
/// (tag invérifiable). Hors Actions (usage local), l'absence de référence est
/// neutre : `None`.
pub fn expected_tag_version(
    github_ref: Option<&str>,
    github_actions: bool,
) -> Result<Option<String>, String> {
    let Some(refer) = github_ref.filter(|r| !r.is_empty()) else {
        if github_actions {
            return Err(
                "GITHUB_REF absent sous GitHub Actions - tag invérifiable, rien n'est publié"
                    .to_string(),
            );
        }
        return Ok(None);
    };
    if let Some(tag) = refer.strip_prefix("refs/tags/") {
        if let Some(version) = tag.strip_prefix('v') {
            if !version.is_empty() && is_semver(version) {
                return Ok(Some(version.to_string()));
            }
        }
        return Err(format!(
            "tag '{tag}' non conforme (attendu vX.Y.Z) - abandon avant toute publication"
        ));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Gate qualité (job `verify`) — chaque étape court-circuite tout le pipeline.
// ---------------------------------------------------------------------------

/// Une étape du gate : nom affiché + commande complète (le premier mot est le
/// programme, le reste les arguments — commandes cargo simples, sans
/// guillemets) + variables d'environnement posées pour l'enfant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    pub name: &'static str,
    pub command: &'static str,
    pub env: &'static [(&'static str, &'static str)],
}

const SANS_ENV: &[(&str, &str)] = &[];

/// Les étapes du gate `verify` : les simulations d'abord (rapides, sans
/// réseau), puis exactement les étapes du job `test` de la CI dans son ordre
/// — fmt, clippy all-features puis sans features par défaut, tests
/// all-features, configuration par défaut puis sans features par défaut,
/// chaque feature isolée (check puis test), exemples, doc. Les mêmes
/// commandes que la CI (invariant : le gate de release ne peut pas être plus
/// laxiste que la CI) — `tests/release_pipeline.rs` fige l'égalité.
pub const GATES: &[Gate] = &[
    Gate {
        name: "simulations du pipeline (aucune publication reelle)",
        command: "cargo test -p release-tool",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo fmt (workspace)",
        command: "cargo fmt --all -- --check",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo clippy (all-features, -D warnings)",
        command: "cargo clippy --workspace --all-targets --all-features -- -D warnings",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo clippy (sans features par defaut, -D warnings)",
        command: "cargo clippy --workspace --all-targets --no-default-features -- -D warnings",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo test (all-features)",
        command: "cargo test --workspace --all-features",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo test (configuration par defaut)",
        command: "cargo test --workspace",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo test (sans features par defaut)",
        command: "cargo test --workspace --no-default-features",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo check (feature macros isolee)",
        command: "cargo check -p litemcp --no-default-features --features macros",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo test (feature macros isolee)",
        command: "cargo test -p litemcp --no-default-features --features macros",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo check (feature schemars isolee)",
        command: "cargo check -p litemcp --no-default-features --features schemars",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo test (feature schemars isolee)",
        command: "cargo test -p litemcp --no-default-features --features schemars",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo check (feature auth-sign isolee)",
        command: "cargo check -p litemcp --no-default-features --features auth-sign",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo test (feature auth-sign isolee)",
        command: "cargo test -p litemcp --no-default-features --features auth-sign",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo check (feature tls isolee)",
        command: "cargo check -p litemcp --no-default-features --features tls",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo test (feature tls isolee)",
        command: "cargo test -p litemcp --no-default-features --features tls",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo build (examples, toutes features)",
        command: "cargo build --examples --all-features",
        env: SANS_ENV,
    },
    Gate {
        name: "cargo doc (workspace, RUSTDOCFLAGS=-D warnings)",
        command: "cargo doc --workspace --no-deps --all-features",
        env: &[("RUSTDOCFLAGS", "-D warnings")],
    },
];

/// Exécution d'une étape du gate (production : process réels ; simulations :
/// verdicts fabriqués). Récepteur `&mut` : les implémentations de simulation
/// sont à état (verdicts consommés, compteurs d'appels).
pub trait Pipeline {
    /// Exécute l'étape ; `Err(code)` = code de sortie non nul.
    fn run_step(&mut self, gate: &Gate) -> Result<(), i32>;
    /// `cargo package -p <crate> --list --allow-dirty` (contenu du paquet).
    fn cargo_package_list(&mut self, crate_name: &str) -> Result<Vec<String>, String>;
}

/// Exécute le gate complet : les étapes [`GATES`] dans l'ordre, puis le
/// contenu des deux paquets. Premier échec = arrêt immédiat, rien n'est
/// publié.
pub fn verify_gates(pipeline: &mut dyn Pipeline) -> Result<(), String> {
    for gate in GATES {
        println!("[verify] {}", gate.name);
        pipeline.run_step(gate).map_err(|code| {
            format!(
                "étape '{}' en échec (exit {code}) - gate rompu, rien n'est publié",
                gate.name
            )
        })?;
    }
    println!("[verify] contenu des deux paquets (cargo package --list)");
    assert_package_contents(pipeline)?;
    println!("[verify] gate qualité vert - publication autorisée par le pipeline");
    Ok(())
}

/// Licences exigées dans les deux paquets, README en plus pour `litemcp`.
/// `--allow-dirty` : la LISTE du contenu n'évalue pas l'état commité (la
/// publication réelle de CI part d'un checkout propre, `docs/RELEASE.md`).
pub fn assert_package_contents(pipeline: &mut dyn Pipeline) -> Result<(), String> {
    let macros = pipeline.cargo_package_list("litemcp-macros").map_err(|e| {
        format!(
            "cargo package -p litemcp-macros --list en échec - paquet macros non inspectable ({e})"
        )
    })?;
    for licence in ["LICENSE-APACHE", "LICENSE-MIT"] {
        if !macros.iter().any(|entry| entry == licence) {
            return Err(format!(
                "licence {licence} absente du paquet litemcp-macros"
            ));
        }
    }
    let racine = pipeline.cargo_package_list("litemcp").map_err(|e| {
        format!("cargo package -p litemcp --list en échec - paquet racine non inspectable ({e})")
    })?;
    for fichier in ["LICENSE-APACHE", "LICENSE-MIT", "README.md"] {
        if !racine.iter().any(|entry| entry == fichier) {
            return Err(format!("{fichier} absent du paquet litemcp"));
        }
    }
    println!("[verify] licences MIT/Apache présentes dans les deux paquets, README dans litemcp");
    Ok(())
}

// ---------------------------------------------------------------------------
// crates.io — statut à trois états : une erreur réseau n'est PAS une absence.
// ---------------------------------------------------------------------------

/// Statut d'une version sur crates.io.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// HTTP 200 : la version est indexée.
    Published,
    /// HTTP 404 : la version n'existe pas (encore).
    Absent,
    /// Tout le reste (réseau, HTTP ≠ 200/404) : INVÉRIFIABLE — jamais
    /// interprété comme une absence.
    Inconclusive,
}

/// Frontières impures de la publication (production : curl + cargo réels).
/// Récepteur `&mut` : les implémentations de simulation sont à état (script
/// de statuts consommé, compteur de publications).
pub trait RegistryOps {
    fn crates_io_status(&mut self, crate_name: &str, version: &str) -> Status;
    /// `cargo publish -p <crate>` ; renvoie (code de sortie, sortie combinée).
    fn run_cargo_publish(&mut self, crate_name: &str, dry_run: bool) -> (i32, String);
    /// Pause entre deux sondes d'indexation (10 s en production ; no-op dans
    /// les simulations).
    fn sleep(&mut self, secs: u64);
}

/// `$true` de l'ancien script : crates.io a refusé la republication parce que
/// la version est DÉJÀ indexée (« already uploaded » / « already exists »).
/// Toute autre sortie (réseau, authentification, validation) est une erreur
/// réelle.
pub fn test_cargo_publish_output(output: &str) -> bool {
    output.contains("already uploaded") || output.contains("already exists")
}

/// Publie une crate après vérification du statut crates.io (trois états),
/// puis attend l'indexation. Idempotent : déjà en ligne = ignorée ;
/// « already uploaded » = traité comme publié ; toute autre erreur remonte
/// TELLE QUELLE.
pub fn publish_crate(
    crate_name: &str,
    version: &str,
    dry_run: bool,
    ops: &mut dyn RegistryOps,
) -> Result<(), String> {
    match ops.crates_io_status(crate_name, version) {
        Status::Published => {
            println!(
                "[publish] {crate_name} {version} déjà en ligne sur crates.io - publication ignorée"
            );
            return Ok(());
        }
        Status::Absent => {
            println!("[publish] {crate_name} {version} absente de crates.io - publication");
        }
        Status::Inconclusive => {
            return Err(format!(
                "statut de {crate_name} {version} invérifiable sur crates.io (réseau/HTTP ≠ 200/404) - abandon SANS publication : une erreur réseau n'est pas une preuve d'absence"
            ));
        }
    }
    let (exit, output) = ops.run_cargo_publish(crate_name, dry_run);
    for ligne in output.lines() {
        println!("{ligne}");
    }
    if exit != 0 {
        if test_cargo_publish_output(&output) {
            println!(
                "[publish] {crate_name} {version} déjà indexée (crates.io a refusé la republication : « already uploaded ») - traité comme publié"
            );
            return Ok(());
        }
        return Err(format!(
            "cargo publish -p {crate_name} en échec (exit {exit}) - aucune interprétation complaisante : l'échec est remonté tel quel"
        ));
    }
    if dry_run {
        println!("[publish] [simulation --dry-run] aucun envoi ; attente d'indexation ignorée");
        return Ok(());
    }
    wait_indexed(crate_name, version, ops)
}

/// Attend que crates.io indexe la version (18 sondes espacées de 10 s). Un
/// statut invérifiable à la fin remonte l'erreur réseau TELLE QUELLE — jamais
/// interprétée comme une absence.
fn wait_indexed(crate_name: &str, version: &str, ops: &mut dyn RegistryOps) -> Result<(), String> {
    const MAX_ATTEMPTS: u32 = 18;
    let mut dernier = Status::Inconclusive;
    for tentative in 1..=MAX_ATTEMPTS {
        dernier = ops.crates_io_status(crate_name, version);
        match dernier {
            Status::Published => {
                println!("[publish] {crate_name} {version} est en ligne sur crates.io");
                return Ok(());
            }
            Status::Absent => {
                println!(
                    "[publish] {crate_name} {version} : pas encore indexée - tentative {tentative}/{MAX_ATTEMPTS}..."
                );
            }
            Status::Inconclusive => {
                println!(
                    "[publish] {crate_name} {version} : statut invérifiable (réseau/HTTP) - tentative {tentative}/{MAX_ATTEMPTS}..."
                );
            }
        }
        if tentative < MAX_ATTEMPTS {
            ops.sleep(10);
        }
    }
    if dernier == Status::Inconclusive {
        return Err(format!(
            "crates.io injoignable (erreur réseau/HTTP) après {MAX_ATTEMPTS} tentatives - indexation de {crate_name} {version} INVÉRIFIABLE ; l'erreur réseau est remontée, jamais interprétée comme une absence"
        ));
    }
    Err(format!(
        "{crate_name} {version} non indexée après {MAX_ATTEMPTS} tentatives"
    ))
}

/// Flux de publication complet : SemVer, cohérence avec le manifeste racine,
/// puis `litemcp-macros` d'abord (litemcp en dépend avec une exigence de
/// version) et `litemcp` ensuite — ordre immuable.
pub fn publish_flow(
    publish_version: &str,
    dry_run: bool,
    facts: &VersionFacts,
    ops: &mut dyn RegistryOps,
) -> Result<(), String> {
    if !is_semver(publish_version) {
        return Err(format!("version '{publish_version}' non SemVer - abandon"));
    }
    if facts.root_version != publish_version {
        return Err(format!(
            "version à publier '{publish_version}' ≠ version du manifeste racine '{}' - relancez le gate de cohérence",
            facts.root_version
        ));
    }
    publish_crate("litemcp-macros", &facts.macros_version, dry_run, ops)?;
    publish_crate("litemcp", publish_version, dry_run, ops)?;
    println!(
        "[publish] litemcp {publish_version} : publiée (ou déjà en ligne) - litemcp-macros {} : publiée (ou déjà en ligne)",
        facts.macros_version
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Implémentations de production (processus réels).
// ---------------------------------------------------------------------------

/// Exécution réelle des étapes du gate : process enfant, sortie héritée.
pub struct LiveRunner;

impl Pipeline for LiveRunner {
    fn run_step(&mut self, gate: &Gate) -> Result<(), i32> {
        let mut mots = gate.command.split_whitespace();
        let Some(programme) = mots.next() else {
            return Err(2);
        };
        let mut commande = Command::new(programme);
        commande.args(mots);
        for (cle, valeur) in gate.env {
            commande.env(cle, valeur);
        }
        match commande.status() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(status.code().unwrap_or(1)),
            Err(error) => {
                println!(
                    "[release] !!! étape '{}' impossible à lancer : {error}",
                    gate.name
                );
                Err(1)
            }
        }
    }

    fn cargo_package_list(&mut self, crate_name: &str) -> Result<Vec<String>, String> {
        let sortie = Command::new("cargo")
            .args(["package", "-p", crate_name, "--list", "--allow-dirty"])
            .output()
            .map_err(|e| e.to_string())?;
        if !sortie.status.success() {
            return Err(format!("exit {}", sortie.status.code().unwrap_or(1)));
        }
        Ok(String::from_utf8_lossy(&sortie.stdout)
            .lines()
            .map(str::to_string)
            .collect())
    }
}

/// Statut crates.io et publication réels (curl + cargo).
pub struct LiveOps;

fn curl_command() -> &'static str {
    if cfg!(windows) { "curl.exe" } else { "curl" }
}

impl RegistryOps for LiveOps {
    fn crates_io_status(&mut self, crate_name: &str, version: &str) -> Status {
        let sortie = Command::new(curl_command())
            .args([
                "-s",
                "--max-time",
                "30",
                "-A",
                REGISTRY_USER_AGENT,
                "-w",
                "\n%{http_code}",
                &format!("https://crates.io/api/v1/crates/{crate_name}/{version}"),
            ])
            .output();
        let Ok(sortie) = sortie else {
            return Status::Inconclusive;
        };
        if !sortie.status.success() {
            return Status::Inconclusive;
        }
        let texte = String::from_utf8_lossy(&sortie.stdout);
        let Some(code) = texte.lines().rev().find(|l| !l.trim().is_empty()) else {
            return Status::Inconclusive;
        };
        match code.trim() {
            "200" => Status::Published,
            "404" => Status::Absent,
            _ => Status::Inconclusive,
        }
    }

    fn run_cargo_publish(&mut self, crate_name: &str, dry_run: bool) -> (i32, String) {
        let mut commande = Command::new("cargo");
        commande.args(["publish", "-p", crate_name]);
        if dry_run {
            // Simulation locale uniquement : pas d'envoi, pas de
            // re-vérification (le gate verify l'a déjà faite) et tolérance
            // d'un arbre de travail en cours — la publication réelle de CI
            // part d'un checkout propre et ne passe jamais par ces drapeaux.
            commande.args(["--dry-run", "--no-verify", "--allow-dirty"]);
        }
        match commande.output() {
            Ok(sortie) => {
                let texte = format!(
                    "{}{}",
                    String::from_utf8_lossy(&sortie.stdout),
                    String::from_utf8_lossy(&sortie.stderr)
                );
                (sortie.status.code().unwrap_or(1), texte)
            }
            Err(error) => (1, format!("cargo publish impossible à lancer : {error}")),
        }
    }

    fn sleep(&mut self, secs: u64) {
        std::thread::sleep(std::time::Duration::from_secs(secs));
    }
}
