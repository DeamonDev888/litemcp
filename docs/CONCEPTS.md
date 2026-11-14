# Concepts : ressources, prompts, MRTR, progression, erreurs

Les registres `resources` et `prompts` (avec leurs variantes binaires, gabarits RFC 6570 et
complétion), le multi round-trip (MRTR), la progression `notifications/progress`, les annotations
et le contrat d'erreur. Le README résume ; cette page est la référence.

[← Retour au README](../README.md)

## Resources & Prompts

Mêmes registres que les outils, mêmes indices de cache (`ttlMs`/`cacheScope`, SEP-2549) :

```rust
use litemcp::prelude::*;

ServerBuilder::new("doc", "1.0.0")
    .resource("mem://readme", "README", "La documentation du service", || {
        Ok(litemcp::protocol::ResourceContents {
            uri: "mem://readme".into(),
            mime_type: Some("text/markdown".into()),
            text: include_str!("../README.md").into(),
        })
    })
    .prompt("revue", "Demande une revue de code", |args| {
        let fichier = args.get("fichier").and_then(|v| v.as_str()).unwrap_or("src/main.rs");
        Ok(vec![PromptMessage::user(format!("Relis {fichier} et liste les risques."))])
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

`resources/read` prend `{"uri": …}` ; `prompts/get` prend `{"name": …, "arguments": {…}}` (objet exigé, sinon `-32602`). Les capacités `tools`, `resources` et `prompts` sont **toujours** annoncées dans `server/discover` (et dans `initialize`, servi en profil legacy) — leurs méthodes sont servies même à registre vide (catalogue vide), et un `Gateway` peut peupler les registres après le démarrage. Aucune sous-capacité non servie (`listChanged`, `subscribe`) n'est déclarée.

**Contenus riches — la forme `ContentBlock` complète de la spec.** Un outil qui rend une image, un audio, un lien de ressource, une ressource embarquée — ou plusieurs blocs mélangés — s'enregistre avec `.tool_result` : le handler construit le `CallToolResult` complet, qui part tel quel (l'API `Ok(Value)` historique reste inchangée et garde sa forme fil) :

```rust
use litemcp::prelude::*;

