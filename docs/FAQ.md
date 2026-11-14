# FAQ

Les questions qui ne tiennent pas en trois lignes dans le README.

[← Retour au README](../README.md)

## Questions fréquentes

**Par où commencer ?**
Par le [tutoriel pas à pas](https://deamondev888.github.io/litemcp-tuto/) — il construit un serveur MCP complet avec LiteMCP, du premier `#[tool]` aux schémas, ressources et prompts. L'[Intégration](../README.md#premier-serveur) du README suffit pour exposer vos premiers outils depuis un programme existant en dix lignes ; les [Concepts](../README.md#concepts) détaillent chaque surface ; la [doc du crate](https://docs.rs/litemcp) (docs.rs) référence aussi le tutoriel.

**Pourquoi un seul transport ?**
Parce qu'un serveur lancé comme sous-processus d'un client meurt avec lui et n'accepte qu'un seul pair — c'est le modèle du client, pas de la lib. LiteMCP fait le choix inverse : **votre programme devient un démon Streamable HTTP stateless**, bindé sur un socket TCP, que plusieurs agents peuvent joindre, qui survit aux clients et se déploie derrière nginx. Le MCP local en sous-processus est hors périmètre, par conception.

**Pourquoi un module de schémas intégré ?**
Une lib MCP sans schéma n'est qu'un routeur JSON : les clients ont besoin de l'`inputSchema` pour présenter vos outils, et vos handlers ont besoin d'arguments garantis. Le module `zod` couvre les deux — dérivation JSON Schema + validation runtime — sans dépendance de plus : il pèse moins que les bibliothèques de schémas génériques qu'on embarquerait sinon.

**Pourquoi pas de tokio ?**
Une lib MCP mono-outil n'a pas besoin d'un runtime async complet pour lire des requêtes et répondre du JSON. Le thread par connexion suffit jusqu'à des centaines de connexions (plafond par défaut : 256). Si votre application est déjà dans un runtime async, l'avez-vous simplement dans le même processus — LiteMCP reste un serveur réseau, appelable en HTTP.

**Pourquoi réécrire HTTP ?**
Le périmètre est volontairement minuscule : une route POST, `Content-Length`, keep-alive, un writer SSE (un événement de réponse, ou N `notifications/progress` puis la réponse). Pas de chunked entrant, pas de TLS, pas de compression. Ce choix est **tranché par les tests** : le bench maison contre le backend alternatif testé (tiny_http) montre la route maison ~1,85× plus rapide en keep-alive et 0 dépendance de plus (voir [Performances](../README.md#performances)) — et elle embarque son durcissement (timeouts, limites d'en-têtes, `Expect: 100-continue`).

**Pourquoi pas de WebSocket ?**
Choix architectural, pas un oubli : WS n'économiserait que le re-parsing des en-têtes HTTP (~1–2 % du trajet — JSON, syscalls et noyau réseau dominent), ne passe pas bien derrière les proxies d'hébergement, et traînerait sessions et reconnexions — les trois choses que le mode stateless évite. La spec 2026-07-28 est HTTP-only ; le keep-alive one-write de LiteMCP est déjà au plancher de TCP.

**Pourquoi pas de SSE longue durée ?**
Le mode stateless n'a aucun flux **permanent** : chaque POST est auto-suffisant, et un flux éventuel (`text/event-stream`, `notifications/progress`) reste **request-scoped** — il s'ouvre pour la durée du handler et se referme avec l'événement de réponse. C'est ce choix qui immunise le transport contre le tamponnage des proxies de VPS et le rend trivial à déployer derrière nginx — voir [Hébergement VPS & proxies](DEPLOY.md). Un flux permanent (abonnements, notifications hors requête) exigerait des sessions : hors périmètre, réévalué si un besoin concret l'exige.

**Comment sont choisies les versions ?**
Pacement volontaire : toute modification atterrit d'abord dans la section `[Unreleased]` du [CHANGELOG](../CHANGELOG.md) ; une version n'est coupée que pour un lot cohérent (fonction, correctif, doc majeure). Le `0.x` court jusqu'à la conformance officielle MCP verte ; la mention « 1.0 » attend ce jalon.
