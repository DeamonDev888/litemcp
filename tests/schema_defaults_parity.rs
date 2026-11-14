//! Test de régression — TODO 2026-11-09 « Unifier les défauts des schémas
//! JSON bruts et les `$ref` imbriqués ».
//!
//! Deux défauts, un même contrat violé (« publié = appliqué ») :
//!
//! 1. `FromJsonSchema` (schéma brut) laissait `default_value()` /
//!    `apply_defaults()` / `has_defaults()` aux no-ops du trait : un
//!    `default` publié dans un document écrit à la main était annoncé à
//!    `tools/list` mais **jamais injecté** — alors que le même document
//!    dérivé par `schemars` l'aurait été. Un default publié dans un schéma
//!    brut accepté par LiteMCP doit suivre la **même politique d'injection**.
//! 2. `FromSchemars::has_defaults` ne résolvait les `$ref` locaux qu'à un
//!    seul niveau (et pas du tout à l'intérieur de sa détection récursive)
//!    alors que l'injection les suivait : un défaut derrière une **chaîne**
//!    `$ref → objet → $ref` (ou derrière un `$ref` imbriqué, ou dans une
//!    branche `allOf`) laissait `has_defaults()` à `false` — le noyau
//!    court-circuitait alors `apply_defaults` : **une injection nécessaire
//!    était désactivée**.
//!
//! Le parcours de détection et celui d'injection partagent désormais la
//! même définition des sous-schémas visités (même résolveur chaîné borné
//! [`MAX_REF_DEPTH`], même détection de cycles), et un `inputSchema`
//! racine **booléen** est refusé à l'enregistrement d'un outil (MCP exige
//! un objet) — sans toucher aux schémas booléens aux emplacements JSON
//! Schema qui les autorisent (`properties.*`, `items`…).
//!
//! Troisième défaut couvert ici : un nœud dont la chaîne de `$ref` est
//! **coupée** (cycle ou budget de sauts épuisé) et qui porte **aussi** des
//! mots-clés frères (`properties`, `items`, combinateurs) voyait ses
//! frères explorés par la détection mais **sautés** par l'injection
//! (reprise avec retour anticipé) — un défaut publié dans ces frères était
//! annoncé par `has_defaults()` puis **jamais injecté**. La reprise ne
//! court-circuite plus rien : détection et injection voient exactement
//! les mêmes sous-schémas.

#![cfg(feature = "schemars")]

use litemcp::ServerBuilder;
use litemcp::schemars::{from_json_schema, from_schemars};
use litemcp::zod::Schema;
use schemars::JsonSchema;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

// ===========================================================================
// Harnais : dispatch direct (même idiome que tests/schemars.rs)
// ===========================================================================

fn call(core: &litemcp::ServerCore, body: &str) -> Value {
    let reply = core.dispatch_unchecked(body.as_bytes()).unwrap();
    serde_json::from_slice(&reply).unwrap()
}

fn call_tool(core: &litemcp::ServerCore, id: i64, name: &str, arguments: Value) -> Value {
    call(
        core,
        &format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
        ),
    )
}

/// Enregistre `schema` pour l'outil `name` et renvoie ce que le handler a
/// reçu (après injection éventuelle).
fn seen_arguments(schema: impl Schema + 'static, arguments: Value) -> Value {
    let seen: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let capture = Arc::clone(&seen);
    let core = ServerBuilder::new("reg", "1.0")
        .tool_typed("t", "t", schema, move |args| {
            *capture.lock().unwrap() = Some(args.clone());
            Ok(json!({ "ok": true }))
        })
        .core();
    let reply = call_tool(&core, 1, "t", arguments);
    assert_eq!(
        reply["error"].as_object(),
        None,
        "l'appel doit réussir : {reply}"
    );
    seen.lock().unwrap().clone().expect("handler exécuté")
}

// ===========================================================================
// 1. Default publié dans un schéma BRUT : même politique que schemars
// ===========================================================================

/// Type schemars équivalent au document brut de [`raw_doc`] — la parité des
/// deux adaptateurs se vérifie sur le même contrat publié.
#[derive(JsonSchema)]
#[allow(dead_code)] // dérivation uniquement
struct ParityArgs {
    #[allow(dead_code)]
    count: u32,
}

/// Type schemars portant réellement un `#[serde(default)]` (schemars publie
/// alors le mot-clé `default` et retire le champ de `required`) — utilisé
/// pour prouver que la détection et l'injection restent solidaires sur la
/// voie dérivée.
#[derive(JsonSchema)]
#[allow(dead_code)] // dérivation uniquement
struct SchemarsDefaultArgs {
    #[serde(default = "def_seven")]
    count: u32,
}

fn def_seven() -> u32 {
    7
}

fn raw_doc() -> Value {
    json!({
        "type": "object",
        "properties": { "count": { "type": "integer", "default": 7 } },
        "required": []
    })
}

#[test]
fn raw_default_est_publie_et_injecte_comme_schemars() {
    let raw = from_json_schema(raw_doc());

    // Moitié PUBLIÉE : le mot-clé `default` reste dans l'inputSchema.
    assert_eq!(
        raw.json()["properties"]["count"]["default"],
        json!(7),
        "le document publié doit rester tel quel (default compris)"
    );

    // Moitié DÉTECTÉE : has_defaults doit voir le défaut brut.
    assert!(
        raw.has_defaults(),
        "un default publié dans un schéma brut doit rendre has_defaults() vrai"
    );

    // Moitié APPLIQUÉE : l'injection part du dispatch complet, comme pour
    // un schéma schemars.
    let vu = seen_arguments(from_json_schema(raw_doc()), json!({}));
    assert_eq!(
        vu,
        json!({ "count": 7 }),
        "le default brut doit être injecté"
    );

    // Parité : le même contrat dérivé par schemars (champ avec
    // `#[serde(default)]` — donc default publié et retiré de required)
    // produit le même résultat.
    let derive = seen_arguments(from_schemars::<SchemarsDefaultArgs>(), json!({}));
    assert_eq!(
        vu, derive,
        "schéma brut et schéma schemars : même politique d'injection"
    );

    // Valeur fournie : jamais écrasée par le défaut (dans les deux mondes).
    let fourni = seen_arguments(from_json_schema(raw_doc()), json!({ "count": 3 }));
    assert_eq!(fourni, json!({ "count": 3 }), "la valeur fournie gagne");
}

