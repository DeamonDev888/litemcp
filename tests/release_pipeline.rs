//! Garde-fous du pipeline de publication.
//!
//! Ces tests figent les invariants qui empêchent une publication prématurée :
//!   1. `publish.yml` (break-glass) n'a **aucun** déclencheur `tag` — un tag
//!      ne peut donc jamais publier avant le gate `verify` de `release.yml` ;
//!   2. l'écriture sur le dépôt (`contents: write`) est réservée au **seul**
//!      job `github-release` ;
//!   3. toutes les actions sont épinglées par SHA de commit ;
//!   4. la notification du tuto part après la release GitHub, jamais avant ;
//!   5. le job `publish` revalide lui-même les versions (`cargo metadata`)
//!      juste avant le premier `cargo publish`, sans dépendre de l'ordre des
//!      jobs, et n'invoque jamais le script avec une variable de version vide
//!      (une liaison `-Param ""` dépend de l'hôte qui lance PowerShell) ; le
//!      tag n'est jamais passé en argument : le script le dérive de `GITHUB_REF`
//!      (source unique, un tag présent ne peut pas être ignoré en silence) ;
//!   6. les licences MIT/Apache accompagnent le paquet `litemcp-macros` ;
//!   7. la CI exécute la **matrice de features** (`--no-default-features`,
//!      chaque feature isolée) et construit la doc avec `RUSTDOCFLAGS=-D
//!      warnings` — le gate `verify` de release exécute les mêmes contrôles ;
//!   8. les deux versions de crate sont alignées (exigence racine =
//!      `litemcp-macros`) et la sonde d'audit privée reste hors de la crate
//!      publiée (jamais d'avertissement `cargo package` ni de fuite).
//!
//! `.github/` est exclu du paquet crates.io (`exclude` du manifeste) : ces
//! assertions ne concernent que l'arbre du dépôt et sont donc ignorées dans
//! une crate empaquetée.

use std::path::{Path, PathBuf};

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Contenu d'un workflow, ou `None` si `.github/` n'est pas présent (paquet).
fn workflow(name: &str) -> Option<String> {
    std::fs::read_to_string(workspace().join(".github").join("workflows").join(name)).ok()
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Corps d'un job : de `  <job>:` jusqu'au prochain nom de job (indent 2).
fn job_block(text: &str, job: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let header = format!("  {job}:");
    let start = lines.iter().position(|l| l.trim_end() == header)?;
    let mut body = Vec::new();
    for line in &lines[start + 1..] {
        let trimmed = line.trim_end();
        let is_next_job = indent_of(trimmed) == 2
            && !trimmed.trim_start().starts_with('#')
            && trimmed.ends_with(':');
        if is_next_job {
            break;
        }
        body.push(*line);
    }
    Some(body.join("\n"))
}

/// Toutes les lignes `uses:` qui ne sont pas épinglées par un SHA de 40 hex.
fn unpinned_actions(text: &str) -> Vec<String> {
    let mut bad = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let rest = trimmed
            .strip_prefix("- uses: ")
            .or_else(|| trimmed.strip_prefix("uses: "));
        let Some(rest) = rest else { continue };
        let spec = rest.split('#').next().unwrap_or("").trim();
        let sha = spec.rsplit_once('@').map(|(_, sha)| sha).unwrap_or("");
        let pinned = sha.len() == 40
            && sha
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
        if !pinned {
            bad.push(spec.to_string());
        }
    }
    bad
}

fn assert_workflows_available() -> Option<(String, String)> {
    match (workflow("release.yml"), workflow("publish.yml")) {
        (Some(release), Some(publish)) => Some((release, publish)),
        _ => {
            eprintln!(
                "workflows absents (paquet crates.io, `.github/` exclu) — garde-fous ignorés"
            );
            None
        }
    }
}

