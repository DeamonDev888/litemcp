<div align="center">

# LiteMCP

**Transformez votre code Rust en serveur MCP en quelques lignes, sans runtime async.**

Déclarez vos outils, ressources et prompts comme des fonctions ordinaires : LiteMCP les sert en Streamable HTTP (`POST /mcp`, réponse JSON ou SSE). Le cœur ne dépend que de `serde`, `serde_json` et `getrandom`.

[![crates.io](https://img.shields.io/crates/v/litemcp.svg)](https://crates.io/crates/litemcp) [![CI](https://img.shields.io/github/actions/workflow/status/DeamonDev888/litemcp/ci.yml?label=CI)](https://github.com/DeamonDev888/litemcp/actions/workflows/ci.yml) [![Docs](https://img.shields.io/docsrs/litemcp)](https://docs.rs/litemcp) ![MSRV](https://img.shields.io/badge/MSRV-1.85-orange) ![Spec](https://img.shields.io/badge/MCP-2026--07--28-blue)

[Premier serveur](#premier-serveur) · [Client](#brancher-un-client) · [Concepts](#concepts) · [Features](#features-cargo) · [Configuration](#configuration-et-valeurs-par-défaut) · [Authentification](#authentification-012) · [Sécurité](#sécurité) · [Conformité](#conformité-mcp) · [Performances](#performances) · [Limites](#limites) · [FAQ](https://github.com/DeamonDev888/litemcp/blob/main/docs/FAQ.md) · [Tutoriel](https://deamondev888.github.io/litemcp-tuto/)

</div>

---

## Ce que c'est

Une **bibliothèque**, pas un serveur à lancer : vous ajoutez la dépendance à un projet existant, vous enregistrez vos fonctions comme outils, ressources ou prompts, `.serve()` bloque — et **votre binaire** parle MCP sur un socket TCP. Rien à installer, aucun processus séparé.

1. **Streamable HTTP uniquement** : un `POST /mcp` auto-suffisant par message, aucun flux permanent, aucune session, pas d'`initialize` obligatoire — la spec `2026-07-28`.
2. **Zéro runtime async** : `std::net::TcpListener`, un thread par connexion ; aucun adaptateur tokio.
3. **Trois dépendances directes** : `serde`, `serde_json`, `getrandom` (CSPRNG du système d'exploitation pour toutes les clés et tous les nonces) ; schémas et validation faits maison (module `zod`), macros `#[tool]` dans une crate locale sans dépendance.
4. **Mesuré et daté** : chaque chiffre est publié avec sa date, sa méthode et ses limites.

Pour qui : une CLI qui expose deux outils sans framework web ; un test qui a besoin d'un vrai serveur sur un port ; un binaire embarqué (edge, IoT) ; un agent qui lance cinquante serveurs MCP.

## Installation

MSRV **1.85** (edition 2024) : `rustup toolchain install stable` suffit sur une machine à jour.

```toml
[dependencies]
litemcp = "0.12"   # macros #[tool]/#[resource]/#[prompt] incluses

# variantes : { version = "0.12", default-features = false }  → sans les macros
#             { version = "0.12", features = ["schemars"] }   → adaptateur schemars
#             { version = "0.12", features = ["auth-sign"] } → signature Ed25519 (RFC 9421)
#             { version = "0.12", features = ["tls"] }       → TLS 1.3 serveur
```

**Le jeu par défaut est complet** : transport HTTP durci, JSON-RPC, module `zod`, erreurs structurées, hooks `on_call`/`on_request`, gateway dynamique, anti-zombie, annonces. Les features sont additives ; la seule qui coûte cher est `schemars` ([chiffres](#features-cargo)).

## Premier serveur

```rust
use litemcp::prelude::*;

fn main() -> std::io::Result<()> {
    let bound = ServerBuilder::new("calc", "1.0.0")
        .tool("add", "Additionne deux nombres", |args| {
            let a = args["a"].as_i64().ok_or("a manquant")?;
            let b = args["b"].as_i64().ok_or("b manquant")?;
            Ok(serde_json::json!({ "somme": a + b }))
        })
        .bind_default()?;          // premier port libre de 23080–23099, repli éphémère
    println!("MCP sur http://{}/mcp", bound.local_addr()?);
    bound.serve()                  // bloque : POST /mcp → JSON ou SSE
}
```

`bind_default()` prend le **premier port libre** de `23080..=23099` ; `.bind("127.0.0.1:23080")` fixe le port. **La lib n'affiche rien** — le `println!` est du code utilisateur.

Vérification en profil strict (défaut) : trois en-têtes et deux clés `_meta`, sur **chaque** requête.

```bash
curl -s http://127.0.0.1:23080/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json' \
  -H 'MCP-Protocol-Version: 2026-07-28' \
  -H 'Mcp-Method: tools/call' \
  -H 'Mcp-Name: add' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"name":"add","arguments":{"a":5,"b":7}}}'
```

```json
{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","content":[{"type":"text","text":"{\"somme\":12}"}],"structuredContent":{"somme":12},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"calc","version":"1.0.0"}}}}
```

Ce corps est comparé **octet pour octet** par [`tests/readme_parity.rs`](tests/readme_parity.rs), qui rejoue la requête `curl` ci-dessus sur une vraie socket, en profil strict (défaut).

> **Sans l'enveloppe**, le même `POST` rend `400` : `-32020 MCP-Protocol-Version header is required in modern MCP profile`, puis `-32602 missing required _meta object in params`. `.legacy(true)` rétablit l'appel nu (mode de tolérance : `initialize` y négocie la révision demandée, `2025-11-25` par défaut) — [`docs/PROTOCOLE.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/PROTOCOLE.md). Exemple complet : `cargo run --example hello`.

## Brancher un client

L'URL est tout ce qu'un client MCP doit connaître : `http://127.0.0.1:23080/mcp`.

```json
{ "mcpServers": { "calc": { "type": "http", "url": "http://127.0.0.1:23080/mcp" } } }
```

La clé et le champ `type` varient d'un client à l'autre (`servers` dans VS Code, par exemple) : LiteMCP n'impose que l'URL. Un inspecteur générique fait l'affaire : `npx @modelcontextprotocol/inspector`. Sonde de vie : `GET /health` → `200 ok`. Pour agréger plusieurs serveurs derrière une seule URL : [MCP Control](https://github.com/DeamonDev888/litemcp/blob/main/docs/INTEGRATIONS.md) (client, gateway, gestionnaire).

**Dépannage** — les réponses qui arrivent en premier :

| Symptôme | Cause |
|---|---|
| `400` + `-32020` | `MCP-Protocol-Version`, `Mcp-Method` ou `Mcp-Name` absent |
| `400` + `-32602` · `404` + `-32601` | `_meta` incomplet · `initialize` sur le profil strict (`.legacy(true)` pour un client legacy) |
| `403` · `503` · connexion refusée | `Origin` non listé · plafond de connexions (256) · mauvais port (`23080..=23099`) |

## Authentification (0.12)

**Le problème résolu** : par défaut, quiconque joint le socket peut appeler vos outils. La 0.12 ajoute l'identité (qui parle), les droits (ce qu'il peut toucher) et la confidentialité (TLS).

Trois configurations, **du plus ouvert au plus fermé** — et sans vérificateur, le comportement du crate est strictement inchangé (aucune allocation, aucun en-tête retenu).

**1. Serveur ouvert** (ce que fait `cargo run --example hello`) : rien à changer.

**2. Serveur signé** — chaque client a une paire Ed25519, le serveur ne connaît que les clés publiques :

```rust
use litemcp::auth::sign::{ClientKey, KeyStore, NonceCache, SignatureVerifier, TestKeyStore};
use litemcp::prelude::*;
use std::sync::Arc;

# fn exemple(cle_publique: [u8; 32]) -> std::io::Result<()> {
let magasin: Arc<TestKeyStore> = Arc::new(TestKeyStore::new());
magasin.insert(
    "atelier-1",
    ClientKey::new("atelier-1", cle_publique).with_right("db.read"),
);
let keys: Arc<dyn KeyStore> = magasin;

let bound = ServerBuilder::new("calc", "1.0.0")
    .tool("db.query", "Lit la base", |_args| Ok(serde_json::json!({})))
    .require_right("db.query", "db.read")   // sans droit, l'outil est refusé ET invisible
    .verifier(Arc::new(SignatureVerifier::new(
        keys,
        NonceCache::with_default_policy(),
    )))
    .on_auth_decision(|event| eprintln!("{} {:?} → {:?}", event.method, event.name, event.verdict))
    .bind("127.0.0.1:23080")?;
# let _ = bound;
# Ok(())
# }
```

> `TestKeyStore` est un magasin en mémoire pour les exemples et les harnais, **pas une implémentation de référence** : en production, fournissez votre `KeyStore` (persistance et chiffrement au repos sont à vous).

Côté client, `auth::sign::Signer::headers(&RequestToSign)` produit les trois en-têtes du profil (Ed25519 RFC 8032, `Content-Digest` RFC 9530, `Signature-Input`/`Signature` RFC 9421) pour **n'importe quel** client HTTP : ajoutez `Content-Digest`, `Signature-Input` et `Signature` à la requête, tel quel. `cargo run --features auth-sign --example auth_sign` montre le vecteur de référence et cinq refus.

**3. Serveur signé + TLS 1.3** — la confidentialité en plus, certificat en mémoire :

```rust
use litemcp::prelude::*;

# fn exemple() -> std::io::Result<()> {
let tls = TlsConfig::self_signed_ephemeral("localhost")
    .expect("certificat éphémère en mémoire");
let empreinte = tls.fingerprint();       // « sha256:… » — à épingler côté client
let bound = ServerBuilder::new("calc", "1.0.0")
    .tool("db.query", "Lit la base", |_args| Ok(serde_json::json!({})))
    .require_right("db.query", "db.read")
    .tls(tls)
    .bind("127.0.0.1:23080")?;
assert_eq!(bound.certificate_fingerprint(), Some(empreinte));
# let _ = bound;
# Ok(())
# }
```

Un client épingle l'identité du serveur avec `auth::PinnedServer::new("sha256:…")` puis `verify(empreinte_observée)` : toute différence est un **refus explicite**, sans option de contournement. `cargo run --features tls,auth-sign --example auth_tls` enchaîne les trois cas sur le loopback.

Réponses : `401` + `WWW-Authenticate: Signature realm="litemcp", tag="litemcp-auth-v1"` et corps **constant** (le motif précis ne sort que par `on_auth_decision`) ; `403` + `-32023` (`data.reason` : `right_missing`, `approval_denied`, `approval_pending`) pour un droit manquant ou une confirmation refusée. `GET /health` reste **ouvert et inchangé**, et `GET /.well-known/oauth-protected-resource` ne répond (document RFC 9728 minimal) que si l'autorisation est active.

Pour une application qui possède **son propre** socket (MCP Control, passerelle maison), un seul point d'entrée : `ServerCore::dispatch_verified(&VerifiedRequest)` rend `AuthResponse { status, headers, body }` et vérifie **toujours** quand l'autorisation est active (`Gateway::dispatch_verified` pour un catalogue dynamique).

> **Ce que la lib ne fait pas** : stockage des clés (le `KeyStore` et les fichiers sont à l'application), appairage et enrôlement (aucun TOFU, aucun QR, aucune découverte), écran de confirmation (le trait décide, l'UI est à vous), émission ou validation de jetons OAuth (le profil expose `Signature`, pas `Bearer` : un client MCP à jeton ne peut pas s'y connecter), rotation/révocation automatiques (une révocation prend effet à la requête suivante), journal d'audit (seul le hook existe), persistance du cache de nonces, terminaison TLS derrière un proxy, mTLS, et **aucun client HTTP** : `Signer` signe pour le client que vous avez déjà.

## Concepts

### Outils — schémas dérivés, validation avant exécution

On déclare un schéma ; l'`inputSchema` de `tools/list` en est dérivé et chaque `tools/call` est validé **avant** d'atteindre votre code (`-32602`, problèmes dans `error.data.issues`). `tool` (handler `&Value` brut) · `tool_typed` (schéma dérivé, `&Value` déjà validé) · `tool_parse` (struct `Deserialize`).

```rust
use litemcp::prelude::*;

#[derive(serde::Deserialize)]
struct SearchArgs { query: String, limit: Option<u32> }

fn main() -> std::io::Result<()> {
    ServerBuilder::new("recherche", "1.0.0")
        .tool_parse(
            "search",
            "Recherche indexée",
            zod::object()
                .field("query", zod::string().min(2).max(64))
                .opt("limit", zod::integer().min(1).max(100))
                .strict(), // refuse les champs inconnus
            |args: SearchArgs| Ok(serde_json::json!({ "query": args.query, "limit": args.limit })),
        )
        .bind("127.0.0.1:23080")?
        .serve()
}
```

`.default(v)` publie le mot-clé `default` **et** injecte la valeur absente ; `record(valeur)` valide chaque valeur d'un dictionnaire ; `array`, `nullable`, `describe`, `enumeration`, `uint` et les `format` (`email`, `uuid`, `url`, `datetime`) sont **appliqués**, pas seulement publiés. Combinateurs, sémantique exacte et bornes (`MAX_DEPTH`, `MAX_ISSUES`) : [`docs/ZOD.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/ZOD.md).

### Ressources et prompts

Mêmes registres, mêmes indices de cache (`ttlMs` 60 000, `cacheScope` `private`) : `.resource(uri, nom, description, f)`, `.resource_data(…)` (texte ou `blob` base64), `.resource_template(…)` (RFC 6570), `.completions(f)` pour `completion/complete`, `.prompt(nom, description, f)` et `.prompt_typed(…)`. `resources/read` prend `{"uri": …}`, `prompts/get` `{"name": …, "arguments": {…}}`.

La lecture reste un **match exact** sur les URIs enregistrées (pas de substitution de gabarit côté noyau). Contenus riches — image, audio, `resource_link`, ressource embarquée — via `.tool_result` et `PromptMessage::user_content` : [`docs/CONCEPTS.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/CONCEPTS.md).

### Transport

Un endpoint, un verbe : `POST /mcp`. Réponse `application/json`, ou `text/event-stream` (un événement, `Content-Length`, clôture) quand le client n'accepte que SSE ou fournit `_meta.progressToken`. Notification de protocole (`notifications/*`, sans `id`) → `202` sans corps ; notification d'action (`tools/call` sans `id`) → `-32600`, et le handler **ne tourne pas**. `GET /mcp` → `405`.

### MRTR, progression, annotations

`.tool_mrtr` / `.prompt_mrtr` / `.resource_mrtr` : un handler peut rendre `InputRequired` (élicitation, échantillonnage, racines), l'état revient **signé** HMAC-SHA256 (TTL 10 min, 8 Kio) et les réponses du client sont revalidées. `progress::report(…)` pousse `notifications/progress` sur le flux SSE de la requête — sans jeton ni SSE, non-action. `.annotate(…)` publie `title`, `readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint` : [`docs/CONCEPTS.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/CONCEPTS.md).

## Features Cargo

Features additives : `macros` (défaut), `schemars`, `auth-sign`, `tls`. Le noyau n'a besoin d'aucune.

| Configuration | Crates ajoutés au noyau | Ce que ça ajoute |
|---|---|---|
| `default` | **+1** | les macros `#[tool]`/`#[resource]`/`#[prompt]` (crate locale `litemcp-macros`, aucune dépendance) |
| `default-features = false` | **0** | rien de moins : le noyau complet |
| `features = ["schemars"]` | **+55** | `schemars` + `jsonschema` : `from_schemars::<T>()`, `from_json_schema`, `format` validés |
| `features = ["auth-sign"]` | **+16** | signature et vérification Ed25519 (RFC 9421 + RFC 9530), +85 Kio de binaire |
| `features = ["tls"]` | **+17** | TLS 1.3 serveur (`rustls` + `ring`, jamais `tls12`) + certificat éphémère (`rcgen`), +657 Kio |

*Deltas comptés le 2026-11-13 (`cargo tree -e normal --prefix none`, dédoublonné, crate du consommateur exclue), Windows 11, rustc 1.99.1 ; `auth-sign`/`tls` remesurés le 2026-11-15 (release `strip` + `lto = "fat"`). Les totaux absolus ne sont plus cités : l'entrée de `getrandom` dans le noyau (2026-11-20) décale le décompte et la campagne n'a pas été rejouée ; les deltas, eux, ne changent pas.*

Ce que le slogan « trois dépendances » ne dit pas : les dépendances **directes** sont `serde`, `serde_json` et `getrandom` — le CSPRNG du système d'exploitation, non optionnel parce que le cœur en tire la clé HMAC des `requestState` même sans feature. Un build par défaut compile aussi `serde_derive`, donc `syn`, `quote`, `proc-macro2`, `unicode-ident` — ils viennent de `serde`, pas de `litemcp-macros`. Et le binaire d'un serveur à un outil pèse **≈ 1 Mio** (961 Kio mesurés, release `lto = "fat"` + `strip`), pas les 263 Kio annoncés auparavant : méthode et chiffres antérieurs dans [`docs/MESURES.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/MESURES.md#tailles-de-binaire).

## Configuration et valeurs par défaut

Toutes les méthodes du builder sont optionnelles et posent le défaut ci-dessous : `ServerBuilder::new(nom, version)` + un outil + un bind suffisent. L'ordre d'appel est libre, et `annotate`/`output_schema` fonctionnent avant **ou** après l'enregistrement de l'outil.

| Méthode / limite | Défaut | Rôle |
|---|---|---|
| `.route(path)` / `.bind_default()` | `/mcp` / `23080..=23099` | chemin unique validé au bind ; premier port libre sur `127.0.0.1`, repli éphémère |
| `.allow_origins(&[…])` | vide | origines autorisées en cross-origin (préflight `204`, ACAO echoé, jamais `*`) — valide aussi la garde anti-DNS-rebinding ([Limites](#limites)) |
| `.tool_timeout(d)` / `.no_tool_timeout()` | **30 s** / — | délai des handlers → `isError`/`-32603` data `timeout` ; le thread n'est pas tué ; `.no_tool_timeout()` revient à « illimité » (désactive le plafond de 256 threads de handlers) |
| `.on_request(hook)` | aucun | verrou par requête **avant la lecture du corps** : `401`/`403`/`429` |
| `.announce()` / `.anti_zombie()` | désactivées | annonce JSON dans `%TEMP%/litemcp-registry/` ; sweep du registre (retrait sur preuve d'identité seule) + drain borné |
| `.request_state_ttl(d)` / `.request_state_key(k)` | 10 min / aléatoire | état MRTR : TTL et clé HMAC partagée en flotte (clé vide → panique) |
| `.max_response_bytes(n)` / `.legacy(true)` | 16 Mio / strict | plafond de réponse (`-32603`) ; mode de tolérance (appel nu, `initialize`/`ping`) |
| Limites de transport | corps 1 Mio · 64 en-têtes · 16 Kio/ligne · 256 connexions · 256 threads | `413` · `431` · `431` · `503` · `rate_limited`/`overloaded` |

Tableau complet (`.on_call`, `.drain_timeout`, `handle.shutdown()`, `GET /health`) : [`docs/CONFIGURATION.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/CONFIGURATION.md).

## Sécurité

**Modèle de menace assumé : une bibliothèque réseau, pas un service exposé.** Quiconque peut joindre le socket peut appeler vos outils — sauf si vous activez l'[authentification par requêtes signées](#authentification-012), ou si vous posez un **verrou de requête**, évalué avant même la lecture du corps :

```rust
use litemcp::prelude::*;

ServerBuilder::new("calc", "1.0.0")
    .on_request(|ctx| {
        match ctx.header("Authorization").and_then(|v| v.strip_prefix("Bearer ")) {
            Some(t) if t == "s3cret" => Ok(()), // jeton « quick fix » — l'OIDC/SAML reste au gateway
            _ => Err(RequestRejection::unauthorized("missing or invalid bearer token")
                .with_header("WWW-Authenticate", "Bearer")),
        }
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

Le **TLS 1.3 est disponible** derrière la feature `tls` (`.tls(TlsConfig)`, certificat en mémoire, empreinte épinglable) ; sans elle, le serveur parle en clair — la terminaison à la périphérie reste un choix valable.

- **Liaison** : bind `127.0.0.1` si le serveur ne parle qu'à des processus locaux ; sinon, reverse proxy pour TLS, authentification forte (OIDC/SAML, OAuth 2.1) et rate-limiting par identité. Derrière un proxy, `ctx.peer` est l'adresse du proxy : ne croyez `X-Forwarded-For` qu'après avoir vérifié le pair ; le plafond de connexions (256) est **global**, le quota par IP est au proxy.
- **Navigateurs** : garde DNS-rebinding sur `Origin` (liste blanche, exception loopback même-origine) ; `Origin: null` et `Sec-Fetch-Site: cross-site` sont refusés. Les en-têtes CORS ne sont émis que pour une origine déclarée dans `allow_origins` ; une autre origine ne peut pas lire la réponse ([Limites](#limites)).
- **Handlers bloquants** : 30 s par défaut (`tool_timeout`) — la réponse est bornée, pas l'exécution (le thread termine en arrière-plan) ; une fermeture de flux SSE lève le drapeau coopératif [`cancel`](https://github.com/DeamonDev888/litemcp/blob/main/docs/PROTOCOLE.md), que le handler consulte à ses points sûrs.
- **Validation** : `tool_typed`/`tool_parse` appliquent le schéma **avant** votre code ; le schéma publié est exécuteur — ce qu'il annonce est exactement ce que la validation applique.

Le transport refuse, sur **toutes** les routes : `Transfer-Encoding` (400, anti request-smuggling), `Content-Length` dupliqué ou malformé (400/411), en-têtes à valeur unique dupliqués (400), ligne d'en-tête sans `:` ou espace avant `:` (400), obs-fold et LF seul (400), version HTTP inconnue (505), route inconnue (404), `Content-Type` non JSON (415), corps > 1 Mio (413), en-têtes > 64 ou ligne > 16 Kio (431), `Origin` non autorisé (403) ; paniques de handler et de hook contenues (`-32603` avec l'`id`). Détail : [`docs/SECURITE.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/SECURITE.md).

## Conformité MCP

Cible : la révision **`2026-07-28`** (stateless, sans `initialize`) — version servie seule, toute autre → `-32022` avec `data.supported`. Chaque résultat porte `resultType: "complete"` et `serverInfo` sous `_meta` ; `server/discover`, `tools/list`, `resources/list`, `prompts/list` et `resources/read` portent `ttlMs: 60000` et `cacheScope: "private"` (SEP-2549) ; `outputSchema` est publié **et** exécuteur ; MRTR (`resultType: "input_required"`) et `notifications/progress` sont servis ; `initialize`/`ping` n'existent qu'en profil legacy (`404` + `-32601` en strict) ; `GET /mcp` → `405`.

**Suite officielle** (`@modelcontextprotocol/conformance` `0.2.0-alpha.12`, requis `2026-07-28`, exécutée le 2026-11-13) : 50/50 scénarios exécutés, **37/37 scénarios scorés verts** ; les 30 checks en échec appartiennent aux 9 scénarios `tasks-*` (extension non implémentée, non scorée). Les deux MUST que l'audit du 2026-11-13 laissait hors couverture — indices de cache de `server/discover`, négociation `2025-11-25` en profil legacy — sont désormais servis et figés par `tests/mcp_discover_cache_2026_11_13.rs` et `tests/mcp_legacy_negotiation_2026_11_13.rs`. Tableau complet des méthodes et codes : [`docs/PROTOCOLE.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/PROTOCOLE.md).

## Performances

Campagne **2026-10-25**, 9 frameworks, même serveur de démonstration (4 outils), client neutre, Windows 11 loopback, 3 passes :

| Métrique | LiteMCP | Meilleur concurrent |
|---|---|---|
| `tools/call` keep-alive (moyenne) | **0,075–0,091 ms** | 0,162–0,186 ms (tower-mcp 0.25) |
| p99 | **0,25–0,29 ms** | 0,90–1,28 ms (tower-mcp) |
| Mémoire après ≈6 900 requêtes (`WorkingSet`) | **5,1 Mo** | 8,5 Mo (rust-mcp-sdk 2.0) |
| Connexions réutilisées | **2 000/2 000** | 2 000/2 000 (tower-mcp, mcp-go) |

Soit **×2,1** sur la latence moyenne et **×1,7** sur la mémoire face au meilleur des huit autres, à 15–30 µs du plancher TCP de la machine (0,060–0,067 ms). Méthode, planchers, pièges de mesure, portée exacte, journal brut : [`docs/BENCH.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/BENCH.md) ; campagnes Linux (`2026-10-24` : 0,031–0,033 ms, 2,7 Mio `VmRSS`), SLO, décisions tranchées par la mesure et comparatif ViteMCP : [`docs/MESURES.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/MESURES.md).

**Ces chiffres ne sont pas reproductibles depuis ce dépôt seul** : la campagne s'appuie sur le dépôt externe `litemcp-it` ; les deux exemples de mesure (`examples/bench_backends`, `examples/slo`) tournent en profil strict et rendent des chiffres, mais ne rejouent qu'un processus local — portée exacte dans `docs/BENCH.md`.

## Limites

- **TLS et authentification forte** : TLS 1.3 serveur et signature de requêtes sont **dans la lib** (features `tls` et `auth-sign`, [guide](#authentification-012)) ; restent à la périphérie l'OIDC/SAML, l'OAuth 2.1 pour clients à jeton, le mTLS, le rate-limit par identité et la terminaison TLS mutualisée.
- **CORS : liste blanche seulement** : `.allow_origins` ouvre le cross-origin aux seules origines déclarées — préflight `OPTIONS` → `204` avec `Access-Control-Allow-Origin` (l'origine listée, echoée une à une, jamais `*`) et `Vary: Origin`, et les réponses `POST` de ces origines portent les mêmes en-têtes ; une origine non listée reste refusée en `403`, sans aucun en-tête CORS. Pas de `Access-Control-Allow-Credentials`, et les rejets de transport (`400`/`413`/`415`) partent avant le verdict CORS : côté navigateur ils apparaissent comme une erreur CORS, pas comme leur statut exact.
- **`Host` : une règle par défaut, pas une authentification** : `ServerBuilder::allowed_hosts` confronte l'autorité reçue au listener **avant le routage** — `421 Misdirected Request` pour tout nom pleinement qualifié non déclaré (`attaquant.example`) ou toute IP étrangère. Par défaut sont acceptées l'IP effective du listener, le bouclage et tout **nom court** non qualifié (`localhost`, `mon-service`, mais aussi un nom sans point non déclaré) : ce n'est pas une liste blanche exhaustive, et le port de l'autorité est ignoré ; une liste `.allowed_hosts` non vide **s'ajoute** à ces autorités, elle ne les remplace pas. Derrière un reverse proxy, déclarez le nom public.
- **Annulation coopérative, jamais préemptive** : la fermeture d'un flux SSE lève un drapeau (`litemcp::cancel::is_cancelled`) que le handler consulte **s'il le veut** ; rien ne l'interrompt (Rust n'a pas d'annulation de thread). `tool_timeout` (30 s par défaut) borne la réponse, pas les effets de bord en cours ; `notifications/cancelled` n'est pas implémentée.
- **`structuredContent` d'un `isError`** : avec un `outputSchema` publié, le champ est **omis** sur un échec (page *Tools* : tout résultat structuré doit conformer au schéma publié) — le détail de l'erreur reste dans `content` ; sans `outputSchema` publié, la forme `{"error": …}` historique est conservée.
- **Pas de pagination** (`cursor` ignoré, pas de `nextCursor`), **pas de souscriptions** (`subscriptions/listen` absent : les clients s'appuient sur `ttlMs` = 60 s), extension **`tasks`** non implémentée.
- **Pas de `chunked` entrant** : tout `Transfer-Encoding` est refusé (`400`) ; un proxy qui re-chunke doit dé-chunker.
- **CI : trois systèmes, couverture inégale** (`.github/workflows/ci.yml`) : la matrice complète (défaut, toutes features, sans défaut, features isolées `macros` / `schemars` / `auth-sign` / `tls`, MSRV 1.85, rustdoc strict, simulations du pipeline de publication dans `cargo test --workspace`) tourne sous `ubuntu-latest` ; les jobs `windows-latest` et `macos-latest` lancent la suite en configuration par défaut **et** en `--all-features`, mais ni clippy, ni rustdoc, ni les features isolées — un écart propre à une plateforme sous une feature isolée n'y serait pas vu. Pour rejouer la suite localement : [`docs/TESTS.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/TESTS.md).

## Licence

`MIT OR Apache-2.0` — [LICENSE-MIT](LICENSE-MIT) · [LICENSE-APACHE](LICENSE-APACHE)

Historique : [CHANGELOG.md](CHANGELOG.md) (politique de versioning incluse) · pipeline de publication : [docs/RELEASE.md](https://github.com/DeamonDev888/litemcp/blob/main/docs/RELEASE.md).