#[test]
fn raw_default_imbrique_est_injecte_recursivement() {
    // Un défaut dans un sous-objet (et dans un tableau d'objets) d'un
    // schéma brut suit la même politique récursive que schemars.
    let doc = json!({
        "type": "object",
        "properties": {
            "outer": {
                "type": "object",
                "properties": { "depth": { "type": "integer", "default": 3 } }
            },
            "list": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "flag": { "type": "boolean", "default": true } }
                }
            }
        }
    });
    let schema = from_json_schema(doc);
    assert!(schema.has_defaults(), "défauts imbriqués détectés");

    let mut value = json!({ "outer": {}, "list": [ {}, {} ] });
    schema.apply_defaults(&mut value);
    assert_eq!(
        value,
        json!({
            "outer": { "depth": 3 },
            "list": [ { "flag": true }, { "flag": true } ]
        }),
        "défauts imbriqués (objet et items) injectés dans le schéma brut"
    );
}

// ===========================================================================
// 2. $ref → objet → $ref → default : détectés ET injectés
// ===========================================================================

/// `$ref` chaînés : `x → A → B → C` où seul `C` porte le défaut. Avant le
/// correctif : `has_defaults()` valait `false` (résolution à un seul niveau)
/// et l'injection, court-circuitée par le noyau, n'arrivait jamais au
/// handler.
fn chained_doc() -> Value {
    json!({
        "type": "object",
        "properties": { "x": { "$ref": "#/$defs/A" } },
        "$defs": {
            "A": { "$ref": "#/$defs/B" },
            "B": { "$ref": "#/$defs/C" },
            "C": {
                "type": "object",
                "properties": { "v": { "type": "integer", "default": 9 } }
            }
        }
    })
}

#[test]
fn ref_vers_objet_vers_ref_default_est_detecte_puis_injecte() {
    let schema = from_json_schema(chained_doc());

    assert!(
        schema.has_defaults(),
        "has_defaults doit suivre la chaîne $ref → objet → $ref → default"
    );

    // Le champ chaîné est présent mais vide : le défaut de `C.v` est
    // injecté à l'intérieur.
    let vu = seen_arguments(from_json_schema(chained_doc()), json!({ "x": {} }));
    assert_eq!(
        vu,
        json!({ "x": { "v": 9 } }),
        "le défaut derrière la chaîne de $ref doit être injecté"
    );

    // Il est absent et ne porte pas de défaut racine : rien n'est inventé.
    let absent = seen_arguments(from_json_schema(chained_doc()), json!({}));
    assert_eq!(
        absent,
        json!({}),
        "x sans défaut propre ne doit pas être fabriqué"
    );
}

#[test]
fn ref_imbriquees_a_tous_les_niveaux_sont_suivies() {
    // Défaut derrière un `$ref` IMBRIQUÉ (pas seulement en tête de
    // propriété) : x → Outer, Outer.properties.inner → Deep, Deep porte
    // le défaut. La détection comme l'injection doivent le voir.
    let doc = json!({
        "type": "object",
        "properties": { "x": { "$ref": "#/$defs/Outer" } },
        "$defs": {
            "Outer": {
                "type": "object",
                "properties": { "inner": { "$ref": "#/$defs/Deep" } }
            },
            "Deep": {
                "type": "object",
                "properties": { "v": { "type": "integer", "default": 3 } }
            }
        }
    });
    let schema = from_json_schema(doc);
    assert!(
        schema.has_defaults(),
        "le défaut derrière un $ref imbriqué doit être détecté"
    );
    let mut value = json!({ "x": { "inner": {} } });
    schema.apply_defaults(&mut value);
    assert_eq!(
        value,
        json!({ "x": { "inner": { "v": 3 } } }),
        "injection à travers les $ref imbriqués"
    );

    // Défaut dans une branche `allOf` derrière un `$ref` : les combinateurs
    // visités par la détection doivent aussi être visités par l'injection
    // (mêmes sous-schémas vus des deux côtés).
    let combinator = json!({
        "type": "object",
        "properties": { "x": { "$ref": "#/$defs/W" } },
        "$defs": {
            "W": {
                "allOf": [
                    { "$ref": "#/$defs/W" },
                    { "type": "object", "properties": { "b": { "default": 1 } } }
                ]
            }
        }
    });
    let schema = from_json_schema(combinator);
    assert!(
        schema.has_defaults(),
        "le défaut dans la branche allOf doit être détecté"
    );
    let mut value = json!({ "x": {} });
    schema.apply_defaults(&mut value);
    assert_eq!(
        value,
        json!({ "x": { "b": 1 } }),
        "la branche allOf porte un défaut publié : il est injecté"
    );
}

#[test]
fn racine_reference_resolue_pour_les_trois_operations() {
    // Document dont la RACINE est un `$ref` (types récursifs schemars) :
    // détection, injection et défaut racine doivent résoudre la référence.
    let doc = json!({
        "$ref": "#/$defs/Root",
        "$defs": {
            "Root": {
                "type": "object",
                "properties": { "n": { "type": "integer", "default": 2 } }
            }
        }
    });
    let schema = from_json_schema(doc);
    assert!(
        schema.has_defaults(),
        "défaut derrière la racine $ref détecté"
    );
    let mut value = json!({});
    schema.apply_defaults(&mut value);
    assert_eq!(value, json!({ "n": 2 }), "injection depuis la racine $ref");
}

