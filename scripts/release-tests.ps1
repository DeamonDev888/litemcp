#Requires -Version 5.1
<#
release-tests.ps1 - simulations du pipeline de publication LiteMCP.

Aucun secret, aucun réseau, aucune publication réelle : les appels à crates.io et
`cargo publish` sont remplacés par des fonctions de test, et chaque scénario
tourne dans un processus PowerShell séparé (un `exit` de contrôle y devient un
code de sortie observable au lieu d'arrêter le harnais).

Cas couverts - ceux exigés par l'audit du pipeline :
  - version existante    -> publication ignorée, `cargo publish` jamais appelé ;
  - réseau indisponible  -> arrêt SANS publication (une erreur réseau n'est
                           jamais une preuve d'absence) ;
  - « already uploaded » -> traité comme déjà publié ; toute autre erreur de
                           `cargo publish` (réseau, auth, validation) est
                           remontée telle quelle ;
  - tag incohérent       -> refus avant tout `cargo publish` ;
  - tag mal formé        -> refus (un tag présent n'est jamais ignoré en
                           silence) ;
  - tests rouges         -> le gate s'arrête à la première étape en échec ;
  - contenu des paquets  -> licences MIT/Apache exigées dans les deux paquets ;
  - SemVer               -> toutes les combinaisons numériques (`0.10.10`...).

Le lien entre ce comportement et le pipeline GitHub (`publish` dépend de
`verify`, revalide les versions, `publish.yml` sans déclencheur tag) est figé
par `tests/release_pipeline.rs`, exécuté par `cargo test`.

Sortie : `[ok]` / `[ECHEC]` par cas, bilan, code 1 si un cas échoue.

Encodage : UTF-8 **avec BOM** (Windows PowerShell 5.1 lit sinon les accents
en ANSI) ; les chaînes évitent les guillemets typographiques pour rester
analysables même sans BOM.
#>
param()

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$releaseScript = Join-Path $PSScriptRoot 'release.ps1'
$script:total = 0
$script:failures = @()

$hostExe = $null
try {
    $hostExe = [System.Diagnostics.Process]::GetCurrentProcess().MainModule.FileName
} catch {
    $hostExe = $null
}
if ([string]::IsNullOrEmpty($hostExe)) {
    $hostExe = (Get-Process -Id $PID).Path
}

function Format-Argument {
    param([string]$Value)
    if ([string]::IsNullOrEmpty($Value)) {
        return '""'
    }
    if ($Value -match '[\s"]') {
        return '"' + ($Value -replace '"', '\"') + '"'
    }
    return $Value
}

# Environnement contrôlé : les variables GitHub du parent sont retirées pour
# qu'un harnais lancé depuis un run de release ne biaise pas les scénarios.
function New-ChildStartInfo {
    param(
        [Parameter(Mandatory)][string]$ScriptPath,
        [string[]]$ArgumentList = @(),
        [hashtable]$Environment = @{},
        [string]$WorkingDirectory = $root
    )
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $hostExe
    $all = @('-NoProfile', '-File', $ScriptPath) + $ArgumentList
    $psi.Arguments = (($all | ForEach-Object { Format-Argument $_ }) -join ' ')
    $psi.WorkingDirectory = $WorkingDirectory
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    foreach ($key in @($psi.EnvironmentVariables.Keys)) {
        if ($key -like 'GITHUB_*' -or $key -eq 'CARGO_REGISTRY_TOKEN' -or $key -eq 'CARGO_REGISTRIES_CRATES_IO_TOKEN') {
            $psi.EnvironmentVariables.Remove($key) | Out-Null
        }
    }
    foreach ($key in $Environment.Keys) {
        $psi.EnvironmentVariables[$key] = [string]$Environment[$key]
    }
    return $psi
}

function Invoke-ChildProcess {
    param(
        [Parameter(Mandatory)][string]$ScriptPath,
        [string[]]$ArgumentList = @(),
        [hashtable]$Environment = @{},
        [string]$WorkingDirectory = $root
    )
    $psi = New-ChildStartInfo -ScriptPath $ScriptPath -ArgumentList $ArgumentList `
        -Environment $Environment -WorkingDirectory $WorkingDirectory
    $process = [System.Diagnostics.Process]::Start($psi)
    $stdout = $process.StandardOutput.ReadToEnd()
    $stderr = $process.StandardError.ReadToEnd()
    $process.WaitForExit()
    return [pscustomobject]@{
        ExitCode = $process.ExitCode
        Stdout   = $stdout
        Output   = $stdout + $stderr
    }
}

# Appel réel de la ligne de commande (mêmes arguments que les workflows).
function Invoke-ReleaseCli {
    param(
        [string[]]$ArgumentList = @(),
        [hashtable]$Environment = @{}
    )
    return Invoke-ChildProcess -ScriptPath $releaseScript -ArgumentList $ArgumentList -Environment $Environment
}

$script:scenarioCounter = 0

# Scénario = script jetable qui dot-source release.ps1 et remplace une frontière
# (crates.io, cargo) par une fonction de test.
function Invoke-Scenario {
    param(
        [Parameter(Mandatory)][string]$Body,
        [hashtable]$Environment = @{}
    )
    $script:scenarioCounter++
    $path = Join-Path ([System.IO.Path]::GetTempPath()) ("litemcp-scenario-{0}-{1}.ps1" -f $PID, $script:scenarioCounter)
    try {
        # BOM UTF-8 : PowerShell 5.1 lit sans BOM en ANSI et abîmerait les accents.
        [System.IO.File]::WriteAllText($path, $Body, (New-Object System.Text.UTF8Encoding($true)))
        return Invoke-ChildProcess -ScriptPath $path -Environment $Environment
    } finally {
        Remove-Item -Path $path -Force -ErrorAction SilentlyContinue
    }
}

function Assert-Case {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][bool]$Condition,
        [string]$Detail = ''
    )
    $script:total++
    if ($Condition) {
        Write-Host "[ok]     $Name"
    } else {
        Write-Host "[ECHEC]  $Name"
        if (-not [string]::IsNullOrEmpty($Detail)) {
            foreach ($line in ($Detail -split "`r?`n")) {
                if (-not [string]::IsNullOrWhiteSpace($line)) {
                    Write-Host "         | $line"
                }
            }
        }
        $script:failures += $Name
    }
}

function Get-RootVersion {
    $raw = & cargo metadata --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) {
        throw 'cargo metadata en échec - harnais inutilisable'
    }
    $document = ($raw -join "`n") | ConvertFrom-Json
    $package = $document.packages | Where-Object { $_.name -eq 'litemcp' } | Select-Object -First 1
    if (-not $package) {
        throw 'paquet litemcp absent de cargo metadata'
    }
    return [string]$package.version
}

$rootVersion = Get-RootVersion

$changelogOk = Join-Path ([System.IO.Path]::GetTempPath()) ("litemcp-changelog-ok-{0}.md" -f $PID)
$changelogSansSection = Join-Path ([System.IO.Path]::GetTempPath()) ("litemcp-changelog-vide-{0}.md" -f $PID)

try {
    Write-Host ''
    Write-Host "== Simulations du pipeline de publication (racine $rootVersion) =="

    # -------------------------------------------------------------------
    # 1. SemVer : toutes les combinaisons numériques, injections refusées.
    # -------------------------------------------------------------------
    $matrice = @(
        @{ Valeur = '0.10.10'; Attendu = $true },
        @{ Valeur = '0.10.11'; Attendu = $true },
        @{ Valeur = '10.10.10'; Attendu = $true },
        @{ Valeur = '0.0.0'; Attendu = $true },
        @{ Valeur = '1.0.0-rc.1'; Attendu = $true },
        @{ Valeur = '1.0.0-alpha.1+build.7'; Attendu = $true },
        @{ Valeur = '0.10.10+20261110'; Attendu = $true },
        @{ Valeur = 'v0.10.10'; Attendu = $false },
        @{ Valeur = '0.10'; Attendu = $false },
        @{ Valeur = '0.10.10.'; Attendu = $false },
        @{ Valeur = '00.10.10'; Attendu = $false },
        @{ Valeur = '0.010.10'; Attendu = $false },
        @{ Valeur = '0.10.010'; Attendu = $false },
        @{ Valeur = '0.10.10-'; Attendu = $false },
        @{ Valeur = '0.10.10+'; Attendu = $false },
        @{ Valeur = '0.10.10-rc..1'; Attendu = $false },
        @{ Valeur = '0.10.10; rm -rf /'; Attendu = $false },
        @{ Valeur = '../0.10.10'; Attendu = $false }
    )
    $body = @'
. ./scripts/release.ps1
$matrice = ConvertFrom-Json $env:SIM_MATRICE
$echecs = @()
foreach ($cas in $matrice) {
    $obtenu = Test-SemVer $cas.Valeur
    if ($obtenu -ne [bool]$cas.Attendu) {
        $echecs += ("{0} : attendu {1}, obtenu {2}" -f $cas.Valeur, $cas.Attendu, $obtenu)
    }
}
if ($echecs.Count -gt 0) {
    $echecs | ForEach-Object { Write-Output "  $_" }
    exit 1
}
Write-Output "matrice SemVer : $($matrice.Count) cas conformes"
exit 0
'@
    $sim = $matrice | ForEach-Object { [pscustomobject]@{ Valeur = $_.Valeur; Attendu = [bool]$_.Attendu } }
    $semver = Invoke-Scenario -Body $body -Environment @{ SIM_MATRICE = ($sim | ConvertTo-Json -Compress) }
    Assert-Case -Name 'SemVer : 0.10.10 et combinaisons multi-chiffres acceptés, injections refusées' `
        -Condition ($semver.ExitCode -eq 0) -Detail $semver.Output

    # -------------------------------------------------------------------
    # 2. Cohérence des versions : tag, macros, CHANGELOG (faits fabriqués).
    # -------------------------------------------------------------------
    $coherenceNominale = @'
. ./scripts/release.ps1
$faits = [pscustomobject]@{
    RootVersion   = '0.10.10'
    MacrosVersion = '0.10.10'
    MacrosReq     = '^0.10.10'
}
Assert-VersionCoherence -Facts $faits -PublishVersion '0.10.10' -TagVersion '0.10.10' `
    -InputVersion '0.10.10' -ChangelogText "# Changelog`n`n## [0.10.10] - 2026-11-10`n`n- correctif`n"
Write-Output 'coherence acceptee'
exit 0
'@
    $cas = Invoke-Scenario -Body $coherenceNominale
    Assert-Case -Name '0.10.10 accepté de bout en bout (racine = tag = input = macros = CHANGELOG)' `
        -Condition ($cas.ExitCode -eq 0 -and $cas.Output.Contains('coherence acceptee')) -Detail $cas.Output

    $macrosDesalignees = @'
. ./scripts/release.ps1
$faits = [pscustomobject]@{
    RootVersion   = '0.10.10'
    MacrosVersion = '0.4.0'
    MacrosReq     = '^0.3.0'
}
Assert-VersionCoherence -Facts $faits -PublishVersion '0.10.10' -ChangelogText "## [0.10.10]"
"jamais atteint"
exit 0
'@
    $cas = Invoke-Scenario -Body $macrosDesalignees
    Assert-Case -Name 'macros désalignées de l''exigence racine : refus avant publication' `
        -Condition ($cas.ExitCode -eq 1 -and $cas.Output.Contains("litemcp-macros 0.4.0")) -Detail $cas.Output

    $exigenceNonExacte = @'
. ./scripts/release.ps1
$faits = [pscustomobject]@{
    RootVersion   = '0.10.10'
    MacrosVersion = '0.10.10'
    MacrosReq     = '^0.10'
}
Assert-VersionCoherence -Facts $faits -PublishVersion '0.10.10' -ChangelogText "## [0.10.10]"
"jamais atteint"
exit 0
'@
    $cas = Invoke-Scenario -Body $exigenceNonExacte
    Assert-Case -Name 'exigence de version non exacte (`^0.10`) : refus' `
        -Condition ($cas.ExitCode -eq 1 -and $cas.Output.Contains('version exacte')) -Detail $cas.Output

    # -------------------------------------------------------------------
    # 3. Tag : dérivé de GITHUB_REF, incohérent ou mal formé => refus.
    # -------------------------------------------------------------------
    $changelogOkBody = "# Changelog`n`n## [Unreleased]`n`n## [$rootVersion] - 2026-11-10`n`n- correctif`n"
    [System.IO.File]::WriteAllText($changelogOk, $changelogOkBody, (New-Object System.Text.UTF8Encoding($false)))
    $changelogSansSectionBody = "# Changelog`n`n## [Unreleased]`n`n- rien`n"
    [System.IO.File]::WriteAllText($changelogSansSection, $changelogSansSectionBody, (New-Object System.Text.UTF8Encoding($false)))

    $cas = Invoke-ReleaseCli -ArgumentList @('check', '-ChangelogPath', $changelogOk) `
        -Environment @{ GITHUB_ACTIONS = 'true'; GITHUB_REF = "refs/tags/v$rootVersion" }
    Assert-Case -Name 'tag = manifeste : cohérence acceptée, version publiée en sortie' `
        -Condition ($cas.ExitCode -eq 0 -and $cas.Output.Contains("version=$rootVersion")) -Detail $cas.Output

    $cas = Invoke-ReleaseCli -ArgumentList @('check', '-ChangelogPath', $changelogOk) `
        -Environment @{ GITHUB_ACTIONS = 'true'; GITHUB_REF = 'refs/tags/v0.0.1' }
    Assert-Case -Name 'tag incohérent : refus avant tout test et toute publication' `
        -Condition ($cas.ExitCode -eq 1 -and $cas.Output.Contains('version du manifeste racine')) -Detail $cas.Output

    $cas = Invoke-ReleaseCli -ArgumentList @('check', '-ChangelogPath', $changelogOk) `
        -Environment @{ GITHUB_ACTIONS = 'true'; GITHUB_REF = 'refs/tags/v' }
    Assert-Case -Name 'tag `v` sans numéro : refus (jamais ignoré en silence)' `
        -Condition ($cas.ExitCode -eq 1 -and $cas.Output.Contains('non conforme')) -Detail $cas.Output

    $cas = Invoke-ReleaseCli -ArgumentList @('check', '-ChangelogPath', $changelogOk) `
        -Environment @{ GITHUB_ACTIONS = 'true'; GITHUB_REF = "refs/tags/$rootVersion" }
    Assert-Case -Name 'tag sans préfixe `v` : refus' `
        -Condition ($cas.ExitCode -eq 1 -and $cas.Output.Contains('non conforme')) -Detail $cas.Output

    $cas = Invoke-ReleaseCli -ArgumentList @('check', '-InputVersion', '9.9.9', '-ChangelogPath', $changelogOk) `
        -Environment @{ GITHUB_ACTIONS = 'true'; GITHUB_REF = 'refs/heads/main' }
    Assert-Case -Name 'version saisie incohérente (break-glass) : refus' `
        -Condition ($cas.ExitCode -eq 1 -and $cas.Output.Contains("version du manifeste racine")) -Detail $cas.Output

    $cas = Invoke-ReleaseCli -ArgumentList @('check', '-ChangelogPath', $changelogOk) `
        -Environment @{ GITHUB_ACTIONS = 'true' }
    Assert-Case -Name 'référence CI illisible : refus (tag invérifiable)' `
        -Condition ($cas.ExitCode -eq 1 -and $cas.Output.Contains('GITHUB_REF absent')) -Detail $cas.Output

    $cas = Invoke-ReleaseCli -ArgumentList @('check', '-ChangelogPath', $changelogSansSection) `
        -Environment @{ GITHUB_ACTIONS = 'true'; GITHUB_REF = "refs/tags/v$rootVersion" }
    Assert-Case -Name 'section CHANGELOG absente : refus' `
        -Condition ($cas.ExitCode -eq 1 -and $cas.Output.Contains('absente')) -Detail $cas.Output

    # -------------------------------------------------------------------
    # 4. Publication : déjà publiée, réseau injoignable, réponses de cargo.
    # -------------------------------------------------------------------
    $dejaPubliee = @'
. ./scripts/release.ps1
function Get-CratesIoVersionStatus { param($Crate, $CrateVersion) return 'published' }
function cargo { throw 'cargo ne doit pas être appelé : version déjà publiée' }
Publish-Crate -Crate 'litemcp' -CrateVersion '0.10.10'
Write-Output 'publication ignoree'
exit 0
'@
    $cas = Invoke-Scenario -Body $dejaPubliee
    Assert-Case -Name 'version déjà sur crates.io : publication ignorée, cargo publish jamais appelé' `
        -Condition ($cas.ExitCode -eq 0 -and $cas.Output.Contains('publication ignoree')) -Detail $cas.Output

    $reseauKo = @'
. ./scripts/release.ps1
function Get-CratesIoVersionStatus { param($Crate, $CrateVersion) return 'inconclusive' }
function cargo { throw 'cargo ne doit pas être appelé : statut crates.io inconnu' }
Publish-Crate -Crate 'litemcp' -CrateVersion '0.10.10'
Write-Output 'ne doit jamais arriver'
exit 0
'@
    $cas = Invoke-Scenario -Body $reseauKo
    Assert-Case -Name 'réseau indisponible : arrêt sans publication, erreur jamais lue comme une absence' `
        -Condition ($cas.ExitCode -eq 1 -and $cas.Output.Contains("n'est pas une preuve d'absence") -and -not $cas.Output.Contains('ne doit jamais arriver')) `
        -Detail $cas.Output

    $dejaTeleversee = @'
. ./scripts/release.ps1
function Get-CratesIoVersionStatus { param($Crate, $CrateVersion) return 'absent' }
function cargo {
    $CargoArgs = @($args)
    Write-Output 'error: crate litemcp@0.10.10 already uploaded'
    $global:LASTEXITCODE = 101
}
Publish-Crate -Crate 'litemcp' -CrateVersion '0.10.10'
Write-Output 'traitee comme publiee'
exit 0
'@
    $cas = Invoke-Scenario -Body $dejaTeleversee
    Assert-Case -Name '« already uploaded » : traité comme déjà publié (idempotence)' `
        -Condition ($cas.ExitCode -eq 0 -and $cas.Output.Contains('traitee comme publiee')) -Detail $cas.Output

    $erreurReelle = @'
. ./scripts/release.ps1
function Get-CratesIoVersionStatus { param($Crate, $CrateVersion) return 'absent' }
function cargo {
    $CargoArgs = @($args)
    Write-Output 'error: failed to get `litemcp` as a dependency of package (network failure)'
    $global:LASTEXITCODE = 101
}
Publish-Crate -Crate 'litemcp' -CrateVersion '0.10.10'
Write-Output 'ne doit jamais arriver'
exit 0
'@
    $cas = Invoke-Scenario -Body $erreurReelle
    Assert-Case -Name 'erreur cargo publish réelle (réseau) : remontée telle quelle, pas de complaisance' `
        -Condition ($cas.ExitCode -eq 1 -and $cas.Output.Contains('complaisante')) -Detail $cas.Output

    $publicationNominale = @'
. ./scripts/release.ps1
$global:appels = 0
function Get-CratesIoVersionStatus {
    param($Crate, $CrateVersion)
    $global:appels++
    if ($global:appels -eq 1) { return 'absent' }
    return 'published'
}
function cargo {
    $CargoArgs = @($args)
    $global:LASTEXITCODE = 0
}
Publish-Crate -Crate 'litemcp-macros' -CrateVersion '0.4.0'
Write-Output 'macros publiee'
exit 0
'@
    $cas = Invoke-Scenario -Body $publicationNominale
    Assert-Case -Name 'version absente + publication réussie : indexation attendue puis confirmée' `
        -Condition ($cas.ExitCode -eq 0 -and $cas.Output.Contains('macros publiee') -and $cas.Output.Contains('en ligne')) -Detail $cas.Output

    $versionIncoherente = @'
. ./scripts/release.ps1
function Get-CratesIoVersionStatus { param($Crate, $CrateVersion) return 'published' }
Invoke-PublishFlow -PublishVersion '0.0.1'
Write-Output 'ne doit jamais arriver'
exit 0
'@
    $cas = Invoke-Scenario -Body $versionIncoherente
    Assert-Case -Name 'publication d''une version ≠ manifeste racine : refus local' `
        -Condition ($cas.ExitCode -eq 1 -and $cas.Output.Contains('version du manifeste racine')) -Detail $cas.Output

    # -------------------------------------------------------------------
    # 5. Gate : une étape rouge arrête tout (tests rouges).
    # -------------------------------------------------------------------
    $testRouge = @'
. ./scripts/release.ps1
Invoke-ReleaseStep -Name 'tests rouges simules' -CommandLine '$global:LASTEXITCODE = 1'
Write-Output 'ne doit jamais arriver'
exit 0
'@
    $cas = Invoke-Scenario -Body $testRouge
    Assert-Case -Name 'tests rouges : gate rompu, rien n''est publié' `
        -Condition ($cas.ExitCode -eq 1 -and $cas.Output.Contains('gate rompu') -and -not $cas.Output.Contains('ne doit jamais arriver')) `
        -Detail $cas.Output

    # -------------------------------------------------------------------
    # 6. Contenu des paquets : licences MIT/Apache exigées.
    # -------------------------------------------------------------------
    $paquetsSansLicence = @'
. ./scripts/release.ps1
function cargo {
    $CargoArgs = @($args)
    $global:LASTEXITCODE = 0
    if ($CargoArgs -contains 'litemcp-macros') {
        return @('Cargo.toml', 'src/lib.rs', 'LICENSE-APACHE')
    }
    return @('Cargo.toml', 'README.md', 'LICENSE-APACHE', 'LICENSE-MIT')
}
Assert-PackageContents
Write-Output 'ne doit jamais arriver'
exit 0
'@
    $cas = Invoke-Scenario -Body $paquetsSansLicence
    Assert-Case -Name 'paquet macros sans licence MIT : gate rompu' `
        -Condition ($cas.ExitCode -eq 1 -and $cas.Output.Contains('LICENSE-MIT absente')) -Detail $cas.Output

    $paquetsComplets = @'
. ./scripts/release.ps1
function cargo {
    $CargoArgs = @($args)
    $global:LASTEXITCODE = 0
    if ($CargoArgs -contains 'litemcp-macros') {
        return @('Cargo.toml', 'src/lib.rs', 'LICENSE-APACHE', 'LICENSE-MIT')
    }
    return @('Cargo.toml', 'README.md', 'LICENSE-APACHE', 'LICENSE-MIT')
}
Assert-PackageContents
Write-Output 'paquets conformes'
exit 0
'@
    $cas = Invoke-Scenario -Body $paquetsComplets
    Assert-Case -Name 'les deux paquets portent MIT/Apache (README en plus pour litemcp) : gate vert' `
        -Condition ($cas.ExitCode -eq 0 -and $cas.Output.Contains('paquets conformes')) -Detail $cas.Output
} finally {
    Remove-Item -Path $changelogOk -Force -ErrorAction SilentlyContinue
    Remove-Item -Path $changelogSansSection -Force -ErrorAction SilentlyContinue
}

Write-Host ''
if ($script:failures.Count -gt 0) {
    Write-Host ("[release-tests] {0} cas en echec sur {1} :" -f $script:failures.Count, $script:total)
    $script:failures | ForEach-Object { Write-Host "  - $_" }
    exit 1
}
Write-Host ("[release-tests] {0} cas conformes - aucune publication reelle, aucun secret utilise" -f $script:total)
exit 0
