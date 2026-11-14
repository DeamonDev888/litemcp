# Hébergement, proxies et tamponnage SSE

LiteMCP sert du Streamable HTTP stateless : un POST, une réponse, clôture. Derrière nginx,
Cloudflare ou un hébergeur mutualisé, c'est ce choix qui évite le tamponnage des flux SSE — et
qui fixe les limites du déploiement.

[← Retour au README](../README.md)

## Hébergement VPS & proxies (le piège du tamponnage SSE)

Derrière beaucoup d'hébergements, un proxy (nginx, Cloudflare…) met les réponses en tampon et n'envoie les flux SSE que lorsqu'il a accumulé ~8 Ko — les événements « s'empilent ». Deux lignes de défense :

- **LiteMCP est structuré pour ça** : son SSE est *request-scoped* — un événement, `Connection: close`, `Content-Length` explicite. Un proxy tamponne les flux *en cours* ; une réponse terminée est toujours poussée. Le symptôme ne peut pas se produire ;
- **les réponses SSE portent les en-têtes anti-tampon** `X-Accel-Buffering: no` (honoré par nginx **par réponse**, même avec `proxy_buffering on` global) et `Cache-Control: no-cache, no-transform`.

Si vous contrôlez le proxy, la configuration propre :

```nginx
location /mcp {
    proxy_pass http://127.0.0.1:23080;
    proxy_http_version 1.1;
    proxy_buffering off;        # optionnel : X-Accel-Buffering: no suffit
    proxy_read_timeout 3600s;
}
```

**Repli officiel si l'hébergeur est hostile au SSE** : le client négocie `Accept: application/json` et reçoit une réponse JSON sèche — aucun flux, rien à tamponner. Les clients MCP gèrent cette négociation automatiquement.

**Choix architectural : pas de SSE longue durée.** Le mode stateless ne tient aucun flux ouvert **entre deux requêtes** : un POST, une réponse, clôture. Une requête qui porte `_meta.progressToken` et accepte `text/event-stream` ouvre un flux **request-scoped** — les `notifications/progress` émises par le handler (`litemcp::progress::report`) précèdent l'événement de réponse, puis la connexion est fermée ; sans jeton, la réponse reste un JSON unique. C'est ce choix qui immunise le transport contre le tamponnage des proxies et le rend trivial à déployer derrière nginx. Un flux permanent (abonnements, notifications hors requête) exigerait des sessions — hors périmètre, réévalué seulement si un besoin concret l'exige. **Conséquence assumée : pas d'annulation MCP** — la déconnexion d'un client pendant l'exécution n'est détectée qu'à l'écriture (événement de réponse, ou première notification de progression qui échoue) et aucun jeton d'annulation n'est exposé aux handlers ; l'évolution (jeton coopératif + détection pendant l'exécution) est chiffrée dans le CHANGELOG « Prévu ».