// ===========================================================================
// 3. Cycles : terminaison garantie, défauts atteignables quand même
// ===========================================================================

#[test]
fn cycle_de_refs_termine_et_injecte_les_defauts_atteignables() {
    // Type récursif : le défaut est sur chaque niveau, le `$ref` reboucle.
    // Détection et injection doivent terminer (borne + détection de cycle)
    // et injecter le long de l'instance **finie**.
    let doc = json!({
        "type": "object",
        "properties": { "node": { "$ref": "#/$defs/Node" } },
        "$defs": {
            "Node": {
                "type": "object",
                "properties": {
                    "value": { "type": "integer", "default": 5 },
                    "next": { "$ref": "#/$defs/Node" }
                }
            }
        }
    });
    let schema = from_json_schema(doc);
    assert!(schema.has_defaults(), "défaut du type récursif détecté");
    let mut value = json!({ "node": { "next": { "next": {} } } });
    schema.apply_defaults(&mut value);
    assert_eq!(
        value,
        json!({ "node": { "value": 5, "next": { "value": 5, "next": { "value": 5 } } } }),
        "injection le long de l'instance finie, sans boucler"
    );

    // Cycle PUR de `$ref` (A → B → A) sans aucun défaut atteignable :
    // terminaison, has_defaults faux, aucune injection.
    let pure = json!({
        "type": "object",
        "properties": { "x": { "$ref": "#/$defs/A" } },
        "$defs": { "A": { "$ref": "#/$defs/B" }, "B": { "$ref": "#/$defs/A" } }
    });
    let schema = from_json_schema(pure);
    assert!(
        !schema.has_defaults(),
        "aucun défaut atteignable derrière le cycle pur"
    );
    let mut value = json!({ "x": {} });
    schema.apply_defaults(&mut value);
    assert_eq!(
        value,
        json!({ "x": {} }),
        "rien à injecter : valeur intacte"
    );
}

#[test]
fn cycle_detecte_dans_la_resolution_chaine_meme_avec_defaut_ailleurs() {
    // La chaîne de résolution entre dans un cycle mais le document porte un
    // défaut AILLEURS : la détection doit terminer et le trouver.
    let doc = json!({
        "type": "object",
        "properties": {
            "x": { "$ref": "#/$defs/A" },
            "y": { "type": "string", "default": "ok" }
        },
        "$defs": { "A": { "$ref": "#/$defs/B" }, "B": { "$ref": "#/$defs/A" } }
    });
    let schema = from_json_schema(doc);
    assert!(
        schema.has_defaults(),
        "le défaut de `y` doit être vu malgré le cycle sous `x`"
    );
    let mut value = json!({});
    schema.apply_defaults(&mut value);
    assert_eq!(
        value,
        json!({ "y": "ok" }),
        "seul le défaut atteignable est injecté"
    );
}

// ===========================================================================
// 4. Borne explicite de profondeur
// ===========================================================================

/// Chaîne de `n` `$ref` (c1 → c2 → … → cn) aboutissant à un schéma porteur
/// d'un défaut.
fn chain_doc(n: usize) -> Value {
    let mut defs = serde_json::Map::new();
    for level in 1..=n {
        let target = if level == n {
            json!({
                "type": "object",
                "properties": { "v": { "type": "integer", "default": 1 } }
            })
        } else {
            json!({ "$ref": format!("#/$defs/c{}", level + 1) })
        };
        defs.insert(format!("c{level}"), target);
    }
    json!({
        "type": "object",
        "properties": { "x": { "$ref": "#/$defs/c1" } },
        "$defs": defs
    })
}

#[test]
fn chaine_dans_la_borne_est_resolue() {
    // 10 sauts : bien sous la borne (16).
    let schema = from_json_schema(chain_doc(10));
    assert!(schema.has_defaults(), "chaîne courte : défaut détecté");
    let mut value = json!({ "x": {} });
    schema.apply_defaults(&mut value);
    assert_eq!(
        value,
        json!({ "x": { "v": 1 } }),
        "défaut injecté à travers 10 $ref"
    );
}

#[test]
fn chaine_longue_finite_est_resolue_entierement() {
    // 24 sauts : une indirection finie et acyclique est légitime — elle est
    // résolue en totalité (le budget de 16 sauts est **par résolution de
    // chaîne**, pas par opération : chaque position de schéma re-résout
    // avec un budget neuf, de façon déterministe).
    let schema = from_json_schema(chain_doc(24));
    assert!(schema.has_defaults(), "chaîne finie : défaut détecté");
    let mut value = json!({ "x": {} });
    schema.apply_defaults(&mut value);
    assert_eq!(
        value,
        json!({ "x": { "v": 1 } }),
        "défaut injecté à travers 24 $ref"
    );
}

/// Document d'une profondeur structurelle de `levels` niveaux d'objets
/// imbriqués, avec un défaut tout au fond.
fn deep_doc(levels: usize) -> Value {
    let mut node = json!({
        "type": "object",
        "properties": { "v": { "type": "integer", "default": 1 } }
    });
    for _ in 0..levels {
        node = json!({ "type": "object", "properties": { "child": node } });
    }
    node
}

#[test]
fn profondeur_structurelle_hors_borne_est_tronquee() {
    // 20 niveaux d'imbrication structurelle : au-delà de la borne (16), la
    // descente s'arrête — comportement défini : défaut non annoncé, aucune
    // injection, aucune panique, aucun hang.
    let schema = from_json_schema(deep_doc(20));
    assert!(
        !schema.has_defaults(),
        "au-delà de la borne de profondeur, le défaut du fond n'est pas annoncé"
    );
    let mut instance = json!({});
    for _ in 0..20 {
        instance = json!({ "child": instance });
    }
    let original = instance.clone();
    schema.apply_defaults(&mut instance);
    assert_eq!(instance, original, "au-delà de la borne : valeur intacte");
}