#[test]
fn publication_workflow_has_no_tag_trigger() {
    let Some((_, publish)) = assert_workflows_available() else {
        return;
    };
    // Un déclencheur `push: tags:` dans publish.yml rouvrirait le chemin de
    // publication sans gate (le groupe de concurrence sérialise, il n'ordonne
    // pas : un run pourrait publier avant `verify`).
    for line in publish.lines() {
        let trimmed = line.trim();
        assert_ne!(
            trimmed, "push:",
            "publish.yml ne doit pas se déclencher sur push (tag = release.yml uniquement)"
        );
        assert_ne!(
            trimmed, "tags:",
            "publish.yml ne doit pas filtrer sur tags (chemin manuel uniquement)"
        );
    }
    assert!(
        publish.contains("workflow_dispatch:"),
        "publish.yml doit rester le chemin manuel (workflow_dispatch)"
    );
}

#[test]
fn release_workflow_is_the_only_tag_entry_point() {
    let Some((release, _)) = assert_workflows_available() else {
        return;
    };
    assert!(
        release.lines().any(|l| l.trim() == "push:"),
        "release.yml doit rester déclenché par un push de tag"
    );
    assert!(
        release.contains("- 'v*'"),
        "release.yml doit filtrer les tags v*"
    );
}

#[test]
fn write_scope_is_limited_to_the_github_release_job() {
    let Some((release, publish)) = assert_workflows_available() else {
        return;
    };
    for (name, text) in [("release.yml", &release), ("publish.yml", &publish)] {
        let writes: Vec<&str> = text
            .lines()
            .filter(|l| l.trim() == "contents: write" && !l.trim_start().starts_with('#'))
            .collect();
        if name == "publish.yml" {
            assert!(
                writes.is_empty(),
                "publish.yml ne doit jamais écrire sur le dépôt (trouvé {} occurrence(s))",
                writes.len()
            );
        }
    }

    let writes: Vec<&str> = release
        .lines()
        .filter(|l| l.trim() == "contents: write" && !l.trim_start().starts_with('#'))
        .collect();
    assert_eq!(
        writes.len(),
        1,
        "release.yml doit élever `contents: write` une seule fois, dans github-release"
    );
    let job = job_block(&release, "github-release").expect("job github-release présent");
    assert!(
        job.lines().any(|l| l.trim() == "contents: write"),
        "`contents: write` doit être porté par le job github-release, pas par le workflow"
    );

    // Valeur par défaut au niveau du workflow : lecture seule.
    let head = release
        .split("jobs:")
        .next()
        .expect("section d'en-tête du workflow");
    assert!(
        head.contains("permissions:\n  contents: read"),
        "le workflow Release doit rester en `contents: read` par défaut"
    );
}

#[test]
fn registry_token_is_optional_and_oidc_is_scoped() {
    let Some((release, publish)) = assert_workflows_available() else {
        return;
    };
    // Trusted publishing crates.io : `id-token: write` est nécessaire à
    // l'échange OIDC, mais uniquement dans le job qui publie. Le jeton reste
    // optionnel : si le secret existe, l'étape OIDC est ignorée et le secret
    // est utilisé (aucune rupture pour les dépôts déjà configurés).
    for (name, text) in [("release.yml", &release), ("publish.yml", &publish)] {
        let head = text.split("jobs:").next().expect("en-tête du workflow");
        assert!(
            !head.contains("id-token"),
            "{name} : `id-token` ne doit pas être élevé au niveau du workflow"
        );
        let occurrences = text
            .lines()
            .filter(|l| l.trim() == "id-token: write")
            .count();
        assert_eq!(
            occurrences, 1,
            "{name} : `id-token: write` doit apparaître une seule fois (job publish)"
        );
        let job = job_block(text, "publish").expect("job publish présent");
        assert!(
            job.contains("id-token: write"),
            "{name} : l'échange OIDC doit être confiné au job publish"
        );
        assert!(
            job.contains("contents: read"),
            "{name} : élever les permissions du job publish ne doit pas retirer contents: read (checkout)"
        );
        assert!(
            job.contains("rust-lang/crates-io-auth-action@"),
            "{name} : le jeton crates.io doit pouvoir venir d'OIDC"
        );
        assert!(
            job.contains("if: env.HAS_REGISTRY_TOKEN != 'true'"),
            "{name} : l'étape OIDC doit être ignorée quand le secret existe"
        );
        assert!(
            job.contains("steps.auth.outputs.token")
                && job.contains("secrets.CARGO_REGISTRY_TOKEN"),
            "{name} : le jeton doit rester optionnel (secret prioritaire, OIDC en repli)"
        );
    }
}

