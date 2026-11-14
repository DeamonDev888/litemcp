# Configuration — toutes les méthodes et leurs valeurs par défaut

Toutes les méthodes du builder sont optionnelles et posent le défaut ci-dessous :
`ServerBuilder::new(nom, version)` + un outil + un bind suffisent à servir. L'ordre d'appel est
libre, et `annotate`/`output_schema` fonctionnent avant **ou** après l'enregistrement de l'outil.
Valeurs vérifiées dans le code (`src/lib.rs`, `src/http.rs`, `src/server.rs`, `src/request_state.rs`).

[← Retour au README](../README.md)

## Tableau

| Méthode / limite | Défaut | Rôle |
|---|---|---|
| `.route(path)` | `/mcp` | chemin unique, validé au bind (commence par `/`, sans query, sans masquer `/health`) |
| `.bind_default()` | `23080..=23099` | premier port libre sur `127.0.0.1`, repli éphémère ; `.bind(addr)` pour un port explicite |
| `.allow_origins(&[…])` | vide | origines tolérées — **filtre anti-DNS-rebinding, pas du CORS** ([Limites](../README.md#limites)) |
| `.read_timeout(d)` | 30 s | timeout de lecture par opération : un client trop lent est déconnecté sans réponse |
| `.max_connections(n)` | 256 / serveur | au-delà : `503` immédiat, aucun thread ouvert |
| `.connection_ttl(d)` | 5 min | durée de vie d'une connexion keep-alive ; `0` désactive |
| `.tool_timeout(d)` | illimité | délai des handlers → `isError` (`ErrorCode::Timeout`) pour un outil, `-32603` data `timeout` sinon ; le thread n'est pas tué |
| `.on_call(hook)` | aucun | observabilité après chaque message (`CallEvent { method, name, duration, status }`), cumulable |
| `.on_request(hook)` | aucun | verrou par requête **avant la lecture du corps** : `401`/`403`/`429` + en-têtes libres |
| `.announce()` | désactivée | annonce JSON dans `%TEMP%/litemcp-registry/<nom>.json` (`addr`, `instance`, PID), retirée à la fin de vie |
| `.anti_zombie()` | désactivée | sweep du registre au bind (retrait **seulement** sur preuve d'identité) + drain borné à l'arrêt, bilan `handle.zombie_report()` |
| `.drain_timeout(d)` | 2 s | borne du drain à la fin de vie ; au-delà, `drained: false` ; sans effet sans `.anti_zombie()` |
| `.request_state_ttl(d)` | 10 min | TTL des `requestState` MRTR (jeton expiré → `-32602`) |
| `.request_state_key(k)` | aléatoire par processus | clé HMAC partagée pour une flotte (clé vide → panique) |
| `.max_response_bytes(n)` | 16 Mio | réponse sérialisée au-delà → `-32603` ; borne le **réseau**, pas le tas |
| `.legacy(true)` | profil strict | mode de tolérance : appel nu accepté, `initialize`/`ping` servis |
| `bound.handle()` | — | `handle.shutdown()` rend `serve()` → `Ok(())`, connexions fermées entre deux requêtes |
| `GET /health` | inclus | `200 ok` sans JSON-RPC (`HEAD` accepté) |
| Corps · en-têtes | 1 Mio · 64 · 16 Kio/ligne | `413` · `431` · `431` ; corps lu par tranches de 64 Ko |
| Threads de handlers | 256 | au-delà : `rate_limited` pour un outil, `-32603` data `overloaded` sinon |