#[test]
fn chaine_hors_borne_composite_est_tronquee() {
    // 300 sauts : la portée maximale (16 niveaux de parcours × 16 sauts par
    // résolution) est dépassée — la résolution s'arrête, comportement
    // défini, sans boucler ni paniquer.
    let schema = from_json_schema(chain_doc(300));
    assert!(
        !schema.has_defaults(),
        "au-delà de la borne composite : non annoncé"
    );
    let mut value = json!({ "x": {} });
    schema.apply_defaults(&mut value);
    assert_eq!(
        value,
        json!({ "x": {} }),
        "au-delà de la borne composite : aucune injection"
    );
}

// ===========================================================================
// 5. Absence de défaut : aucun changement, aucun coût comportemental
// ===========================================================================

#[test]
fn absence_de_default_ne_change_rien() {
    let doc = json!({
        "type": "object",
        "properties": {
            "a": { "type": "integer" },
            "b": { "type": "object", "properties": { "c": { "type": "string" } } }
        },
        "required": ["a"]
    });
    let schema = from_json_schema(doc);
    assert!(
        !schema.has_defaults(),
        "aucun default publié : has_defaults doit rester faux"
    );
    let original = json!({ "a": 1, "b": { "c": "x" } });
    let mut value = original.clone();
    schema.apply_defaults(&mut value);
    assert_eq!(value, original, "sans défaut publié, la valeur est intacte");

    // Bout en bout : le handler reçoit exactement les arguments envoyés.
    let vu = seen_arguments(
        from_json_schema(json!({
            "type": "object",
            "properties": {
                "a": { "type": "integer" },
                "b": { "type": "object", "properties": { "c": { "type": "string" } } }
            },
            "required": ["a"]
        })),
        json!({ "a": 1, "b": { "c": "x" } }),
    );
    assert_eq!(vu, json!({ "a": 1, "b": { "c": "x" } }));
}

#[test]
fn default_dans_une_definition_non_referencee_n_injecte_rien() {
    // Un `default` dans `$defs` mais jamais référencé par la surface servie
    // n'est pas un défaut publié de l'inputSchema : rien n'est injecté.
    let doc = json!({
        "type": "object",
        "properties": { "a": { "type": "integer" } },
        "$defs": { "Orphelin": { "type": "integer", "default": 42 } }
    });
    let schema = from_json_schema(doc);
    assert!(
        !schema.has_defaults(),
        "un $defs orphelin ne rend pas has_defaults vrai"
    );
    let mut value = json!({});
    schema.apply_defaults(&mut value);
    assert_eq!(value, json!({}));
}

// ===========================================================================
// 6. Preuve : has_defaults ne désactive jamais une injection nécessaire
// ===========================================================================

/// La propriété centrale du couple détection/injection : **si** appliquer
/// les défauts **change** la valeur, alors `has_defaults()` doit être vrai —
/// sinon le noyau (server.rs) court-circuite `apply_defaults` et une
/// injection nécessaire est perdue. Vérifié sur une batterie de documents
/// couvrant chaque forme visitée (défaut direct, imbriqué, chaîne de `$ref`,
/// `$ref` imbriqué, branche `allOf`, `items`, racine `$ref`) et sur les deux
/// adaptateurs.
#[test]
fn has_defaults_ne_desactive_jamais_une_injection_necessaire() {
    let cases: Vec<(Value, Value)> = vec![
        // (document, instance) — l'injection doit-elle changer l'instance ?
        (
            json!({
                "type": "object",
                "properties": { "n": { "type": "integer", "default": 1 } }
            }),
            json!({}),
        ),
        (
            json!({
                "type": "object",
                "properties": {
                    "outer": { "type": "object", "properties": { "d": { "default": 2 } } }
                }
            }),
            json!({ "outer": {} }),
        ),
        (chained_doc(), json!({ "x": {} })),
        (
            json!({
                "type": "object",
                "properties": { "x": { "$ref": "#/$defs/Outer" } },
                "$defs": {
                    "Outer": {
                        "type": "object",
                        "properties": { "inner": { "$ref": "#/$defs/Deep" } }
                    },
                    "Deep": { "type": "object", "properties": { "v": { "default": 3 } } }
                }
            }),
            json!({ "x": { "inner": {} } }),
        ),
        (
            json!({
                "type": "object",
                "properties": { "x": { "$ref": "#/$defs/W" } },
                "$defs": {
                    "W": {
                        "allOf": [
                            { "$ref": "#/$defs/W" },
                            { "type": "object", "properties": { "b": { "default": 1 } } }
                        ]
                    }
                }
            }),
            json!({ "x": {} }),
        ),
        (
            json!({
                "type": "object",
                "properties": {
                    "list": {
                        "type": "array",
                        "items": { "type": "object", "properties": { "f": { "default": true } } }
                    }
                }
            }),
            json!({ "list": [ {} ] }),
        ),
        (
            json!({
                "$ref": "#/$defs/Root",
                "$defs": {
                    "Root": {
                        "type": "object",
                        "properties": { "n": { "type": "integer", "default": 2 } }
                    }
                }
            }),
            json!({}),
        ),
        // Frères d'un nœud à `$ref` coupé (cycle) : la détection voit les
        // frères, l'injection doit donc les voir aussi.
        (cycle_cut_doc(), json!({ "x": {} })),
        // Frères d'un nœud à `$ref` coupé (budget de sauts épuisé).
        (budget_cut_doc(), json!({ "x": {} })),
    ];

    for (index, (document, instance)) in cases.into_iter().enumerate() {
        let schema = from_json_schema(document.clone());
        let before = instance.clone();
        let mut after = instance.clone();
        schema.apply_defaults(&mut after);
        if after != before {
            assert!(
                schema.has_defaults(),
                "cas {index} : apply_defaults a injecté ({before} → {after}) mais \
                 has_defaults() est faux — le noyau désactiverait cette injection : {document}"
            );
        }
        // Symétrique (l'injection n'invente rien) : chaque cas changeant est
        // couvert ci-dessus ; les cas sans défaut sont couverts par les
        // tests `absence_de_default…` et `default_dans_une_definition…`.
    }

    // Même preuve côté schemars : le défaut dérivé est détecté ET appliqué.
    let schema = from_schemars::<SchemarsDefaultArgs>();
    assert!(
        schema.has_defaults(),
        "le défaut dérivé par schemars doit être détecté"
    );
    let mut value = json!({});
    schema.apply_defaults(&mut value);
    assert_eq!(value, json!({ "count": 7 }));

    // Et la contre-preuve : un type sans `#[serde(default)]` ne publie ni
    // n'injecte rien.
    let schema = from_schemars::<ParityArgs>();
    assert!(!schema.has_defaults());
    let mut value = json!({});
    schema.apply_defaults(&mut value);
    assert_eq!(value, json!({}));

    // Un `default` porté par le nœud racine lui-même (contrat de
    // `default_value()`, pas de l'injection dans les champs) : détecté,
    // exposé par `default_value()`, et sans effet sur les champs absents.
    let schema = from_json_schema(json!({
        "type": "object",
        "default": { "n": 4 },
        "properties": { "n": { "type": "integer" } }
    }));
    assert!(schema.has_defaults());
    assert_eq!(schema.default_value(), Some(&json!({ "n": 4 })));
}

