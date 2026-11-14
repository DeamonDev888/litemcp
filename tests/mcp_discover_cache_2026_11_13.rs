//! C4 — `server/discover` doit porter les indices de cache de la page
//! *Caching* (`2026-07-28`), avec les mêmes valeurs et le même mécanisme que
//! `tools/list` / `resources/list`.
//!
//! Page : <https://modelcontextprotocol.io/specification/2026-07-28/server/utilities/caching>
//! « Servers MUST include caching hints on results with `resultType:
//! "complete"` returned by the following operations: `server/discover`,
//! `tools/list`, … ».
//!
//! Ces tests échouent **avant** correctif : `DiscoverResult` n'avait ni
//! `ttlMs` ni `cacheScope`.

use litemcp::{ServerBuilder, ServerCore};
use serde_json::{Value, json};

fn core() -> ServerCore {
    ServerBuilder::new("discover", "0.1.0")
        .tool("add", "Additionne", |_| Ok(json!({ "somme": 2 })))
        .core()
}

fn dispatch(core: &ServerCore, id: i64, method: &str) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        },
    });
    let reply = core
        .dispatch(serde_json::to_string(&body).unwrap().as_bytes())
        .expect("une requête identifiée reçoit une réponse");
    serde_json::from_slice(&reply).expect("réponse JSON-RPC lisible")
}

/// La page *Caching* exige `ttlMs >= 0` et un `cacheScope` parmi
/// `"public"` / `"private"` sur tout résultat `complete` de `server/discover`.
#[test]
fn discover_carries_the_caching_hints() {
    let v = dispatch(&core(), 1, "server/discover");
    assert!(
        v.get("error").is_none(),
        "server/discover doit réussir : {v}"
    );
    assert_eq!(v["result"]["resultType"], "complete", "{v}");
    assert_eq!(v["result"]["ttlMs"], 60_000, "{v}");
    assert_eq!(v["result"]["cacheScope"], "private", "{v}");
    // Les champs historiques restent servis.
    assert_eq!(
        v["result"]["supportedVersions"],
        json!(["2026-07-28"]),
        "{v}"
    );
    assert!(
        v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"].is_object(),
        "identité serveur attendue sous _meta : {v}"
    );
}

/// Même mécanisme que les catalogues : la constante `DEFAULT_LIST_TTL_MS`
/// et la même portée, donc des valeurs **identiques** à `tools/list` et
/// `resources/list` — pas une seconde politique de cache.
#[test]
fn discover_cache_hints_match_the_list_catalogues() {
    let core = core();
    let discover = dispatch(&core, 1, "server/discover");
    for (id, method) in [(2, "tools/list"), (3, "resources/list")] {
        let list = dispatch(&core, id, method);
        assert_eq!(
            discover["result"]["ttlMs"], list["result"]["ttlMs"],
            "{method} : ttlMs divergent — {discover} vs {list}"
        );
        assert_eq!(
            discover["result"]["cacheScope"], list["result"]["cacheScope"],
            "{method} : cacheScope divergent — {discover} vs {list}"
        );
    }
}
