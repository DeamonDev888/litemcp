#Requires -Version 5.1
<#
release.ps1 - contrôles partagés du pipeline de publication LiteMCP.

Le MÊME script est exécuté par les deux workflows de publication :
  - release.yml  (canonique, push de tag `v*`)          -> check + verify + publish
  - publish.yml  (break-glass, dispatch manuel unique)  -> check + verify + publish

Sous-commandes :
  semver  -Version <x.y.z>        valide la version contre la regex SemVer
                                  complète (semver.org) - toutes les
                                  combinaisons numériques (0.10.10 incluse).
  check   [-TagVersion <v>] [-InputVersion <v>] [-ChangelogPath <fichier>]
                                  cohérence AVANT toute publication, dérivée
                                  de `cargo metadata` : version racine =
                                  tag/input, version litemcp-macros = exigence
                                  exacte de la dépendance racine, section
                                  `## [X.Y.Z]` présente dans le CHANGELOG.
                                  Sur un push de tag, le tag est dérivé de
                                  `GITHUB_REF` (refs/tags/vX.Y.Z) : aucun
                                  argument optionnel ne peut donc vider le
                                  contrôle, et un tag mal formé est refusé.
                                  Écrit `version=<x.y.z>` sur stdout (dernière
                                  ligne du contrôle) pour le workflow.
  verify                          gate qualité : simulations du pipeline
                                  (release-tests.ps1 - aucun secret, aucune
                                  publication), fmt (workspace), clippy
                                  -D warnings, tests all-features ET sans les
                                  features par défaut, chaque feature isolée
                                  (macros, schemars), doc rustdoc STRICTE
                                  (RUSTDOCFLAGS=-D warnings, workspace), contenu
                                  des deux paquets (licences MIT/Apache,
                                  README) - échec = arrêt immédiat.
  publish -Version <x.y.z> [-DryRun]
                                  publie litemcp-macros puis litemcp, dans cet
                                  ordre, avec distinction stricte entre
                                  « déjà indexée » (crates.io le confirme) et
                                  toute autre erreur (réseau, auth, validation)
                                  qui est RÉELLE : une erreur réseau n'est
                                  JAMAIS interprétée comme une preuve d'absence.
                                  -DryRun : `cargo publish --dry-run` (aucun
                                  envoi, aucun jeton requis) pour les
                                  simulations locales.

Codes de sortie : 0 succès, 1 échec de contrôle, 2 usage invalide.

Encodage : UTF-8 **avec BOM** (Windows PowerShell 5.1 lit sinon les accents
en ANSI). Les chaînes évitent volontairement les guillemets typographiques
(0x92/0x93/0x94 en CP1252) : même privé de BOM, le script reste analysable.
#>
param(
    [Parameter(Position = 0)]
    [string]$Command,

    [string]$Version = '',
    [string]$TagVersion = '',
    [string]$InputVersion = '',
    [string]$ChangelogPath = 'CHANGELOG.md',
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

# Regex SemVer officielle (semver.org) : majeure/mineure/correctif numériques
# sans zéro initial (toutes largeurs - 0.10.10, 10.10.10...), prérelease et
# build metadata. Remplace l'ancienne garde à motifs fixes qui refusait des
# versions valides comme 0.10.10.
$SemVerPattern = '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$'

$CratesIoApi = 'https://crates.io/api/v1/crates'
$RegistryUserAgent = 'litemcp-release (https://github.com/DeamonDev888/litemcp)'
$GITHUB_REF_PREFIX = 'refs/tags/'

function Test-SemVer {
    param([string]$Value)
    return ($Value -match $SemVerPattern)
}

function Stop-Release {
    param([string]$Message)
    Write-Host "[release] !!! $Message"
    exit 1
}

# ---------------------------------------------------------------------------
# Gate qualité (job `verify`) - chaque étape court-circuite tout le pipeline.
# ---------------------------------------------------------------------------

function Invoke-ReleaseStep {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$CommandLine
    )
    Write-Host ''
    Write-Host "[verify] $Name"
    Invoke-Expression -Command $CommandLine
    if ($LASTEXITCODE -ne 0) {
        Stop-Release "étape '$Name' en échec (exit $LASTEXITCODE) - gate rompu, rien n'est publié"
    }
}