// ===========================================================================
// 7. Symétrie exacte : frères d'un nœud à `$ref` coupé
// ===========================================================================

/// Nœud `A` portant À LA FOIS un `$ref` entrant dans un cycle (`A → B → A`)
/// et des mots-clés frères (`properties`) porteur du défaut. La résolution
/// de la chaîne est coupée par le cycle : le nœud examiné après résolution
/// porte encore un `$ref` — ses frères restent des sous-schémas publiés,
/// que la détection explore toujours.
fn cycle_cut_doc() -> Value {
    json!({
        "type": "object",
        "properties": { "x": { "$ref": "#/$defs/A" } },
        "$defs": {
            "A": {
                "$ref": "#/$defs/B",
                "properties": { "b": { "type": "integer", "default": 1 } }
            },
            "B": { "$ref": "#/$defs/A" }
        }
    })
}

/// Chaîne de 20 `$ref` dont le maillon `c16` — exactement le point où le
/// budget de [`MAX_REF_DEPTH`] sauts coupe la résolution depuis `x`
/// (`x→c1` est le premier saut) — porte un `$ref` (vers `c17`) **et** des
/// frères publieurs de défaut. Les maillons strictement intermédiaires
/// (traversés par la résolution) restent transparents pour les **deux**
/// parcours — seuls les frères du nœud rendu par la résolution sont en jeu.
fn budget_cut_doc() -> Value {
    let mut defs = serde_json::Map::new();
    for level in 1..=20 {
        let target = match level {
            16 => json!({
                "$ref": "#/$defs/c17",
                "properties": { "b": { "type": "integer", "default": 1 } }
            }),
            20 => json!({ "type": "object" }),
            _ => json!({ "$ref": format!("#/$defs/c{}", level + 1) }),
        };
        defs.insert(format!("c{level}"), target);
    }
    json!({
        "type": "object",
        "properties": { "x": { "$ref": "#/$defs/c1" } },
        "$defs": Value::Object(defs)
    })
}

#[test]
fn freres_d_un_noeud_ref_coupe_par_cycle_injectes_comme_detectes() {
    let schema = from_json_schema(cycle_cut_doc());

    // La détection voit le défaut dans les frères du nœud coupé…
    assert!(
        schema.has_defaults(),
        "le défaut dans les frères du nœud à `$ref` coupé (cycle) doit être détecté"
    );

    // …l'injection doit donc l'appliquer aussi (mêmes sous-schémas vus des
    // deux côtés) : avant correctif, la reprise de la chaîne coupée
    // court-circuitait les frères et le défaut n'arrivait jamais.
    let vu = seen_arguments(from_json_schema(cycle_cut_doc()), json!({ "x": {} }));
    assert_eq!(
        vu,
        json!({ "x": { "b": 1 } }),
        "le défaut publié dans les frères d'un nœud à `$ref` coupé (cycle) doit être injecté"
    );

    // Champ absent : le nœud coupé ne porte pas de défaut racine — rien
    // n'est inventé (le défaut des frères s'adresse à l'intérieur de `x`).
    let absent = seen_arguments(from_json_schema(cycle_cut_doc()), json!({}));
    assert_eq!(absent, json!({}), "x sans défaut propre n'est pas fabriqué");
}

#[test]
fn freres_d_un_noeud_ref_coupe_par_budget_injectes_comme_detectes() {
    let schema = from_json_schema(budget_cut_doc());

    assert!(
        schema.has_defaults(),
        "le défaut dans les frères du nœud à `$ref` coupé (budget) doit être détecté"
    );
    let vu = seen_arguments(from_json_schema(budget_cut_doc()), json!({ "x": {} }));
    assert_eq!(
        vu,
        json!({ "x": { "b": 1 } }),
        "le défaut publié dans les frères du point de coupe (16 sauts) doit être injecté"
    );

    // Le reste de la chaîne reste résolu normalement : un défaut au-delà du
    // point de coupe est toujours vu des deux côtés (les maillons
    // intermédiaires traversés par la résolution, eux, restent transparents
    // pour les deux parcours — sémantique documentée de la résolution).
    let mut defs = serde_json::Map::new();
    for level in 1..=15 {
        defs.insert(
            format!("c{level}"),
            json!({ "$ref": format!("#/$defs/c{}", level + 1) }),
        );
    }
    defs.insert("c16".to_string(), json!({ "$ref": "#/$defs/c17" }));
    defs.insert(
        "c17".to_string(),
        json!({
            "type": "object",
            "properties": { "v": { "type": "integer", "default": 2 } }
        }),
    );
    let suite = json!({
        "type": "object",
        "properties": { "x": { "$ref": "#/$defs/c1" } },
        "$defs": Value::Object(defs)
    });
    let schema = from_json_schema(suite.clone());
    assert!(
        schema.has_defaults(),
        "défaut au-delà du point de coupe détecté"
    );
    let vu = seen_arguments(from_json_schema(suite), json!({ "x": {} }));
    assert_eq!(
        vu,
        json!({ "x": { "v": 2 } }),
        "le défaut au bout de la chaîne (au-delà du point de coupe) est injecté"
    );
}