ServerBuilder::new("ecran", "1.0.0")
    .tool_result("screenshot", "Capture d'écran annotée", |_args| {
        Ok(CallToolResult::from_content([
            ContentBlock::text("Voici la capture :"),
            ContentBlock::image_bytes(&[0x89, 0x50, 0x4E, 0x47], "image/png"), // base64 réel
            ContentBlock::resource_link("app://doc", "Doc"),
        ]))
    })
    .prompt("visuel", "Prompt avec image", |_args| {
        Ok(vec![
            PromptMessage::user_content(ContentBlock::image_bytes(&[0xFF, 0xD8, 0xFF], "image/jpeg")),
            PromptMessage::user("Décris cette image."),
        ])
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

Les données binaires (`image`/`audio`/`blob`) sont **base64 encodées par la lib** depuis les octets réels (`from_bytes`/`binary`) — jamais sérialisées en texte JSON pour simuler le support.

**Ressource binaire** — `.resource_data` laisse le handler choisir texte ou binaire (`blob`) à chaque lecture :

```rust
use litemcp::prelude::*;

ServerBuilder::new("assets", "1.0.0")
    .resource_data("app://icon", "Icône", "Icône PNG de l'app", || {
        Ok(ResourceData::binary("app://icon", Some("image/png".into()), &PNG_BYTES))
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

**Gabarits et complétion** — `.resource_template(uri, nom, description)` publie un gabarit RFC 6570 via `resources/templates/list` (indices de cache SEP-2549, couvert par la capacité `resources`) ; `.completions(f)` branche un fournisseur de suggestions pour `completion/complete` — la capacité `completions` n'est annoncée que quand un fournisseur existe :

```rust
use litemcp::prelude::*;

ServerBuilder::new("calc", "1.0.0")
    .resource_template("calc://history/{date}", "Historique", "Historique par date")
    .completions(|request: &CompletionRequest| match &request.reference {
        CompleteReference::Prompt { name, .. } if name == "revue" => {
            Ok(Completion::new(vec!["src/main.rs".into()]))
        }
        _ => Ok(Completion::new(vec![])),
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

La borne de la spec (100 suggestions) est appliquée par le noyau : au-delà, troncature + `hasMore: true`. La **lecture** d'une ressource reste un match exact sur les URIs enregistrées — la substitution de gabarit côté noyau n'est pas implémentée (inscrivez les URIs concrètes correspondantes).

## Multi round-trip (MRTR)

Un handler qui a besoin d'une entrée client (formulaire d'élicitation, échantillonnage, racines) renvoie `ToolOutcome::InputRequired` / `PromptOutcome::InputRequired` / `ResourceOutcome::InputRequired` depuis un handler enregistré avec `.tool_mrtr` / `.prompt_mrtr` / `.resource_mrtr` — il est **réappelé** au rejeu avec les réponses du client (non fiables) et l'état vérifié ([`InputRound`] : `turn`, `input_responses`, `state`, `.accepted(clé)`) :

```rust
use litemcp::prelude::*;

ServerBuilder::new("booking", "1.0.0")
    .tool_mrtr("reserve", "Réservation à confirmation", |_args, round| {
        if round.turn == 1 {
            Ok(ToolOutcome::InputRequired(
                InputRequiredSpec::new()
                    .elicit("confirm", "Confirmer ?", serde_json::json!({"type": "object"}))
                    .with_state(serde_json::json!({ "etape": 1 })),
            ))
        } else {
            Ok(ToolOutcome::Value(serde_json::json!({
                "confirme": round.accepted("confirm").is_some()
            })))
        }
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

Le noyau fait le reste : validation des requêtes embarquées (`elicitation/create`, `sampling/createMessage`, `roots/list` — une autre méthode ou une forme fil invalide est un bogue de handler rendu `-32603` **sans rien émettre**), exigence de la capacité client déclarée correspondante (`-32021` sinon), `requestState` **opaque signé** (HMAC-SHA256, TTL 10 min par défaut — `.request_state_ttl`, clé partagée en flotte — `.request_state_key`, borne d'état 8 Kio), liaison du jeton à la méthode et aux paramètres saillants. Le client réémet la requête **sous un id différent** avec `inputResponses` et l'écho exact du jeton ; un jeton altéré, expiré, malformé ou lié à une autre requête est refusé en `-32602` avant tout handler. Le jeton est **porteur et non consommé** (imposez la consommation unique dans le handler si nécessaire) ; les champs MRTR sont ignorés sur toute autre méthode et **aucune capacité serveur supplémentaire n'est annoncée** — rien n'est servi au-delà des résultats `input_required`. Aucune dépendance nouvelle : SHA-256, HMAC-SHA256 et base64url sont implémentés localement.

## Progression (`notifications/progress`)

Un handler qui exécute une tâche longue peut rapporter sa progression ; la notification est poussée sur le flux SSE **de la requête courante**, avant l'événement de réponse :

```rust
use litemcp::prelude::*;

ServerBuilder::new("index", "1.0.0")
    .tool("build", "Indexe un corpus", |_args| {
        let _ = progress::report(0.0, Some(100.0), Some("starting"));
        // … travail …
        let _ = progress::report(100.0, Some(100.0), None);
        Ok(serde_json::json!({ "done": true }))
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

Le canal n'existe que si le client a fourni `params._meta.progressToken` **et** accepte `text/event-stream` : le transport ouvre alors le flux avant le dispatch, chaque `report` écrit une notification portant **le jeton de la requête** (jamais choisi par le handler), puis l'événement de réponse ferme le flux et la connexion. Toujours une seule requête, aucun flux permanent, aucun état de session. Sans jeton, sans SSE ou une fois la réponse partie, `report` rend `false` (non-action) : un handler qui rapporte ne casse rien chez un client JSON. Le canal est lié au fil du handler pour la durée du dispatch — y compris sous `.tool_timeout`, où il est réinstallé dans le fil détaché ; une émission tardive d'un handler qui a dépassé son délai est ignorée. Un hôte qui possède déjà son transport branche `ServerCore::dispatch_request_with_progress` sur son propre `ProgressChannel`.

## Annotations

Métadonnées de la spec 2026-07-28, publiées dans `tools/list` : `.title()`, `.read_only()`, `.destructive()`, `.idempotent()`, `.open_world()`. Un client sait ainsi qu'un outil ne modifie rien avant de l'appeler.

## Erreurs

- **Hors schéma** → `-32602` avant exécution ; `error.data.issues` liste chaque problème en anglais (champ manquant, type, borne, champ inconnu) ;
- **Erreur d'outil** (`ToolError`) → `CallToolResult::isError` avec le message en texte **et** un objet machine-lisible sous `structuredContent.error` (`code`, `message`, `retryable`, `retryAfterMs` optionnel). Six constructeurs seulement — `ToolError::rate_limited` / `timeout` / `upstream` / `invalid_input` / `internal` / `unclassified` (`src/error.rs:147-177`) ; les erreurs texte (`String`, `&str`) restent acceptées via `?` (`From<String>` / `From<&str>`, classées `unclassified`) ;
- **Erreur de protocole** (mauvaise requête, outil inconnu, JSON invalide) → erreur JSON-RPC typée (-32700, -32602, -32601…), rendue de façon opaque.

La valeur de retour d'un outil devient `structuredContent` — un objet JSON passe tel quel ; toute autre valeur (scalaire, tableau, `null`) est enveloppée dans `{"result": …}` (**choix d'API** : `structuredContent` toujours objet, contrat simple pour les clients) — **sauf** si un `outputSchema` de racine non-objet est attaché à l'outil : la valeur brute est alors envoyée telle quelle (la spec MCP 2026-07-28 autorise toute valeur JSON dans `structuredContent`, et l'enveloppe serait elle-même non conforme au schéma publié). Une `Err(msg)` devient `isError: true` avec le message en contenu texte. Un `outputSchema` attaché via `.output_schema("outil", schéma)` valide cette sortie **avant envoi** : non conforme → résultat `isError`.

## Tableau des fonctions

| | |
|---|---|
| ⚡ **Minuscule** | 2 dépendances, binaire d'un serveur à un outil **≈ 1 Mio** (re-mesuré le 2026-11-13, voir [`MESURES.md#tailles-de-binaire`](MESURES.md#tailles-de-binaire)), démarrage en quelques ms |
| 🚀 **Sans runtime** | threads `std`, un par connexion — pas de tokio au cœur |
| 🧩 **Schémas à la Zod** | `inputSchema` dérivé du schéma, publié dans `tools/list` ; `outputSchema` **exécuteur** (sortie validée avant envoi) |
| ✅ **Validation avant exécution** | un appel hors schéma ne touche jamais votre handler |
| 🦀 **Handlers typés** | arguments désérialisés dans vos structs `Deserialize` ; macro `#[tool]` (incluse par défaut) qui dérive le schéma des types des paramètres |
| 📚 **Resources & Prompts** | `resources/list`·`read`·`templates/list`, `prompts/list`·`get` — mêmes indices de cache que `tools/list` (SEP-2549) ; lectures **binaires** (`blob` base64), gabarits RFC 6570 et `completion/complete` inclus |
| 🔁 **Multi round-trip (MRTR)** | un handler d'outil/prompt/ressource demande une entrée client (`resultType: "input_required"`) : élicitation, échantillonnage, racines — état opaque signé HMAC-SHA256, rejeu avec `inputResponses` sous un id différent |
| 📈 **Progression** | `notifications/progress` **request-scoped** : `progress::report(...)` dans le handler, poussée sur le flux SSE borné à la requête (`_meta.progressToken` + `text/event-stream`) — aucun flux permanent, aucun état de session |
| ⏱️ **Handlers bornés** | `.tool_timeout(duration)` : un handler qui dépasse rend une erreur, la connexion n'est plus retenue indéfiniment |
| 🩺 **Erreurs structurées & observabilité** | `ToolError` (`rate_limited`, `timeout`, `upstream`… avec `retryable`/`retryAfterMs`) + `.on_call(hook)` pour métriques et latences |
| 🔐 **Verrou de requête** | `.on_request(hook)` : verdict par requête **avant la lecture du corps** — auth par token, rate-limit par source ; rejet `401`/`403`/`429` + `Retry-After`, paniques contenues |
| 🛑 **Arrêt propre** | `bound.handle()` puis `handle.shutdown()` : `serve()` rend `Ok(())`, les connexions ferment entre deux requêtes |
| 🧟 **Anti-zombie** | sweep du registre d'annonce au bind — **ne détruit qu'en preuve positive** : une annonce usurpée (le port répond avec une identité d'instance différente de celle publiée) est retirée ; un port sans réponse ou une identité invérifiable = doute → **conservée** (un refus TCP n'est pas une preuve de mort) ; drain borné des connexions à l'arrêt, bilan vérifiable (`handle.zombie_report()`) — rien ne reste accroché |
| 🔀 **Gateways dynamiques** | `Gateway` : la surface d'outils change **pendant que le serveur sert** — un serveur détecté apparaît/disparaît de `tools/list` sans redémarrage, les requêtes en vol finissent sur leur snapshot |
| 🛡️ **Sûr par défaut** | `#![forbid(unsafe_code)]`, garde DNS-rebinding, corps limité |
