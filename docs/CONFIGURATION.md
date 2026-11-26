# Configuration — toutes les méthodes et leurs valeurs par défaut

Toutes les méthodes du builder sont optionnelles et posent le défaut ci-dessous :
`ServerBuilder::new(nom, version)` + un outil + un bind suffisent à servir. L'ordre d'appel est
libre, et `annotate`/`output_schema` fonctionnent avant **ou** après l'enregistrement de l'outil.
Valeurs vérifiées dans le code (`src/lib.rs`, `src/http.rs`, `src/server.rs`, `src/server/builder.rs`,
`src/request_state.rs`).

[← Retour au README](../README.md)

## Tableau

| Méthode / limite | Défaut | Rôle |
|---|---|---|
| `.route(path)` | `/mcp` | chemin unique, validé au bind (commence par `/`, sans query, sans masquer `/health`) |
| `.bind_default()` | `23080..=23099` | premier port libre sur `127.0.0.1`, repli éphémère ; `.bind(addr)` pour un port explicite |
| `.allow_origins(&[…])` | vide | origines tolérées (filtre anti-DNS-rebinding) **et** autorisées à lire la réponse en CORS : préflight `OPTIONS` → `204`, `Access-Control-Allow-Origin` + `Vary: Origin` sur les POST ([Limites](../README.md#limites)) |
| `.allow_preflight_header(name)` | liste de base seule | en-tête de requête **annoncé en plus** dans `Access-Control-Allow-Headers` du préflight (base : `Content-Type`, `Accept`, `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`, complétée des en-têtes de signature dès qu'un `.verifier` existe et des `Mcp-Param-*`) — cumulable, dédoublonné sans casse, panique sur un nom vide ou non ASCII ; pour l'en-tête que **votre** application lit (`Authorization` via `.on_request`, clé d'API) : sans déclaration, le navigateur bloque la requête cross-origin au préflight |
| `.allowed_hosts(…)` | autorités locales | hôtes acceptés dans `Host` ; défaut : adresse effective du listener, littéral de bouclage, nom court non qualifié ; tout nom pleinement qualifié ou littéral étranger → `421` (liste déclarée **additive**, insensible à la casse, port ignoré) |
| `.read_timeout(d)` | 30 s | timeout de lecture par opération : un client trop lent est déconnecté sans réponse ; fixe aussi l'échéance **globale** d'écriture de la réponse (`× 2`, bornée à 1 s..=60 s) |
| `.write_deadline(d)` | `read_timeout × 2`, bornée 1 s..=60 s | échéance **globale** de l'envoi d'une réponse (coupe un client qui lit au compte-gouttes) ; remplace la valeur dérivée — utile sur un lien lent ; toute valeur < 1 s est relevée à 1 s ; le budget de lecture reste celui de `.read_timeout` |
| `.max_connections(n)` | 256 / serveur | au-delà : `503` immédiat, aucun thread ouvert |
| `.connection_ttl(d)` | 5 min | durée de vie d'une connexion keep-alive ; `0` désactive |
| `.tool_timeout(d)` / `.no_tool_timeout()` | **30 s** / — | délai des handlers → `isError` (`ErrorCode::Timeout`) pour un outil, `-32603` data `timeout` sinon ; le thread n'est pas tué. `.no_tool_timeout()` rétablit « illimité » (et désactive le plafond de threads de handlers) |
| `.on_call(hook)` | aucun | observabilité après chaque message (`CallEvent { method, name, duration, status }`), cumulable |
| `.on_request(hook)` | aucun | verrou par requête **avant la lecture du corps** : `401`/`403`/`429` + en-têtes libres |
| `.announce()` | désactivée | annonce JSON dans `%TEMP%/litemcp-registry/<nom>.json` (`addr`, `instance`, PID, `authRequired`, `certificateFingerprint` sous TLS seulement), retirée à la fin de vie |
| `.tls(config)` | désactivé (feature `tls`) | sert la route MCP en **TLS 1.3** : certificat et clé en mémoire uniquement, ALPN `http/1.1`, pas de mTLS ; l'empreinte du certificat servi est publiée par `bound.certificate_fingerprint()` (épinglage client) et reprise dans l'annonce (`certificateFingerprint`) quand `.announce()` est actif |
| `.anti_zombie()` | désactivée | sweep du registre au bind (retrait **seulement** sur preuve d'identité) + drain borné à l'arrêt, bilan `handle.zombie_report()` |
| `.drain_timeout(d)` | 2 s | borne du drain à la fin de vie ; au-delà, `drained: false` ; sans effet sans `.anti_zombie()` |
| `.request_state_ttl(d)` | 10 min | TTL des `requestState` MRTR (jeton expiré → `-32602`) |
| `.request_state_key(k)` | aléatoire par processus | clé HMAC partagée pour une flotte (clé vide → panique) |
| `.max_response_bytes(n)` | 16 Mio | réponse sérialisée au-delà → `-32603` ; borne le **réseau**, pas le tas |
| `.legacy(true)` | profil strict | mode de tolérance : appel nu accepté, `initialize`/`ping` servis |
| `.require_right(tool, right)` | aucun droit exigé | droit d'appeler cet outil — cumulable, droits **tous** exigés (ET), chaînes comparées exactement (aucun joker) ; inerte sans `.verifier` ; dès qu'un vérificateur est configuré, un outil **sans droit déclaré** est refusé (`-32023` `right_missing`) et disparaît de `tools/list` (échec fermé) |
| `.require_resource_right(uri, right)` | aucun droit exigé | droit pour lire cette ressource (`resources/read`) ou lister ce gabarit (`resources/templates/list`) — mêmes règles que `.require_right` ; catalogues ressources et gabarits indépendants |
| `.require_prompt_right(name, right)` | aucun droit exigé | droit pour rendre ce prompt (`prompts/get`) — mêmes règles que `.require_right` |
| `.verifier(verifier)` | aucun | active l'**autorisation** : établit l'identité de chaque requête — sans lui, ni droits, ni listes filtrées, ni confirmations (comportement inchangé) ; panique du vérificateur contenue = refus. Le transport HTTP intégré authentifie via `ServerCore::authenticate` (`401` + `WWW-Authenticate` en cas de refus, dispatch avec `AuthContext` en cas de succès) ; les hôtes à socket propre passent par `dispatch_verified` ou `authenticate` + `dispatch_authenticated` |
| `.resource_mime_type(uri_ou_gabarit, mime)` | aucun | déclare le `mimeType` statique publié dans `resources/list` ou `resources/templates/list` — ordre libre (avant ou après l'enregistrement) |
| `.resource_template_title(uri_template, title)` | aucun | déclare le `title` publié dans `resources/templates/list` — ordre libre |
| `.resource_template_handler(uri_template, nom, desc, f)` | aucun | enregistre un gabarit RFC 6570 simple (`{var}`) **et** son handler dynamique de lecture `resources/read` (priorité aux URI statiques exactes, droits `require_resource_right` appliqués) ; panique sur un opérateur complexe (`{+x}`, `{?q}`) |
| `.register(f)` | — | alias lisible de `.with_tool(f)` pour enchaîner les `register_<nom>` générés par `#[tool]`, `#[resource]` et `#[prompt]` |
| `.require_approval(tool)` | aucune confirmation | marque l'outil comme exigeant une **confirmation externe** : la politique est interrogée **après** les droits et **avant** le handler, avec les arguments exacts de l'appel ; sans `.approval_policy`, l'outil est **refusé** (échec fermé) ; inerte sans `.verifier` |
| `.approval_policy(policy)` | aucune | politique de confirmation des outils marqués `.require_approval` : `Approved` laisse exécuter le handler ; `Denied` / `Pending` → `403` / `-32023` sans rien exécuter ; panique de la politique traitée comme un refus |
| `.on_auth_decision(hook)` | aucun | hook d'**audit** des décisions d'autorisation — cumulable, appelés dans l'ordre d'enregistrement ; `AuthDecisionEvent` porte le verdict et le motif **précis** (`right_missing`, `approval_denied`…) que le réseau ne voit jamais ; panique du hook contenue |
| `bound.handle()` | — | `handle.shutdown()` rend `serve()` → `Ok(())`, connexions fermées entre deux requêtes |
| `GET /health` | inclus | `200 ok` sans JSON-RPC (`HEAD` accepté) |
| Corps · en-têtes | 1 Mio · 64 · 16 Kio/ligne | `413` · `431` · `431` ; corps lu par tranches de 64 Ko |
| Threads de handlers | 256 | actif dès que `tool_timeout` est configuré (c'est le défaut) ; au-delà : `rate_limited` pour un outil, `-32603` data `overloaded` sinon. Désactivé par `.no_tool_timeout()` |

## Annulation coopérative (`litemcp::cancel`)

La fermeture du flux SSE d'une requête est traitée comme une annulation (spec Streamable HTTP
`2026-07-28`, MUST). Le transport ne peut pas interrompre un fil : il lève le drapeau de la requête
à la première écriture SSE qui échoue, et le handler le consulte à ses points sûrs.

| Surface | Comportement |
|---|---|
| `cancel::is_cancelled()` | `true` dès que le client a fermé le flux de **cette** requête ; `false` hors flux SSE (client JSON, dispatch embarqué) |
| `cancel::token()` | Jeton `CancelToken` clonable, à passer aux fils dérivés d'un handler ; `None` hors flux SSE |
| Coût | une charge atomique par appel ; un seul `Arc<AtomicBool>` par requête SSE (aucune allocation sur le chemin JSON) |
| Limite | **coopératif** : un handler qui ne consulte pas le drapeau s'exécute jusqu'au bout — c'est `tool_timeout` (30 s) qui borne la réponse |
