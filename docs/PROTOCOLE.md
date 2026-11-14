# Protocole MCP `2026-07-28` — détail

Le README résume le protocole en une dizaine de lignes ; cette page donne le tableau complet des
comportements (codes, en-têtes, erreurs) et l'enveloppe obligatoire du profil strict.

[← Retour au README](../README.md)

## Tableau complet des comportements

**Spec MCP `2026-07-28`** (stateless, sans handshake) — comportements servis, écarts connus en fin de page :

| Comportement | Réponse |
|---|---|
| `POST /mcp` valide | `200 application/json` — réponse JSON-RPC |
| `POST` avec `Accept: text/event-stream` seul | `200 text/event-stream` — un événement `data:` (SSE request-scoped) + en-têtes anti-tampon ; avec `_meta.progressToken`, les `notifications/progress` précèdent l'événement de réponse |
| Notification de protocole (`notifications/*`, sans `id`) | `202 Accepted` sans corps (acceptée et ignorée) |
| Notification d'action (`tools/call`, `resources/read`, `prompts/get`, `completion/complete` sans `id`) | `-32600` **« notification refusée »** — le mode stateless ne peut pas livrer le résultat : le handler **ne tourne pas** et le client le sait |
| `server/discover` (SEP-2575) | `200` — versions supportées, capacités, `serverInfo` dans `_meta`, et les indices de cache `ttlMs`/`cacheScope` comme `tools/list` (page *Caching*, SEP-2549) |
| Version de protocole dans `_meta` | `2026-07-28` acceptée · version inconnue : `-32022` (`UnsupportedProtocolVersionError`, `data.supported`) · **absence refusée** en profil strict (`-32602` : la clé est obligatoire) ; tolérée seulement avec `.legacy(true)` |
| `resultType` (SEP-2322) | présent dans **chaque** résultat (`"complete"`) |
| `serverInfo` (SEP-2575) | le serveur s'identifie sous `_meta` de **chaque** résultat |
| `tools/list` — `ttlMs` / `cacheScope` (SEP-2549) | `60000` / `"private"` |
| `outputSchema` | publié dans `tools/list` **et exécuteur** : une sortie `structuredContent` non conforme devient un résultat `isError` (`.output_schema("outil", schéma)`) |
| `resources/list` / `resources/read` | `200` — catalogue et lecture, `ttlMs`/`cacheScope` comme `tools/list` (SEP-2549) ; uri inconnue/manquante : `-32602` ; handler en échec : `-32603` (data `handler_failed`) ; lecture **binaire** : `contents[].blob` (base64 — jamais un texte qui simule le binaire) |
| `resources/templates/list` | `200` — catalogue des gabarits (URI templates RFC 6570), `ttlMs`/`cacheScope` comme `resources/list` ; servi même sans gabarit (liste vide) |
| `prompts/list` / `prompts/get` | `200` — catalogue et rendu (messages `user`/`assistant`, `content` = bloc `ContentBlock` complet : texte, image, audio, `resource_link`, ressource embarquée) ; nom inconnu : `-32602` ; `arguments` non-objet : `-32602` |
| `completion/complete` | `200` — suggestions (`completion.values`, ≤ 100 : au-delà tronquées + `hasMore: true`) ; fournisseur absent ou params invalides (`ref`, `argument`) : `-32602` ; échec du fournisseur : `-32603` (data `handler_failed`) ; capacité `completions` annoncée seulement quand un fournisseur existe |
| Résultat `input_required` (MRTR) | `200` — `{ resultType: "input_required", inputRequests?, requestState, _meta }` ; rejeu (id **différent**) avec `inputResponses` + écho du jeton ; état signé HMAC-SHA256 (TTL 10 min, liaison méthode/paramètres saillants) : altéré/expiré/malformé/autre clé/requête modifiée → `-32602` avant tout handler ; requête embarquée dont la capacité client n'est pas déclarée → `-32021` ; requête embarquée invalide ou état > 8 Kio → `-32603` (bogue de handler, rien n'est émis) ; `inputResponses` sans `requestState` → `-32602` ; champs MRTR ignorés sur toute autre méthode |
| `notifications/progress` | `200 text/event-stream` — flux ouvert **avant** le dispatch quand `_meta.progressToken` est fourni et que le client accepte SSE : chaque `progress::report` écrit une notification (jeton de la requête, `progress`/`total`/`message`), l'événement final clôt le flux ; sans jeton ou sans SSE, rapport ignoré et réponse JSON unique ; aucune notification après la réponse |
| Capacités `tools` / `resources` / `prompts` | **toujours** annoncées dans `server/discover` (et dans `initialize`, en profil legacy) — les méthodes correspondantes sont servies même à registre vide (aucune sous-capacité non servie déclarée) |
| `Mcp-Method` / `Mcp-Name` (SEP-2243) | acceptés ; **incohérents avec le corps** → `-32020` (`HeaderMismatch`) |
| `initialize` / `ping` | **non servis** en profil strict (`404` + `-32601 Unknown method`) ; servis avec `.legacy(true)`, où `initialize` **négocie** `2025-11-25` (version demandée si elle est servie, sinon plus récente révision legacy servie) |
| Sonde `GET /health` | `200` « ok » (sans JSON-RPC ; probes k8s/systemd) · `HEAD /health` : mêmes en-têtes, sans corps · **un corps annoncé est refusé** (`400`, la sonde n'en accepte aucun) · autre méthode : `405` (`Allow: GET, HEAD`) |
| `GET` sur `/mcp` | `405 Method Not Allowed` (pas de listener SSE permanent en stateless) |
| Version HTTP ≠ `HTTP/1.0`/`HTTP/1.1` exactes (sensible à la casse) · ligne de requête malformée | `505` · `400` |
| HTTP/1.1 sans `Host` (ou `Host` dupliqué) · ligne obs-fold · en-tête sans `:` · espace avant `:` · HTAB dans la ligne de requête · fin de ligne LF seul | `400` |
| Route inconnue | `404` · `Content-Length` absent (POST) : `411` · malformé : `400` · corps > 1 Mio : `413` |
| `Content-Type` ≠ `application/json` | `415 Unsupported Media Type` (absent : accepté) |
| `Transfer-Encoding` présent (toute route) · `Content-Length` dupliqué | `400` (anti request-smuggling) |
| `Content-Type` / `Mcp-Method` / `Mcp-Name` dupliqués | `400` (en-têtes à valeur unique — un proxy combine ou retient la première valeur : le verdict ne doit pas dépendre de l'ordre) |
| En-tête `Origin` non autorisé | `403` (garde DNS-rebinding ; exception par défaut : origine loopback strictement égale au `Host` et au listener sur un bind loopback ; CLI sans `Origin` : libre) |
| Requête refusée par un verrou `on_request` | statut du rejet (`401`/`403`/`429`… — hors `400`–`599` corrigé à `400`), message en corps JSON-RPC, en-têtes supplémentaires **assainis** (nom hors jeton ou valeur à CRLF écartés), connexion fermée · hook en panique : `500` générique, le serveur continue · `/health` et les routes inconnues ne passent jamais par le verrou |
| Toute erreur JSON-RPC | l'`id` de la requête est **repris** dans la réponse (JSON-RPC 2.0 §5) |

## Enveloppe obligatoire du profil strict (par défaut)

Le profil par défaut est `ProtocolProfile::Strict2026_07_28` : **chaque requête** — pas seulement
la première — porte trois en-têtes et deux clés `_meta`.

```bash
curl -s -i http://127.0.0.1:23080/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json' \
  -H 'MCP-Protocol-Version: 2026-07-28' \
  -H 'Mcp-Method: tools/call' \
  -H 'Mcp-Name: add' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"name":"add","arguments":{"a":5,"b":7}}}'
```

| Élément | Exigence (profil strict) | Si absent / faux |
|---|---|---|
| `MCP-Protocol-Version` | obligatoire, égale à `_meta.…protocolVersion` | `400` + `-32020` |
| `Mcp-Method` | obligatoire, égale au `method` du corps | `400` + `-32020` |
| `Mcp-Name` | obligatoire pour `tools/call`, `resources/read`, `prompts/get` | `400` + `-32020` |
| `_meta.io.modelcontextprotocol/protocolVersion` | obligatoire, valeur supportée | `400` + `-32602` (absente) / `-32022` (inconnue, `data.supported` fourni) |
| `_meta.io.modelcontextprotocol/clientCapabilities` | obligatoire, objet JSON | `400` + `-32602` |

`Mcp-Param-*` est accepté et vérifié quand il est présent. Le sentinel `=?base64?…?=` est décodé
côté `Mcp-Name`.

### Profil legacy (`.legacy(true)`)

`legacy(true)` fait du serveur un serveur **dual-era** : l'appel nu — sans en-têtes ni `_meta` — est
accepté, `initialize` et `ping` sont servis, et `initialize` **négocie** la révision legacy
`2025-11-25`. Règle appliquée (révision `2025-11-25`, Lifecycle, « Version Negotiation ») : la
version demandée est renvoyée si le profil la sert, sinon la plus récente révision legacy servie
(`protocol::LEGACY_SUPPORTED_VERSIONS`, aujourd'hui `["2025-11-25"]`). Jamais `2026-07-28` : cette
révision n'a pas de handshake, une telle réponse serait fausse.

Les résultats du profil legacy gardent la forme `2026-07-28` (`resultType`, `ttlMs`, `cacheScope`,
`_meta.serverInfo`, et `server/discover` en plus des méthodes `2025-11-25`). Un client `2025-11-25`
les accepte : le schéma officiel de cette révision ne ferme pas les objets (`additionalProperties`
absent de `InitializeResult`, `ListToolsResult`, …) et la base protocol précise que « The `result`
**MAY** follow any JSON object structure » ; les trois champs obligatoires d'`InitializeResult`
(`protocolVersion`, `capabilities`, `serverInfo`) sont présents.

`.legacy(true)` reste un **mode de tolérance** : `_meta` et les en-têtes ne sont pas exigés,
`server/discover` continue d'annoncer les seules révisions modernes (`2026-07-28`), et aucune
session `Mcp-Session-Id` n'est créée (la révision `2025-11-25` l'autorise : « MAY assign »).

## Écarts connus par rapport à la révision `2026-07-28`

| Écart | Effet |
|---|---|
| fermeture d'un flux SSE non traitée comme annulation | le handler n'est pas interrompu, aucun jeton d'annulation n'est exposé |
| erreur `-32601` sur `initialize` sans `data.supported` | la spec le recommande (« SHOULD »), pas d'obligation |
| `structuredContent` d'un résultat `isError` non confronté à `outputSchema` | le succès est validé, l'échec non |
| extension `tasks` non implémentée | 9 scénarios du harness non scorés, hors périmètre du cœur |

Conformité mesurée le 2026-11-13 : harness officiel `@modelcontextprotocol/conformance`
`0.2.0-alpha.12`, requis `2026-07-28` → **37/37 scénarios scorés verts** (50/50 scénarios
exécutés ; les 30 checks en échec appartiennent aux 9 scénarios `tasks-*`, non scorés).
