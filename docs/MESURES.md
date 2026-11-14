# Mesures, SLO et comparatifs

Chiffres datés, décisions tranchées par la mesure et comparatif ViteMCP. La campagne de
référence (9 frameworks, `2026-10-25`) et sa méthode vivent dans [`BENCH.md`](BENCH.md) ; cette
page garde les campagnes antérieures, le SLO et le comparatif 1-contre-1 que le README ne
résume plus.

Aucun de ces chiffres n'est reproductible depuis ce dépôt seul : la campagne à 9 frameworks
s'appuie sur le dépôt externe `litemcp-it` (client neutre, serveurs de démonstration, journal
brut), et les deux exemples de mesure du dépôt (`examples/bench_backends`, `examples/slo`) ne
rejouent pas la campagne complète : ils envoient désormais l'enveloppe stricte et rendent des
chiffres, mais sur un seul processus local (voir [`BENCH.md`](BENCH.md) pour ce qu'ils mesurent
réellement et ce qu'ils ne mesurent pas).

[← Retour au README](../README.md)

## Mesures 0.5.1 — Windows 11 (rustc 1.99.1, release LTO fat/strip, loopback)

Trois passes consécutives, même machine, même session :

| Métrique | Valeur | Détail |
|---|---|---|
| `tools/call` keep-alive (moyenne) | **0,077–0,086 ms/req** | 2 000 appels, warmup 200 déduit |
| p50 / p99 keep-alive | 0,062–0,065 / **0,297–0,442 ms** | 2 000 appels |
| Connexions neuves | **1 576–1 759 conn/s** | 300 connexions |
| Binaire release strippé — **minimal** (1 outil) | **263 Kio** (269 312 o) | LTO fat + `codegen-units = 1`, mesuré depuis un projet consommateur (0.9.0 publiée) |
| Binaire release strippé — **démo `hello`** (3 registres + annotations) | **596 Kio** (610 816 o) | idem |
| Démarrage in-process (bind + 1ʳᵉ réponse) | **5,4 ms** | `examples/slo` |
| Mémoire après 2 300 requêtes | n/a hors Linux | `VmRSS` via `/proc/self/status` |

> Le p99 de 0,3–0,44 ms reflète les micro-pics du scheduler Windows en
> environnement bureautique ; les campagnes Linux du 2026-10-24 (0,031–0,033 ms
> en moyenne, p99 0,048–0,065 ms) restent la référence pour un serveur
> dédié — les conditions figées de chaque campagne sont dans [`docs/BENCH.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/BENCH.md).

## Décisions prises sur mesures

**Route maison vs backend alternatif `tiny_http`** (bench A/B du 2026-10-22,
`examples/bench_backends`) :

| Métrique | Route maison | tiny_http | Verdict |
|---|---|---|---|
| `tools/call` keep-alive | **0,10–0,11 ms/req** | 0,19–0,20 ms/req | maison ~1,85× plus rapide |
| Connexions neuves | 1 750–2 000 conn/s | 2 080–2 180 conn/s | tiny +10–25 % |
| Coût binaire | — | +360 Ko (+80 %) | maison plus légère |
| Dépendances ajoutées | 0 | +4 (ascii, chunked_transfer, httpdate, log) | maison plus légère |

**Décision tranchée par les chiffres** : la route maison reste le seul backend — plus rapide en régime keep-alive (le cas d'un agent qui parle à ses serveurs), plus légère en deps et en binaire. Le backend alternatif a été **retiré du dépôt** : le crate garde exactement deux dépendances (`serde`, `serde_json`).

**Optimisations « léger & rapide »** (A/B dos à dos, même machine et même session) :

| Métrique | 0.3.0 | Après optimisation | Verdict |
|---|---|---|---|
| `tools/call` keep-alive (moyenne) | 0,097–0,111 ms | **0,065–0,070 ms** | **~35 % plus rapide** |
| p50 / p99 keep-alive | 0,085–0,112 / 0,157–0,166 ms | **0,061 / 0,095–0,123 ms** | plus serré |
| Mémoire de lecture par connexion | 8 Ko | **4 Ko** | 1 Mio au plafond de 256 connexions |

Les leviers : réponse HTTP assemblée dans un seul buffer (**un seul write** par réponse — avant : deux syscalls, deux paquets avec nodelay), parsing d'en-têtes **zéro allocation** (`Origin`/`Host`/`Accept`/`Connection` réduits à des verdicts, sans chaînes ni `Vec`), buffers de lecture réutilisés sur toute la connexion keep-alive, **zéro clone des arguments** dans le dispatch (`params` déplacé hors du JSON parsé, handler et validation travaillent par référence), `route`/`allow_origins` partagés par `Arc`. Dépendances inchangées : toujours exactement deux (`serde`, `serde_json`).

## SLO — cibles et campagnes figées

| Métrique | Cible | Linux (Ubuntu 26.04, 2026-10-24) |
|---|---|---|
| Binaire release stripped | < 1,5 Mo | **687 Ko** ✅ |
| `tools/call` keep-alive (moyenne) | < 1 ms | **0,031–0,033 ms** ✅ |
| p99 keep-alive | < 2 ms | **0,048–0,065 ms** ✅ |
| Connexions neuves | — | **4 400–5 041 conn/s** |
| Mémoire après 2 300 requêtes | < 10 Mo | **2,7 Mio `VmRSS`, +4 Kio pendant la charge** ✅ |
| Démarrage in-process | < 10 ms | **~0,4 ms** ✅ |

`examples/slo` mesure lui-même ses bornes : démarrage in-process, 2 300 requêtes keep-alive (warmup déduit) et `VmRSS` avant/après via `/proc/self/status` (n/a hors Linux) ; il envoie l'enveloppe stricte (3 en-têtes + 2 clés `_meta`) et rend ses chiffres sur le profil par défaut (voir [`BENCH.md`](BENCH.md)).

Les comptes de tests cités dans les anciennes campagnes (**547 / 504 / 481**) sont périmés : mesurés le 2026-11-13, la suite donne **557** tests avec toutes les features, **514** avec le jeu par défaut et **491** sans aucune feature (0 échec) — tous exécutés sous Windows 11 ; la CI, elle, ne tourne que sous Linux.

## Tailles de binaire

Les chiffres de 2026-10-28 (**263 Kio** minimal, **320 Kio** par défaut, **596 Kio** pour la démo `hello`, cités dans le tableau « Mesures 0.5.1 » ci-dessus) ne sont plus reproductibles : re-mesure du 2026-11-13, même méthode (projet consommateur externe, release `lto = "fat"` + `codegen-units = 1` + `strip`, Windows 11, rustc 1.99.1) :

| Consommateur | `default-features = false` | `default` |
|---|---|---|
| Un serveur, un outil `add` en `tool_typed` + schéma `zod` | **984 064 o (961 Kio)** | **984 064 o (961 Kio)** |
| `cargo build --release --example hello` (3 registres + annotations) | — | **1 015 808 o (992 Kio)** |

Deux enseignements : les macros ne coûtent **rien** quand elles ne sont pas utilisées (le linker élimine `litemcp-macros`, d'où deux binaires identiques au byte près), et un serveur à un outil tient dans **≈ 1 Mio**, pas dans 263 Kio. L'écart avec les chiffres publiés n'a pas été expliqué ; ils sont donc retirés du README plutôt que corrigés à l'estime. Le code exact du consommateur de mesure est conservé avec les sondes d'audit (`.zwork/rapports-2026-11-13/size-min`, `size-def`).

## Comparatif — ViteMCP (TypeScript)

[`@vitemcp/server`](https://github.com/vitemcp/server) est le framework TS qui cible la **même spec** stateless 2026-07-28 — le comparatif naturel. Même serveur démo (4 outils identiques) des deux côtés, **un seul client de bench neutre** (Rust, `litemcp-it/examples/bench_remote`), Windows 11, loopback, passes alternées (campagne du 2026-10-22).

**Résumé express :**

| | LiteMCP | ViteMCP (TS) |
|---|---|---|
| ⚡ Temps de réponse moyen | **0,09 ms** | 1,43 ms |
| 😰 Le pire cas (p99) | **0,18 ms** | 5,4 ms |
| 🔌 Connexions réutilisées | **2000/2000** | 0/2000 |
| 📊 Nouvelles connexions par seconde | **~1000** | ~400 |
| 🧠 Mémoire après 2300 requêtes | **5,5 Mo** | ~417 Mo |

Détail complet (plages, passes alternées) :

| Métrique | **LiteMCP** | @vitemcp/server | Verdict |
|---|---|---|---|
| keep-alive (moyenne) | **0,09–0,10 ms** | 1,43–1,73 ms | **~15–18× plus rapide** |
| p99 keep-alive | **0,18–0,22 ms** | 5,4–5,9 ms | ~25× plus resserré |
| Connexions réutilisées | **2000/2000** | 0/2000 | flux SSE maintenu ouvert côté ViteMCP → reconnect à chaque appel |
| Nouvelles connexions acceptées **(connexions/seconde)** | **848–1257 connexions/s** | 354–498 connexions/s | **~2–3× plus de nouveaux clients par seconde** |
| Mémoire après 2 300 requêtes | **5,5 Mo** (0,9 Mo privé, reste = DLL partagées) | ~417 Mo (3 processus Node : 47 + 63 + 306) | **~75× moins de mémoire** |

L'écart structurel : LiteMCP délimite ses réponses par `Content-Length` et clôt le flux après l'événement — la connexion TCP reste proprement réutilisable ; ViteMCP répond en **SSE encadré** qu'il maintient ouvert, le client doit donc rouvrir une connexion par appel séquentiel. (Conditions : tsx/Node 24 côté TS ; les deux serveurs complétaient les mêmes requêtes JSON-RPC.)
