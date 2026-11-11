# Changelog

Toutes les évolutions notables de ce projet sont documentées dans ce fichier.
Format : [Keep a Changelog](https://keepachangelog.com/) — versioning : SemVer.

## Politique de versioning

- **Flux permanent** : toute modification (fonction, correctif, doc) atterrit d'abord dans la section **[Unreleased]** ci-dessous, au fil des commits.
- **Pacement** : une version n'est coupée que pour un **lot cohérent** — une fonctionnalité complète, un correctif notable, ou une étape de documentation majeure. Pas de bump rituel à chaque commit.
- **0.x** jusqu'à la conformance officielle MCP verte (suite 2026-07-28, mode serveur, 0 FAILURE) ; la mention **1.0** attend ce jalon.
- Chaque version publiée est taguée `vX.Y.Z` sur le commit exact dont elle est issue (`.cargo_vcs_info.json` vérifié avant `cargo publish`).

## [Unreleased]

## [0.10.3] - 2026-11-10

### Corrigé

- **Extraits d'installation du README en forme de plage (`0.10`)** — la 0.10.2 annonçait encore `0.10.1` : un extrait épinglé à une version exacte vieillit dès le patch suivant et contredit le badge de version. La forme `0.10` (`^0.10`) installe toujours la dernière `0.10.x` compatible. Aucun changement de code ;

## [0.10.2] - 2026-11-10

### Corrigé

- **README du crate aligné sur la version publiée** — les extraits d'installation et le badge de statut annonçaient encore `0.10.0` alors que `litemcp 0.10.1` est en ligne ; le badge de statut devient `0.10.x` (la version exacte est déjà portée par le badge crates.io, dynamique). Aucun changement de code : patch documentaire, publié pour que la page crates.io rende le README corrigé (une version publiée est immuable) ;

## [0.10.1] - 2026-11-10

### Publication

- **Pipeline de publication durci (CI uniquement, aucun changement d'API)** : `release.yml` reste le **seul** chemin déclenché par un tag `v*` (`publish.yml` n'a plus aucun déclencheur tag et devient le break-glass manuel, avec le **même** gate `verify`) ; les contrôles vivent dans `scripts/release.ps1` (`cargo metadata` : racine = tag/input, `litemcp-macros` = exigence exacte de la dépendance racine, section CHANGELOG présente ; regex SemVer complète — `0.10.10` accepté ; statut crates.io à trois états 200/404/autre — une erreur réseau n'est **jamais** une preuve d'absence). Le job `publish` **rejoue lui-même** le contrôle de cohérence juste avant le premier `cargo publish`, la notification du tuto part **après** la publication **et** la release GitHub, le tag est dérivé de `GITHUB_REF` (aucun argument optionnel vide, un tag mal formé est refusé), `contents: write` est réservé au seul job `github-release`, les actions sont épinglées par SHA, les licences MIT/Apache accompagnent le paquet `litemcp-macros`, et un jeton crates.io **OIDC** (trusted publishing) peut remplacer le secret `CARGO_REGISTRY_TOKEN` sans le rendre obligatoire. Garde-fous : `tests/release_pipeline.rs` (structure des workflows) et `scripts/release-tests.ps1` (20 simulations — tag incohérent, tests rouges, version déjà publiée, réseau indisponible, contenu des paquets), sans secret ni publication réelle ;

### Ajouté (audit du 2026-11-08 — surfaces de contenu MCP)

- **Contenus d'outil riches et multiples (spec 2026-07-28, `ContentBlock`)** — le noyau ne permettait qu'un `content` de texte unique : impossible d'émettre une image, un audio, un `resource_link`, une ressource embarquée, ni plusieurs blocs mélangés (la suite officielle de conformance couvre ces scénarios). Nouveau `ContentBlock` (sérialisation sans tag : chaque variante porte son discriminant fil `"type": "text" | "image" | "audio" | "resource_link" | "resource"`) avec `ImageContent`/`AudioContent` (`data` **base64**, encodage interne — un binaire n'est jamais déguisé en texte JSON pour simuler le support ; `from_bytes` encode les octets réels), `ResourceLinkContent` et `EmbeddedResourceContent`. Nouvelle méthode d'enregistrement **`tool_result`** (handler → `Result<CallToolResult, ToolError>` : le résultat part tel que construit, contenus mixtes et `structuredContent` explicite cohabitent) ; l'API `Value` historique (`tool`/`tool_typed`/`tool_parse` + `ToolResult`) est **inchangée** et sa forme fil reste intangible (test octet pour octet). `CallToolResult::from_content` construit le résultat de contenu ; conversions `From` (`&str`/`String`/chaque bloc → `ContentBlock`, bloc(s) → `CallToolResult`). L'`outputSchema` reste exécuteur sur un `structuredContent` explicite (validé tel quel, sans enveloppe) et un `isError: true` construit par le handler est rapporté aux hooks `on_call` comme `ToolFailure` ;
- **Prompts image/audio/lien/ressource embarquée** — `PromptMessage.content` porte désormais un `ContentBlock` complet (le champ texte historique s'écrit pareil via `PromptMessage::user`/`assistant`, forme fil inchangée) ; `PromptMessage::user_content`/`assistant_content` ouvrent les blocs riches. *Rupture documentée* : le champ `content` change de type (`PromptContent` → `ContentBlock`) et le type `PromptContent` est retiré (aucun usage connu) ;
- **Ressources binaires (`resources/read` — `blob`)** — le noyau ne pouvait lire que du texte : impossible de servir un PNG, un PDF… Nouvelle `ResourceData` (union sans tag `Text`/`Blob`) + `BlobResourceContents` (`blob` **base64**), enregistrée via **`resource_data`** (le handler choisit texte ou binaire à chaque lecture) ; `ResourceContents` (texte) et `.resource()` restent **inchangés** — la forme fil d'une lecture texte est identique (test de régression) ;
- **`resources/templates/list`** — méthode absente (`-32601`) : nouveau catalogue `ResourceTemplate` (`uriTemplate` RFC 6570, `name`, `title`/`description`/`mimeType` optionnels, ordre déterministe), enregistré via **`resource_template`** (+ `remove_resource_template` pour les gateways), servi même à registre vide avec les indices de cache SEP-2549 (`ttlMs`/`cacheScope`) et couvert par la capacité `resources` ;
- **`completion/complete`** — méthode absente (`-32601`) : nouveau fournisseur **`completions`** (reçoit une `CompletionRequest` validée — référence `ref/prompt`/`ref/resource`, argument, `context.arguments` — rend des `Completion` bornés à 100 valeurs : au-delà, le noyau tronque et pose `hasMore: true`). Params mal formés ou fournisseur absent → `-32602`, échec du fournisseur → `-32000`, exécution bornée par `tool_timeout` comme les autres handlers. `completion/complete` est traitée comme méthode d'action : une notification est refusée `-32600` (rien ne s'exécute en silence). La capacité `completions` n'est annoncée que si un fournisseur existe — une capacité annoncée est une capacité servie. *Rupture documentée* : `CallToolResult.content` change de type (`Vec<TextContent>` → `Vec<ContentBlock>`) — les constructeurs publics (`from_structured`, `from_tool_error`, `From<Result<Value, ToolError>>`) gardent leurs signatures ;
- **Cycle MRTR complet (multi round-trip requests, spec 2026-07-28)** — `ResultType::InputRequired` existait mais **aucun handler ne pouvait le produire** (et le socle ne compilait pas : `verify_mrtr`/`emit_input_required` appelés mais absents, module `request_state` non déclaré dans la racine, double `derive(Clone)` sur `ToolHandler`, `tool_typed` écrivant un champ `f` disparu, signatures de `read_resource`/`get_prompt` en retard sur leurs corps). Nouvelles méthodes d'enregistrement **`tool_mrtr`** / **`prompt_mrtr`** / **`resource_mrtr`** (le handler reçoit [`InputRound`] : `turn`, réponses client non fiables, `state` vérifié) et spécification **`InputRequiredSpec`** (`.request`/`.elicit`/`.elicit_url`/`.create_message`/`.list_roots` + `.with_state`). Le noyau valide chaque requête embarquée (seules `elicitation/create`, `sampling/createMessage`, `roots/list` sont embarquables ; forme fil vérifiée — un bogue de handler rend `-32603` **sans rien émettre** sur le fil), exige la capacité client déclarée correspondante (`-32021`, forme officielle `data.requiredCapabilities` indexée par capacité), frappe un `requestState` **opaque signé** (HMAC-SHA256, TTL 10 min via `.request_state_ttl`, clé de flotte via `.request_state_key`, borne d'état 8 Kio — dépassement en `-32603`, liaison méthode + paramètres saillants + tour) et répond `{ resultType: "input_required", inputRequests?, requestState, _meta }` en HTTP 200. Au rejeu (id **différent**), `inputResponses` + l'écho exact du jeton sont vérifiés **avant tout handler** : altéré, expiré, malformé, émis sous une autre clé ou lié à une autre requête → `-32602` ; `inputResponses` sans `requestState` → `-32602` ; les champs MRTR sont ignorés sur toute autre méthode et **aucune capacité serveur supplémentaire n'est annoncée** (rien n'est servi au-delà des résultats `input_required`) ; le jeton est porteur et non consommé (consommation unique à imposer dans le handler), l'id n'est volontairement pas lié. Types désormais exportés à la racine et dans le prelude (`ToolOutcome`, `PromptOutcome`, `ResourceOutcome`, `InputRound`, `InputRequiredSpec`). *Sans dépendance nouvelle* : SHA-256, HMAC-SHA256 et base64url implémentés localement (vecteurs RFC 6234/4231/4648 en tests). Tests : cycles complets outil/prompt/ressource, trois tours, état seul sans `inputRequests`, états altéré/expiré/malformé/autre clé/autre requête, capacités non déclarées, bogues de handler, champs superflus ignorés, méthodes non concernées, absence de capacité supplémentaire annoncée, statuts HTTP du profil strict (`tests/mcp_mrtr_cycle_2026_11_09.rs`, 18 tests) ;
- **Schémas publiés = validation appliquée : sémantiques JSON Schema pour les entiers, profils publiés pour les formats, corpus différentiel contre le validateur de référence** — l'équivalence promise (« publié = appliqué ») était fausse sur deux points : `zod::integer()` refusait `3.0`/`1e2`/`-0.0` alors que JSON Schema draft 2020-12 les accepte comme `integer`, et les formats `email`/`uri`/`date-time` n'appliquaient qu'un profil approximatif non publié. **Choix délibéré (documenté)** :
  - *Entiers* — **conformité JSON Schema + normalisation sûre** : tout nombre **mathématiquement intégral** est accepté et **converti en entier exact** avant que le handler ne voie les arguments (nouveau trait `Schema::has_integers`/`normalize_integers`, appelé après une validation réussie — une seule copie d'arguments quand des défauts **ou** des entiers sont en jeu, zéro copie sinon) : le motif documenté `args["a"].as_i64()` réussit toujours, la conformité ne casse aucun handler. La plage 64 bits est **encodée dans le schéma publié** (`minimum`/`maximum` toujours posés) : un intégral hors plage (`1e300`, `u64::MAX`) est un refus de borne chez n'importe quel validateur conforme — jamais une conversion silencieuse. La normalisation vaut aussi pour les adaptateurs `schemars`/`from_json_schema` (marche guidée par le document publié, `$ref` locaux résolus, cycles détectés, même budget global que l'injection des défauts) : un champ `u32` dérivé reçoit un entier désérialisable même si le client envoie `3.0` ;
  - *Énumérations d'entiers* — **égalité mathématique + normalisation** : `integer_enumeration(&[1, 2, 3])` refusait `3.0`/`2e2`/`-0.0` alors que le schéma publié (`{"type": "integer", "enum": [1, 2, 3]}`) les accepte chez tout validateur conforme (JSON Schema 2020-12 §4.2.1 : deux nombres sont égaux si leurs **valeurs mathématiques** le sont — vérifié en différentiel contre la référence). Ils sont désormais acceptés et **réécrits en l'entier exact du membre** avant le handler (l'énumération déclare des entiers : même passe `has_integers`/`normalize_integers` que `integer()` — le motif documenté `args["mode"].as_i64()` réussit toujours) ; les messages distinguent un entier hors énumération (« must be one of … », y compris un intégral hors plage i64) d'un non-intégral (« expected an integer ») ; un entier hors énumération et une chaîne restent refusés sans conversion ;
  - *Formats* — **profils explicitement publiés** (les formats ne sont pas normatifs en JSON Schema) : `email` (bornes RFC 5321, domaine **avec point** — `user@localhost` refusé, la référence l'accepte) et `uri` (**autorité `://` obligatoire** — `mailto:`/`urn:` refusés, la référence les accepte) sont des extensions **plus strictes** que la référence ; `date-time` suit **RFC 3339 §5.6 + §5.7, notes incluses** — séparateur `T`/`t`/**espace** (NOTE : la référence refuse l'espace — divergence publiée), `Z`/`z`, calendrier vérifié, seconde intercalaire `60` acceptée **au seul point UTC 23:59** décalé par le fuseau (§5.7 : « in time zones other than "Z", the leap second point is shifted by the zone offset » — `01:59:60+02:00` vaut `23:59:60Z` et passe comme chez la référence, `23:59:60+02:00` vaut `21:59:60Z` et est refusé comme chez la référence ; le mois réel — juin/décembre à ce jour — n'est pas vérifiable sans la table IERS, la référence ne le vérifie pas non plus) ; les espaces superflus (tête/fin/doublé) sont refusés **de concert** avec la référence (RFC 3339 strict). La promesse vague « vérifiés comme dans `zod` » (adaptateur `schemars`) est remplacée par la référence à ces profils ;
  - *Corpus différentiel* — **`tests/zod_diff_jsonschema.rs`** (14 tests, feature `schemars`, **aucune dépendance ajoutée** : réutilise la crate `jsonschema` 0.60 déjà optionnelle) : entiers + bornes — **équivalence verdict par verdict** avec la référence sur le schéma publié (21 cas, dont `3.0`, `1e2`, `±0.0`, `2^53`, bornes inclusives, intégraux hors plage) **et `uint()` en équivalence complète** (plage u64, bornes inclusives) ; énumérations d'entiers — **équivalence complète** (`3.0`, `2e2`, `-0.0`, hors énumération, hors plage i64, mauvais types) avec normalisation vérifiée unitairement et de bout en bout (dispatch : le handler lit `as_i64()` sur un `3.0` client et reçoit le membre exact d'une énumération sur un `2.0`) ; normalisation vérifiée unitairement (objets/tableaux/records imbriqués, chaînes et non-entiers intacts) ; formats — divergences **épinglées des deux côtés** (`user@localhost`, `mailto:`, `urn:`, espace date-time) et convergences majeures vérifiées des deux côtés (email/uri/date-time, y compris minuscules, seconde intercalaire décalée correctement par le fuseau, année 0000, offset `-00:00`, intercalaire hors point UTC 23:59 et espaces superflus refusés des deux côtés) ;
  - *Changement de fil 0.x* : `integer()` accepte désormais `3.0`/`1e2`/`-0.0` (refusés avant) — le handler les reçoit **normalisés en entiers exacts** (`3`, `100`, `0`) ; `integer()` accepte désormais `-2^63` (exactement `i64::MIN`, refusé avant) ; les refus d'intégraux hors plage portent le message de borne (`must be ≤ …`) au lieu de « expected an integer » ; `integer_enumeration()` accepte désormais un flottant intégral égal à un membre (`3.0`/`2e2`/`-0.0` — refusés avant) et le handler reçoit **l'entier exact du membre** ; `date-time` refuse désormais une seconde intercalaire `60` qui ne retombe pas sur 23:59 **UTC** après décalage du fuseau (RFC 3339 §5.7 — ex. `2026-06-30T23:59:60+02:00`, accepté avant, vaut 21:59:60Z) ; aucune autre forme fil ne change. README mis en cohérence (tableau des schémas, sections `schemars`/macros) ;

### Modifié

- **Macros `#[tool]`/`#[resource]`/`#[prompt]` incluses par défaut** (`default = ["macros"]`) — le schéma dérivé des types des paramètres marche d'emblée, sans configurer de feature. Le coût reste contenu : `litemcp-macros` est une crate **locale** à **zéro dépendance externe** (pur `proc_macro` standard, ni `syn`, ni `quote`, ni `proc-macro2`), mesurée à **+57 Kio** de binaire (320 Kio au lieu de 263 Kio) — les dépendances **externes** du cœur restent exactement `serde` + `serde_json`. Le binaire strictement minimal s'obtient avec `default-features = false` (263 Kio, le noyau complet reste servi). README mis en cohérence (installation, coût des features, stack par défaut, concepts) ;

### Corrigé (audit du 2026-11-08)

- **`server/discover`/`initialize` annoncent les capacités réellement servies, même à registres vides** — `resources` et `prompts` étaient omis de l'annonce quand leur registre était vide alors que leurs méthodes (`*/list`, `*/read`, `*/get`) restent servies (catalogue vide) et qu'un `Gateway` peut peupler les registres après coup : un client conforme voyait un serveur « sans ressources/prompts », puis la capacité apparaître au premier `edit` sans changement de surface réel — la suite officielle de conformance échouait déjà sur `prompts`. Les trois capacités sont désormais annoncées en permanence et **aucune sous-capacité non servie n'est déclarée** (`listChanged`, `subscribe` ne sont pas prétendus). *Changement de fil* : `server/discover` et `initialize` renvoient désormais les trois clés `tools`/`resources`/`prompts` y compris à registres vides — l'annonce devient un sur-ensemble constant de la surface ; les méthodes réellement appelables ne changent pas. Tests : serveur vide puis ajout dynamique via `Gateway::edit`, cohérence annonce ↔ appels réels (`tests/interop_2026_11_08.rs`, `tests/features.rs`) ;
- **Filtre `Origin` : exception loopback légitime sans rouvrir le DNS rebinding** — le refus par défaut de tout `Origin` rejetait aussi la paire légitime `Host: 127.0.0.1:<port>` + `Origin: http://127.0.0.1:<port>` (client navigateur même-origine ; le scénario officiel de conformance la confirme). L'exception par défaut est strictement bornée : listener lié à une adresse de boucle **spécifique** (`127.0.0.0/8`, `::1` — un bind `0.0.0.0`/`[::]` ou une interface externe ne l'ouvre pas), origine `http://` dont l'autorité est identique (casse ignorée) à la fois au `Host` reçu et au littéral `ip:port` du listener — toute divergence de schéma, d'hôte ou de port reste refusée, un nom résolu (`localhost`) ne participe pas, un pair `Host`/`Origin` externe même identique est refusé ; `Origin: null` et `Sec-Fetch-Site: cross-site` restent refusés (l'exception n'ouvre pas d'échappatoire Fetch Metadata : seul un `Origin` explicitement listé dans `allow_origins` l'ouvre, et `allow_origins` reste inchangé pour les déploiements explicites/derrière proxy). *Changement de fil* : sur un bind loopback, une requête navigateur même-origine passe désormais en `200` au lieu de `403` ; aucun autre verdict ne change. Tests : matrice socket loopback accepté / `evil.example.com` refusé / divergences schéma-hôte-port refusées, bind non local, IPv6, allowlist (`tests/interop_2026_11_08.rs`) ;
- **Défauts de schéma : « publié = appliqué » tient aussi pour les schémas JSON bruts, la détection suit les `$ref` à tous les niveaux, et la racine booléenne est refusée comme `inputSchema` d'outil** — trois voies rompaient encore la promesse ou le contrat MCP. (1) *Défauts bruts jamais injectés* : `FromJsonSchema` (document écrit à la main) laissait `default_value()`/`apply_defaults()`/`has_defaults()` aux no-ops du trait — un `default` publié dans un schéma brut était annoncé à `tools/list` mais **jamais injecté**, alors que le même document dérivé par `schemars` l'aurait été ; les deux adaptateurs délèguent désormais au **même moteur** (détection `schema_has_default`, injection `apply_node_defaults`, défaut racine résolu par `effective_root`) : un défaut publié dans un schéma brut accepté par LiteMCP suit exactement la même politique d'injection que celui dérivé par schemars. (2) *Détection aveugle aux `$ref`* : `has_defaults` ne résolvait les `$ref` locaux qu'à un seul niveau alors que l'injection les suivait — un défaut derrière une chaîne `$ref → objet → $ref`, un `$ref` imbriqué, une branche `allOf` ou une racine `$ref` laissait la détection à `false` et le noyau court-circuitait `apply_defaults` : **une injection nécessaire était désactivée**. La détection partage maintenant le résolveur chaîné borné de l'injection (budget de sauts par résolution, détection de cycles par adresse, borne `MAX_REF_DEPTH`), et les deux parcours voient **exactement les mêmes sous-schémas** — y compris les mots-clés frères (`properties`, `items`, combinateurs) portés À CÔTÉ d'un `$ref` sur un nœud dont la chaîne est coupée (cycle ou budget épuisé) : la reprise de l'injection les court-circuitait encore après la mutualisation, un défaut publié dans ces frères était annoncé puis jamais appliqué. (3) *Racine booléenne* : un `inputSchema` d'outil MCP est nécessairement un objet — `tool_typed`/`tool_parse` refusent désormais une racine `true`/`false` (panique d'enregistrement, même convention que `from_json_schema`) ; les schémas booléens restent valides partout où JSON Schema les autorise (`properties.*`, `items`, `output_schema`, documents bruts hors usage outil). *Changement de fil* : (a) les défauts des schémas bruts sont désormais appliqués, pas seulement publiés ; (b) des défauts jusqu'ici perdus silencieusement (chaînes et imbriquations de `$ref`, frères d'un nœud référent) arrivent dans le handler ; (c) un outil enregistré avec un `inputSchema` booléen panique au démarrage au lieu d'être publié tel quel ; les maillons strictement intermédiaires d'une chaîne de `$ref` traversée par la résolution restent transparents pour les deux parcours (sémantique documentée de la résolution, comportement défini au-delà de la borne). Tests : `tests/schema_defaults_parity.rs` (20 tests) — défaut brut publié + injecté + parité schemars, `$ref → objet → $ref → default`, `$ref` imbriqués, branche `allOf`, racine `$ref`, cycles (terminaison + défauts atteignables), chaînes dans/hors borne (10, 24, 300 sauts, profondeur structurelle 20), absence de défaut, `$defs` orphelin, preuve « `has_defaults` ne désactive jamais une injection nécessaire » (batterie incluant les nœuds à `$ref` coupé), racine booléenne refusée (`true`/`false`, `tool_typed`/`tool_parse`) et booléens restés valides en sous-schéma ;
- **Contrat `u64` des macros : bornes exactes publiées et appliquées (nouveau `zod::uint()`)** — `#[tool]`/`#[prompt]` annonçaient des bornes exactes qu'ils ne tenaient pas pour les types non signés : un paramètre `u64` publiait `integer().min(0)` donc `maximum: i64::MAX`, `usize` publiait `maximum` borné à `i64::MAX`, et l'extraction passait par `as_i64()` — `i64::MAX + 1` et `u64::MAX` étaient refusés à la validation (et l'extraction les aurait rendus `0`) alors que le type Rust les représente ; un littéral par défaut `u64::MAX` ne compilait même pas (`DefaultLit::Int(i64)`). Nouveau schéma **`zod::uint()`** (pub, miroir non signé d'`integer()`) : entier non signé exact, plage `[min, max]` en u64 (`[0, u64::MAX]` par défaut), même sémantique JSON Schema (intégraux `3.0`/`1e19` acceptés et **normalisés en u64 exact** ; un négatif est refusé — jamais transformé en grand entier) ; `usize` publie la borne **de la cible** (`usize::MAX as u64`, pas de littéral figé 64 bits). Extraction non signée : `as_u64()` + `try_from` (jamais de troncature `as`) ; littéraux par défaut portés par `i128` et publiés en `u64` quand ils dépassent `i64::MAX` — au-delà des plages exactes i64/u64, diagnostic de compilation explicite (jamais de troncature). Types signés inchangés (`integer()` et extraction `as_i64()`, compatibilité préservée). *Changement de fil* : pour un paramètre `u64`/`usize`, l'`inputSchema` publié porte désormais `maximum: u64::MAX` / `usize::MAX` (au lieu de `i64::MAX`) et les valeurs `[i64::MAX + 1, u64::MAX]` sont acceptées au lieu d'être refusées en `-32602`. Tests : `tests/macro_u64_contract.rs` (14 tests — 0, `i64::MAX`, `i64::MAX + 1`, `u64::MAX`, `-1`, bornes et débordements de chaque type, `usize` selon la cible, `isize`, `Vec<u64>`, `Option<u64>`, littéraux par défaut avec et sans suffixe, intégraux flottants normalisés) + 3 tests unitaires du parsing/émission des littéraux dans la crate macros ; vérifié « avant correctif » par exécution des tests contre la macro HEAD (échec de compilation du défaut `u64::MAX`, schéma publié `i64::MAX`, refus des valeurs au-delà) ;
- **Preuves de vie du registre : identité d'instance vérifiable et verrou inter-processus** — le sweep déduisait la mort d'un process d'un **simple échec de connexion sur `127.0.0.1`** : un serveur vivant lié à IPv6 ou à une IP non bouclage perdait son annonce (le refus prouvait le bind ailleurs, pas la mort), et un port réutilisé par une autre instance était déclaré vivant au nom de l'annonceur mort ; `retract` faisait un check-then-delete non atomique (une annonce plus récente pouvait être supprimée entre la lecture et le retrait) ; le bilan de fin de vie lisait `handlers_alive` **au début** du drain (un handler qui y terminait restait compté vivant). **Correctif** : l'annonce publie l'**adresse effective connectable** (`addr` — bouclage substitué pour un bind multi-interface, bind spécifique publié tel quel) et une **identité d'instance** (`instance`, SplitMix64 sur PID/port/nanos/séquence — jamais 0), exposée par `GET /health` en en-tête `X-Instance-Id` ; le sweep sonde l'adresse **publiée** et ne détruit qu'en **preuve positive** — l'adresse répond avec une identité différente = usurpation prouvée (décision re-vérifiée sous verrou) → retrait ; port sans réponse, identité absente ou invérifiable = **doute → conservée** (`undecided`, nouveau compteur public de `SweepReport`) : un refus TCP n'est plus jamais une preuve de mort ; `retract`/`announce` sont sérialisés par un **verrou inter-processus par nom** (dossier `<nom>.lock` créé exclusivement — zéro dépendance —, péremption par âge pour les verrous abandonnés, renoncement borné à 250 ms) : la course check-then-delete est fermée par construction ; le retrait du serveur lié vérifie en plus l'**identité d'instance** publiée (le couple `(pid, port)` seul ne distingue pas un redémarrage dans le même process sur le même port) et la sonde n'accepte une identité que dans le **bloc d'en-têtes d'une réponse 2xx** — une chaîne `x-instance-id:` dans un corps ne fabrique pas une usurpation ; les compteurs du sweep sont honnêtes (`corrupt` seulement pour un débris effectivement retiré ; un retrait impossible ou un fichier illisible — erreur d'accès, partage Windows — va dans `skipped`, le fichier restant en place) ; `handlers_alive` est lu **après** le drain (closure passée à `ZombieState::finish`) et le slot du handler est rendu **avant** la remise de son résultat — la lecture post-drain ne compte jamais un handler qui y a terminé. *Changements de fil* : l'annonce gagne les champs `addr`/`instance` (lecture tolérante inchangée) ; `/health` émet `X-Instance-Id` (additif, corps « ok » inchangé) ; les fantômes d'un kill non suivi de réutilisation de port restent en place — le conservateur prime sur la destruction. API : `announce()` retourne désormais l'identité publiée (source-compatible) ; nouveau `announce_at(name, version, addr, instance)` ; `SweepReport.undecided` ; `port_alive` conservé, sa doc ne prétend plus que le refus prouve la mort. Tests : `tests/registre_vie.rs` (8 — doute sur port sans réponse, usurpation par réutilisation de port retrait, IPv6 et IP non bouclage sondées sur l'adresse publiée, e2e annonce réelle addr+instance cohérents avec `/health`, handler terminé pendant le drain compté 0, stress d'annonces/retraits concurrents, retrait d'une ancienne instance qui ne supprime pas l'annonce d'une autre sur le même couple pid/port) ; tests unitaires verrou (exclusivité, retrait sérialisé, casse par âge), identités uniques, sonde d'identité (vérifiée/usurpée/muet/refus, identité hors des en-têtes ou sur un statut d'erreur = doute) et sweep (vivant/usurpé/doute/sans identité, compteurs disjoints, fichier illisible conservé, compteur de retrait échoué) ; les tests de `tests/antizombie.rs` alignés sur le nouveau contrat (doute conservé, jamais de retrait sans preuve) ;
- **Sortir les erreurs ressource/prompt/complétion, timeout et surcharge de la plage legacy `-32000`** — MCP 2026-07-28 déconseille d'introduire de nouveaux codes dans la plage implementation-defined `-32000..-32019` : l'échec d'un handler de ressource (`resources/read`), de prompt (`prompts/get`) ou du fournisseur de complétion (`completion/complete`), le dépassement de `tool_timeout` et la saturation des threads de handlers (`MAX_INFLIGHT_HANDLERS`) rendaient tous `-32000` — un client conforme ne pouvait distinguer ces défaillances d'aucun autre usage legacy. Toutes rendent désormais le code interne réservé **`-32603`** avec **data structurée** (`data.reason` : `handler_failed` / `timeout` — avec `timeout_ms` — / `overloaded` — avec `max_in_flight`), et la saturation transport (503 « too many connections » / « server saturated ») pareillement. Les erreurs d'outils restent **inchangées** : un échec/timeout/saturation d'outil est un résultat `isError` avec `structuredContent` (`ErrorCode`), jamais une erreur JSON-RPC. *Changement de fil* : `resources/read`/`prompts/get`/`completion/complete` en échec, timeout ou surcharge répondent `-32603` au lieu de `-32000` (la cause précise migre dans `data.reason`) — les verdicts `isError` des outils ne changent pas d'un octet. Tests : codes + data émis pour chaque surface — échec ressource/prompt/complétion, timeout ressource/prompt avec `timeout_ms`, surcharge noyau forcée via le compteur inflight (module interne `tests_codes_erreurs_2026_11_10`), saturation transport 503 (`tests/features.rs`, `tests/mcp_content_surfaces.rs`, `tests/http.rs`) ;

### Corrigé (audit complet du 2026-10-30)

- **`antizombie` — sweep : retraits résilients aux violations de partage** — le dossier d'annonce est **partagé** (MCP Control le lit pendant que les serveurs y écrivent) : sous Windows, un lecteur concurrent (ou un scan antivirus) tenant le fichier quelques millisecondes faisait échouer `fs::remove_file` en violation de partage — l'annonce morte restait un tour de plus. Nouveau `remove_with_retry` : trois tentatives espacées (25/50/75 ms), `NotFound` court-circuité (un sweep concurrent a gagné — même verdict) ; appliqué aux retraits de fantômes **et** de débris corrompus. Écarté au passage par le diagnostic : le budget du sweep ne mesurait qu'une **échéance d'horloge murale** — faussement épuisée quand le scheduler retire le CPU au thread (machine chargée, tests parallèles) alors qu'aucun travail n'a été fait. `SWEEP_BUDGET` mesure désormais le **temps réel passé dans les sondes de port** (seul coût bornable, ≤ `PROBE_TIMEOUT` par sonde) — même borne pratique, insensible à la charge ;
- **LF seul : réponses en `400` au lieu d'une fermeture silencieuse** — le README promettait « fin de ligne LF seul → `400` » mais le transport fermait la connexion **sans répondre** (la ligne lisible mais sans CRLF tombait dans le même chemin qu'une coupure client). Nouvelle variante `LineFail::MalformedEnding` : une ligne UTF-8 lisible mais terminée par un LF nu est désormais répondue en `400` (« invalid line ending … », connexion fermée), au stade de la ligne de requête comme à celui des en-têtes — un client HTTP apprend pourquoi il est refusé ; les coupures réelles et les slowloris restent sans réponse (comme documenté) ;
- **`zod::string().url()` — IPv6 nue (sans crochets) acceptée** : `http://::1/x` et `http://fe80::1/x` passaient la validation (le `rsplit_once(':')` de la partie port fabriquait un hôte dégénéré `":"`/`"fe80:"` qui échappait aux trois seuls contrôles d'hôte). RFC 3986 §3.2.2 : un hôte IPv6 exige des crochets — tout hôte non crocheté contenant un `:` est désormais refusé (les IPv6 crochetées restent acceptées, port inclus) ;
- **README : l'exemple de réponse JSON de « Intégration » ne correspondait plus à la sortie réelle** (ordre des champs trié alphabétiquement, comme produit par un `Value` intermédiaire — la sérialisation actuelle, directe par struct, émet `resultType`, `content`, `structuredContent`, `_meta` et, dans `content`, `type` avant `text`). Exemple corrigé à la sortie exacte, et désormais **épinglé octet pour octet** par un test ;
- **README : compte de tests périmé dans la section « Tests »** (« 289 » au lieu de 313 all-features ; la section SLO, elle, était à jour) ;
- **CHANGELOG : section [0.9.0] dupliquée** — un second bloc « Ajouté »/« Tests » (ancienne formulation du module gateway) et un « Prévu (suite) » en double traînaient après le bloc principal ; nettoyage (+ un « weren't » hérité d'une édition bilingue) ;
- **Tests `antizombie` stabilisés** (flaky CI) : les ports de marqueur venaient de la plage éphémère — Windows les réattribue en LIFO, un `bind("127.0.0.1:0")` parallèle reprenait le port du marqueur entre le `drop` et la sonde du sweep (`alive: 1` parasite). Les marqueurs vivent désormais dans une tranche **sous toutes les plages éphémères** (23100+, même logique que la tranche LiteMCP) que l'OS n'attribue jamais spontanément.

### Documentation

- **README : le jeu par défaut explicité à l'Installation** — une phrase fixe ce que vous obtenez sans aucune feature (tout le noyau, deux dépendances) et ce que les features ajoutent (extras opt-in, coût mesuré) ;
- **README : la section Configuration précise d'emblée que toutes les méthodes du builder sont optionnelles** — chacune a un défaut sûr documenté dans la colonne « Défaut », l'ordre d'appel est libre, et `annotate`/`output_schema` fonctionnent avant ou après l'enregistrement de l'outil ;
- **Annulation MCP : non-prise en charge documentée au lieu d'une annulation fictive** — le flux de réponse SSE/JSON n'est ouvert qu'après calcul complet du handler : une déconnexion client pendant l'exécution n'est détectée qu'à l'écriture finale (une seule écriture, qui échoue alors silencieusement — aucune écriture ultérieure n'existe dans le transport) et **aucun jeton d'annulation n'est exposé aux handlers** ; le handler termine toujours (Rust n'a pas d'annulation de thread) et `notifications/cancelled` n'est pas implémentée — le transport ne revendique pas la conformité d'annulation MCP. Documenté au point d'écriture SSE (`src/http.rs`), dans le doc de module du transport et dans le README (« Handlers bloquants » + choix architectural SSE) ; l'évolution réelle est chiffrée en « Prévu » ;

### Tests

- **`tests/readme_parity.rs` — parité README ↔ code, de bout en bout (vraies sockets)** : la réponse de l'exemple « Intégration » comparée **octet pour octet** à celle documentée ; l'exemple « Concepts » (`tool_parse`) vérifié dans les trois sens — `inputSchema` dérivé exact (`minLength`/`maxLength`/`minimum`/`maximum`/`required`), appel valide exécuté, appel hors schéma refusé en `-32602` avec l'issue citant le chemin du champ et **le handler jamais exécuté** ; l'exemple « Valeurs par défaut » vérifié publié (`default: 10`, champ hors `required`) **et** injecté (absent → 10, présent → 42) ; l'exemple « Dictionnaires » vérifié (`{"a":"x"}` passe, `{"a":1}` refusé avec le chemin `arguments.labels.a`) ; la sonde `GET /health` vérifiée (`200`, `text/plain`, corps « ok »). Tout écart futur entre un exemple documenté et le comportement réel casse la CI avant publication. Suite : **313 tests verts all-features** (295 sans feature optionnelle), clippy `-D warnings`, fmt.

### Prévu (suite)

- **Annulation coopérative des handlers (SSE/HTTP)** — détecter la fermeture du flux **pendant** l'exécution (watcher borné par requête : clone de socket + `peek` non consommant, vie arrêtée au retour du dispatch) et exposer un **jeton coopératif** public (`Arc<AtomicBool>` + `is_cancelled()`) aux handlers : via `InputRound` (MRTR, non-breaking) et une variante d'enregistrement pour outils/ressources/prompts (l'API `Fn(&Value) -> …` actuelle ne transporte pas de contexte — ajouter un paramètre casserait chaque signature publique) ; court-circuiter l'écriture finale quand la déconnexion est confirmée ; jamais de prétention à tuer un thread Rust (le handler termine toujours). Chiffrage : **~2-3 jours** (type public + variantes d'enregistrement + plomberie dispatch/Gateway + tests fermeture client pendant handler bloquant sans aucune écriture ultérieure) ;
- Tests clients réels (MCP Inspector, Claude, Cursor) et corpus de fuzz dédié au parseur ;
- Phase 7 « écosystème entreprise » : guide de déploiement (TLS au gateway/mesh, OIDC à la périphérie), recette Prometheus sur `on_call`, story stateless/scaling.

## [0.10.0] - 2026-10-28

### Ajouté

- **Module `antizombie` — anti-zombie : registre sans fantômes, fin de vie drainée et vérifiable** (opt-in via `.anti_zombie()`) : **(1) au bind**, le registre d'annonce partagé (`%TEMP%/litemcp-registry/`) est passé au peigne fin — une annonce dont le port ne répond plus (`TcpStream::connect_timeout` sur `127.0.0.1`, `PROBE_TIMEOUT` 250 ms) est le fantôme d'un process tué brutalement (son `Drop` n'a jamais couru) : elle est retirée ; un `.json` présent mais non parsable ne peut pas être une écriture en cours (l'écriture d'annonce est atomique par rename) : débris retirés ; les `.tmp` orphelins de plus de 60 s idem ; les annonces d'instances vivantes, les fichiers d'un schéma inconnu et l'annonce du process courant sont intouchés — budget total borné (`SWEEP_BUDGET` 2 s), et `sweep_registry()` / `sweep_dir(dir)` utilisables seuls pour un nettoyage périodique ; **(2) à la fin de vie** du serveur lié (arrêt gracieux **ou** `drop` sans `serve()`), le `Drop` **draine** les threads de connexion — attente bornée (`drain_timeout`, défaut 2 s, `0` = bilan immédiat) jusqu'à zéro connexion vivante — puis publie le bilan ; **(3) le bilan** (`ZombieReport` : `drained`, `connections_pending`, `handlers_alive`, `sweep`) est lisible via `ServerHandle::zombie_report()` après la fin de vie — un handler qui dépasse son `tool_timeout` termine en arrière-plan (Rust ne tue pas les threads) : le rapport le compte au lieu de le taire. Les compteurs utilisés existent déjà (`live_connections`, threads de handler bornés) : sans `.anti_zombie()`, zéro code de plus sur les chemins chauds et `zombie_report()` rend `None`. **16 nouveaux tests** (`tests/antizombie.rs` + unitaires du module) : sweep sur dossiers isolés (morts re-apés, vivants respectés, corrompus retirés, schéma inconnu respecté, propre annonce intouchée, `.tmp` récents respectés, idempotence), bind qui nettoie avant de servir, drain borné avec handler accroché signalé, aucun rapport sans le module, port rebinaible après la fin de vie ;

## [0.9.1] - 2026-10-28

### Ajouté

- **`ServerBuilder::bind_default()` — auto-attribution du port** : bind sur `127.0.0.1` en prenant **le premier port libre** de la tranche LiteMCP `23080..=23099` (nouvelle constante `litemcp::DEFAULT_PORT_RANGE`, prelude inclus) — le deuxième, le dixième… serveur LiteMCP d'une machine démarre sans coordination ni collision, et si les vingt ports sont occupés, repli sur un **port éphémère** alloué par l'OS (jamais d'échec faute de place dans la tranche). Le port effectif reste lisible via `BoundServer::local_addr()` et publié par `.announce()` (MCP Control connaît toujours le port réel, y compris après repli). La validation de route est identique à `.bind(addr)`, qui reste inchangé pour un port explicite — le corps commun (`validate_route` + construction) est partagé par les deux ;

### Modifié

- **Port par défaut des exemples et de la documentation : 23080** (remplace 8080) — le 8080 est le port HTTP-alternatif le plus squatté (proxys, Tomcat, serveurs de dev) ; 23080 est en dessous de toutes les plages éphémères par défaut (Linux `32768–60999`, Windows/macOS `49152–65535`) et non attribué par l'IANA ; la seule ressource connue sur ce port (Soracom Funnel/Beam) est un point d'entrée cloud distant, jamais un socket local. Nouvelles constantes `litemcp::DEFAULT_PORT` (23080) et `litemcp::DEFAULT_ADDRESS` (`127.0.0.1:23080`, prelude inclus) ; la tranche `DEFAULT_PORT_RANGE` (23080–23099) est parcourue automatiquement par `bind_default()` (ci-dessus) ;
- **README recentré sur la nuance lib/serveur** : la section « Démarrage rapide » devient **« Intégration »** — LiteMCP est une bibliothèque, pas un serveur à lancer : rien à installer, rien à démarrer, on ajoute la dépendance à un projet existant et *son programme* devient un serveur MCP ; l'exemple capture `bound` et affiche lui-même l'adresse effective (la lib reste **silencieuse par design** : aucun log, aucun banner, stdout/stderr appartiennent à l'utilisateur).

## [0.9.0] — 2026-10-27

### Ajouté

- **Module `gateway` — registre dynamique (`Gateway`)** : le socle des gateways MCP — un serveur dont la surface d'outils change **pendant qu'il sert** (serveurs détectés qui apparaissent et disparaissent, outils activés à chaud). `Gateway::new(builder)` publie un premier snapshot `ServerCore` immuable ; `edit(|b| …)` reconstruit un noyau neuf puis l'échange atomiquement sous `RwLock` — les requêtes en vol finissent sur le snapshot où elles ont commencé (un lecteur ne voit jamais un catalogue à moitié reconstruit), `snapshot()` donne le handle courant, `dispatch`/`dispatch_with_headers` dispatchent sur le snapshot courant. Deux `edit` concurrents sont sérialisés (verrou réentrant du builder) : **aucune mutation n'est perdue** ; une réentrance (un `edit` dans un `edit`) est refusée par une panique explicite au lieu de figer le thread. Verrous sans inversion (le dispatch ne prend que la `RwLock` en lecture). Le dispatch embarqué (`ServerCore`) se monte tel quel dans la route HTTP d'une app hôte qui possède déjà son socket durci — c'est la recette qu'utilise [MCP Control](https://github.com/DeamonDev888/mcp-control) pour devenir gateway MCP (toute la surface de l'UI en tools + proxy vers les serveurs détectés). Nouveaux exports : `litemcp::Gateway` (prelude inclus) ;
- **`Gateway::edit_batch`** : plusieurs mutations en **une seule** reconstruction et un seul échange de snapshot — le catalogue ne passe jamais par un état intermédiaire (une détection qui annonce plusieurs serveurs est visible d'un bloc) ;
- **`Gateway::remove_resource(uri)` / `Gateway::remove_prompt(name)`** : le pendant retrait de `remove_tool` pour les ressources et les prompts — un serveur détecté qui disparaît emporte aussi sa ressource et son prompt ;
- **`ServerBuilder::remove_resource(uri)` / `ServerBuilder::remove_prompt(name)`** : les mêmes opérations au niveau du builder (utilisables seules) ;
- **`ServerBuilder::tool_names()`** et **`ServerCore::tool_names()`** : introspection du registre sans sérialisation (journaux, tests, gateways qui comparent leur registre à leur état détecté) ;
- **`ServerBuilder::max_response_bytes(max)`** : le plafond de taille d'une réponse JSON-RPC était en dur — il est désormais configurable (`0` = illimité) ;
- **`ServerBuilder::remove_tool(name)`** : retrait d'un outil sans reconstruire le serveur — l'outil disparaît de `tools/list` et répond `-32602`. Les annotations et schémas de sortie **en attente** pour ce nom sont purgés (un ré-enregistrement repart propre).

### Corrigé (audit complet du 2026-10-27 — rapport interne)

- **Deadlock `Gateway::edit`** (CRITIQUE) : la closure de mutation s'exécutait sous le `Mutex` du builder — un `std::sync::Mutex` n'étant pas réentrant, un `edit` qui ré-appelle `edit` sur le même gateway **figeait le thread définitivement** (reproductible à 100 %, aucun message, aucun déverrouillage). La réentrance est désormais détectée et refusée par une **panique explicite et actionnable** ; la sérialisation entre threads distincts et la publication du snapshot restent inchangées (aucune perte de mise à jour) ;
- **Résidu de corps non cadré** (CRITIQUE) : les octets reçus **au-delà** du `Content-Length` annoncé étaient relus comme une ligne de requête au tour suivant — un `400` parasite s'intercalait dans la file de réponses et la vraie réponse du client était perdue. Le résidu est maintenant attribué à la requête qui a mal annoncé sa longueur (`400 request body larger than Content-Length`, connexion fermée) ; le **pipelining légitime** reste servi ;
- **Fuite du timeout de lecture entre requêtes** (MAJEUR) : `tighten_read_timeout` ne faisait que *resserrer* le timeout du socket, jamais le restaurer — une requête lente laissait la connexion keep-alive avec un timeout résiduel minuscule et la requête suivante était **coupée sans réponse** alors que son gap était légal. Le timeout nominal est reposé à chaque tour de la boucle keep-alive ;
- **Doublons d'en-têtes d'identité non refusés** (MAJEUR) : `Authorization`, `Cookie` et `X-Forwarded-For` n'étaient pas dans la liste des en-têtes à valeur unique refusés en `400` — le verdict d'authentification dépendait de l'**ordre de lecture** (première occurrence ici, combinaison ou dernière occurrence chez un reverse proxy) ;
- **Saturation : refus écrit dans la boucle d'accept** (MAJEUR) : la branche « trop de refus en vol » tombait dans le repli inline (écriture du 503 + drain de 50 ms) **dans** la boucle d'accept, alors que la doc annonce une fermeture sans réponse — la latence d'acceptation était multipliée par ~66 pendant une vague de saturation. La branche ferme désormais net, comme documenté ;
- **Feature `schemars` : `default` publié mais jamais injecté** (MAJEUR) : `FromSchemars` implémentait `json()` et `validate()` mais laissait `default_value()`/`apply_defaults()`/`has_defaults()` aux valeurs par défaut du trait `Schema` — un `"default": 7` publié dans l'`inputSchema` n'était **jamais** injecté dans les arguments (le handler recevait un objet privé du champ). Les trois méthodes sont désormais implémentées (injection récursive, résolution des `$ref` locaux, aucune résolution réseau) ;
- **Test concurrent flak** (MAJEUR) : `edit_concurrent_avec_dispatch_ne_perd_aucune_mutation` échouait par intermittence (2 échecs sur 40 runs) — les lecteurs n'étaient pas garantis actifs avant le premier `edit`. Une barrière rend le harnais déterministe (40/40 verts) ;
- **Chiffres du README** : le compte de tests annoncé (« 196 ») était périmé — mesuré et corrigé (289 tests all-features / 271 sans feature) ;
- **Écart doc/code sur le hook `on_request`** : la doc promettait que le hook ne s'exécute pas sur une requête « déjà refusée par le durcissement de transport » ; il s'exécute **avant** le verdict de méthode (l'auth gagne sur le `405`, pour ne pas révéler l'existence de la route). La doc est explicite sur ce point.

### Tests

- **15 tests de régression dédiés** (`tests/audit_2026_10_27.rs`, `tests/zod_gaps_schemars_default.rs` + 3 tests dans le module `gateway`) : chacun rejoue une **sonde qui avait prouvé un bug avant correction** — résidu de corps non conforme, pipelining légitime, fuite du timeout de lecture, doublons d'en-têtes d'identité (×2), réentrance refusée + message actionnable, mutations concurrentes sans perte, `default` `schemars` publié **et** injecté (+ imbriqué, + cas sans défaut) ;
- **9 tests bout-en-bout du gateway** (`tests/gateway_e2e.rs`) : dispatch sur de **vraies sockets** (app hôte qui monte le gateway dans sa route HTTP), cycle de vie dynamique (apparition → disparition → réapparition), cohérence du contrat (tout outil publié est appelable avec les arguments de SON schéma publié, et le schéma est exécuté), isolation des requêtes en vol (une requête en cours reste servie par son snapshot), persistance des ressources/prompts, isolation entre deux gateways, lots (`edit_batch`), retrait de ressource/prompt, résilience (un handler qui panique rend un `-32603` et le gateway continue de servir). Suite : **289 tests verts** (271 sans feature optionnelle), clippy `-D warnings`, fmt.

## [0.8.0] - 2026-10-27

### Ajouté

- **Verrou de requête — `ServerBuilder::on_request()` (types `RequestContext` / `RequestRejection`)** : verdict d'accès par requête HTTP — **auth** (token, signature, ACL) et **rate-limit** en process — exécuté **avant la lecture du corps** et avant le `Expect: 100-continue` : un rejet ne transporte jamais le payload. Le hook reçoit un `RequestContext` (méthode, chemin sans la query, query, **adresse du pair**, en-têtes via `ctx.header(nom)` insensible à la casse — première occurrence reçue ; `Debug` n'imprime que les **noms**, jamais les valeurs : un `Authorization` ne fuit pas dans un log) et rend `Ok(())` ou un `RequestRejection` : `unauthorized` (401), `forbidden` (403), `rate_limited(retry_after)` (429 + en-tête `Retry-After` arrondi au-dessus, minimum 1 s) ou `new(statut, message)` — tout statut hors `400..=599` est ramené à `400` — avec `.with_header(nom, valeur)` pour `WWW-Authenticate` & cie. Les rejets partent avec le message en corps JSON (même enveloppe que le transport), les en-têtes supplémentaires **assainis au départ** (un nom hors jeton RFC 9110 ou une valeur portant un CRLF est écarté : un rejet ne devient jamais une injection d'en-têtes) et la connexion **fermée** (corps non lu, aucune réinterprétation possible). Périmètre du verrou : la **route MCP uniquement** — ni `/health` (sondes k8s sans authentification), ni les routes inconnues (`404`), ni les requêtes déjà refusées par le durcissement de transport (`400`/`431`/`403` Origin/`411`/`413`) ; en revanche **avant** le verdict de méthode (un `GET /mcp` non authentifié reçoit `401`, pas `405`) et avant le contenu (`415`). Paniques de hook contenues (`500` générique sans contenu de panique réfléchi, serveur continue), hooks **cumulables** (exécution dans l'ordre, premier rejet court-circuite), notifications `202` incluses. Sans hook enregistré, le transport ne retient **aucun** en-tête et le chemin chaud reste sans allocation ni copie. Derrière un reverse proxy, `ctx.peer` est l'adresse du proxy — `X-Forwarded-For` ne se croit qu'après avoir vérifié le pair. Nouveaux exports : `litemcp::RequestContext`, `litemcp::RequestRejection` (prelude inclus) ;
- **Intégration MCP Control — `ServerBuilder::announce()` (opt-in) et module `litemcp::registry`** : au bind réussi, le serveur publie une annonce JSON dans le dossier temporaire partagé (`%TEMP%/litemcp-registry/<nom>.json`, sinon `$TMPDIR` ou `/tmp`) : `{ name, version, port, pid, exe, args, cwd, ts }` — **port effectif** du listener (connu même après un bind sur le port 0), PID, exécutable et ligne de commande. Le client [MCP Control](https://github.com/DeamonDev888/mcp-control) peut alors démarrer, arrêter et relancer les serveurs LiteMCP nativement, sans scan de ports ni saisie manuelle. L'annonce vit dans le `Drop` de `BoundServer` : elle est retirée à l'arrêt gracieux **comme** après un simple `drop` d'un serveur lié jamais servi — tout chemin de fin de vie nettoie. Opt-in strict : sans `.announce()`, zéro code exécuté et zéro coût ;
- **Durcissements d'audit de l'annonce** : noms dangereux refusés (vide, `..`, séparateurs `/` et `\` **et `:`** — sous Windows, `PathBuf::join("C:nom.json")` remplace la base du chemin et `nom:flux` écrirait un flux NTFS alternatif : l'annonce sortirait du dossier de registre) ; écriture **atomique** — fichier temporaire créé **exclusivement** (`create_new`) puis `fs::rename`, qui remplace la destination **sans la suivre** (un symlink planté dans ce dossier partagé ne détourne jamais l'écriture, et aucun lecteur ne voit jamais un JSON partiel) ; permissions **`0600` sous Unix** (le contenu publie exe/argv/cwd) ; retrait **conditionné au couple `(pid, port)`** — `retract` ne supprime le fichier que si le `pid` **et** le `port` qu'il porte sont ceux de l'instance appelante : une instance plus récente du même nom (autre process ou autre port) survit à l'arrêt d'une ancienne, et un fichier illisible ou corrompu est laissé au jugement de la sonde de vie du lecteur ;
- **Erreur de compilation explicite des macros sur fonction générique** : `#[tool]` / `#[prompt]` appliqués à une `fn` générique (`fn f<T>(…)`) rendent désormais un diagnostic qui nomme la cause, au lieu du message générique « type non supporté » portant sur le paramètre dérivé du type générique.

### Corrigé

- **Conformité JSON-RPC 2.0 §4 — une notification ne reçoit jamais de réponse** : une notification (sans `id`) dont `Mcp-Method` / `Mcp-Name` contredit le corps recevait une erreur `-32020` en réponse (effet du contrôle d'en-têtes passé avant le retour anticipé des notifications en 0.7.1). Elle est désormais traitée comme toute notification : **aucune réponse** (silence, `202` sans corps) — l'observation via le hook `on_call` reste émise ;
- **Conformité RFC 9110 §12.5.1 — `q=0.0000…` reconnu comme poids nul** : la négociation `Accept` ne reconnaissait `q=0` qu'avec au plus trois décimales (`0`, `0.0`, `0.00`, `0.000`) — `q=0.0000` (et au-delà) était lu comme un poids **non nul**, laissant acceptable un type de média que le client avait expressément désactivé. Toute suite de décimales zéro est désormais reconnue comme poids nul.

### Tests

- **252 tests** au total, dont **11 dédiés au verrou de requête** (vraies sockets : bearer OK/KO, `/health` et routes inconnues exemptes, contexte complet vu par le hook, 429 + `Retry-After`, panique contenue puis reprise du service, injection CRLF écartée, `401` avant `405`, hygiène keep-alive de la rétention d'en-têtes, ordre des hooks multiples, clamp de statut hors 4xx/5xx, notifications incluses).

## [0.7.1] - 2026-10-25

### Documentation — comparatif 9 frameworks, README recentré sur la lib

- **Comparatif des 9 frameworks MCP** (campagne du 2026-10-25, même démo de 4 outils, client neutre unique, Windows 11 loopback, 3 passes) : LiteMCP `0.7.0` face à `tower-mcp` 0.25, `mcp-go` 1.3, `rust-mcp-sdk` 2.0, `rmcp` 3.7, `vitemcp` 1.9.7 (SSE et JSON), `mcp` Python 2.3 et `fastmcp` 4.1.3. En keep-alive (médiane de 3 passes) : **0,075–0,091 ms** pour LiteMCP, soit ×2,1 tower-mcp, ×2,2 mcp-go, ×4,4 rust-mcp-sdk, ×5,5 rmcp, ×22–26 vitemcp, ×31 le SDK Python, ×45 FastMCP ; seuls LiteMCP, tower-mcp et mcp-go laissent la connexion réutilisable (2000/2000 contre 0/2000 pour les transports SSE) ;
- **README** : section bench volontairement courte (3 lignes de résultats + renvoi vers `docs/BENCH.md`), le reste du document reste centré sur la lib ; titre et description alignés sur « la lib MCP la plus légère en mémoire et la plus rapide au monde » ;
- **[`docs/BENCH.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/BENCH.md)** : méthode, définitions des métriques, planchers de transport (TCP nu 0,060–0,067 ms · WebSocket 0,065–0,082 ms · socket local 0,032–0,037 ms · HTTP 0,075–0,091 ms), queue de distribution (p99,9 0,64–0,97 ms, max 1,03–1,17 ms) et budgets temps réel, mémoire/binaire, pièges de mesure, campagnes écartées et **portée exacte** des chiffres ; graphique `docs/bench-arena-2026-10-25.svg` ;
- **Deux enseignements de banc** : `rmcp` et `rust-mcp-sdk` exigent les en-têtes 2026-07-28 (`Mcp-Method`, `Mcp-Name`, `_meta`) pour un `tools/call` sans session, et **tower-mcp voit sa mémoire croître avec le volume** (5,8 Mo à froid → 144–183 Mo après ≈7 000 requêtes, reproduit deux fois) ;
- **Banc reproductible hors crate** : `litemcp-it/bench_arena.sh` + `arena_aggregate.py`, serveurs de démonstration dans `mcp-bench-arena/`, planchers dans `examples/bench_floor`, `bench_ws` et `bench_pipe` ;
- **Paquets `.crate` officiels** attachés à chaque release GitHub, SHA-256 vérifié contre crates.io.

## [0.7.0] - 2026-10-25

### Ajout — erreurs d'outil structurées, observabilité et anglais sur le fil

- **Erreurs d'outil structurées (`ToolError`, `ErrorCode`)** : un échec d'outil (`CallToolResult::isError`) porte désormais à la fois le message en texte (`content[0].text`) et un objet machine-lisible sous `structuredContent.error` (`code`, `message`, `retryable`, `retryAfterMs` optionnel). Constructeurs par catégorie (`rate_limited`, `timeout`, `upstream`, `invalid_input`, `unauthorized`, `not_found`, `internal`, `unclassified`) ; `From<String>` / `From<&str>` préservent l'ergonomie de `?` sur des erreurs texte ;
- **Hook d'observabilité (`ServerBuilder::on_call`)** : callback cumulable appelé après le traitement complet de chaque message JSON-RPC avec `CallEvent { method, name, duration, status }` (`CallStatus::Success`, `ToolFailure(ErrorCode)`, `ProtocolError(i64)`, `Notification`). Les paniques de hook sont contenues ;
- **Messages de validation et de protocole en anglais** : toutes les erreurs envoyées sur le fil (`zod`, `schemars`, JSON-RPC, transport HTTP) sont désormais rédigées en anglais.

### Correctifs — audit complet du 2026-10-25

- **[P2] Macros `#[tool]` / `#[prompt]` — littéraux par défaut négatifs et validation stricte des littéraux** : `default(offset = -5)` ou `default(temp = -2.5)` générait `-(::serde_json::Value::from(5i64))` qui ne compilait pas (`Value` n'implémente pas `Neg`), et `is_json_literal` acceptait tout `Expr::Lit`/`Expr::Unary` (transformant caractères/octets en `null` ou ignorant `!`). Le signe `-` est désormais parsé dans le littéral numérique (`i64`/`f64`) et les doublons `default(a = 1, a = 2)` sont refusés à la compilation ;
- **[P2] Macros `#[tool]` / `#[prompt]` — bornes exactes par type entier** : tous les types entiers (`u8`..`u64`, `usize`, `i8`..`i32`, `isize`) publiaient auparavant les bornes `i64` et castaient par `as #ty` (un `-1` passé à un `u64`/`usize` devenait silencieusement `u64::MAX`, `300` sur `u8` devenait `44`). Chaque type publie et applique désormais ses bornes exactes (`.min(…)`/`.max(…)`) ;
- **[P2] `prompt_typed` et `#[prompt]` — injection des défauts et publication de `arguments` dans `prompts/list`** : `get_prompt` validait le schéma mais n'appelait jamais `schema.apply_defaults(&mut arguments)`, et `prompts/list` n'exposait pas les `PromptArgument` (`name`, `description`, `required`) dérivés du schéma ;
- **[P2] `zod::describe` et `zod::nullable` — conservation de `default_value()` et propagation récursive** : `Described` et `Nullable` n'implémentaient pas `Schema::default_value()`, ce qui marquait un champ `describe("…", s.default(v))` ou `nullable(s.default(v))` comme `required` et empêchait l'injection du défaut. `Obj::apply_defaults` propage désormais aussi récursivement les défauts internes lorsqu'un objet par défaut est injecté ; `Record::default(v)` et `#[derive(Clone)]` sur `Nullable`/`Described` sont ajoutés ;
- **[P2] Paniques de handlers sans `tool_timeout`** : lorsque `tool_timeout` valait `None` (défaut), une panique de handler remontait hors de `ServerCore::dispatch` et court-circuitait tous les hooks `on_call`. `ServerCore` attrape désormais les paniques dans `call_tool` et `run_bounded` dans tous les modes (`-32603` avec `id` préservé et notification de `on_call`) ;
- **[P2] `ServerHandle::shutdown()` sur `0.0.0.0` / `[::]` (Windows)** : lorsque le listener était lié à une adresse non spécifiée (`0.0.0.0:port` ou `[::]:port`), `connect_timeout` vers `0.0.0.0` échouait immédiatement sous Windows (`AddrNotAvailable`) sans réveiller `accept`. `BoundServer::handle()` substitue désormais l'adresse de bouclage (`127.0.0.1` / `::1`) ;
- **[P3] En-têtes de routage `Mcp-Method` / `Mcp-Name` (SEP-2243)** : vérifiés désormais avant le retour anticipé des notifications, et `Mcp-Name` est exigé cohérent même lorsque `params.name` est omis ou vide sur `tools/call` et `prompts/get` ;
- **[P3] `zod::Format::Datetime` (`is_datetime`)** : l'offset de fuseau `±HH:MM` vérifie désormais `HH <= 23` et `MM <= 59` (RFC 3339 §5.6) ;
- **[P3] Messages sur le fil & CI** : traduction des derniers messages français résiduels (`http.rs`, `schemars.rs`), mise en cache `OnceLock` de `resources/list` et `prompts/list`, et publication préalable automatique de `litemcp-macros` dans `.github/workflows/publish.yml`.

### Performance & Dépendances — `litemcp-macros` zéro dépendance et chemin chaud

- **`litemcp-macros` 100 % zéro dépendance externe** : suppression complète de `syn`, `quote` et `proc-macro2` dans `litemcp-macros/Cargo.toml` (`[dependencies]` vide — uniquement `proc_macro` fourni par `rustc`) ;
- **`litemcp-macros` 0.4.0** : la 0.3.0 déjà publiée sur crates.io embarque `syn`/`quote`/`proc-macro2` et ne peut pas être réécrite — le bump est la seule façon de livrer la réécriture zéro dépendance aux consommateurs ;
- **Sérialisation directe des réponses (`Outgoing::success_vec`)** : élimination de l'arbre `serde_json::Value` intermédiaire (`serde_json::to_value`) sur `tools/call`, `resources/read` et `prompts/get`, suppression des `.clone()` sur les catalogues mis en cache (`tools/list`, `resources/list`, `prompts/list`, `server/discover`) et mise en cache `OnceLock` de `initialize`, `ping` et `_meta` (`serverInfo` précalculé une seule fois à la construction de `ServerCore`) ;
- **Transport HTTP & JSON-RPC sans allocations superflues** : extraction par déplacement de `method` (`object.remove("method")` dans `jsonrpc.rs`), vérification de `Content-Type` par booléens sans allocation de `String`, réutilisation du buffer `body` sur la connexion keep-alive via `BufRead::fill_buf` (suppression du buffer temporaire `chunk = vec![0u8; …]`), et pré-allocation de capacité dans `zod::Path::root`.

### Durcissement — plafonds de ressources

- **`MAX_INFLIGHT_HANDLERS` (256)** : quand `tool_timeout` est configuré, un handler au-delà du plafond de threads simultanés rend `rate_limited` (outil) ou `-32000` (ressource/prompt) au lieu de créer un thread noyau ; le slot reste réservé jusqu'à la terminaison effective du thread détaché (y compris après timeout) ;
- **`MAX_RESPONSE_BYTES` (16 Mio)** : une réponse JSON-RPC sérialisée qui dépasse ce plafond est remplacée par `-32603` (« response size … exceeds maximum … ») — un handler qui rend une valeur démesurée ne peut plus pousser un corps illimité.

### Tests

- **230 tests** au total (56 unitaires dont le plafond d'inflight, 19 `audit_2026_10_25`, 15 `http_security_audit`, 10 `audit_2026_10_24`, 11 `zod_gaps_2026_10_24`, 9 features, 3 fuzz proptest, 81 HTTP sur vraies sockets dont le plafond de réponse, 9 macros, 3 schemars, 5 smoke, 9 doctests — 3 doctests `ignore`).

### Mesures — campagne 2026-10-25 (libs à jour, client neutre)

- **vs `@vitemcp/server` 1.9.7** (3 passes alternées, Windows 11, loopback, `litemcp-it/examples/bench_remote`) : keep-alive **0,073–0,082 ms** contre 1,16–1,33 ms (**~16–17×**), p99 **0,16–0,23 ms** contre 3,0–3,8 ms, **2000/2000** connexions réutilisées contre **0/2000**, **1 968–2 122 conn/s** neuves contre 521–737, et **5,8 Mo** de WorkingSet contre 179–228 Mo. La 1.9.7 améliore son propre score d'environ 20 % en latence et 50 % en mémoire par rapport à la campagne du 2026-10-22 (1,43–1,73 ms, ~417 Mo) sans changer l'écart structurel ;
- **non-régression côté route maison** : `examples/bench_backends` sur 0.7.0 → **0,087 ms** de moyenne, p50 0,077 / p99 0,325 ms, **1 983 conn/s** — identique à la 0.5.1.

## [0.6.0] - 2026-10-24

### Ajout — comblement des trous du DSL Zod

Quatre manques signalés à l'usage, tous comblés :

- **Valeurs par défaut** — `.default(v)` attache une valeur par défaut à n'importe quel schéma (`string`, `integer`, `number`, `boolean`, `enum`, `array`, `object`, `record`, `nullable`, `describe`). Le défaut est **publié** (mot-clé `default` du JSON Schema) **et appliqué** : le champ devient optionnel (plus dans `required`) et la valeur est **injectée** dans les arguments **avant** l'exécution du handler (comme Zod). Le défaut est lui-même validé contre son schéma — un défaut incohérent est une erreur de programmation, refusée à la validation. Les défauts s'appliquent **récursivement** (un objet imbriqué porte ses propres défauts) ;
- **`record(valeur)`** — objet à **valeurs typées** (dictionnaire) : chaque valeur est validée par le schéma interne, les clés sont libres. Publié `{"type": "object", "additionalProperties": <schéma>}` ; les chemins d'issue pointent la clé fautive (`arguments.labels.a`) ;
- **Énumération d'entiers** — `integer_enumeration(&[1, 2, 3])` publie `{"type": "integer", "enum": [1, 2, 3]}` (la variante `enumeration` reste limitée aux chaînes, comme avant) ;
- **Schéma JSON Schema brut** — `schemars::from_json_schema(document)` prend un document JSON Schema écrit à la main et le branche comme n'importe quel schéma : il est **publié tel quel** et **appliqué** par le validateur `jsonschema`. Couvre ce que le DSL n'exprime pas (`oneOf`, `not`, `patternProperties`, `if`/`then`/`else`, `$ref` internes…). Le document doit être un objet ou un booléen JSON Schema — sinon panique de développement.

### Ajout — valeurs par défaut dans les macros

- `#[tool(default(limit = 10, mode = "fast"))]` — les valeurs par défaut sont déclarées **au niveau de la macro** (Rust interdit les attribute macros sur les paramètres d'une fonction). Le champ devient optionnel, la valeur est publiée **et injectée** si elle est absente des arguments. La valeur doit être un **littéral** (elle est publiée en JSON) ; un `default(nom = …)` qui ne désigne aucun paramètre est refusé à la compilation.

### API

- `Schema` gagne trois méthodes : `default_value()` (la valeur par défaut), `apply_defaults()` (injection récursive) et `has_defaults()` (permet d'éviter toute copie des arguments quand aucun défaut n'est à injecter — le chemin nominal reste **zéro allocation**).

### Documentation

- **Coût réel des features, mesuré et publié** — le tableau du README compare les trois jeux (défaut / `+ macros` / `+ schemars`) sur un **projet consommateur externe**, feature réellement utilisée : 19 → 20 → 70 crates, 147 Kio → 214 Kio → **6,0 Mio**. Le point clé : `schemars` coûte +5,9 Mio de binaire quand on s'en sert (le linker élimine tout si on l'active sans l'utiliser) — c'est l'argument qui tranche pour l'embarqué, pas le décompte de crates.

### Tests

- **168 tests** (13 de régression dédiés : `tests/zod_gaps_2026_10_24.rs` pour le DSL, +1 dans `tests/macros.rs` pour les valeurs par défaut des macros).

## [0.5.1] - 2026-10-24

### Correctifs — audit complet 2026-10-24

- **`outputSchema` réellement atteignable** (P2, conformité MCP) : l'enveloppe invariante `{ "result": … }` rendait le `structuredContent` **non conforme au schéma publié** dès que le schéma n'était pas un objet — un `outputSchema` scalaire ou tableau ne pouvait donc **jamais** réussir (l'outil renvoyait toujours `isError`). Le noyau choisit désormais la forme selon le schéma publié : objet → valeur brute ou enveloppe ; racine non-objet → valeur brute (la spec `2026-07-28` / SEP-2106 autorise toute valeur JSON dans `structuredContent`). **Comportement observable modifié** : un outil à `outputSchema` scalaire/tableau reçoit maintenant `structuredContent: 42` au lieu de `{"result": 42}` ;
- **`format` appliqué par l'adaptateur `schemars`** (P2, promesse « schéma exécuteur ») : `format` est une simple annotation en JSON Schema 2020-12 — le validateur l'**ignorait**, donc un `format: "email"` publié par `schemars` n'était **jamais vérifié**. Le validateur est désormais compilé avec `should_validate_formats(true)` : `email`, `uuid`, `date-time`, `uri`… sont appliqués comme dans le module `zod` ;
- **Ré-enregistrement d'un outil ne perd plus ses métadonnées** (P3) : `.tool("x", …).tool("x", …)` effaçait silencieusement l'`outputSchema` et les annotations déjà déclarés — le `tools/list` publié ne décrivait alors plus l'outil réellement servi. Les déclarations en attente **et** celles de l'outil précédent sont désormais conservées ;
- **Notifications d'action refusées explicitement** (P3, écart JSON-RPC 2.0 §4) : une notification `tools/call` / `resources/read` / `prompts/get` (sans `id`) ne peut **pas** être traitée en mode stateless (le résultat serait invisible) — le noyau refuse désormais explicitement (`-32600`, « notification refusée : … ») au lieu du `202` silencieux qui laissait croire à une exécution. Les notifications de protocole (`notifications/*`) restent acceptées et ignorées (202 sans corps) ;
- **`Box<S>` / `Arc<S>` / `&S` implémentent `Schema`** (API additive) : impossible de stocker des schémas **hétérogènes** sans wrapper manuel — `Box<dyn zod::Schema>` ne satisfait pas `S: Schema`. Les schémas dynamiques (issus d'une config, d'une base, d'un registre) sont désormais utilisables directement ;
- **`CallToolResult::from_structured`** (API) : constructeur public pour un résultat de succès à partir d'un contenu structuré donné (le texte reflète la valeur réellement envoyée) ;
- **Forme du `structuredContent` précalculée** (performance) : le schéma de sortie était re-sérialisé à **chaque** appel d'outil pour déterminer la forme de la sortie — le calcul est désormais fait **une fois** à l'enregistrement (`ToolDef::output_object_rooted`) ;
- **Compte de tests du README** (P3) : « 109 tests » / « 134 tests » → **154** ;
- **Chiffres du README re-mesurés pour 0.5.1** (P3) : la taille de binaire annoncée (385 Ko) et la latence ne correspondaient plus au code courant — remesuré sur la machine de release : binaire **minimal 148 Kio** (151 552 o), démo `hello` **596 Kio** (610 816 o), keep-alive **0,077–0,086 ms** (3 passes), p50 0,062–0,065 ms, connexions neuves **1 576–1 759 conn/s**, démarrage in-process **5,4 ms**. Les campagnes Linux du 2026-10-24 restent citées à part (serveur dédié) ;
- **Exemple `curl` du README rendu exact** (P3) : la réponse montrait une forme abrégée (sans `resultType` ni `_meta`) — désormais identique à la sortie réelle ;
- **Lien `docs/ROADMAP.md` pointé sur GitHub** (P3) : `docs/` est exclu du paquet crates.io (`exclude = [".github", "docs"]`) — le lien relatif était mort sur crates.io et docs.rs ;
- **Command Center déplacé en fin de README** (P3) : c'est un **client** consommateur de la lib, pas un composant — la section le présentait au milieu du document, comme une intégration de la lib ;
- **Suite de tests** : 10 tests de régression dédiés (`tests/audit_2026_10_24.rs`) couvrant les cinq correctifs ci-dessus.

### Constatés documentés (choix assumés)

- **`Content-Length` avec zéros initiaux** (`0002`) accepté (P3, conformité RFC 9112 §6.3 : la valeur est un entier, `0002` == 2) ; `+2` est refusé (`400`) ;
- **Pas de plafond sur la taille d'une réponse** (Info) : une sortie d'outil de 4 Mio est servie en entier (`Content-Length` explicite, mémoire proportionnelle) — le plafond de 1 Mio porte sur le corps **de requête** ;
- **`output_schema` exige un schéma `'static`** (Info) : un schéma construit à partir d'une configuration runtime (durée de vie courte) doit être possédé (`Box`/`Arc`) pour être attaché — les implémentations `Box`/`Arc`/`&S` facilitent cette possession.

## [0.5.0] - 2026-10-24

### Zod — durcissement & optimisation (audit du module, 2026-10-24)

- **Bornes cassées impossibles à publier** : `min > max` (integer/string/number/array), bornes non finies (`NaN`/±∞ sur `number()` — `serde_json` les aurait publiées en `null`, borne jamais appliquée), `enumeration` vide (insatisfaisable) ou doublonnée (meta-schema JSON Schema exige des éléments uniques) → **panique de développement au démarrage**, comme le doublon de champ ;
- **Chemin de validation zéro allocation** (`zod::Path`) : le chemin était alloué (`format!`) par champ et par élément de tableau **même quand tout était valide** (12 allocations pour un appel valide à 2 champs + 10 éléments) — désormais un buffer unique étendu/tronqué, une chaîne n'est construite qu'en émettant une issue. *Breaking* : `Schema::validate` prend `&mut Path` au lieu de `&str` (les implémentations custom de `Schema` doivent s'aligner) ;
- **`tools/list` et `server/discover` ne resérialisent plus rien** : le catalogue est immuable après `bind()`/`core()` — le JSON est calculé une fois (`OnceLock`) puis servi tel quel.

### Ajout — zod et écosystème

- **Formats de chaîne exécutés** : `.email()`, `.uuid()`, `.url()`, `.datetime()` — publiés `format` (`email`/`uuid`/`uri`/`date-time`, mots-clés JSON Schema 2020-12) **et appliqués** (`date-time` vérifie le calendrier : jours par mois, années bissextiles, fuseau obligatoire RFC 3339 ; aucun regex, aucune dépendance) ;
- **`zod::nullable(schéma)`** : accepte `null` en plus du schéma interne, publié `anyOf: [schéma, {"type":"null"}]` — compose avec tout, sans changer la cardinalité (`.field` reste requis, `.opt` optionnel) ;
- **`zod::describe("…", schéma)`** : description publiée dans le JSON Schema ;
- **Feature opt-in `schemars`** : l'adaptateur `from_schemars::<T>()` rend n'importe quel type `#[derive(JsonSchema)]` utilisable partout où l'API attend `impl zod::Schema` — le schéma publié est celui de schemars, la validation applique **le même document** via le validateur `jsonschema` (erreurs mappées en issues bornées, chemins lisibles). Deux deps de plus (`schemars`, `jsonschema`), uniquement si activée ; le cœur reste à `serde` + `serde_json`.

### Documentation
- Lien réciproque vers le [MCP Command Center](https://github.com/DeamonDev888/mcp-command-center) — client qui détecte les serveurs litemcp par fingerprint (badge dédié) — section « Client — MCP Command Center », **placée en fin de README** (c'est un client consommateur, pas un composant de la lib).

## [0.4.1] - 2026-10-24

### Ajout — suite de la feuille de route

- **Macros `#[resource]` et `#[prompt]`** (feature `macros`) : même mécanique de dérivation que `#[tool]`. `#[resource]` prend une fonction sans paramètre (attributs `uri` requise, `name`, `description` requise) et génère l'enregistrement `.resource(…)` ; `#[prompt]` dérive le schéma des types des paramètres et valide les `arguments` d'un `prompts/get` **avant** l'exécution du handler.
- **`.prompt_typed(name, description, schéma, handler)`** : la variante validée de `.prompt()` — hors schéma → `-32602` avec `error.data.issues`, le handler ne tourne jamais (même contrat que `tool_typed`).
- **Fuzz du transport HTTP (proptest, dev-dépendance uniquement)** : trois propriétés sur serveur réel — (1) des octets arbitraires ne produisent que des réponses HTTP **complètes et correctement cadrées** (status line, en-têtes terminés, `Content-Length` exact) ou rien ; (2) des requêtes « réalistes » (verbes/chemins plausibles, en-têtes piégeux : duplications, obs-fold, `Transfer-Encoding`, `Content-Length` menteurs, séparateurs TAB) respectent le même invariant et le serveur **survit** à chaque cas ; (3) toute requête JSON-RPC valide reçoit exactement une enveloppe bien formée (`jsonrpc: "2.0"` + `result`|`error`, code numérique). 144 cas générés par exécution de la suite.

### Mesures — campagne SLO Linux (conditions figées)

**Suite complète sur Linux** : 134/134 tests (Ubuntu 26.04 LTS, kernel 7.0.0-38, 8 cœurs, rustc 1.99.1) — y compris le fuzz proptest et les sockets. Nouvel exemple **`examples/slo`** (démarrage in-process, 2 300 requêtes keep-alive warmup déduit, `VmRSS` avant/après via `/proc/self/status`) : la campagne est reproductible par n'importe qui (`cargo run --release --example slo`).

| Métrique | Windows 11 | Linux (Ubuntu 26.04, 8 cœurs) |
|---|---|---|
| keep-alive moyenne / p99 | 0,065–0,070 / 0,095–0,123 ms | **0,031–0,033 / 0,048–0,065 ms** |
| Connexions neuves | 848–1 257 conn/s | **4 400–5 041 conn/s** |
| Mémoire après 2 300 requêtes | 5,5 Mo (WorkingSet) | **2,7 Mio `VmRSS`, +4 Kio pendant la charge** |
| Démarrage in-process (bind + 1ʳᵉ réponse) | — | **~0,4 ms** |
| Binaire release stripped (hello) | 385 Ko | 687 Ko |

### CI — protection de la branche `main`

- Workflow : groupe `concurrency` (les runs supplantés d'une même branche/PR sont annulés) ; les jobs `test`/`msrv`/`advisories` se déclenchent automatiquement sur **tout push et toute pull request**.
- Protection de branche `main` : les trois checks CI sont **requis**, une **approbation** est exigée avant merge (reviews périmées rejetées sur nouveau push), histoire linéaire requise — les admins conservent l'accès direct.

## [0.4.0] - 2026-10-24

### Ajout — le lot « gros ROI » de la feuille de route

- **Arrêt propre de `serve()`** (ferme le point B16 des audits) : `bound.handle()` rend une `ServerHandle` cloneable ; `handle.shutdown()` (idempotent) réveille la boucle d'`accept` par une socket factice → `serve()` rend `Ok(())`, et chaque connexion vivante se ferme **entre deux requêtes** (une lecture bloquée en cours ferme au `read_timeout`). La requête en cours termine toujours proprement — les threads ne sont pas tués.
- **Timeout d'exécution des handlers** (`.tool_timeout(duration)`, défaut illimité) : un handler d'outil qui dépasse rend un résultat `isError` (le résultat timeout reste un `result` complet : `resultType`, `_meta`), un handler de ressource/prompt une erreur `-32000` (plage implementation-defined). **Limite assumée et documentée** : le thread du handler n'est pas tué (Rust n'a pas d'annulation de thread) — il termine en arrière-plan, résultat ignoré ; en mode borné, une panique de handler est rendue en `-32603` et les arguments d'un `tools/call` sont clonés une fois par appel.
- **`outputSchema`** (`.output_schema("outil", schéma)`) : publié dans `tools/list` et **exécuteur** comme l'input — la valeur `structuredContent` réellement envoyée est validée avant de partir ; non conforme → résultat `isError` qui cite le `outputSchema`. Ordre d'appel indépendant (même mécanique que `.annotate()`).
- **Resources** : `.resource(uri, name, description, handler)` — `resources/list` et `resources/read` implémentées avec les indices SEP-2549 (`ttlMs`/`cacheScope`) et l'identification SEP-2575 (`_meta` serverInfo) ; uri inconnue → `-32602`, handler en échec → `-32000`.
- **Prompts** : `.prompt(name, description, handler)` — `prompts/list` et `prompts/get` (messages `user`/`assistant` via `PromptMessage::user`/`::assistant`), `arguments` objet exigé (même contrat que `tools/call`).
- Les **capacités** `resources`/`prompts` sont annoncées dans `server/discover` et `initialize` seulement quand leur registre n'est pas vide (une capacité annoncée est une capacité servie).
- **Macro `#[tool]`** — nouvelle crate `litemcp-macros` (workspace), feature opt-in `macros` : le schéma d'entrée est **dérivé des types des paramètres** d'une fonction ordinaire (`i64`→integer, `f64`→number, `String`→string, `Option<T>`→champ optionnel, `Vec<T>`→array), la macro génère `register_<fonction>` à enchaîner via `.with_tool(…)`. Le cœur garde exactement **2 dépendances** : `syn`/`quote` ne sont que des deps de compilation activées par la feature. Aucune exigence nouvelle pour le consommateur (pas besoin de `serde` direct : l'extraction est générée, pas désérialisée).
- `ToolResult` ré-exporté à la racine (`litemcp::ToolResult`) — le type de retour documenté de la macro.
- CI alignée : `--workspace --all-features` sur clippy/test, job MSRV sur le workspace.

### Sécurité & conformité — audit du 2026-10-24

Audit complet (statique + sondes dynamiques + vérification du changelog officiel MCP `2026-07-28` en ligne). **Aucune vulnérabilité critique ou haute** ; les correctifs de l'audit du 2026-10-23 sont vérifiés présents et couverts (rapport d'audit interne, non publié). Les 6 nouveaux constats (P3/Info) sont corrigés :

- **[P3] `serverInfo` dans chaque résultat** (SEP-2575 : les serveurs *SHOULD* s'identifier dans le `_meta` de chaque résultat) : `tools/list` et `tools/call` — succès **et** `isError` — portent désormais `_meta["io.modelcontextprotocol/serverInfo"]`, comme `server/discover` et `initialize` ;
- **[P3] `ping` porte `resultType` + `_meta`** : la promesse « `resultType` dans chaque résultat » devient vraie sans exception (méthode legacy, résultat enrichi — additif pour les clients `2025-11-25`) ;
- **[P3] En-têtes à valeur unique dupliqués refusés** (`Content-Type`, `Mcp-Method`, `Mcp-Name` → `400`) : le verdict dépendait de **l'ordre** (prouvé : `Content-Type: application/json` puis `text/plain` → `415`, ordre inverse → `200` ; `Mcp-Method: tools/list` + `Mcp-Method: ping` accepté silencieusement) — dernier-gagne divergent d'un proxy qui combine les valeurs ou retient la première. Même traitement que `Origin`/`Host` ;
- **[P3] Route validée au bind** : `.route()` doit commencer par `/`, ne pas contenir de `?` et ne pas masquer `/health` — sinon `InvalidInput` au `bind` au lieu de démarrer un serveur **silencieusement injoignable** (prouvé : route `/health` → tout `POST` prenait le `405` de la sonde) ;
- **[P3] `structuredContent` : justification périmée corrigée** — l'enveloppe objet (`{ "result": … }` pour les non-objets) est désormais documentée comme un **choix d'API** ; la spec `2026-07-28` (SEP-2106) autorise en effet toute valeur JSON, l'exigence d'objet date d'avant. Comportement inchangé, promesse honnête ;
- **[Info] CI** : `cargo clippy --all-targets -- -D warnings` (les tests et exemples sont désormais lintés en CI, comme localement) + job `msrv` sur la toolchain 1.85 déclarée dans `Cargo.toml`. Au passage, le commentaire du job `advisories` est rendu exact : `Cargo.lock` n'étant pas versionné (convention lib), `cargo-deny` résout les dépendances à la volée — la vérification porte sur les versions compatibles les plus récentes, pas sur un lockfile committé ;

### Tests

- Avec le lot « gros ROI » ci-dessus : **128 tests au total** (27 unitaires, 8 features, 80 HTTP sur vraies sockets, 3 macros, 5 smoke, 5 doctests) — 15 nouveaux pour verrouiller les cinq fonctionnalités (timeout, outputSchema, resources, prompts, arrêt propre, macro).

## [0.3.3] - 2026-10-23

### Conformité MCP 2026-07-28 — la spec est maintenant implémentée **entièrement**

La lib annonçait viser la spec `2026-07-28` mais implémentait encore la surface *legacy* `2025-11-25`. La spec `2026-07-28` a changé de modèle (stateless, sans handshake, version par requête) — l'écart est désormais comblé :

- **`server/discover`** (SEP-2575, **MUST**) : implémentée — annonce `supportedVersions`, `capabilities` et `serverInfo` sous `_meta["io.modelcontextprotocol/serverInfo"]` ;
- **`resultType` obligatoire** (SEP-2322) : présent dans **chaque** résultat (`"complete"`) ;
- **`ttlMs` / `cacheScope`** (SEP-2549) : présents sur `tools/list` (`60000` / `"private"`) ;
- **Gestion de version** (SEP-2575) : la version est lue dans `_meta["io.modelcontextprotocol/protocolVersion"]` ; une version **explicite** non supportée est refusée en `-32022` (`UnsupportedProtocolVersionError`) avec `data.supported` / `data.requested`. L'**absence** de version est tolérée (compatibilité `2025-11-25`) ;
- **En-têtes de routage** (SEP-2243) : `Mcp-Method` et `Mcp-Name` sont acceptés et **vérifiés contre le corps** de la requête ; incohérents → `-32020` (`HeaderMismatch`) ;
- `initialize` et `ping` (retirées de la spec) restent **tolérées** comme extensions legacy.

### Sécurité — correctifs de l'audit du 2026-10-23

Audit statique (code intégral) + revue indépendante croisée + **vérifications dynamiques sur serveur réel** (build release, config par défaut). Un **P1** et quatre **P2** corrigés :

- **[P1] Désynchronisation de la file sur `/health`** : la branche `health_probe` répondait **sans consommer le corps**. Le corps résiduel devenait la requête suivante — un `GET /health` avec un `Content-Length` et un corps contenant une `POST /mcp` produisait **2 réponses** et **exécutait le corps caché** (reproduit : `id: 42`). Correctif : **un corps est refusé sur la sonde** (`400`, fermeture) — plus simple et plus sûr qu'un drain, qui aurait obligé à attendre des octets pouvant ne jamais arriver (un `Content-Length: 1048576` sans corps retenait un slot jusqu'à la deadline de 60 s, et `Expect: 100-continue` provoquait un blocage croisé) ;
- **[P1] `Transfer-Encoding` refusé APRÈS le routage** : `TE: chunked` sur `/health` était **accepté** (200) et les octets chunked résiduels étaient relus comme requête. Le contrôle est désormais appliqué **avant** le routage, donc sur toutes les routes ;
- **[P2] Le `503` « trop de connexions » était perdu** : le `close()` partait avec la requête du client **non lue** dans le tampon de réception → RST → le tampon du client (qui contient le 503) était purgé. Mesuré : **4/20 réponses lues**. Correctif : drain borné (64 Kio / 50 ms) + `shutdown(Write)`, et le refus est écrit **hors de la boucle d'`accept`** (thread court, borné à 64 refus simultanés) — mesuré : **15/15** lues, latence médiane par refus **1,4 ms** (contre 240 ms de blocage de la boucle auparavant). Le test existant passait pour la mauvaise raison (il n'envoyait rien avant de lire) ;
- **[P2] Amplification mémoire ×9 800** : le corps était pré-alloué et zéroé en entier avant la moindre lecture (`vec![0u8; len]`). 256 clients annonçant 1 Mio sans envoyer de corps consommaient **264 Mio** de commit pour 28 Kio reçus. Lecture désormais **par tranches** (buffer 64 Ko réutilisé) → **+24 Mio** mesurés (×864) ;
- **[P2] TTL de connexion** : le deadline par requête était réinitialisé à chaque requête keep-alive — un client qui drip-feed des requêtes valides (sous le `read_timeout`) retenait son slot **indéfiniment**, et 256 clients lents suffisaient à saturer le plafond durablement. Nouvelle option `.connection_ttl(duration)` (défaut **5 min**) ;
- **[P2] L'`id` JSON-RPC était perdu sur les erreurs** : toute réponse `-32600` portait `id: null`, comme la réponse de panique. L'`id` est désormais extrait *best-effort* **avant** la validation et repris dans la réponse (JSON-RPC 2.0 §5) ;
- **[P2] `arguments` non-objet accepté** : un outil sans schéma recevait un scalaire/tableau/`null` en violation du contrat de l'API (« la closure reçoit `arguments` (objet JSON) »). Refusé en `-32602` dans **tous** les cas ;
- **[P3] Schéma Zod exécuteur** : `integer()` acceptait `3.0`/`1e2` alors que `Value::as_i64()` y renvoie `None` — le motif documenté `args["a"].as_i64().unwrap()` **paniquait** sur une valeur que le schéma déclarait valide. `integer()` n'accepte plus que les entiers JSON **syntaxiques**, et publie ses bornes exactes (`minimum`/`maximum`) : ce que le schéma annonce est **exactement** ce que la validation applique — aucune dérogation possible ;
- **[P3] Doublon de nom de champ** : `Obj::field`/`Obj::opt` ne refusaient pas les doublons → `required: ["a","a"]` (JSON Schema invalide) et un outil insatisfaisable. Panique de développement au démarrage ;
- **[P3] Parsing strict** : séparateurs de la ligne de requête = **espaces ASCII (SP) uniquement** (le HTAB et les caractères de contrôle sont refusés) ; fin de ligne **CRLF stricte** (le LF seul — vecteur *httpoxy* — est refusé) ; `Accept` et `Expect` comparés **par jeton exact** (`text/event-stream-x` ne vaut pas `text/event-stream` ; `not-100-continued` ne déclenche pas de `100 Continue`) ; `Connection: close` est **absorbant** en HTTP/1.0 ;
- **[P3] `Content-Type` vérifié** : un `POST` en `text/plain` était traité comme du JSON → `415 Unsupported Media Type` (absent : accepté) ;
- **[P3] `.annotate()` sans effet selon l'ordre** : les annotations sont désormais stockées à part et fusionnées à l'enregistrement de l'outil — `.annotate()` **avant** ou **après** `.tool()` donne le même résultat (avant : perte silencieuse) ;
- **[P3] `tool_parse` ne clone plus le `Value`** : la désérialisation passait par `value.clone()` (copie de l'arbre JSON des arguments, jusqu'à 1 Mio par appel) — désormais sérialisation unique puis désérialisation ;
- **[P3] `u64::MAX` accepté comme `id`** : JSON-RPC autorise tout `Number` ; les ids au-delà de `i64::MAX` sont désormais acceptés (`RequestId::Unsigned`) ;
- **[P3] `core()` documenté comme un instantané** ; `serve()` reste non interruptible (les tests fuient ~51 threads — connu, non corrigé) ;
- **CI** : job `cargo-deny` (advisories RustSec sur `Cargo.lock`) + `deny.toml` minimal ;
- **README** : section « Sécurité » (modèle de menace opérationnel — TLS/auth/rate-limit par IP au proxy, plafond global ≠ par IP, TTL de connexion, timeout des handlers bloquants à la charge du handler, format des origines autorisées : origine normalisée navigateur, sans port par défaut) + tableau « Protocole » aligné sur la spec `2026-07-28`.

### Tests

- **109 tests au total** (27 unitaires JSON-RPC/zod/protocol, 73 HTTP sur vraies sockets, 5 smoke, 4 doctests) — 29 nouveaux, un par correctif ci-dessus.

## [0.3.2] - 2026-10-22

### Ajout — anti-tampon SSE (réponse au problème des VPS derrière un proxy)

- **En-têtes anti-tampon sur toute réponse SSE** (`SSE_ANTI_BUFFERING`) : `X-Accel-Buffering: no` — la directive **par réponse** que nginx honore même quand `proxy_buffering` est actif globalement — et `Cache-Control: no-cache, no-transform`. Le symptôme « les événements s'empilent jusqu'au seuil du buffer » ne peut de toute façon pas se produire (SSE request-scoped : un événement, `Connection: close`, `Content-Length` explicite — un proxy pousse toujours une réponse terminée) ; les en-têtes ferment le dossier pour les proxies les plus zélés. JSON non affecté (2 tests : présents en SSE, absents en JSON).
- **Choix architectural documenté** : **pas de SSE longue durée** (progress, notifications serveur→client) — le mode stateless ne tient aucun flux ouvert ; documenté dans le code (contrat : tout écrivain SSE flush après chaque événement et porte `SSE_ANTI_BUFFERING`) et dans le README.
- README : nouvelle section « Hébergement VPS & proxies » (explication du tamponnage, recette nginx, repli JSON officiel, choix architectural), FAQ « Pourquoi pas de WebSocket ? » (choix architectural : ~1–2 % de gain au mieux, proxies hostiles, sessions), table Configuration avec la sonde `/health`.

### Tests

- 72 tests au total (19 unitaires JSON-RPC/zod/protocol, 44 HTTP sur vraies sockets, 5 smoke, 4 doctests).

## [0.3.1] - 2026-10-22

### Ajout — point fort repris de ViteMCP

- **Sonde `GET /health`** → `200` « ok » sans JSON-RPC (`HEALTH_ROUTE`, inspiration ViteMCP) : probes k8s/systemd/équilibreur ; compatible keep-alive (la sonde pipelinée ne casse pas la connexion) ; autre méthode → `405` avec `Allow: GET`. `GET /mcp` reste `405`.

### Comparatif ViteMCP (TypeScript) — 2026-10-22

- Premier face-à-face avec [`@vitemcp/server`](https://github.com/vitemcp/server) — le framework TS ciblant la même spec stateless 2026-07-28. Même serveur démo (4 outils) des deux côtés, client de bench neutre unique (`litemcp-it/examples/bench_remote`), Windows 11 loopback, passes alternées :
  - keep-alive : **0,09–0,10 ms** vs 1,43–1,73 ms (**~15–18× plus rapide**) ;
  - p99 : **0,18–0,22 ms** vs 5,4–5,9 ms ;
  - connexions neuves : **848–1257 conn/s** vs 354–498 conn/s ;
  - réutilisation : **2000/2000** vs 0/2000 — ViteMCP répond en SSE encadré maintenu ouvert, le client doit rouvrir une connexion par appel séquentiel ; litemcp délimite par `Content-Length` et clôt le flux ;
  - mémoire après 2 300 requêtes : **5,5 Mo** (0,9 Mo privé) vs ~417 Mo (chaîne de 3 processus Node : 47 + 63 + 306 Mo) — **~75× moins**.
- Constat de fiabilité : `enableJsonResponse` est déclaré dans les types de `@vitemcp/server` mais sans implémentation dans le code — l'option est un no-op.
- Banc reproduisible, hors dépôt : un projet TypeScript (`@vitemcp/server` + tsx) d'un côté, le banc externe d'intégration de l'autre (serveur démo 4 outils + client de bench neutre `bench_remote`).

### Performance — lot « le plus léger, le plus rapide »

- **Latence keep-alive ~35 % meilleure** (A/B dos à dos, même machine et même session : code 0.3.0 0,097–0,111 ms → code optimisé 0,065–0,070 ms/req ; p50 0,061 ms, p99 0,095–0,123 ms) :
  - réponse HTTP assemblée dans un seul buffer → **un seul write** par réponse (avant : deux syscalls et, avec nodelay, deux paquets) ;
  - parsing d'en-têtes **zéro allocation** : noms comparés `eq_ignore_ascii_case` (plus de minuscules allouées), `Origin`/`Host` réduits à des verdicts booléens, `Accept`/`Connection` sans chaînes ni `Vec` temporaires ;
  - ligne de requête et en-têtes lus dans des buffers **réutilisés** sur toute la connexion (keep-alive inclus) ;
  - **zéro clone des arguments** dans le dispatch : `params` et `id` sont déplacés hors du JSON parsé (`Map::remove`), et handler comme validation reçoivent les arguments par référence — `tool_parse` seul clone encore, pour la désérialisation typée ;
  - `route` et `allow_origins` partagés par `Arc` (plus de clone par connexion sur le chemin d'accept) ;
  - `Outgoing.jsonrpc` en `&'static str` (une allocation de moins par réponse).
- **Binaire release −18,5 %** (483 Ko → 385 Ko) : `lto = "fat"` + `codegen-units = 1` dans le profil release du dépôt (les consommateurs gardent la main sur leur propre profil).
- **Mémoire de lecture par connexion divisée par deux** : buffer `BufReader` 4 Ko (1 Mio au plafond de 256 connexions) — un corps plus grand que la capacité est lu directement depuis la socket.
- Bench `examples/bench_backends` : warmup déduit et percentiles **p50/p99** en plus de la moyenne — passes comparables d'une campagne à l'autre.
- **Confirmation indépendante côté consommateur** : banc externe (`litemcp-it`, serveur démo de 4 outils important le crate depuis crates.io vs `path` local) — keep-alive 0,102–0,108 ms (0.3.0 publié) → **0,087–0,091 ms** (lot en cours), plages sans chevauchement, **~13 % plus rapide** sur ce serveur plus riche ; les 12 tests d'intégration du banc passent à l'identique dans les deux modes.
- Docs : section « Pourquoi litemcp » réécrite (bibliothèque vs service expliqué simplement, cas d'usage introduits).

### Tests

- 70 tests au total (19 unitaires JSON-RPC/zod/protocol, 42 HTTP sur vraies sockets, 5 smoke, 4 doctests).

## [0.3.0] - 2026-10-22

### Added

- **Durcissement HTTP (Phase 3)** :
  - timeout de lecture par opération anti-slowloris (`READ_TIMEOUT` 30 s, configurable `.read_timeout(durée)`) — un client trop lent est déconnecté sans réponse ;
  - timeout d'écriture (`WRITE_TIMEOUT` 10 s) ;
  - limites `MAX_HEADERS` (64) et `MAX_LINE` (16 Kio) → `431 Request Header Fields Too Large`, avec lecture de lignes bornée en mémoire (`read_line_capped`) — un client hostile ne peut plus faire grossir le buffer ;
  - support `Expect: 100-continue` (intermédiaire `100` avant lecture du corps) ;
  - keep-alive multi-requêtes testé (pipelining 2 requêtes sur une connexion).
- **Backend HTTP alternatif testé puis retiré** (bench A/B du 2026-10-22, `examples/bench_backends`) : tiny_http (vérifié 100 % Rust) s'est avéré ~1,85× plus lent en keep-alive (0,19 vs 0,10 ms/req), +360 Ko de binaire et +4 deps — il a été **supprimé du dépôt** : la route maison est le seul backend, le crate garde exactement **2 dépendances** (`serde`, `serde_json`).
- **Première campagne de benchmarks** : binaire hello 462 Ko (cible < 1,5 Mo ✅), keep-alive 0,10 ms (cible < 1 ms ✅), RSS 4,7 Mo (WorkingSet Windows, mesure `VmRSS` Linux à venir), démarrage ~130 ms via shell Windows (mesure in-process à venir).
- 67 tests au total (19 unitaires JSON-RPC/zod/protocol, 39 HTTP sur vraies sockets, 5 smoke, 4 doctests).

### Audit complet du 2026-10-22 — durcissement & conformité

**Sécurité / robustesse :**

- **Plafond d'issues de validation** — nouveau collecteur borné `zod::Issues` (`MAX_ISSUES` = 64) : au-delà du plafond, les problèmes sont comptés sans être stockés. Avant : un corps de 1 Mio d'éléments invalides produisait ~15 Mo d'issues (~15× le corps, ~4 Go potentiels sur 256 connexions). L'amplification mémoire est éliminée.
- **Lignes obs-fold rejetées `400`** (RFC 9112 §5.2) : une ligne d'en-tête commençant par une espace/tabulation serait susceptible d'être réassemblée différemment par un proxy — refus net plutôt qu'acceptation ambiguë.
- **`Content-Length` malformé → `400`** (RFC 9112 §6.3, non numérique ou hors usize) ; le `411` est désormais réservé à l'absence du champ.
- **En-tête `Host`** : requis et unique en HTTP/1.1 → `400` sinon (RFC 9112 §3.2) ; HTTP/1.0 dispensé.
- **Version HTTP validée** : ligne de requête malformée (version absente, jeton superflu) → `400` ; version ≠ HTTP/1.0/1.1 → `505`.
- **Réponse `503` non bloquante** : les timeouts socket sont posés avant la branche « trop de connexions » — l'écriture ne peut plus bloquer l'accept loop sur un client qui ne lit pas.
- **Compteur de connexions par serveur** : le plafond vit dans `BoundServer` (plus de static global partagé entre toutes les instances du processus).

**Conformité MCP / JSON-RPC :**

- **`structuredContent` toujours objet** (spec MCP) : une valeur de handler scalaire/tableau/`null` est enveloppée dans `{ "result": … }`, et le contenu texte reflète la valeur structurée réellement envoyée — `structuredContent: 42` ou `: null` violait la spec.
- **`integer()` ne sature plus** : les valeurs f64 aux bornes ±2^63 sont rejetées au lieu de saturer en `i64::MIN`/`i64::MAX` au cast.
- Refus des `id` JSON-RPC fractionnaires (1.5) documenté comme choix assumé (la spec les déconseille, MCP ne les emploie jamais).

**Changements incompatibles (0.x) :**

- `zod::Schema::validate` accumule dans `zod::Issues` (borné) au lieu d'un `Vec<String>` — les implémentations du trait doivent s'adapter (une ligne).
- Réenregistrer un outil du même nom remplace l'ancien — documenté (comportement inchangé).

**Docs :**

- Doc-comment de `respond_error` aligné sur le code (fermeture systématique, jamais de drain) + compromis RST documenté.
- Note `panic = "abort"` : neutralise `catch_unwind`, une panique de handler arrête le processus.
- README : compte de tests corrigé, lignes de protocole `505`/`Host`/obs-fold/CL malformé, `.max_connections` dans la configuration, enveloppe `structuredContent` documentée.

## [0.2.0] - 2026-10-22

### Added

- **Module `zod` — schémas à la Zod** (`src/zod.rs`, 11 tests unitaires) :
  - `object()` avec `.field(nom, schéma)` (requis), `.opt(nom, schéma)` (optionnel), `.strict()` (refuse les champs inconnus) ;
  - primitives : `string().min/.max` (longueur), `integer().min/.max` (i64), `number().min/.max` (f64), `boolean()`, `enumeration(&[..])` (ensemble fermé), `array(s).min/.max` (éléments validés récursivement) ;
  - chaque schéma produit son fragment **JSON Schema draft 2020-12** (`type`, `properties`, `required`, `additionalProperties`, `minLength`/`maxLength`, `minimum`/`maximum`, `minItems`/`maxItems`, `enum`) **et** valide à l'exécution en accumulant tous les problèmes (pas d'arrêt au premier) ;
  - trait `Schema` public : extension possible par l'utilisateur ;
  - zéro dépendance de plus — toujours `serde` + `serde_json` uniquement.
- **`ServerBuilder::tool_typed(nom, description, schéma, closure)`** :
  - l'`inputSchema` publié dans `tools/list` est **dérivé** du schéma (fini le générique `{"type":"object"}`) ;
  - chaque `tools/call` est **validé avant exécution** : un appel hors schéma reçoit `-32602` avec `error.data.issues` — la liste lisible des problèmes (champ manquant, type attendu, borne franchie, champ inconnu) — et le handler n'est **jamais** appelé ;
  - la closure reçoit `&Value` garanti conforme au schéma.
- **`ServerBuilder::tool_parse(nom, description, schéma, closure)`** — comme `tool_typed`, avec les arguments désérialisés dans un struct `Deserialize` : le handler reçoit des données **typées** (`|args: SearchArgs|`).
- **Annotations MCP 2026-07-28** — `protocol::ToolAnnotations` avec builder fluide `.title()`, `.read_only()`, `.destructive()`, `.idempotent()`, `.open_world()` ; attachées via `.annotate(nom_outil, ...)` et sérialisées dans `tools/list` (`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) ; champs non posés omis du JSON (défauts de la spec côté client).
- **`jsonrpc::ErrorObject::with_data(code, message, data)`** — erreurs JSON-RPC avec `data` structuré (utilisé pour les issues de validation, disponible pour les handlers).
- **Exemple `hello` enrichi** — montre les trois registres d'outils (closure brute / schéma validé / struct typé) + annotations sur deux outils.
- **Tests** : 32 au total (11 unitaires `zod`, 3 tests E2E typés sur vraies sockets — dérivation de schéma + annotations dans `tools/list`, exécution valide, rejet `-32602` avec issues — en plus des 18 existants).

### Documentation

- README : section « Outils — schémas à la Zod » (exemple `tool_parse` complet, tableau des trois registres, catalogue des schémas), section « Annotations », FAQ « Zod en Rust », badges crates.io/CI/docsrs actifs, badge statut 0.2.0.
- ROADMAP : Phase 4 close (« Ergonomie Zod »), macros décalées en 0.3, décision technique #6 mise à jour, journal v5.
- Cargo.toml : `keywords` `model-context-protocol` → `mcp-server` (limite crates.io : 20 caractères par mot-clé).

## [0.1.0] - 2026-10-21

### Added

- **Noyau JSON-RPC 2.0** (`src/jsonrpc.rs`) :
  - `RequestId` (nombre ou chaîne, untagged), `ErrorObject` avec les cinq codes standards (-32700, -32600, -32601, -32602, -32603) ;
  - `Incoming` / `Outgoing` : parse error avec `id: null` comme exigé par la spec, `success` n'embarque jamais de champ `error` (et inversement) ;
  - notifications (sans `id`) détectées et sans réponse.
- **Dispatch MCP stateless** (`src/server.rs`) :
  - méthodes `initialize` (renvoie `protocolVersion` + `serverInfo` — toléré pour les inspecteurs, jamais requis), `ping` → `{}`, `tools/list`, `tools/call` ;
  - `ServerBuilder::new(name, version).tool(nom, description, closure).bind(addr)?.serve()` + `prelude` ;
  - `ServerCore` réutilisable sans transport (tests, intégrations custom) ;
  - outil en erreur → `isError: true` + message en contenu texte ; outil inconnu / params invalides → `-32602` ; méthode inconnue → `-32601`.
- **Transport Streamable HTTP stateless** (`src/http.rs`) :
  - socket bindé `std::net::TcpListener`, un thread `std` par connexion, zéro runtime async ;
  - `POST /mcp` par message : réponse `application/json`, ou SSE request-scoped mono-événement (`Accept: text/event-stream` seul) ;
  - notification → `202 Accepted` sans corps ; `GET` → `405` (pas de listener SSE permanent en stateless) ;
  - HTTP/1.1 minimal maison : ligne de requête, en-têtes, `Content-Length`, keep-alive, `Connection: close` ;
  - erreurs propres : `404` route inconnue, `411` sans `Content-Length`, `413` au-delà de `MAX_BODY` (1 Mio) ;
  - garde DNS-rebinding : toute requête avec `Origin` non listée est refusée `403` (`allow_origins()` pour la liste blanche) ; les clients CLI sans `Origin` passent.
- **Types MCP 2026-07-28** (`src/protocol.rs`) : `PROTOCOL_VERSION`, `Tool` / `inputSchema`, `ToolsListResult`, `CallToolResult` (`content` + `isError` + `structuredContent`), `ServerInfo`.
- **Tests** : 18 (4 unitaires JSON-RPC + 13 d'intégration HTTP sur vraies sockets bindées `127.0.0.1:0` + 1 smoke), clippy `-D warnings` zéro, `cargo audit` zéro vulnérabilité.

### Security

- `#![forbid(unsafe_code)]` — zéro `unsafe` dans le crate.
- Deux dépendances seulement : `serde` + `serde_json`.
- Garde DNS-rebinding sur `Origin` actif par défaut.

### Publication

- crates.io : [litemcp 0.1.0](https://crates.io/crates/litemcp) — package 17 fichiers, `.cargo_vcs_info.json` vérifié (sha1 = commit tagué).
- GitHub : dépôt [DeamonDev888/litemcp](https://github.com/DeamonDev888/litemcp), tag `v0.1.0`, release, CI verte (fmt / clippy / test / doc).
- Token API crates.io stocké dans `~/.cargo/credentials.toml` (hors repo), garde `.cargo/` au `.gitignore`.

[Unreleased]: https://github.com/DeamonDev888/litemcp/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/DeamonDev888/litemcp/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/DeamonDev888/litemcp/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/DeamonDev888/litemcp/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/DeamonDev888/litemcp/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/DeamonDev888/litemcp/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/DeamonDev888/litemcp/compare/v0.3.3...v0.4.0
[0.3.3]: https://github.com/DeamonDev888/litemcp/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/DeamonDev888/litemcp/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/DeamonDev888/litemcp/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/DeamonDev888/litemcp/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/DeamonDev888/litemcp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/DeamonDev888/litemcp/releases/tag/v0.1.0