function Assert-PackageContents {
    Write-Host ''
    Write-Host '[verify] contenu des deux paquets (cargo package --list)'
    # --allow-dirty : la LISTE du contenu n'évalue pas l'état commité (le
    # `cargo package` complet de la séquence locale, lui, s'en charge -
    # docs/RELEASE.md). Sans lui, le gate serait inutilisable sur un arbre
    # de travail en cours ; sur un checkout CI propre il est sans effet.
    $macros = & cargo package -p litemcp-macros --list --allow-dirty
    if ($LASTEXITCODE -ne 0) {
        Stop-Release 'cargo package -p litemcp-macros --list en échec - paquet macros non inspectable'
    }
    foreach ($licence in @('LICENSE-APACHE', 'LICENSE-MIT')) {
        if ($macros -notcontains $licence) {
            Stop-Release "licence $licence absente du paquet litemcp-macros"
        }
    }

    $racine = & cargo package -p litemcp --list --allow-dirty
    if ($LASTEXITCODE -ne 0) {
        Stop-Release 'cargo package -p litemcp --list en échec - paquet racine non inspectable'
    }
    foreach ($fichier in @('LICENSE-APACHE', 'LICENSE-MIT', 'README.md')) {
        if ($racine -notcontains $fichier) {
            Stop-Release "$fichier absent du paquet litemcp"
        }
    }

    Write-Host '[verify] licences MIT/Apache présentes dans les deux paquets, README dans litemcp'
}

function Get-HostExecutable {
    # Chemin de l'hôte PowerShell courant : le harnais de simulations et
    # chaque scénario tournent dans un processus enfant (5.1 comme 7).
    $path = $null
    try {
        $path = [System.Diagnostics.Process]::GetCurrentProcess().MainModule.FileName
    } catch {
        $path = $null
    }
    if ([string]::IsNullOrEmpty($path)) {
        $path = (Get-Process -Id $PID).Path
    }
    return $path
}

function Invoke-VerifyGates {
    # Simulations d'abord (quelques secondes, aucun secret) : le pipeline se
    # vérifie lui-même avant le gate lourd. Processus séparé obligatoire - le
    # harnais dot-source ce script (il redéfinirait ses fonctions ici) et ses
    # cas négatifs sortent en `exit 1`, qui doit rester un code de sortie
    # observable au lieu de terminer brutalement le gate.
    $hostExe = Get-HostExecutable
    $tests = Join-Path $PSScriptRoot 'release-tests.ps1'
    Invoke-ReleaseStep 'simulations du pipeline (aucune publication reelle)' "& '$hostExe' -NoProfile -File '$tests'"
    Invoke-ReleaseStep 'cargo fmt (workspace)' 'cargo fmt --all -- --check'
    Invoke-ReleaseStep 'cargo clippy (-D warnings)' 'cargo clippy --workspace --all-targets --all-features -- -D warnings'
    Invoke-ReleaseStep 'cargo test (all-features)' 'cargo test --workspace --all-features'
    # Matrice de features - mêmes commandes que ci.yml : le gate de release ne
    # peut pas être plus laxiste que la CI. Le noyau sans features par défaut
    # et chaque feature isolée sont des surfaces publiques distinctes.
    Invoke-ReleaseStep 'cargo test (sans features par defaut)' 'cargo test --workspace --no-default-features'
    Invoke-ReleaseStep 'cargo check (feature macros isolee)' 'cargo check -p litemcp --no-default-features --features macros'
    Invoke-ReleaseStep 'cargo check (feature schemars isolee)' 'cargo check -p litemcp --no-default-features --features schemars'
    # Doc STRICTE : un lien intra-doc cassé ou redondant rompt la publication.
    # RUSTDOCFLAGS doit être posé pour le processus enfant lancé par
    # Invoke-Expression ; restauré ensuite pour ne pas colorer les étapes
    # suivantes (en cas d'échec le script sort de toute façon).
    $previousRustdocFlags = $env:RUSTDOCFLAGS
    $env:RUSTDOCFLAGS = '-D warnings'
    try {
        Invoke-ReleaseStep 'cargo doc (workspace, RUSTDOCFLAGS=-D warnings)' 'cargo doc --workspace --no-deps --all-features'
    } finally {
        $env:RUSTDOCFLAGS = $previousRustdocFlags
    }
    Assert-PackageContents
    Write-Host ''
    Write-Host '[verify] gate qualité vert - publication autorisée par le pipeline'
}

