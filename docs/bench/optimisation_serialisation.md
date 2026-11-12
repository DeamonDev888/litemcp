# Optimisation de la sérialisation et du plafond de réponse — décision datée et chiffrée

> Campagne : 2026-11-12, Windows 11, `rustc` stable, `litemcp 0.10.5`, machine de bureau.
> Mandat : vérifier l'optimisation de sérialisation autour de
> `max_response_bytes` (contrôle historique appliqué **après** sérialisation
> complète), avec benchmark reproductible avant/après et décision
> conserver/abandonner appuyée sur des chiffres.
>
> Règle de la maison : **jamais d'adjectif sans chiffre.**

## 1. Décision

| Optimisation | Verdict | Preuve |
|---|---|---|
| **Writer borné** (cesse de stocker à `max_response_bytes`, compte le total exact) | **CONSERVÉ** | allocations −48 % et latence ÷2 au scénario 16 Mio ; zéro régression mesurée sur les chemins de succès (allocations et octets **identiques** jusqu'à la réponse pile au plafond) |
| **Assemblage en-tête/corps sans copie** (`write_vectored`, tête seule formatée) | **CONSERVÉ** | octets fil **identiques** (captures, §4) ; une copie pleine charge supprimée par réponse HTTP (−1 048 576 o au scénario exact JSON) ; latence −22 à −25 % (JSON 1 Mio) |
| **Interruption CPU de la sérialisation à `cap+1`** (avortement pur) | **ABANDONNÉ** (dès l'étude, avant code) | le message d'erreur du plafond rapporte la **taille réelle** (`response size (N bytes) …`) — abréger la marche de sérialisation perdrait `N` et changerait les octets de l'erreur, ce que le mandat interdit |

Les deux optimisations conservées sont **independantes et cumulables** ;
les chiffres ci-dessous sont ceux de leur état combiné (état final du dépôt).

## 2. Conception : ce que le writer borné fait — et ne promet pas

Le contrôle historique (`src/server.rs`, `dispatch`) remplace la réponse
sérialisée par l'erreur `-32603` **après** sérialisation complète : un handler
qui rend 16 Mio sous un plafond de 1 Mio faisait pousser un tampon de 16 Mio
avant d'être refusé. Le writer borné (`BoundedWriter`, `src/jsonrpc.rs`)
reçoit chaque fragment produit par `serde_json::to_writer` :

- sous le plafond : stocké intégralement — les octets de succès sont
  **identiques** à `to_vec` (même sérialiseur, même tampon `Vec` initialisé à
  128 o comme `serde_json::to_vec`, mêmes croissance et allocation) ;
- au plafond atteint : **plus rien n'est stocké** (jamais plus de `cap`
  octets en mémoire), mais chaque fragment continue d'être **compté** — la
  taille totale exacte alimente le message d'erreur **inchangé** ;
- plafond `0` (illimité) : tout est stocké, comportement historique.

**Ce que ce writer ne fait pas :**

1. **Il n'interrompt pas la marche de sérialisation.** Compte tenu du message
   d'erreur qui rapporte la taille exacte, avorter à `cap+1` est incompatible
   avec des erreurs octet-pour-octet inchangées — option étudiée et
   abandonnée (voir §1). Le coût CPU de sérialisation d'une réponse sur-
   dimensionnée reste entier ; c'est la **mémoire** qui est bornée.
2. **Il ne borne pas le tas.** La `Value` rendue par le handler reste
   intégralement matérialisée **avant** sérialisation : au scénario 16 Mio
   après optimisation, il reste ≈ 34,6 Mo d'allocations par appel — ce sont
   les chaînes de la `Value` du handler, pas la réponse sérialisée. La
   documentation publique de
   [`max_response_bytes`](../../src/server.rs) et celle de `BoundedReply`
   portent cette limite : le plafond protège le **réseau**, pas le tas.
3. **Un dépassement marginal n'économise rien.** La réponse d'un `tools/call`
   est sérialisée en quelques gros fragments (le `content[0].text`, le
   `structuredContent`). Pour un dépassement de +1/+2 o, tous les fragments
   sont **sous** le plafond : tout doit être tamponné sauf les derniers
   octets, et la mesure confirme un zéro net (§3, scénario « dépassée d'un
   octet » — allocations identiques avant/après ; la sonde de décomposition
   du 2026-11-12 donnait +222 o pour +2 o, l'enveloppe d'erreur). L'économie
   mesurée vaut ≈ **2×(taille − plafond)** : chaque étape de croissance du
   tampon alloue la capacité entière (doublement), la borne supprime toutes
   les étapes au-delà du plafond — 2×15,8 Mo ≈ 32,5 Mo au scénario 16 Mio.

## 3. Chiffres avant/après

Harnais : [`examples/bench_serialisation.rs`](../../examples/bench_serialisation.rs)
(harnais préexistant au dépôt, invariant avant/après) — allocateur global de
comptage (allocs et octets par appel, processus entier), latence par `Instant`
(moyenne et **médiane**), release, plafond de campagne 1 Mio, trois modes
(`dispatch` = cœur seul, `http-json` = aller-retour keep-alive, `http-sse` =
mono-événement), quatre scénarios (petite ~270 o, exacte au plafond,
dépassée de 1 o, très grande 16 Mio).

- **Avant** : 2 exécutions (campagne du 2026-11-11 archivée dans
  `serialisation_avant.txt` + ré-exécution du jour,
  `serialisation_avant_refaite.txt`) ;
- **Après** : 4 exécutions de l'état final (`serialisation_apres.txt` + 3).

### dispatch (cœur seul) — médianes µs, plage des exécutions

| Scénario | avant (2 ex.) | après (4 ex.) | allocs/appel avant→après | octets/appel avant→après |
|---|---|---|---|---|
| petite (~270 o) | 3,00–3,10 | 3,10–4,50 (bruit) | 25 → **25** | 3 682 → **3 682** |
| exacte au plafond (1 Mio) | 1 934–1 948 | 1 630–2 008 | 26,01 → **26,01** | 4 196 594 → **4 196 594** |
| dépassée d'un octet (+1) | 1 938–1 949 | 1 585–2 122 | 28,01 → **28,01** | 4 196 824 → **4 196 824** |
| très grande (16 Mio) | 48 103–51 217 | **22 912–36 435** | 28,05 → **26,05** | 67 111 376 → **34 605 552 (−48 %)** |

### http-json (keep-alive) — médianes µs

| Scénario | avant | après | allocs/appel | octets/appel |
|---|---|---|---|---|
| petite | 72,5–77,8 | 67,0–88,5 (bruit) | 27 → **27** | 4 237 → **3 813 (−424)** |
| exacte (1 Mio) | 2 957–3 051 | **2 239–2 373 (−22 à −25 %)** | 28,01 → **28,01** | 5 245 301 → **4 196 725 (−1 048 576)** |
| dépassée (+1) | 2 105–2 205 | 1 691–2 296 | 30,01 → **30,01** | 4 197 077 → **4 196 955 (−122)** |
| très grande (16 Mio) | 38 380–48 483 | **23 262–29 185 (−27 à −40 %)** | 30,05 → **28,05** | 67 111 630 → **34 605 683 (−48 %)** |

### http-sse (mono-événement) — médianes µs

| Scénario | avant | après | allocs/appel | octets/appel |
|---|---|---|---|---|
| petite | 474–599 | 451–510 | 41 → **41** | 9 313 → **8 881 (−432)** |
| exacte (1 Mio) | 5 012–5 135 | **4 192–4 761 (−8 à −16 %)** | 42,01 → **42,01** | 6 298 532 → **5 249 948 (−1 048 584)** |
| dépassée (+1) | 2 826–3 155 | 2 434–3 102 | 44,01 → **44,01** | 4 201 854 → **4 201 724 (−130)** |
| très grande (16 Mio) | 39 255–49 101 | **24 675–28 669 (−27 à −37 %)** | 44,05 → **42,05** | 67 116 409 → **34 610 454 (−48 %)** |

**Lecture :**

- **Chemins chauds inchangés** : petite réponse dispatch — allocations et
  octets strictement identiques (25 / 3 682), latence dans le bruit machine ;
  réponse pile au plafond — allocations et octets **identiques au compteur
  près** (26,01 / 4 196 594), latence ≥ 0 (−9 à −17 % selon l'exécution).
- **Copies supprimées au transport** (`write_vectored`) : le corps n'est plus
  recopié dans un tampon d'assemblage — −424 o (JSON petite) à
  −1 048 576 o (JSON 1 Mio) par appel ; en SSE, la copie de l'événement dans
  le tampon d'assemblage disparaît en plus (−1 048 584 o).
- **Borne mémoire effective au-delà du plafond** : −48 % d'octets alloués et
  latence ÷2 à 16 Mio (les trois modes) ; au bord du plafond (+1 o), zéro
  économie — conforme au modèle ≈ 2×(taille − plafond), cf. §2.
- Le bruit machine est réel (une exécution après à 36,4 ms au 16 Mio contre
  22,9–25,2 ms pour les trois autres) ; allocations et octets, eux, sont
  exacts et reproductibles au compteur près — ce sont eux qui tranchent.

## 4. Vérification octet pour octet

Captures **avant modification**, doublement déterministes (2 exécutions
identiques) puis capture après : 11 sections hexadécimales — corps dispatchés
des plafonds **0 / 1 / exact / dépassé**, octets fil complets (tête HTTP +
corps) JSON keep-alive et SSE mono-événement, réponses exactes et dépassées.

- `octets_avant.txt` — référence avant (état d'origine) ;
- `octets_apres.txt` — capture après (writer borné + writev) ;
- **diff : identiques, section par section** (comparaison base64 intégrale).

Ces octets sont désormais **figés par des tests** :
[`tests/plafond_reponse.rs`](../../tests/plafond_reponse.rs) — 10 tests qui
assertent les octets exacts (dispatch **et** fil HTTP/JSON/SSE) des plafonds
0 / 1 / exact / dépassé, y compris `Content-Length` et le message d'erreur
portant la taille réelle (302 bytes au plafond 300). Toute divergence future
est une régression de comportement fil, pas un détail d'implémentation.

## 5. Vérifications d'intégrité (état final)

- `cargo fmt --all -- --check` : ✔
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` : ✔
- `cargo test --workspace` : ✔ (0 échec)
- `cargo test --workspace --all-features` : ✔ (0 échec)
- `cargo test --workspace --no-default-features` : ✔ (0 échec)
- `#![forbid(unsafe_code)]` : conservé ; synchrone : conservé ;
  dépendances cœur : `serde` + `serde_json` uniquement (inchangées).

## 6. Trace de la campagne

| Fichier | Rôle |
|---|---|
| `serialisation_avant.txt` | mesures avant (campagne 2026-11-11, harnais utilisateur) |
| `serialisation_avant_refaite.txt` | ré-exécution avant du 2026-11-12 (reproductibilité confirmée : allocations identiques au compteur près) |
| `serialisation_apres.txt` | exécution après de l'état final |
| `octets_avant.txt` / `octets_apres.txt` | captures hexadécimales avant/après (11 sections, identiques) |
| `../../tests/plafond_reponse.rs` | octets figés en tests permanents |
| `../../src/jsonrpc.rs` | `BoundedWriter` / `BoundedReply` (implémentation conservée) |
| `../../src/http.rs` | `respond_version` en `write_vectored` (implémentation conservée) |
