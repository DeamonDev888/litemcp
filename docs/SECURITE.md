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

Le **TLS**, lui, n'est pas implémenté (choix architectural : voir le comparatif ViteMCP, « OAuth 2.1 intégré » est écarté volontairement) — il vit à la périphérie, exactement comme pour tout service interne d'un SI. Le durcissement intégré couvre le protocole ; l'identité et le chiffrement sont à votre charge :

- **Liaison** : bind `127.0.0.1:23080` (comme dans tous les exemples) si le serveur ne parle qu'à des processus locaux ; sinon, exposez derrière un **reverse proxy** qui apporte TLS, l'authentification forte (OIDC/SAML, OAuth 2.1 — l'OIDC intégré au serveur reste écarté volontairement : c'est le métier du gateway) et le rate-limiting **par identité/IP** — pour l'auth token « quick fix » ou un rate-limit maison, le verrou `.on_request` suffit en process (derrière un proxy, `ctx.peer` est l'adresse du proxy : ne croyez `X-Forwarded-For` qu'après avoir vérifié le pair). Le plafond de connexions de LiteMCP est **global au serveur** (256 par défaut) : 256 clients lents peuvent l'occuper et renvoyer `503` aux autres, c'est à ça que sert le quota par IP du proxy (le TTL de connexion de 5 min borne par ailleurs la durée d'occupation d'un slot) ;
- **Navigateurs** : garde DNS-rebinding intégrée — toute requête portant un `Origin` non listé dans `.allow_origins` est refusée (`403`), sauf l'exception loopback par défaut (origine `http://` strictement égale au `Host` et au listener sur un bind loopback : client même-origine) ; `Origin: null` et `Sec-Fetch-Site: cross-site` restent toujours refusés, et les préflights CORS échouent par défaut (`OPTIONS` → `405`) ;
- **Handlers bloquants** : par défaut il n'y a pas de timeout d'exécution — un handler qui bloque retient son thread et son slot de connexion. `.tool_timeout(duration)` borne la **réponse** (résultat `isError` pour un outil, `-32603` avec data `timeout` pour une ressource/prompt passé le délai) mais ne peut pas tuer le thread du handler (Rust n'a pas d'annulation de thread) : il termine en arrière-plan et son résultat est ignoré. Pour des effets de bord annulables, encadrez vos appels lents vous-même. La **déconnexion du client** pendant l'exécution ne change rien non plus : le flux de réponse n'est ouvert qu'après calcul complet, la fermeture n'est détectée qu'à l'écriture finale (une seule écriture — aucune écriture ultérieure) et **aucun jeton d'annulation n'est exposé aux handlers** ;
- **Validation** : privilégiez `tool_typed`/`tool_parse` (schéma appliqué **avant** votre code) ; `tool` brut vous laisse la validation des arguments — c'est là que naissent les bugs d'injection côté handler, pas dans le transport. Le schéma publié est **exécuteur** : ce qu'il annonce est exactement ce que la validation applique.

Le durcissement transport est intégré et vérifié par la suite de tests : limites (corps 1 Mio, 64 en-têtes, lignes 16 Kio, 256 connexions, TTL 5 min), timeouts (lecture 30 s, deadline 60 s — slowloris coupé), anti request-smuggling (`Transfer-Encoding` ou `Content-Length` dupliqué → `400` sur **toutes** les routes), parsing strict (ligne d'en-tête sans `:` ou espace avant `:` → `400`, séparateurs SP uniquement, CRLF strict, version sensible à la casse, `Accept`/`Expect` comparés par jeton exact), `Host` requis, obs-fold refusé, en-têtes à valeur unique **dupliqués** (`Origin`, `Content-Type`, `Mcp-Method`, `Mcp-Name`) refusés (`400` — le verdict ne doit pas dépendre de l'ordre), `Content-Type` vérifié (`415`), paniques de handlers contenues (`-32603` **avec l'`id` de la requête**, le serveur continue). Le transport a fait l'objet d'un audit complet (statique + dynamique sur serveur réel) ; les correctifs qui en découlent sont consignés dans le [CHANGELOG](../CHANGELOG.md).

## Ce que le transport ne fait pas

- **Pas de CORS.** `.allow_origins` est un **filtre** (garde anti-DNS-rebinding), pas une autorisation :
  aucune réponse ne porte `Access-Control-Allow-Origin`, et un préflight `OPTIONS` rend `405`. Un client
  navigateur cross-origin ne peut donc rien lire, même si son origine est listée (mesuré : POST `200`
  sans en-tête CORS).
- **`Host` vérifié en présence et en syntaxe, jamais confronté au listener.** La garde DNS-rebinding
  repose sur `Origin`/`Sec-Fetch-Site` : un client qui n'envoie pas d'`Origin` passe (mesuré, `200`).
- **`chunked` entrant refusé.** Tout `Transfer-Encoding` rend `400` : un proxy en amont doit
  dé-chunker, ce qui supprime toute divergence de découpage avec lui.
- **Corps non consommé sur les réponses anticipées.** 404/405/415 répondent avant `read_body` ; la
  sûreté vient de la **fermeture systématique** de la connexion après une réponse d'erreur, pas d'une
  lecture du corps.
- **Pas d'annulation.** La fermeture d'un flux SSE n'interrompt pas le handler (aucun jeton exposé),
  et `tool_timeout` est illimité par défaut.