# ---------------------------------------------------------------------------
# Cohérence des versions (avant toute publication) - dérivée de cargo metadata.
# ---------------------------------------------------------------------------

function Get-VersionFacts {
    $raw = & cargo metadata --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) {
        Stop-Release 'cargo metadata en échec - versions invérifiables, rien n''est publié'
    }
    $document = ($raw -join "`n") | ConvertFrom-Json

    $root = $document.packages | Where-Object { $_.name -eq 'litemcp' } | Select-Object -First 1
    if (-not $root) {
        Stop-Release 'paquet litemcp absent de cargo metadata'
    }
    $macros = $document.packages | Where-Object { $_.name -eq 'litemcp-macros' } | Select-Object -First 1
    if (-not $macros) {
        Stop-Release 'paquet litemcp-macros absent de cargo metadata'
    }
    $dependency = $root.dependencies | Where-Object { $_.name -eq 'litemcp-macros' } | Select-Object -First 1
    if (-not $dependency) {
        Stop-Release 'la dépendance litemcp-macros est absente du manifeste racine'
    }

    return [pscustomobject]@{
        RootVersion   = [string]$root.version
        MacrosVersion = [string]$macros.version
        MacrosReq     = [string]$dependency.req
    }
}

function Assert-VersionCoherence {
    param(
        [Parameter(Mandatory)]$Facts,
        [Parameter(Mandatory)][string]$PublishVersion,
        [string]$TagVersion,
        [string]$InputVersion,
        [Parameter(Mandatory)][string]$ChangelogText
    )

    # 1) SemVer complet - y compris les combinaisons numériques multi-chiffres
    #    (`0.10.10`) que l'ancienne garde refusait.
    if (-not (Test-SemVer $PublishVersion)) {
        Stop-Release "version '$PublishVersion' non SemVer (regex semver.org complète) - abandon"
    }

    # 2) Tag poussé et/ou input manuel : égaux à la version du manifeste
    #    racine (dérivée de cargo metadata, jamais extraite par sed).
    foreach ($source in @(
            [pscustomobject]@{ Nom = 'tag'; Valeur = $TagVersion },
            [pscustomobject]@{ Nom = 'input'; Valeur = $InputVersion }
        )) {
        if (-not [string]::IsNullOrEmpty($source.Valeur) -and $source.Valeur -ne $PublishVersion) {
            Stop-Release "$($source.Nom) '$($source.Valeur)' ≠ version du manifeste racine '$PublishVersion' - abandon avant toute publication"
        }
    }

    # 3) Version litemcp-macros = exigence exacte de la dépendance racine
    #    (crates.io refuse une dépendance versionnée non publiée : un écart
    #    ferait échouer la publication de litemcp après celle des macros).
    #    `cargo metadata` normalise l'exigence par défaut en `^X.Y.Z` : on
    #    accepte `^`/`=`/rien devant, mais ce qui suit doit être un SemVer
    #    complet ÉGAL à la version des macros - aucune plage (`^0.4`, `*`,
    #    virgules) n'est tolérée.
    $requirement = $Facts.MacrosReq.Trim()
    $pinned = $requirement -replace '^[=~^]', ''
    if (-not (Test-SemVer $pinned)) {
        Stop-Release "l'exigence litemcp-macros '$requirement' n'est pas une version exacte (SemVer complet) - alignez-la sur la version des macros"
    }
    if ($pinned -ne $Facts.MacrosVersion) {
        Stop-Release "litemcp-macros $($Facts.MacrosVersion) ≠ exigence racine '$requirement' - alignez les deux avant de publier"
    }

    # 4) CHANGELOG : la section de la version publiée doit exister (le corps de
    #    la release GitHub en est extrait ; une section absente = publication
    #    refusée, plus de corps de repli silencieux sur le chemin nominal).
    if ([string]::IsNullOrEmpty($ChangelogText)) {
        Stop-Release "CHANGELOG introuvable ou vide ($ChangelogPath)"
    }
    $section = '(?m)^## \[' + [regex]::Escape($PublishVersion) + '\]'
    if ($ChangelogText -notmatch $section) {
        Stop-Release "section '## [$PublishVersion]' absente de $ChangelogPath - coupez la section [Unreleased] avant de publier"
    }
}

