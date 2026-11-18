# Authentification LiteMCP — spécification

Statut : lot L0, figée le 2026-11-15 pour `litemcp` 0.11.0 (HEAD `b350c16`), **révisée au lot L3** et livrée en **0.12.0**.
Décisions appliquées : **DL1 = a** (crates auditées derrière features), **DL2 = a** (RFC 9421 + RFC 9530, profil strict), **DL3 = a**
(pas d'OAuth 2.1 ; lot L4 non lancé). Cette page est la promesse : un client compatible doit pouvoir être écrit dans un autre
langage, et chaque « DOIT » est testable.

## 1. Modèle

Chaque client possède une paire Ed25519 ; le serveur ne connaît que des clés publiques (`KeyStore` fourni par
l'application). Chaque requête porte une signature détachée RFC 9421 sur des composants choisis (§2) : méthode, chemin,
autorité, corps exact, instant, nonce. Le transport établit l'identité (`AuthContext`), le noyau décide des droits (§4)
et de la confirmation (§5). Ce profil **n'est pas** le flux OAuth de MCP 2026-07-28 : le serveur expose
`WWW-Authenticate: Signature`, pas `Bearer`, et un client MCP standard à jeton porteur ne peut pas s'y connecter.

## 2. Requête signée (DL2 = a)

### 2.1 En-têtes de la requête

| En-tête | Rôle | Contraint |
|---|---|---|
| `Host`, `Content-Length`, `Content-Type` | autorité, cadrage, type | `Host`/`Content-Length` exigés (400) ; `Content-Type` facultatif mais non-JSON → 415, et **couvert** ici (donc exigé) |
| `Content-Digest` | empreinte du corps (RFC 9530) | **obligatoire**, un seul membre `sha-256` |
| `Mcp-Method` | routage SEP-2243 | **obligatoire**, **couvert**, doit égaler le corps |
| `Mcp-Name` | routage SEP-2243 | exigé par le transport pour `tools/call`, `prompts/get`, `resources/read`, interdit ailleurs ; **couvert quand présent** ; lié par méthode à `params.name` (`tools/call`, `prompts/get`) ou `params.uri` (`resources/read`) |
| `Signature-Input`, `Signature` | RFC 9421 §4.1/§4.2 | **exactement un membre** chacun, même label |

Non couverts, sans effet sur l'exécution : `Accept`, `User-Agent`, `Date`, `Origin`, `X-Forwarded-For`, `Expect`, `MCP-Protocol-Version`
(contredit par `_meta.protocolVersion` du corps couvert). `Mcp-Param-*` : **refusé en 401** dès que l'authentification est
active — inutilisé aujourd'hui (`ToolDef::mcp_params` : `#[allow(dead_code)]`) mais réservé : on refuse plutôt que de
signer à moitié.

### 2.2 Composants couverts

Liste **fermée**, ordre **imposé** ; une seule des deux listes est admise : avec nom
`("@method" "@path" "@authority" "content-digest" "content-type" "mcp-method" "mcp-name")`, ou la même **moins**
`"mcp-name"` (six composants). La liste voyage dans `Signature-Input`, elle-même couverte par `@signature-params` :
un intermédiaire ne peut pas la réduire. Un composant portant un paramètre (`;sf`, `;bs`, `;key`, `;req`, `;tr`) est
refusé ; aucun autre composant, aucune répétition, aucun autre ordre.

Valeurs dérivées (RFC 9421 §2.2) : `@method` = méthode reçue, le profil exige exactement `POST` ; `@path` = chemin absolu
sans query, octets `%XX` **non décodés**, `/` si vide ; `@authority` = `Host` normalisé (minuscules, port par défaut omis).
Valeur d'un champ : valeur reçue, OWS de bord retirés, **une seule occurrence**. `Content-Digest` : `sha-256=:` + base64
**standard avec padding** de `SHA-256(corps)` (32 octets → 44 caractères) + `:`, calculé sur les octets du corps **reçu**
(après décadrage, avant analyse JSON) ; `sha-512`, un second membre ou du base64url sont refusés.

### 2.3 Paramètres, horloge, nonce

Exactement ces cinq, dans cet ordre, aucun autre (pas d'`expires`) : `created` (Integer, secondes UNIX), `nonce`
(String, 22 caractères base64url sans padding = 16 octets aléatoires du système), `keyid` (String, §3), `alg`
(String, exactement `ed25519`), `tag` (String, exactement `litemcp-auth-v1`).

`created` DOIT être dans `[now − 60 s, now + 60 s]` à la vérification (horloge murale, arithmétique saturante). Le
nonce DOIT être neuf pour ce `keyid` : un nonce déjà vu dans la fenêtre est refusé. Cache borné, **par clé** : 4096
nonces retenus 120 s (≈ 34 req/s par clé), et un nonce encore dans la fenêtre n'est **jamais** évincé — compartiment
plein, l'insertion est refusée (`nonce_cache_full`) et la requête ne passe pas. Une clé qui inonde ne peut donc ni
évincer ses propres nonces valides ni ceux d'une autre clé (≈ 0,3 Mio par clé à pleine charge, estimation). Un
redémarrage vide le cache (rejeu possible dans la fenêtre de 60 s — assumé, §11).

### 2.4 Chaîne à signer, octet par octet

RFC 9421 §2.5. Concaténation : pour chaque composant, `"nom"` + `:` + **un** espace + valeur + **LF** (`0x0A`) ; puis
la ligne `@signature-params` **sans LF final**.

- séparateur : LF seul — **jamais CRLF** ; aucun `CR` (`0x0D`) ; ASCII uniquement ; nom de champ en minuscules ;
  espace après les deux-points : exactement un ; aucun blanc avant ou après une valeur ; aucun LF final ;
- `@signature-params` = la valeur du membre de `Signature-Input`, **identique caractère pour caractère** (ordre des
  composants et des paramètres compris) ; message signé = ces octets tels quels, Ed25519 **pur** (RFC 8032 §5.1.6,
  sans préhash) ; signature = 64 octets `R‖S`, transportée en base64 standard avec padding dans `Signature: label=:…:`.

### 2.5 Exemple complet et reproductible

Clé : **vecteur 1 de RFC 8032 §7.1**, clé publique `d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a`.
`created=1794700800` = 2026-11-15T00:00:00Z, nonce = base64url des octets `00 01 … 0f` : **vecteur de test**, pas une
requête rejouable (hors fenêtre, nonce déjà vu). Corps : 171 octets, `SHA-256` =
`253bcc7b12151c19c8c89e90aca8275baa363a27ee43027151feea68bbed94cd`.

```http
POST /mcp HTTP/1.1
Host: 127.0.0.1:23080
Content-Type: application/json
Content-Length: 171
Content-Digest: sha-256=:JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0=:
Mcp-Method: tools/call
Mcp-Name: db.query
Signature-Input: sig1=("@method" "@path" "@authority" "content-digest" "content-type" "mcp-method" "mcp-name");created=1794700800;nonce="AAECAwQFBgcICQoLDA0ODw";keyid="client-demo";alg="ed25519";tag="litemcp-auth-v1"
Signature: sig1=:kOnwfzENaxBF6ubQiJGlXkRfUYUJuELYzHnvqwrzpScvO3JchjFOsREa4j7CsRrdiEZifLmZZZEBBFPJqpsJDA==:

{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"db.query","arguments":{"sql":"select 1"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}
```

Chaîne à signer (427 octets, 8 lignes, pas de LF final) :

```text
"@method": POST
"@path": /mcp
"@authority": 127.0.0.1:23080
"content-digest": sha-256=:JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0=:
"content-type": application/json
"mcp-method": tools/call
"mcp-name": db.query
"@signature-params": ("@method" "@path" "@authority" "content-digest" "content-type" "mcp-method" "mcp-name");created=1794700800;nonce="AAECAwQFBgcICQoLDA0ODw";keyid="client-demo";alg="ed25519";tag="litemcp-auth-v1"
```

Signature attendue :
`90e9f07f310d6b1045eae6d08891a55e445f518509b842d8cc79efab0af3a5272f3b725c86314eb1111ae23ec2b11add8846627cb9996591010453c9aa9b090c`

Recalcul (standard pour le corps et la chaîne, `cryptography` pour la vérification) :

```python
import base64, hashlib
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
body = open("corps.json","rb").read()            # 171 octets, sans LF final
assert base64.b64encode(hashlib.sha256(body).digest()).decode() == "JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0="
base = "\n".join([...])                          # les 8 lignes ci-dessus, dans l'ordre, sans LF final
sig = bytes.fromhex("90e9f07f310d6b1045eae6d08891a55e445f518509b842d8cc79efab0af3a527" "2f3b725c86314eb1111ae23ec2b11add8846627cb9996591010453c9aa9b090c")
Ed25519PublicKey.from_public_bytes(bytes.fromhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")).verify(sig, base.encode("ascii"))
```

### 2.6 Liaison au serveur destinataire

`@authority` et `@path` sont couverts : rejouer la requête sur un autre hôte, port ou chemin échoue. Deux serveurs
partageant la même autorité **et** le même chemin derrière un même proxy ne sont pas distingués par la signature : le
`KeyStore` de chacun tranche (clé non inscrite = refus). Derrière un terminateur TLS, `@authority` est l'autorité vue par
le serveur : `allowed_hosts` doit être configuré en conséquence.

### 2.7 Motifs de refus (liste fermée)

Les motifs sont rendus à l'application (hook §6), **jamais** au réseau : tous les 401 ont le **même corps**, celui
des rejets de transport existants (`code -32600`, `id: null`, message fixe).

| Motif interne | HTTP | Code |
|---|---|---|
| `signature_input_missing`, `signature_missing`, `multiple_signatures`, `label_mismatch` | 401 | `-32600` |
| `covered_set_not_allowed`, `component_param`, `params_not_allowed` | 401 | `-32600` |
| `alg_not_ed25519`, `tag_mismatch`, `keyid_malformed`, `mcp_param_present` | 401 | `-32600` |
| `key_unknown`, `key_revoked`, `signature_invalid` | 401 | `-32600` (identiques) |
| `content_digest_missing`, `content_digest_mismatch`, `clock_skew`, `nonce_replay`, `nonce_cache_full` | 401 | `-32600` |
| `right_missing`, `unauthenticated`, `approval_denied`, `approval_pending` | 403 | `-32023` + `data.reason` (§5) |
| `Content-Type` non JSON, `Transfer-Encoding`, corps illisible | 415 / 400 | transport, **avant** vérification |

`unauthenticated` : refus du **noyau** quand l'autorisation est active et qu'aucune identité n'est fournie — `403`, `-32023`, `data.reason = "unauthenticated"` ; le transport intégré rend `401` avant le dispatch. Doublon d'un en-tête **à valeur unique du transport** (`Content-Type`, `Mcp-Method`, `Mcp-Name`, `MCP-Protocol-Version`) = `400`, avant vérification ; un `Content-Digest` dupliqué est refusé en `401`, la vérification de signature tranchant.

En-tête de tout 401 : `WWW-Authenticate: Signature realm="litemcp", tag="litemcp-auth-v1"` (valeur exacte) ; le schéma
`Signature` n'est pas enregistré auprès de l'IANA, les clients ne doivent rien en déduire d'autre. Ordre des contrôles, du
moins cher au plus cher : en-têtes → composants et paramètres → `content-digest` → horloge → nonce → `keyid`/`KeyStore`
→ Ed25519 **en dernier**. Dans le transport intégré, la vérification vient **après** la lecture du corps et les contrôles
de transport (`400`, `404`, `405`, `415`, `421` passent avant) : un refus de transport n'est pas un refus d'identité. Une
notification sans signature est refusée en `401` (échec fermé), elle n'est jamais exécutée.

## 3. Identités

`keyid` : 1 à 128 octets ASCII, `^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$`. Refusés : vide, guillemet, antislash, espace,
caractère de contrôle, non-ASCII, plus de 128 octets. Choisi par l'application ; la lib ne l'interprète pas et ne le
dérive jamais de la clé.

Empreinte d'une clé Ed25519 : `ed25519:` + `SHA-256(clé publique, 32 octets)` en hexadécimal **minuscule**, soit
`ed25519:` + 64 caractères ; forme courte (affichage, appairage) : `ed25519:` + les 16 premiers caractères
hexadécimaux de la même empreinte (8 octets). Comparaison en temps constant, via un type `Fingerprint` qui n'expose
ni `PartialEq` brut ni `Display` ambigu. Empreinte TLS : `sha256:` + `SHA-256(DER)` (64 caractères).

Export / import : un seul bloc texte, une seule clé par fichier, fins de ligne LF ou CRLF.

```
-----BEGIN LITEMCP ED25519 PRIVATE KEY-----
<base64 standard avec padding des 32 octets de graine, sur une ligne>
-----END LITEMCP ED25519 PRIVATE KEY-----
```

Variante publique : `PUBLIC KEY` et les 32 octets de clé publique. Refusés : autre libellé, bloc public présenté à
l'import d'une clé privée (et réciproquement), contenu décodé de longueur ≠ 32, alphabet base64url, base64 non paddé,
blanc à l'intérieur du base64, texte hors des gardes, second bloc, fichier vide. Permissions du fichier non testées (§9).

## 4. Droits

Modèle **générique** : un droit est une chaîne opaque déclarée par l'application, `^[a-z0-9][a-z0-9._:-]{0,63}$`
(64 octets au plus, minuscules) ; la lib ne fait que tester l'appartenance à un ensemble — aucun droit prédéfini,
aucune hiérarchie, aucun joker.

- Déclaration : `.require_right(tool, right)`, `.require_resource_right(uri_ou_gabarit, right)`, `.require_prompt_right(nom, right)` ;
  plusieurs droits sur un même élément = **tous** exigés (ET). Une URI déclarée **exactement** (ressource enregistrée ou clé de gabarit) ne dépend que de ses
  propres droits : aucun gabarit ne la restreint (pas de sur-restriction), et elle n'hérite d'aucun des leurs.
- Gabarits chevauchants : une URI couverte par **plusieurs** gabarits exige l'**union des droits de tous les gabarits couvrants**, indépendamment de l'ordre
  des clés ; un gabarit couvrant sans droit déclaré fait refuser l'URI (échec fermé). Couverture : un motif simple `{var}` couvre un segment non vide sans `/` ;
  tout autre motif (`{+x}`, `{?x}`, `{a,b}`, expressions accolées ou non fermées) est couvert de façon **conservatrice** — préfixe, suffixe et littéraux
  intermédiaires dans l'ordre, chaque expression valant n'importe quelle chaîne : il peut sur-couvrir, jamais sous-couvrir.
- Activation : la présence d'un `Verifier` (`.verifier(...)`) ; sans `Verifier`, comportement strictement inchangé.
- **Élément sans droit déclaré = refusé** dès que l'autorisation est active (échec fermé) : `tools/call` →
  `-32023` / `right_missing`, et l'élément n'apparaît dans aucune liste.
- Listes : `tools/list`, `resources/list`, `resources/templates/list`, `prompts/list` ne rendent que les éléments dont
  les droits sont tous détenus par l'appelant. `tools/list` n'est alors plus servi depuis le cache précalculé
  (`ServerCore::tools_list_json`) mais construit par requête.
- `Gateway` : mêmes règles sur le snapshot courant ; un snapshot échangé pendant un appel ne vaut que pour le
  dispatch suivant. Un dispatch imbriqué (handler appelant `Gateway::dispatch`) hérite de l'`AuthContext` du fil,
  comme `progress` et `cancel` le font déjà pour leurs canaux.
- Appelant sans droit sur un élément existant : `-32023` / `right_missing`, jamais le contenu ni le handler (compteur
  à zéro dans le test). Élément inexistant : `-32602` « Unknown tool » — l'existence des noms n'est pas un secret (§11).

## 5. Confirmation externe

Trait fourni par l'application, interrogé **après** les droits et **avant** le handler, avec le nom du tool, le
client, l'`id` JSON-RPC et les arguments **tels que le handler les recevra** (après validation et injection des valeurs
par défaut — ce qui est approuvé est exactement ce qui s'exécute) :

| État | Effet | Réponse |
|---|---|---|
| `Approved` | le handler s'exécute | réponse normale |
| `Denied` | le handler n'est jamais appelé | 403, `-32023`, `data.reason = "approval_denied"` |
| `Pending { retry_after }` | rien n'est exécuté ni mis en file | 403, `-32023`, `data.reason = "approval_pending"`, `data.retryable = true`, `data.retryAfterMs` si fourni |

Ce qui est exécuté est **exactement** ce qui a été soumis : le handler reçoit la même valeur `serde_json::Value`,
sérialisée à l'identique (test : octets identiques capturés avant et après).

Code JSON-RPC : **`-32023`**, un seul code pour les trois motifs, `data.reason` porte la distinction. Justification :
`-32700/-32600/-32601/-32602/-32603` ont un sens JSON-RPC fixé ; `-32020/-32021/-32022` sont déjà alloués par MCP
et par `ErrorObject` (`src/jsonrpc.rs:60-71`) ; `-32000..-32019` est la plage implementation-defined que MCP
2026-07-28 déconseille et que le crate a déjà refusé d'utiliser pour ses échecs de handler (`src/server.rs:239` :
« MCP 2026-07-28 déconseille d'introduire de nouveaux codes dans la plage implementation-defined »). `-32023` est la
première valeur libre après le bloc MCP, hors plage déconseillée ; un futur SEP pourrait la réclamer, d'où une
constante unique et des clients qui décident sur `data.reason`. Les codes de `ToolError` (`src/error.rs` :
`rate_limited`, `timeout`…) classifient une exécution d'outil, ce ne sont pas des codes JSON-RPC : non réutilisés.

`approval_pending` n'est pas une erreur d'outil : le handler n'a pas tourné (ni `isError` ni `structuredContent`) et aucun
état n'est conservé — le client réessaie (même corps, nouvel `id`, nouveau `created`, nouveau nonce).

## 6. API publique (C1 à C12)

Le noyau (`src/auth.rs`, **aucune feature**, aucune dépendance) porte C1–C5, C8 et C9 (épinglage) ; `auth-sign` porte
C6–C7 et le signeur ; `tls` porte C10.

| # | Élément exact |
|---|---|
| C1 | `auth::AuthContext` (`client()`, `label()`, `rights()`, `has_right(&str)`, `proof()`, `verified_at()`), `auth::ProofMethod { Signature { keyid }, Token { issuer }, Application }` (`#[non_exhaustive]`) |
| C2 | `ServerBuilder::require_right(tool, right)`, `::require_resource_right(uri, right)`, `::require_prompt_right(nom, right)` |
| C3 | filtrage dans `ServerCore` (listes) et dans `Gateway` (même code, snapshot courant) |
| C4 | `ServerBuilder::on_auth_decision(Fn(&auth::AuthDecisionEvent<'_>))` ; `AuthDecisionEvent { method, name: Option<&str>, client: Option<&str>, verdict, reason: Option<&str> }`, `auth::AuthVerdict { Allowed, RightMissing, ApprovalDenied, ApprovalPending, Unauthenticated }` |
| C5 | `ServerBuilder::require_approval(tool)`, `::approval_policy(Arc<dyn auth::ApprovalPolicy>)`, `auth::ApprovalPolicy::decide(&ApprovalRequest) -> ApprovalVerdict`, `ApprovalRequest { client, tool, arguments, request_id }` |
| C6 | `auth::sign::KeyStore::lookup(&str) -> Option<ClientKey>`, `ClientKey { client, label, public_key: [u8; 32], rights, status }`, `KeyStatus { Active, Revoked }` |
| C7 | `auth::sign::Signer` (`new(SigningKey)`, `headers(&RequestToSign) -> SignedHeaders`), `SignatureVerifier::new(Arc<dyn KeyStore>, NonceCache)`, `NonceCache::new(par_clé, ttl)`, `key::{SigningKey, VerifyingKey, Fingerprint}`, `auth::AuthError` (`#[non_exhaustive]`) |
| C8 | `ServerCore::dispatch_verified(&VerifiedRequest<'_>) -> auth::AuthResponse` (idem `Gateway`), `auth::VerifiedRequest { method, path, authority, headers, body }`, `auth::AuthResponse { status: u16, headers: Vec<(String, String)>, body: Vec<u8> }`, `McpHeaders::from_headers` ; `dispatch_authenticated` reste l'API d'un transport qui vient de prouver l'identité |
| C9 | `auth::sign::Signer` + `auth::PinnedServer` (`new`, `matches`, `verify`) — **aucun client HTTP n'existe** (§9) |
| C10 | `tls::TlsConfig` (`from_pem`, `from_der`, `self_signed_ephemeral`, `certificate_der`), `::fingerprint() -> tls::CertificateFingerprint`, `ServerBuilder::tls(TlsConfig)`, `BoundServer::certificate_fingerprint()` |
| C11 | `GET /health` inchangé (aucune vérification, aucun en-tête de plus qu'`X-Instance-Id`) ; `401` + `WWW-Authenticate` (§2.7) ; `GET /.well-known/oauth-protected-resource` servi seulement quand l'autorisation est active |
| C12 | aucune signature existante ne change ; ajouts seuls : `HttpStatus::Forbidden` (`#[non_exhaustive]`) et les éléments ci-dessus |

`ServerBuilder::verifier(Arc<dyn auth::Verifier>)` active l'autorisation ; `Verifier::verify(&VerifiedRequest)` rend
un `AuthContext` ou un `AuthError`. Deux types existants ne suffisent pas et ne sont pas modifiés : `DispatchResponse`
(aucun emplacement d'en-tête, statut limité à `Ok/BadRequest/NotFound/InternalServerError`, `src/server.rs:777-830`)
et `RequestRejection` (rejet **avant** lecture du corps, donc sans `content-digest`).

## 7. Crates retenues (DL1 = a)

| crate | version | licence | feature | dépendances transitives | taille ajoutée mesurée | dernier audit public connu |
|---|---|---|---|---|---|---|
| `ed25519-dalek` | 3.0.0 | BSD-3-Clause | `auth-sign` | **+16** | **+116 736 o** (114 Kio) | Quarkslab, 2019-08-26 : `subtle`, `curve25519-dalek`, `bulletproofs`, plus « regard marginal » sur `ed25519-dalek`. Aucun audit public de la 3.0.0. |
| `rustls` + `ring` | 0.23.46 (`ring` 0.17.14) | Apache-2.0 OR ISC OR MIT ; `ring` Apache-2.0 AND ISC | `tls` | **+17** | **+672 768 o** (657 Kio) au-dessus de `auth-sign` | Cure53, mai–juin 2020 (`ring`, `webpki`, `rustls`), publié 2020-06. |
| `rcgen` (certificat éphémère) | 0.14.7 | MIT OR Apache-2.0 | `tls` | **+0** (`ring`, `time`, `yasna` déjà comptés ci-dessus) | inclus dans les 657 Kio ci-dessus | Aucun audit public connu ; crate de l'organisation `rustls`, backend `ring`, `default-features = false`. |

Mesure (2026-11-15, Windows x86-64, `rustc` 1.99.1) : projet jetable dans `%TEMP%`, `[workspace]` vide, `strip = true`,
`lto = "fat"`, `codegen-units = 1`, panique `abort` ; binaire qui signe et vérifie un message (`ed25519-dalek`) puis
construit un `ClientConfig` (`rustls`). Référence sans crate : **112 128 o** ; avec `ed25519-dalek` : **228 864 o** ;
avec `rustls` de plus : **901 632 o**. MSRV du crate (1.85) respectée : `ed25519-dalek` déclare 1.85, `rustls` 1.71,
`ring` 1.66, `rcgen` 1.71.

Décompte **mesuré** des transitives (`cargo tree -e normal --prefix none`, noms de paquets dédoublonnés, racine et
`serde`/`serde_json` exclues) : base 12 crates ; `auth-sign` en ajoute **16** ; `tls` en ajoute **17** ; les deux
ensemble **30** (`subtle`, `cfg-if` et `zeroize` sont partagés). Les chiffres du lot L0 (19 et 26) comptaient autrement
(toutes les arêtes, doublons compris) : ce sont ceux ci-dessus qui décrivent le build d'un consommateur.
`rustls` est pris en `default-features = false, features = ["ring", "std"]` : sans `tls12`, TLS 1.2 est
**incompilable** ; `rcgen` en `default-features = false, features = ["ring"]` (ni `pem`, ni `aws-lc`).

## 8. TLS (feature `tls`)

- **Version** : TLS 1.3 uniquement ; un ClientHello 1.2 échoue à la poignée de main (test : client `rustls` limité à
  1.2 → erreur, aucun octet HTTP). ALPN : `http/1.1` seulement.
- **Suites** : les trois suites TLS 1.3 de `rustls`, non configurables par l'appelant — `TLS13_AES_256_GCM_SHA384`,
  `TLS13_AES_128_GCM_SHA256`, `TLS13_CHACHA20_POLY1305_SHA256` (`rustls-0.23.46/src/crypto/ring/tls13.rs`).
- **Certificat** : fourni par l'application (`from_pem`/`from_der`) ou auto-signé **en mémoire** sur demande explicite
  (`self_signed_ephemeral`, `rcgen`) ; la lib **n'écrit jamais** clé ni certificat sur disque (§9).
- **Empreinte** : `sha256:` + `SHA-256(DER)` hexadécimal minuscule (`SHA-256` réutilise celui de `request_state`, celui
  du `Content-Digest`), exposée par `BoundServer::certificate_fingerprint()` et `TlsConfig::fingerprint()`.
- **Épinglage client** : `auth::PinnedServer::new("sha256:…")` puis `verify(empreinte_observée)` — comparaison en temps
  constant, toute différence = refus explicite, **sans option de contournement** (aucun `danger_accept_invalid_certs`).
- **Refusés** : TLS ≤ 1.2, certificat substitué, autorité inconnue (aucun magasin racine système par défaut), `http://`
  en clair quand `tls` est configuré, ALPN autre que `http/1.1`, mTLS (pas d'authentification de client TLS).

## 9. Ce que la lib ne fait pas

Stockage des clés (le `KeyStore` et les fichiers sont à l'application), appairage et enrôlement (aucun TOFU, aucun QR,
aucune découverte), interface de confirmation (le trait décide, l'écran est à l'application), émission ou validation de
jetons (DL3 = a : aucun OAuth, aucun serveur d'autorisation, aucun jeton porteur, aucun document `bearer_methods_supported`
trompeur — le document RFC 9728 servi est un squelette honnête : `resource`, `methods`, `www_authenticate`,
`authorization_servers: []`), rotation et révocation automatiques (une révocation prend effet à la requête suivante),
journal d'audit (seul le hook `on_auth_decision` existe), persistance du cache de nonces, terminaison TLS derrière un proxy,
mTLS, génération ou persistance du certificat serveur (l'éphémère vit en mémoire), et **aucun client HTTP** : 0.12.0
est un serveur seul ; C9 se limite à `Signer` (produire les en-têtes pour le client que l'application possède déjà) et à
`PinnedServer` (refuser un serveur dont l'empreinte a changé). Un client HTTP serait une nouvelle surface réseau à
auditer, sans besoin pour MCP Control, qui a son propre socket et appelle `dispatch_verified` (C8).

## 10. Règles de sécurité (testables)

1. Aucun secret dans une réponse, une erreur, un `Debug` ou un exemple (corps des 401 constant, `SigningKey` sans `Debug`
   lisible ni `Display`, clé privée effacée par `zeroize`) ; comparaisons en temps constant (signature, empreintes, digest).
2. Aléa : uniquement celui du système d'exploitation (`RandomState` pour les nonces, `ring`/OS pour TLS) ; aucun générateur
   ensemencé, aucune horloge comme graine. Échec fermé :
   tout cas non prévu (paramètre ou composant inconnu, `KeyStore` muet, panique du trait) aboutit à un refus.
3. Aucune primitive asymétrique, aucun TLS, aucun aléa écrit à la main : uniquement `ed25519-dalek` et `rustls` ;
   aucune évaluation par un tiers n'est revendiquée.

## 11. Points non tranchés

1. **Code de refus d'autorisation** : `-32023` (recommandé, §5) / `-32010` (plage déconseillée par MCP) /
   `resultType: input_required` du MRTR (réutilise l'existant mais laisse le client répondre à sa place).
2. **Statut HTTP de `approval_pending`** : 403 (recommandé : un seul statut pour les trois motifs) / 202 (plus juste
   sémantiquement, mais un corps d'erreur sous 202 surprend) / 200.
3. **`Mcp-Param-*`** : refus en 401 quand l'auth est active (recommandé) / couverture par signature (impossible dans
   une liste fermée, noms dynamiques) / ignorés comme aujourd'hui.
4. **Droits des gabarits de ressources** : `require_resource_right` accepte la clé `uriTemplate` (recommandé) / un
   droit unique pour tout `resources/*` / aucun filtrage des gabarits.
5. **Existence des noms** : `-32602` pour un tool inconnu et `-32023` pour un tool interdit (recommandé, noms non
   secrets) / un verdict indistinct (meilleure confidentialité, diagnostic dégradé).
6. **Certificat « généré si absent »** (plan L3 §5) : impossible sans écrire une clé privée sur disque — **décidé** :
   éphémère en mémoire (`self_signed_ephemeral`), l'application persiste si elle veut un épinglage stable.
   **Client (C9)** : aucun client HTTP n'existe dans le crate (0.11.0/0.12.0 est un serveur seul) — **décidé** : C9 =
   `Signer` + `PinnedServer`, sans client ; MCP Control a le sien et utilise C8 (`dispatch_verified`).
7. **Cache de nonces** : 4096/clé, 120 s, refus au-delà sans éviction (recommandé, corrigé après audit) / taille globale (dénial croisé) / persistance (hors modèle sans état) — **livré** : cache borné par clé, sans éviction, sans persistance.
8. **Document RFC 9728** : un document `oauth-protected-resource` complet supposerait un serveur d'autorisation et des
   jetons porteurs, que DL3 exclut. **Décidé** : document minimal et honnête, servi seulement quand l'autorisation est
   active, sans `bearer_methods_supported` ni `authorization_servers` mensongers.

## Révisions

- 2026-11-15 — L3 : `dispatch_verified`/`AuthResponse` (C8), `PinnedServer` (C9 réduit), feature `tls` (TLS 1.3,
  `rcgen`), annonce `authRequired`/`certificateFingerprint`, RFC 9728 minimal, `completion/complete` filtré, transitives
  mesurées, `AuthDecisionEvent` à champs `Option`, arguments d'approbation = valeur exécutée (après défauts).