// ===========================================================================
// 8. inputSchema racine booléen : refusé pour un outil MCP
// ===========================================================================

#[test]
#[should_panic(expected = "inputSchema")]
fn input_schema_racine_booleen_vrai_refuse_pour_un_outil() {
    // `true` est un schéma JSON Schema valide… mais pas un inputSchema MCP
    // (qui exige un objet) : l'enregistrement d'un outil le refuse.
    let _ = ServerBuilder::new("reg", "1.0").tool_typed(
        "t",
        "t",
        from_json_schema(json!(true)),
        |_args| Ok(json!({})),
    );
}

#[test]
#[should_panic(expected = "inputSchema")]
fn input_schema_racine_booleen_faux_refuse_pour_un_outil() {
    let _ = ServerBuilder::new("reg", "1.0").tool_typed(
        "t",
        "t",
        from_json_schema(json!(false)),
        |_args| Ok(json!({})),
    );
}

#[test]
#[should_panic(expected = "inputSchema")]
fn input_schema_racine_booleen_refuse_aussi_via_tool_parse() {
    let _ = ServerBuilder::new("reg", "1.0").tool_parse(
        "t",
        "t",
        from_json_schema(json!(true)),
        |_args: Value| Ok(json!({})),
    );
}

#[test]
fn schemas_booleens_aux_emplacements_json_schema_restants() {
    // Le refus ne vise que la RACINE d'un inputSchema d'outil : un schéma
    // booléen reste valide partout où JSON Schema l'autorise (valeur de
    // `properties`, `items`…) — l'enregistrement passe et l'outil marche.
    let doc = json!({
        "type": "object",
        "properties": { "anything": true },
        "required": []
    });
    let vu = seen_arguments(from_json_schema(doc), json!({ "anything": 12 }));
    assert_eq!(
        vu,
        json!({ "anything": 12 }),
        "un sous-schéma booléen (true = toujours valide) reste accepté"
    );

    let items_doc =
        json!({ "type": "object", "properties": { "list": { "type": "array", "items": true } } });
    let vu = seen_arguments(from_json_schema(items_doc), json!({ "list": [1, 2] }));
    assert_eq!(vu, json!({ "list": [1, 2] }), "`items: true` reste accepté");

    // La construction `from_json_schema(bool)` reste elle-même valide (un
    // booléen est un schéma JSON Schema) : c'est l'USAGE comme inputSchema
    // d'outil qui est refusé.
    let _ = from_json_schema(json!(true));
}

// ===========================================================================
// 9. M1 — aucun défaut injecté dans une branche `anyOf`/`oneOf` non choisie
// ===========================================================================

/// Verdict du validateur de **référence** (`jsonschema`) sur un document.
fn reference_valid(document: &Value, instance: &Value) -> bool {
    jsonschema::options()
        .build(document)
        .expect("le document doit compiler chez la référence")
        .is_valid(instance)
}

/// Verdict de l'adaptateur sur une instance.
fn accepted(schema: &dyn Schema, instance: &Value) -> bool {
    let mut issues = litemcp::zod::Issues::new();
    let mut path = litemcp::zod::Path::root("arguments");
    schema.validate(&mut path, instance, &mut issues);
    issues.is_empty()
}

/// `oneOf` de deux objets **stricts** : `{"a": 5}` ne satisfait que la
/// première branche, la seconde porte un défaut `b`.
fn one_of_strict_doc() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": { "b": { "type": "integer", "default": 2 } },
                "additionalProperties": false
            }
        ]
    })
}

#[test]
fn aucun_defaut_injecte_dans_une_branche_one_of_non_selectionnee() {
    // Preuve du défaut (audit M1) : la référence juge `{"a": 5}` valide…
    assert!(
        reference_valid(&one_of_strict_doc(), &json!({ "a": 5 })),
        "l'instance de départ est valide"
    );
    // …et l'appel doit donc réussir : avant correctif, le `b: 2` de l'autre
    // branche était injecté, rendant l'instance invalide (-32602).
    let vu = seen_arguments(from_json_schema(one_of_strict_doc()), json!({ "a": 5 }));
    assert_eq!(
        vu,
        json!({ "a": 5 }),
        "aucun défaut ne doit venir d'une branche non sélectionnée"
    );
}

#[test]
fn defaut_de_la_branche_selectionnee_reste_injecte() {
    // Le correctif n'est pas « plus d'injection du tout » : `{"a": 5}`
    // sélectionne la première branche, qui porte `c` avec défaut 7.
    let doc = json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "a": { "type": "integer" },
                    "c": { "type": "integer", "default": 7 }
                },
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": { "b": { "type": "integer" } },
                "additionalProperties": false
            }
        ]
    });
    let vu = seen_arguments(from_json_schema(doc), json!({ "a": 5 }));
    assert_eq!(
        vu,
        json!({ "a": 5, "c": 7 }),
        "le défaut de la branche choisie reste injecté"
    );
}