function Get-ExpectedTagVersion {
    # Sur un push de tag, `GITHUB_REF` vaut `refs/tags/vX.Y.Z`. Le tag est lu
    # ici plutôt que passé en argument : un `-TagVersion` vide (argument vide
    # non portable selon l'hôte PowerShell, oubli dans un appel) ferait
    # disparaître en silence la comparaison tag <-> manifeste. Un tag qui n'est
    # pas `v...` est refusé, jamais ignoré.
    if (-not [string]::IsNullOrEmpty($env:GITHUB_REF) -and $env:GITHUB_REF -like 'refs/tags/*') {
        $tag = $env:GITHUB_REF.Substring($GITHUB_REF_PREFIX.Length)
        if ($tag -cnotmatch '^v(.+)$') {
            Stop-Release "tag '$tag' non conforme (attendu vX.Y.Z) - abandon avant toute publication"
        }
        return $Matches[1]
    }
    if ($env:GITHUB_ACTIONS -eq 'true' -and [string]::IsNullOrEmpty($env:GITHUB_REF)) {
        Stop-Release 'GITHUB_REF absent sous GitHub Actions - tag invérifiable, rien n''est publié'
    }
    return ''
}

function Invoke-CoherenceCheck {
    $facts = Get-VersionFacts
    $publish = $facts.RootVersion
    if (-not (Test-Path -Path $ChangelogPath -PathType Leaf)) {
        Stop-Release "CHANGELOG introuvable : $ChangelogPath"
    }
    $changelog = Get-Content -Path $ChangelogPath -Raw
    # Tag explicite prioritaire (usage local) ; sinon dérivé de la référence CI.
    $expectedTag = $TagVersion
    if ([string]::IsNullOrEmpty($expectedTag)) {
        $expectedTag = Get-ExpectedTagVersion
    }
    Assert-VersionCoherence -Facts $facts -PublishVersion $publish `
        -TagVersion $expectedTag -InputVersion $InputVersion -ChangelogText $changelog
    Write-Host ''
    Write-Host "[check] cohérence vérifiée : racine=$publish, litemcp-macros=$($facts.MacrosVersion) (exigence '$($facts.MacrosReq)'), CHANGELOG '## [$publish]' présent"
    Write-Host "version=$publish"
}

# ---------------------------------------------------------------------------
# crates.io - statut à trois états : une erreur réseau n'est PAS une absence.
# ---------------------------------------------------------------------------

function Get-CurlCommand {
    $onWindows = Get-Command -Name 'curl.exe' -ErrorAction SilentlyContinue
    if ($onWindows) {
        return 'curl.exe'
    }
    return 'curl'
}

function Get-CratesIoVersionStatus {
    param(
        [Parameter(Mandatory)][string]$Crate,
        [Parameter(Mandatory)][string]$CrateVersion
    )
    $curl = Get-CurlCommand
    $body = & $curl -s --max-time 30 -A $RegistryUserAgent -w "`n%{http_code}" `
        "$CratesIoApi/$Crate/$CrateVersion"
    if ($LASTEXITCODE -ne 0 -or -not $body) {
        return 'inconclusive'
    }
    $code = (($body -split "`r?`n") | Where-Object { $_ -ne '' } | Select-Object -Last 1).Trim()
    switch ($code) {
        '200' { return 'published' }
        '404' { return 'absent' }
        default { return 'inconclusive' }
    }
}

