# Sécurité : modèle de menace et durcissement du transport

Le README résume la posture ; cette page détaille ce que le transport refuse et pourquoi, avec
les limites connues.

[← Retour au README](../README.md)

## Sécurité

**Modèle de menace assumé : LiteMCP est une bibliothèque réseau, pas un service exposé.** Quiconque peut joindre le socket peut appeler vos outils — sauf si vous posez un **verrou de requête** [`on_request`](../README.md#configuration-et-valeurs-par-défaut) : un verdict par requête, avant même la lecture du corps (auth par token, rate-limit par source ou par clé), rejet `401`/`403`/`429` avec en-têtes libres (`WWW-Authenticate`, `Retry-After`) :

```rust
use litemcp::prelude::*;

// jeton « quick fix » — l'OIDC/SAML reste au gateway

ServerBuilder::new("calc", "1.0.0")
    .on_request(|ctx| {
        match ctx.header("Authorization").and_then(|v| v.strip_prefix("Bearer ")) {
            Some(t) if t == "s3cret" => Ok(()),
            _ => Err(RequestRejection::unauthorized("missing or invalid bearer token")
                .with_header("WWW-Authenticate", "Bearer")),
        }
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

Le **TLS 1.3** est disponible derrière la feature `tls` (`.tls(TlsConfig)`, certificat auto-signé en mémoire, empreinte épinglable côté client) ; sans elle, le serveur parle en clair et le chiffrement vit à la périphérie, exactement comme pour tout service interne d'un SI. L'OIDC/OAuth 2.1 intégré reste écarté volontairement (voir le comparatif ViteMCP) : c'est le métier du gateway. Le durcissement intégré couvre le protocole et l'identité signée ; l'autorisation d'entreprise reste à votre charge :

- **Liaison** : bind `127.0.0.1:23080` (comme dans tous les exemples) si le serveur ne parle qu'à des processus locaux ; sinon, exposez derrière un **reverse proxy** qui apporte TLS, l'authentification forte (OIDC/SAML, OAuth 2.1 — l'OIDC intégré au serveur reste écarté volontairement : c'est le métier du gateway) et le rate-limiting **par identité/IP** — pour l'auth token « quick fix » ou un rate-limit maison, le verrou `.on_request` suffit en process (derrière un proxy, `ctx.peer` est l'adresse du proxy : ne croyez `X-Forwarded-For` qu'après avoir vérifié le pair). Le plafond de connexions de LiteMCP est **global au serveur** (256 par défaut) : 256 clients lents peuvent l'occuper et renvoyer `503` aux autres, c'est à ça que sert le quota par IP du proxy (le TTL de connexion de 5 min borne par ailleurs la durée d'occupation d'un slot) ;
- **Navigateurs** : garde DNS-rebinding intégrée — toute requête portant un `Origin` non listé dans `.allow_origins` est refusée (`403`), sauf l'exception loopback par défaut (origine `http://` strictement égale au `Host` et au listener sur un bind loopback : client même-origine) ; `Origin: null` et `Sec-Fetch-Site: cross-site` restent toujours refusés. L'en-tête `Host`, lui, est confronté au **listener** (`.allowed_hosts`) : par défaut seules les autorités locales passent (adresse effective du listener, littéral de bouclage, nom court non qualifié), tout nom pleinement qualifié ou littéral d'adresse étranger rend `421 Misdirected Request` **même sans `Origin`** — la garde ne dépend donc plus du navigateur. Une origine autorisée reçoit les en-têtes **CORS** (préflight `OPTIONS` → `204`, `Access-Control-Allow-Origin` + `Vary: Origin` sur les POST) : un client navigateur cross-origin peut lire le résultat, ce qui n'était pas le cas avant L1 ;
- **Handlers bloquants** : `tool_timeout` vaut **30 s par défaut** (`DEFAULT_TOOL_TIMEOUT`) — un handler plus lent rend un `isError` (`ErrorCode::Timeout`) pour un outil, `-32603` data `timeout` pour une ressource/prompt, et `.no_tool_timeout()` rétablit l'illimité (en désactivant le plafond de threads de handlers). Le délai borne la **réponse**, pas l'exécution : Rust n'a pas d'annulation de thread, le handler termine en arrière-plan et son résultat est ignoré — pour des effets de bord annulables, consultez `litemcp::cancel::is_cancelled()` à vos points sûrs. La **déconnexion du client** est détectée sur un flux SSE : la première écriture d'événement qui échoue (ou l'écriture de l'événement final) lève le drapeau **coopératif** de la requête (`cancel::is_cancelled()`, jeton clonable via `cancel::token()`), mesuré à 40 ms sur un handler qui sonde à chaque itération. Rien n'est interrompu de force : un handler qui ne consulte jamais le drapeau s'exécute jusqu'au bout, borné par `tool_timeout` ;
- **Validation** : privilégiez `tool_typed`/`tool_parse` (schéma appliqué **avant** votre code) ; `tool` brut vous laisse la validation des arguments — c'est là que naissent les bugs d'injection côté handler, pas dans le transport. Le schéma publié est **exécuteur** : ce qu'il annonce est exactement ce que la validation applique.

Le durcissement transport est intégré et vérifié par la suite de tests : limites (corps 1 Mio, 64 en-têtes, lignes 16 Kio, 256 connexions, TTL 5 min), timeouts (lecture 30 s, deadline 60 s — slowloris coupé), anti request-smuggling (`Transfer-Encoding` ou `Content-Length` dupliqué → `400` sur **toutes** les routes), parsing strict (ligne d'en-tête sans `:` ou espace avant `:` → `400`, séparateurs SP uniquement, CRLF strict, version sensible à la casse, `Accept`/`Expect` comparés par jeton exact), `Host` requis, obs-fold refusé, en-têtes à valeur unique **dupliqués** (`Origin`, `Content-Type`, `Mcp-Method`, `Mcp-Name`) refusés (`400` — le verdict ne doit pas dépendre de l'ordre), `Content-Type` vérifié (`415`), paniques de handlers contenues (`-32603` **avec l'`id` de la requête**, le serveur continue). Le transport a fait l'objet d'un audit complet (statique + dynamique sur serveur réel) ; les correctifs qui en découlent sont consignés dans le [CHANGELOG](../CHANGELOG.md).

## Ce que le transport ne fait pas

- **Pas de `*` CORS.** `.allow_origins` reste un **filtre** (garde anti-DNS-rebinding) *avant* d'être une
  autorisation : l'origine est echoée une à une dans `Access-Control-Allow-Origin`, jamais remplacée par
  `*`, et une origine absente de la liste rend `403` sans aucun en-tête CORS. Le préflight `OPTIONS`
  n'est ouvert que pour une origine listée (`204`) ; sans `Origin`, il rend `403`.
- **En-têtes CORS sur les réponses `POST` servies, pas sur les `4xx` de transport.** Un `400`/`413`/`415`
  (framing, taille, type de média) part avant le verdict applicatif et ne porte pas
  `Access-Control-Allow-Origin` : côté navigateur, il apparaît comme une erreur CORS et non comme le
  statut exact. Les résultats JSON-RPC (succès **et** erreurs applicatives) sont, eux, toujours lisibles.
- **La règle `Host` porte sur le nom, pas sur le port.** Par défaut, `Host: 127.0.0.1:9999` ou
  `Host: mon-service:80` sont acceptés dès lors que le nom désigne la machine locale : c'est le nom qui
  décide de la résolution DNS, donc de l'attaque. Tout nom **pleinement qualifié** (`mcp.exemple.com`)
  doit être déclaré (`.allowed_hosts`) — y compris derrière un reverse proxy.
- **`503` de saturation en `HTTP/1.1`, quel que soit le client.** Le refus part **avant** toute lecture :
  la version de la requête n'est pas connue au moment d'écrire la ligne de statut. Toutes les autres
  réponses d'erreur propagent la version reçue (`HTTP/1.0` → `HTTP/1.0 400`).
- **`chunked` entrant refusé.** Tout `Transfer-Encoding` rend `400` : un proxy en amont doit
  dé-chunker, ce qui supprime toute divergence de découpage avec lui.
- **Corps non consommé sur les réponses anticipées.** 404/405/415 répondent avant `read_body` ; la
  sûreté vient de la **fermeture systématique** de la connexion après une réponse d'erreur, pas d'une
  lecture du corps.
- **Écriture bornée, pas infinie.** L'échéance globale d'écriture (`read_timeout × 2`, bornée à
  1 s..=60 s — 60 s par défaut ; `ServerBuilder::write_deadline` la remplace, une valeur sous 1 s
  étant relevée au minimum) borne l'envoi **complet** de la réponse, et elle est désormais appliquée
  à deux niveaux : le reliquat est découpé en tranches de **256 Kio** (un seul appel ne peut pas
  absorber tout le budget — sur Windows un `send` de plusieurs Mio rendait la main après avoir tout
  accepté, l'échéance n'était donc jamais évaluée) et **chaque** tentative est bornée par le temps
  restant (`SO_SNDTIMEO = min(WRITE_TIMEOUT, restant)`, session TLS comprise). Mesuré sur Windows 11
  en clair, budget 1 s, réponse de 8,4 Mio : client muet → slot de connexion rendu à **1,62 s** ;
  lecteur à 17 Kio/s → **1,83 s** ; lecteur à 4 Mio/s → coupure en plein corps à **2,18 s** (3,9 Mio
  livrés sur 8,4, puis fin de flux). Sans ce mécanisme, aucune des trois situations n'était coupée
  (mesuré : 20 s de transfert, connexion toujours ouverte côté client, et un `send` de plusieurs Mio
  rendant la main après avoir tout accepté).
  Trois limites, mesurées elles aussi : (1) les octets **déjà remis au noyau** continuent d'être
  délivrés au rythme du client — la coupure rend le slot mais ne rappelle pas ces octets, si bien
  qu'un lecteur à 17 Kio/s ne voit la fin de flux qu'après avoir drainé ce qui était en file
  (~300 Kio, soit ≈ 18 s) et qu'un lecteur à 4 o/s ne la voit pas du tout ; (2) la coupure est une
  fin de flux (`shutdown(Write)`), **pas un RST** : `SO_LINGER` n'est pas exposé par la bibliothèque
  standard, donc les octets en file côté noyau restent livrables ; (3) le plafond est **dur** et il
  tronque : une réponse de 16 Mio exige ≈ 2,2 Mbit/s soutenus pour tenir dans les 60 s par défaut, un
  client plus lent est coupé en plein corps avec un `Content-Length` jamais honoré — rupture assumée
  pour un client légitime lent (voir `CHANGELOG`). Linux, macOS et TLS ne sont pas mesurés ici.
- **Pas d'annulation préemptive.** La fermeture d'un flux SSE lève un drapeau **coopératif**
  (`litemcp::cancel::is_cancelled()`, jeton clonable via `cancel::token()`) que le handler consulte
  s'il le veut — rien ne peut tuer un thread en Rust, donc un handler qui ne le consulte pas
  s'exécute jusqu'au bout. `tool_timeout` vaut **30 s par défaut** (`.no_tool_timeout()` pour
  l'illimité) et borne la réponse, pas les effets de bord déjà engagés. `notifications/cancelled`
  n'est pas traitée, et hors flux SSE aucun drapeau n'est levé.
