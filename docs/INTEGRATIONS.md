# MCP Control — client, gateway et gestionnaire

Le projet sibling qui consomme LiteMCP. Le README n'en garde que l'essentiel ; cette page décrit
les trois rôles, la reconnaissance par fingerprint et le contrat d'annonce (`.announce()`),
écrit par le serveur.

[← Retour au README](../README.md)

## MCP Control — trois rôles

Le [**MCP Control**](https://github.com/DeamonDev888/mcp-control) est le pendant côté exploitation de LiteMCP — un seul outil, trois rôles :

- **Client** : superviseur de serveurs MCP en Rust natif — détection multi-machines par scan et SSH, état, outils, latence ;
- **Gestionnaire** : une UI qui démarre, arrête, relance et installe (boutique) vos serveurs, avec démarrage automatique ;
- **Gateway** : grâce au module [`Gateway`](../README.md#fonctionnalités) de cette lib, il agrège tous les serveurs détectés derrière **un seul endpoint MCP** — côté client (Claude, Cursor, vos agents), **un seul serveur à déclarer**, et toute la surface exposée dans l'UI.

Il reconnaît les serveurs LiteMCP **par fingerprint** — `initialize` répondu avec la révision legacy négociée (`2025-11-25`) + sonde `GET /health` « ok » — et affiche alors un badge dédié.

**Intégration native (opt-in)** — `.announce()` sur le builder : au bind réussi, le serveur écrit une annonce JSON dans le dossier temporaire partagé (`%TEMP%/litemcp-registry/<nom>.json`, sinon `$TMPDIR` ou `/tmp`) — `{ name, version, addr, port, instance, pid, exe, args, cwd, ts }` : l'**adresse effective connectable** du listener (un bind multi-interface est publié par son bouclage — `0.0.0.0` → `127.0.0.1`, `::` → `::1` ; un bind spécifique — IPv6, IP précise — est publié tel quel), une **identité d'instance vérifiable** (`instance`, également exposée par `GET /health` en en-tête `X-Instance-Id`), le **port effectif**, le PID et la ligne de commande. MCP Control peut alors démarrer, arrêter et relancer vos serveurs nativement, sans scan de ports ni saisie manuelle. L'annonce est **retirée à la fin de vie du serveur** — arrêt gracieux ou simple `drop` après `bind` sans `serve()` — et seulement si le couple `pid`/`port` **et l'identité d'instance** (`instance`) du fichier sont ceux de l'instance appelante — un redémarrage dans le même process sur le même port publie une autre identité, dont l'annonce survit au retrait de l'ancienne ; écriture et retrait sont **sérialisés par un verrou inter-processus par nom** (dossier créé exclusivement) : une annonce plus récente ne peut plus être détruite par le retrait d'une instance plus ancienne. Le sweep anti-zombie juge l'annonce sur **preuve d'identité** : l'adresse publiée répond avec une identité différente → usurpation prouvée → retrait ; un port sans réponse ou une identité invérifiable → **conservée** (un refus TCP n'est pas une preuve de mort — un serveur vivant peut refuser, et un serveur lié à IPv6 ou à une IP précise ne répond pas sur `127.0.0.1`). Durcissements intégrés : nom de fichier sûr (`:`/`/`/`\`/`..` refusés — sous Windows `C:x` remplacerait la base du chemin et `nom:flux` écrirait un flux NTFS alternatif), écriture **atomique** (fichier temporaire créé exclusivement puis `rename` — jamais de JSON partiel lu, jamais d'écrasement à travers un symlink planté dans un dossier partagé), permissions `0600` sous Unix. Toute défaillance est silencieuse : l'annonce est une commodité d'intégration, jamais une condition de fonctionnement du serveur — et zéro coût tant que `.announce()` n'est pas appelé.

C'est un projet **sibling** : il consomme LiteMCP, il n'en fait pas partie. La lib n'ajoute **aucune** dépendance
ni code pour cette intégration — la reconnaissance repose uniquement sur des comportements déjà publics
(`initialize`, `/health`).