function Test-CargoPublishOutput {
    param([string[]]$OutputLines)
    # $true = crates.io a refusé la republication parce que la version est
    # DÉJÀ indexée (« already uploaded » / « already exists »). Toute autre
    # sortie (réseau, authentification, validation) est une erreur réelle.
    $text = $OutputLines -join "`n"
    return ($text -match 'already uploaded|already exists')
}

function Wait-CratesIoIndexed {
    param(
        [Parameter(Mandatory)][string]$Crate,
        [Parameter(Mandatory)][string]$CrateVersion,
        [int]$MaxAttempts = 18
    )
    $lastStatus = 'inconclusive'
    for ($attempt = 1; $attempt -le $MaxAttempts; $attempt++) {
        $lastStatus = Get-CratesIoVersionStatus -Crate $Crate -CrateVersion $CrateVersion
        if ($lastStatus -eq 'published') {
            Write-Host "[publish] $Crate $CrateVersion est en ligne sur crates.io"
            return
        }
        $detail = if ($lastStatus -eq 'absent') { 'pas encore indexée' } else { 'statut invérifiable (réseau/HTTP)' }
        Write-Host "[publish] $Crate $CrateVersion : $detail - tentative $attempt/$MaxAttempts..."
        if ($attempt -lt $MaxAttempts) {
            Start-Sleep -Seconds 10
        }
    }
    if ($lastStatus -eq 'inconclusive') {
        Stop-Release "crates.io injoignable (erreur réseau/HTTP) après $MaxAttempts tentatives - indexation de $Crate $CrateVersion INVÉRIFIABLE ; l'erreur réseau est remontée, jamais interprétée comme une absence"
    }
    Stop-Release "$Crate $CrateVersion non indexée après $MaxAttempts tentatives"
}

function Publish-Crate {
    param(
        [Parameter(Mandatory)][string]$Crate,
        [Parameter(Mandatory)][string]$CrateVersion,
        [switch]$DryRun
    )

    $status = Get-CratesIoVersionStatus -Crate $Crate -CrateVersion $CrateVersion
    switch ($status) {
        'published' {
            Write-Host "[publish] $Crate $CrateVersion déjà en ligne sur crates.io - publication ignorée"
            return
        }
        'absent' {
            Write-Host "[publish] $Crate $CrateVersion absente de crates.io - publication"
        }
        default {
            Stop-Release "statut de $Crate $CrateVersion invérifiable sur crates.io (réseau/HTTP ≠ 200/404) - abandon SANS publication : une erreur réseau n'est pas une preuve d'absence"
        }
    }

    $cargoArguments = @('publish', '-p', $Crate)
    if ($DryRun) {
        # Simulation locale uniquement : pas d'envoi, pas de vérification
        # interne re-dupliquée (le gate verify l'a déjà faite) et tolérance
        # d'un arbre de travail en cours - la publication réelle de CI part
        # d'un checkout propre et ne passe jamais par ces drapeaux.
        $cargoArguments += @('--dry-run', '--no-verify', '--allow-dirty')
    }
    # Splatting interdit dans un pipeline : l'appel puis la conversion sont
    # séparés (les lignes stderr d'un échec deviennent des chaînes).
    # PS 5.1 : `2>&1` sous ErrorActionPreference='Stop' transforme le stderr
    # natif (« Updating crates.io index »...) en erreur terminante - on borne
    # la préférence à la capture uniquement (pwsh 7 en CI n'a pas ce
    # comportement, mais le script doit tourner dans les deux).
    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $rawOutput = & cargo @cargoArguments 2>&1
    } finally {
        $ErrorActionPreference = $previousPreference
    }
    $exit = $LASTEXITCODE
    $output = $rawOutput | ForEach-Object { "$_" }
    $output | ForEach-Object { Write-Host $_ }
    if ($exit -ne 0) {
        if (Test-CargoPublishOutput -OutputLines $output) {
            Write-Host "[publish] $Crate $CrateVersion déjà indexée (crates.io a refusé la republication : « already uploaded ») - traité comme publié"
            return
        }
        Stop-Release "cargo publish -p $Crate en échec (exit $exit) - aucune interprétation complaisante : l'échec est remonté tel quel"
    }

    if ($DryRun) {
        Write-Host "[publish] [simulation --dry-run] aucun envoi ; attente d'indexation ignorée"
        return
    }
    Wait-CratesIoIndexed -Crate $Crate -CrateVersion $CrateVersion
}