#[test]
fn publication_status_is_reported_by_the_job_result() {
    let Some((release, publish)) = assert_workflows_available() else {
        return;
    };
    // Plus d'output `published=true` posé inconditionnellement après l'étape
    // de publication : il affirmait « publié » même quand la version était
    // déjà en ligne. La notification s'appuie sur le résultat du job.
    for (name, text) in [("release.yml", &release), ("publish.yml", &publish)] {
        assert!(
            !text.contains("published:") && !text.contains("published=true"),
            "{name} : aucun output `published` fabriqué ne doit subsister"
        );
        let job = job_block(text, "notifier").expect("job notifier présent");
        assert!(
            job.contains("needs.publish.result == 'success'"),
            "{name} : la notification doit dépendre du résultat réel de la publication"
        );
    }
}

#[test]
fn every_workflow_pins_actions_by_commit_sha() {
    // Politique du dépôt : toute action est épinglée par SHA de commit, dans
    // TOUS les workflows (un tag peut être redéplacé, un SHA non).
    let dir = workspace().join(".github").join("workflows");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("workflows absents (paquet crates.io) — garde-fou ignoré");
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == "yml" || e == "yaml")
        })
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "aucun workflow trouvé sous .github/workflows"
    );
    for path in files {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} illisible : {e}", path.display()));
        let bad = unpinned_actions(&text);
        assert!(
            bad.is_empty(),
            "{} : actions non épinglées par SHA de commit : {bad:?}",
            path.display()
        );
    }
}

#[test]
fn notifier_waits_for_the_github_release() {
    let Some((release, _)) = assert_workflows_available() else {
        return;
    };
    let job = job_block(&release, "notifier").expect("job notifier présent");
    let needs: Vec<&str> = job
        .lines()
        .filter(|l| l.trim_start().starts_with("needs:"))
        .collect();
    assert_eq!(needs.len(), 1, "notifier doit déclarer un bloc `needs:`");
    let needs = needs[0];
    assert!(
        needs.contains("publish"),
        "notifier doit dépendre de publish ({needs})"
    );
    assert!(
        needs.contains("github-release"),
        "notifier doit dépendre de github-release : la notification ne peut pas \
         partir avant que la release existe ({needs})"
    );
    assert!(
        job.contains("needs.github-release.result"),
        "notifier doit tolérer un github-release ignoré (dispatch manuel) sans \
         se déclencher sur un échec"
    );
}

#[test]
fn publish_job_rechecks_versions_before_publishing() {
    let Some((release, publish)) = assert_workflows_available() else {
        return;
    };
    for (name, text) in [("release.yml", &release), ("publish.yml", &publish)] {
        let job = job_block(text, "publish").expect("job publish présent");
        assert!(
            job.contains("needs: verify"),
            "{name} : le job publish doit dépendre du gate verify"
        );
        let check = job.find("ARGS=(check)").unwrap_or_else(|| {
            panic!(
                "{name} : le job publish doit revalider les versions (cargo metadata) \
                 avant le premier cargo publish"
            )
        });
        let publication = job
            .find("release.ps1 publish")
            .unwrap_or_else(|| panic!("{name} : le job publish doit publier via release.ps1"));
        assert!(
            check < publication,
            "{name} : la revalidation doit précéder la publication"
        );
    }

    let verify = job_block(&release, "verify").expect("job verify présent");
    assert!(
        verify.contains("release.ps1 verify"),
        "release.yml : le gate verify doit exécuter le script partagé"
    );
    assert!(
        verify.contains("ARGS=(check)"),
        "release.yml : le gate doit vérifier la cohérence des versions"
    );
}