/// Forme publiée par `schemars` pour une énumération taguée avec un défaut
/// dans une variante (`oneOf` d'objets, `const` sur le discriminant).
fn tagged_enum_doc() -> Value {
    json!({
        "type": "object",
        "oneOf": [
            {
                "type": "object",
                "properties": { "kind": { "type": "string", "const": "fast" } },
                "required": ["kind"]
            },
            {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "const": "slow" },
                    "delay": { "type": "integer", "format": "uint8", "default": 0, "maximum": 255, "minimum": 0 }
                },
                "required": ["kind"]
            }
        ]
    })
}

#[test]
fn instance_valide_avant_injection_reste_valide_apres() {
    let cases: Vec<(Value, Value)> = vec![
        // `oneOf` strict : le défaut de l'autre branche invalidait l'instance.
        (one_of_strict_doc(), json!({ "a": 5 })),
        // `anyOf` strict : même défaut, autre mot-clé.
        (
            json!({
                "anyOf": [
                    { "type": "object", "properties": { "a": { "type": "integer" } }, "additionalProperties": false },
                    { "type": "object", "properties": { "b": { "type": "integer", "default": 2 } }, "additionalProperties": false }
                ]
            }),
            json!({ "a": 5 }),
        ),
        // `anyOf` : `{}` satisfait les deux branches, une seule reçoit ses défauts.
        (
            json!({
                "anyOf": [
                    { "type": "object", "properties": { "a": { "type": "integer", "default": 1 } }, "additionalProperties": false },
                    { "type": "object", "properties": { "b": { "type": "integer", "default": 2 } }, "additionalProperties": false }
                ]
            }),
            json!({}),
        ),
        // Énumération taguée (forme publiée par schemars).
        (tagged_enum_doc(), json!({ "kind": "fast" })),
        (tagged_enum_doc(), json!({ "kind": "slow" })),
        // Objet simple, défaut imbriqué.
        (
            json!({
                "type": "object",
                "properties": { "outer": { "type": "object", "properties": { "d": { "type": "integer", "default": 2 } } } }
            }),
            json!({ "outer": {} }),
        ),
        // `allOf` : toutes les branches s'appliquent, les défauts restent injectés.
        (
            json!({ "type": "object", "allOf": [ { "properties": { "a": { "type": "integer", "default": 1 } } } ] }),
            json!({}),
        ),
    ];
    for (index, (document, instance)) in cases.into_iter().enumerate() {
        assert!(
            reference_valid(&document, &instance),
            "cas {index} : l'instance de départ doit être valide : {instance} / {document}"
        );
        let mut after = instance.clone();
        from_json_schema(document.clone()).apply_defaults(&mut after);
        assert!(
            reference_valid(&document, &after),
            "cas {index} : l'injection a invalidé une instance valide ({instance} → {after}) : {document}"
        );
    }
}

#[test]
fn enum_taguee_schemars_ne_recoit_pas_le_defaut_d_une_autre_variante() {
    #[derive(JsonSchema)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    #[allow(dead_code)]
    enum Mode {
        Fast,
        Slow {
            #[serde(default)]
            delay: u8,
        },
    }

    // `{"kind":"fast"}` : valide, et le handler ne doit PAS recevoir le
    // `delay` de la variante `slow`.
    let vu = seen_arguments(from_schemars::<Mode>(), json!({ "kind": "fast" }));
    assert_eq!(vu, json!({ "kind": "fast" }), "aucun `delay` inventé");

    // `{"kind":"slow"}` : la branche sélectionnée porte le défaut → injecté.
    let vu = seen_arguments(from_schemars::<Mode>(), json!({ "kind": "slow" }));
    assert_eq!(
        vu,
        json!({ "kind": "slow", "delay": 0 }),
        "le défaut de la variante choisie reste injecté"
    );
}

// ===========================================================================
// 10. M3 — `Option<T>` : même verdict `null` sur les trois chemins
// ===========================================================================

#[derive(JsonSchema)]
#[allow(dead_code)] // dérivation uniquement
struct OptionParity {
    n: Option<u64>,
    s: Option<String>,
}

#[test]
fn option_null_verdict_identique_pour_zod_et_schemars() {
    let zod_schema = litemcp::zod::object()
        .opt("n", litemcp::zod::uint())
        .opt("s", litemcp::zod::string());
    let derived = from_schemars::<OptionParity>();

    // Le schéma PUBLIÉ dit la même chose des deux côtés : `null` admis, et
    // la borne haute de `u64` présente (schemars l'omet, l'adaptateur la
    // complète).
    for (label, document) in [("zod", zod_schema.json()), ("schemars", derived.json())] {
        assert_eq!(
            document["properties"]["s"]["type"],
            json!(["string", "null"]),
            "type publié côté {label} : {document}"
        );
        assert_eq!(
            document["properties"]["n"]["type"],
            json!(["integer", "null"]),
            "type publié côté {label} : {document}"
        );
        assert_eq!(
            document["properties"]["n"]["maximum"],
            json!(u64::MAX),
            "borne haute publiée côté {label} : {document}"
        );
    }

    // …et le verdict est identique sur `null`, l'absence et les valeurs.
    let two_pow_64: Value = serde_json::from_str("18446744073709551616").unwrap();
    for instance in [
        json!({}),
        json!({ "n": null, "s": null }),
        json!({ "n": 7, "s": "x" }),
        json!({ "n": 7.0 }),
        json!({ "n": -1 }),
        json!({ "n": 1.5 }),
        json!({ "s": 3 }),
        json!({ "n": u64::MAX }),
        json!({ "n": two_pow_64 }),
        json!({ "n": 1e19 }),
    ] {
        assert_eq!(
            accepted(&zod_schema, &instance),
            accepted(&derived, &instance),
            "verdict divergent sur {instance}"
        );
    }
}