function Invoke-PublishFlow {
    param(
        [Parameter(Mandatory)][string]$PublishVersion,
        [switch]$DryRun
    )
    if (-not (Test-SemVer $PublishVersion)) {
        Stop-Release "version '$PublishVersion' non SemVer - abandon"
    }
    $facts = Get-VersionFacts
    if ($facts.RootVersion -ne $PublishVersion) {
        Stop-Release "version à publier '$PublishVersion' ≠ version du manifeste racine '$($facts.RootVersion)' - relancez le gate de cohérence"
    }
    # Ordre immuable : la macros d'abord (litemcp en dépend avec une exigence
    # de version), puis la racine.
    Publish-Crate -Crate 'litemcp-macros' -CrateVersion $facts.MacrosVersion -DryRun:$DryRun
    Publish-Crate -Crate 'litemcp' -CrateVersion $PublishVersion -DryRun:$DryRun
    Write-Host ''
    Write-Host "[publish] litemcp $PublishVersion : publiée (ou déjà en ligne) - litemcp-macros $($facts.MacrosVersion) : publiée (ou déjà en ligne)"
}

# ---------------------------------------------------------------------------
# Dispatch - uniquement à l'exécution directe. Un dot-source charge les
# fonctions sans rien exécuter (simulations locales : cohérence à faits
# fabriqués, classification des sorties `cargo publish`, propagation des
# échecs) - aucun crochet de simulation dans le chemin de production.
# ---------------------------------------------------------------------------

if ($MyInvocation.InvocationName -ne '.') {
    if ([string]::IsNullOrEmpty($Command)) {
        Write-Host 'usage : release.ps1 <semver|check|verify|publish> [-Version <x.y.z>] [-TagVersion <v>] [-InputVersion <v>] [-DryRun]'
        exit 2
    }
    switch ($Command) {
        'semver' {
            if ([string]::IsNullOrEmpty($Version)) {
                Write-Host 'usage : release.ps1 semver -Version <x.y.z>'
                exit 2
            }
            if (Test-SemVer $Version) {
                Write-Host "[semver] OK : $Version"
                exit 0
            }
            Write-Host "[semver] refusé : '$Version' (regex semver.org complète - 0.10.10 et préreleases acceptés, zéros initiaux et injection refusés)"
            exit 1
        }
        'check' {
            Invoke-CoherenceCheck
            exit 0
        }
        'verify' {
            Invoke-VerifyGates
            exit 0
        }
        'publish' {
            if ([string]::IsNullOrEmpty($Version)) {
                Write-Host 'usage : release.ps1 publish -Version <x.y.z> [-DryRun]'
                exit 2
            }
            Invoke-PublishFlow -PublishVersion $Version -DryRun:$DryRun
            exit 0
        }
        default {
            Write-Host "commande inconnue : '$Command' - attendues : semver | check | verify | publish"
            exit 2
        }
    }
}