#[test]
fn workflows_never_pass_an_empty_version_argument() {
    let Some((release, publish)) = assert_workflows_available() else {
        return;
    };
    // `pwsh -File script.ps1 -Param ""` n'est pas portable : Windows
    // PowerShell 5.1 supprime l'argument vide et la liaison échoue en
    // « argument manquant ». Les workflows construisent donc la liste
    // d'arguments et n'ajoutent que les valeurs non vides ; le tag, lui, est
    // lu par le script depuis `GITHUB_REF` — jamais recopié en argument.
    for (name, text) in [("release.yml", &release), ("publish.yml", &publish)] {
        for forbidden in [
            "-TagVersion",
            "release.ps1 check -InputVersion",
            "release.ps1 check -TagVersion",
        ] {
            assert!(
                !text.contains(forbidden),
                "{name} : `{forbidden}` ne doit pas apparaître (le tag vient de GITHUB_REF, \
                 les arguments vides ne sont jamais transmis)"
            );
        }
        assert!(
            text.contains("ARGS=(check)") && text.contains("\"${ARGS[@]}\""),
            "{name} : l'appel de cohérence doit n'ajouter que les arguments non vides"
        );
    }
}

/// Version déclarée par un manifeste (première ligne `version = "…"`).
///
/// Volontairement sans dépendance TOML : le manifeste racine et celui de
/// `litemcp-macros` sont de forme connue, un parseur complet serait une
/// dépendance de plus pour trois lignes.
fn manifest_version(manifest: &str) -> Option<String> {
    manifest.lines().map(str::trim).find_map(|line| {
        line.strip_prefix("version = ")
            .and_then(|value| value.trim().trim_matches('"').split('"').next())
            .map(str::to_string)
    })
}

/// Exigence de version de la dépendance `litemcp-macros` du manifeste racine.
fn macros_requirement(manifest: &str) -> Option<String> {
    let line = manifest
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("litemcp-macros = "))?;
    let after_key = line.split("version = ").nth(1)?;
    after_key.split('"').nth(1).map(str::to_string)
}

#[test]
fn feature_matrix_is_enforced_in_ci_and_release_gate() {
    // `--all-features` ne prouve rien des configurations servies aux
    // utilisateurs : le noyau sans features par défaut et chaque feature
    // isolée sont des surfaces publiques. Les mêmes commandes doivent tourner
    // en CI (retour rapide) et dans le gate de release (avant publication).
    const COMMANDS: [&str; 3] = [
        "cargo test --workspace --no-default-features",
        "cargo check -p litemcp --no-default-features --features macros",
        "cargo check -p litemcp --no-default-features --features schemars",
    ];

    let Some(ci) = workflow("ci.yml") else {
        return;
    };
    for command in COMMANDS {
        assert!(
            ci.contains(command),
            "ci.yml : la matrice de features doit exécuter `{command}`"
        );
    }

    // `scripts/` est exclu du paquet crates.io : dans une crate empaquetée,
    // le gate de release n'est pas là et l'assertion est sans objet.
    let Ok(release) = std::fs::read_to_string(workspace().join("scripts").join("release.ps1"))
    else {
        eprintln!("scripts/release.ps1 absent (paquet crates.io) — assertion ignorée");
        return;
    };
    for command in COMMANDS {
        assert!(
            release.contains(command),
            "release.ps1 (verify) : la matrice de features doit exécuter `{command}`"
        );
    }
}