#[test]
fn option_null_arrive_comme_absent_dans_les_deux_adaptateurs() {
    // `null` n'est ni refusé ni remplacé : il arrive tel quel au handler,
    // qui le lit comme l'absence (`Option<T>` → `None`).
    for (label, vu) in [
        (
            "zod",
            seen_arguments(
                litemcp::zod::object()
                    .opt("n", litemcp::zod::integer().min(0))
                    .opt("s", litemcp::zod::string()),
                json!({ "n": null }),
            ),
        ),
        (
            "schemars",
            seen_arguments(from_schemars::<OptionParity>(), json!({ "n": null })),
        ),
    ] {
        assert_eq!(vu, json!({ "n": null }), "côté {label}");
    }
}

// ===========================================================================
// 10bis. D1 — `Option<T>` + `default` : `null` ≡ absence, un seul verdict
// ===========================================================================

/// Le cas laissé en exception par C1 : un champ `Option<u64>` couvert par un
/// `#[serde(default)]` — schemars publie `type: ["integer", "null"]` et
/// accepte `null`, zod publiait `integer` et le refusait.
#[derive(JsonSchema)]
#[allow(dead_code)] // dérivation uniquement
struct OptionDefaultParity {
    #[serde(default)]
    v: Option<u64>,
}

fn zod_uint_default() -> impl Schema + 'static {
    litemcp::zod::object().opt("v", litemcp::zod::uint().default(3))
}

#[test]
fn option_avec_defaut_meme_type_publie_et_meme_verdict_sur_null() {
    let zod_schema = zod_uint_default();
    let derived = from_schemars::<OptionDefaultParity>();
    let zod_doc = zod_schema.json();
    let derived_doc = derived.json();

    // Même `type` publié : tableau incluant `null`, et `default` conservé.
    for (label, document) in [("zod", &zod_doc), ("schemars", &derived_doc)] {
        assert_eq!(
            document["properties"]["v"]["type"],
            json!(["integer", "null"]),
            "type publié côté {label} : {document}"
        );
    }
    assert_eq!(zod_doc["properties"]["v"]["default"], json!(3));
    assert_eq!(derived_doc["properties"]["v"]["default"], Value::Null);

    // Même verdict sur l'absence, `null`, une valeur, un négatif, un non entier.
    for instance in [
        json!({}),
        json!({ "v": null }),
        json!({ "v": 7 }),
        json!({ "v": -1 }),
        json!({ "v": 1.5 }),
    ] {
        assert_eq!(
            accepted(&zod_schema, &instance),
            accepted(&derived, &instance),
            "verdict divergent sur {instance}"
        );
    }

    // Côté zod : `{}` ET `null` injectent le défaut (null ≡ absence).
    for instance in [json!({}), json!({ "v": null })] {
        assert_eq!(
            seen_arguments(zod_uint_default(), instance.clone()),
            json!({ "v": 3 }),
            "le défaut doit être injecté sur {instance}"
        );
    }

    // Côté schemars : le défaut serde (`None`) reste injecté pour `{}` — la
    // sémantique serde n'est pas modifiée (seul le VERDICT est figé).
    assert_eq!(
        seen_arguments(from_schemars::<OptionDefaultParity>(), json!({})),
        json!({ "v": null }),
        "le défaut serde reste injecté"
    );
}

// ===========================================================================
// 11. M5 — inputSchema dont la racine n'admet pas d'objet : refusé à
//     l'enregistrement (même convention que la racine booléenne)
// ===========================================================================

#[test]
#[should_panic(expected = "inputSchema")]
fn input_schema_racine_string_refusee_pour_un_outil() {
    let _ = ServerBuilder::new("reg", "1.0").tool_typed(
        "t",
        "t",
        from_json_schema(json!({ "type": "string" })),
        |_args| Ok(json!({})),
    );
}

#[test]
#[should_panic(expected = "inputSchema")]
fn input_schema_racine_enum_refusee_pour_un_outil() {
    // Une racine `enum` sans `object` n'admet aucun argument : l'outil
    // serait inutilisable (tout appel rendrait -32602).
    let _ = ServerBuilder::new("reg", "1.0").tool_typed(
        "t",
        "t",
        from_json_schema(json!({ "enum": ["a", "b"] })),
        |_args| Ok(json!({})),
    );
}

#[test]
#[should_panic(expected = "inputSchema")]
fn input_schema_racine_ref_vers_non_objet_refusee() {
    let _ = ServerBuilder::new("reg", "1.0").tool_typed(
        "t",
        "t",
        from_json_schema(json!({
            "$ref": "#/$defs/S",
            "$defs": { "S": { "type": "string" } }
        })),
        |_args| Ok(json!({})),
    );
}

#[test]
#[should_panic(expected = "inputSchema")]
fn input_schema_racine_non_objet_refusee_via_from_schemars() {
    // L'audit (M5) : `enum Simple { Alpha, Beta }` publie
    // `{"enum":["Alpha","Beta"],"type":"string"}` — inutilisable comme
    // inputSchema, découvert seulement au premier appel avant correctif.
    #[derive(JsonSchema)]
    #[allow(dead_code)]
    enum Simple {
        Alpha,
        Beta,
    }
    let _ =
        ServerBuilder::new("reg", "1.0")
            .tool_typed("t", "t", from_schemars::<Simple>(), |_args| Ok(json!({})));
}

#[test]
fn input_schema_racine_qui_admet_un_objet_reste_accepte() {
    // Formes valides : `type` objet, tableau contenant `object`, `anyOf`
    // d'objets, racine `$ref` vers un objet, racine sans `type`.
    let documents = [
        json!({ "type": "object" }),
        json!({ "type": ["object", "null"] }),
        json!({ "anyOf": [ { "type": "object", "properties": { "a": { "type": "integer" } } } ] }),
        json!({ "$ref": "#/$defs/O", "$defs": { "O": { "type": "object" } } }),
        json!({ "properties": { "a": { "type": "integer" } } }),
    ];
    for document in documents {
        let _ = ServerBuilder::new("reg", "1.0").tool_typed(
            "t",
            "t",
            from_json_schema(document.clone()),
            |_args| Ok(json!({})),
        );
    }
}