#[test]
fn rustdoc_warnings_are_denied_in_ci_and_release_gate() {
    // Un lien intra-doc cassé ou redondant ne doit jamais passer en silence :
    // la doc est construite avec `-D warnings` sur TOUT le workspace.
    let Some(ci) = workflow("ci.yml") else {
        return;
    };
    assert!(
        ci.contains("RUSTDOCFLAGS: -D warnings") || ci.contains("RUSTDOCFLAGS=\"-D warnings\""),
        "ci.yml : la doc doit être construite avec RUSTDOCFLAGS=-D warnings"
    );
    assert!(
        ci.contains("cargo doc --workspace --no-deps --all-features"),
        "ci.yml : la doc doit couvrir tout le workspace"
    );

    let Ok(release) = std::fs::read_to_string(workspace().join("scripts").join("release.ps1"))
    else {
        eprintln!("scripts/release.ps1 absent (paquet crates.io) — assertion ignorée");
        return;
    };
    assert!(
        release.contains("$env:RUSTDOCFLAGS = '-D warnings'"),
        "release.ps1 : le gate de doc doit poser RUSTDOCFLAGS=-D warnings"
    );
    assert!(
        release.contains("cargo doc --workspace --no-deps --all-features"),
        "release.ps1 : le gate de doc doit couvrir tout le workspace"
    );
}

#[test]
fn crate_versions_are_aligned() {
    // crates.io refuse une dépendance versionnée non publiée : un bump de
    // `litemcp-macros` sans son exigence racine (ou l'inverse) fait échouer la
    // publication de `litemcp` APRÈS celle des macros. Une version publiée est
    // immuable — le bump doit donc toujours toucher les deux manifestes.
    let root = workspace();
    let root_manifest =
        std::fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml racine lisible");
    let Ok(macros_manifest) =
        std::fs::read_to_string(root.join("litemcp-macros").join("Cargo.toml"))
    else {
        eprintln!("litemcp-macros absent (paquet crates.io) — assertion ignorée");
        return;
    };
    let macros_version = manifest_version(&macros_manifest)
        .expect("version déclarée dans litemcp-macros/Cargo.toml");
    let requirement = macros_requirement(&root_manifest)
        .expect("exigence `litemcp-macros = { … version = \"X.Y.Z\" }` dans Cargo.toml racine");
    assert_eq!(
        requirement, macros_version,
        "l'exigence `litemcp-macros` du manifeste racine doit être exactement la version publiée de la macros \
         (une version crates.io est immuable : bump macros ET exigence ensemble)"
    );
}

#[test]
fn audit_probe_stays_out_of_the_published_crate() {
    // Sonde d'audit locale (`examples/audit_target.rs`, ignorée par Git) :
    // laissée dans `examples/`, elle n'est pas dans le paquet et `cargo
    // package` avertit à chaque empaquetage. Elle vit donc dans `.zwork/`,
    // exclue par construction ; l'exclusion explicite reste en défense en
    // profondeur si une copie réapparaît le temps d'une campagne.
    let root = workspace();
    let manifest =
        std::fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml racine lisible");
    assert!(
        manifest.contains("\"examples/audit_target.rs\""),
        "Cargo.toml : la sonde d'audit doit rester explicitement exclue du paquet"
    );
    assert!(
        !root.join("examples").join("audit_target.rs").exists(),
        "examples/audit_target.rs est une sonde privée : elle vit dans .zwork/ (sinon `cargo package` \
         avertit et le fichier n'est de toute façon jamais publié)"
    );
}

#[test]
fn macros_package_ships_both_licenses() {
    let macros = workspace().join("litemcp-macros");
    // Le paquet crates.io de `litemcp` ne contient pas la crate macros (elle
    // est publiée séparément) : dans ce cas seulement, l'assertion est sans
    // objet. Dans le dépôt, elle est toujours évaluée.
    let Ok(manifest) = std::fs::read_to_string(macros.join("Cargo.toml")) else {
        eprintln!("litemcp-macros absent (paquet crates.io) — assertion ignorée");
        return;
    };
    assert!(
        manifest.contains("license = \"MIT OR Apache-2.0\""),
        "litemcp-macros doit déclarer la double licence"
    );
    for licence in ["LICENSE-APACHE", "LICENSE-MIT"] {
        assert!(
            Path::new(&macros.join(licence)).is_file(),
            "{licence} doit être présent à la racine de litemcp-macros (inclus dans le paquet)"
        );
    }
}
