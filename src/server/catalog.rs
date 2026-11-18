//! Catalogues pré-sérialisés et listings : `Visible`, `Catalogue`,
//! `initialize`, `discover` et les quatre listes.

use super::authz::rights_held;
use super::handlers::derive_prompt_arguments;
use super::*;

/// Visibilité d'un catalogue pour une requête — voir
/// `ServerCore::visibility`.
pub(super) enum Visible<'a> {
    /// Autorisation **inactive** : tout est visible (cache précalculé).
    All,
    /// Autorisation active : seuls les éléments dont **tous** les droits sont
    /// détenus par l'appelant.
    Context(&'a auth::AuthContext),
    /// Autorisation active sans identité : rien n'est visible (échec fermé).
    Nothing,
}

impl Visible<'_> {
    /// Cet élément apparaît-il dans la liste ?
    ///
    /// Un élément **sans droit déclaré** n'apparaît jamais dès que
    /// l'autorisation est active ([`rights_held`] est faux sur un ensemble
    /// vide).
    fn shows(&self, required: &[String]) -> bool {
        match self {
            Self::All => true,
            Self::Context(context) => rights_held(context, required),
            Self::Nothing => false,
        }
    }
}

/// Catalogue **pré-sérialisé** : la charge `result` complète (chemin ouvert,
/// servie telle quelle) et la même liste découpée en éléments sérialisés,
/// chacun avec les droits qu'il exige (chemin filtré par appelant).
///
/// Le découpage est **exact** : `prefixe` + éléments joints par `,` +
/// `suffixe` reproduit octet pour octet `complet` (vérifié en `debug` à la
/// construction). Le chemin filtré ne réassemble donc que des octets déjà
/// sérialisés : aucun schéma n'est reconstruit, et aucun `Value` n'est cloné
/// par requête.
///
/// Une charge filtrée est **toujours** réassemblée à partir des droits de
/// l'appelant : elle n'est jamais stockée, donc jamais servie à un autre.
pub(super) struct Catalogue {
    prefixe: Box<[u8]>,
    suffixe: Box<[u8]>,
    /// (droits exigés, élément sérialisé) dans l'ordre du registre.
    elements: Vec<(Vec<String>, Box<[u8]>)>,
    complet: Box<[u8]>,
}

impl Catalogue {
    /// Découpe `resultat` (liste déjà construite) autour de son tableau
    /// `cle_liste` — `droits` suit exactement l'ordre publié.
    fn preserialiser<'a>(
        resultat: &Value,
        cle_liste: &str,
        droits: impl Iterator<Item = &'a [String]>,
    ) -> Self {
        let elements_value: Vec<Value> = resultat
            .get(cle_liste)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let elements: Vec<(Vec<String>, Box<[u8]>)> = elements_value
            .iter()
            .zip(droits)
            .map(|(element, droits)| {
                (
                    droits.to_vec(),
                    serde_json::to_vec(element)
                        .unwrap_or_else(|_| b"null".to_vec())
                        .into_boxed_slice(),
                )
            })
            .collect();
        // Enveloppe : le même résultat avec la liste vidée, coupé juste après
        // le `[` du tableau (le `]` de fermeture ouvre le suffixe).
        let mut vide = resultat.clone();
        if let Some(objet) = vide.as_object_mut() {
            objet.insert(cle_liste.to_string(), Value::Array(Vec::new()));
        }
        let octets_vides = serde_json::to_vec(&vide).unwrap_or_default();
        let marqueur = format!("\"{cle_liste}\":[");
        let debut = octets_vides
            .windows(marqueur.len())
            .position(|fenetre| fenetre == marqueur.as_bytes())
            .map(|position| position + marqueur.len())
            .unwrap_or(octets_vides.len());
        let prefixe = octets_vides[..debut].to_vec().into_boxed_slice();
        // Le `]` de fermeture du tableau appartient au suffixe.
        let suffixe = octets_vides
            .get(debut..)
            .unwrap_or_default()
            .to_vec()
            .into_boxed_slice();
        let complet = serde_json::to_vec(resultat)
            .unwrap_or_default()
            .into_boxed_slice();
        let catalogue = Self {
            prefixe,
            suffixe,
            elements,
            complet,
        };
        // Invariant vérifié en debug (tests inclus) : le réassemblage complet
        // reproduit exactement la charge servie sur le chemin ouvert.
        debug_assert_eq!(
            catalogue.reassembler(|_| true),
            &catalogue.complet[..],
            "découpage de catalogue non fidèle : prefixe={:?} suffixe={:?}",
            String::from_utf8_lossy(&catalogue.prefixe),
            String::from_utf8_lossy(&catalogue.suffixe)
        );
        catalogue
    }

    /// Réassemble la charge d'une liste filtrée à partir des éléments gardés.
    fn reassembler(&self, garde: impl Fn(&[String]) -> bool) -> Vec<u8> {
        let mut octets = Vec::with_capacity(self.complet.len());
        octets.extend_from_slice(&self.prefixe);
        let mut premier = true;
        for (droits, element) in &self.elements {
            if garde(droits) {
                if !premier {
                    octets.push(b',');
                }
                octets.extend_from_slice(element);
                premier = false;
            }
        }
        octets.extend_from_slice(&self.suffixe);
        octets
    }
}

/// Charge d'un catalogue pour une visibilité donnée : empruntée au cache sur
/// le chemin ouvert, réassemblée à la demande sinon.
pub(super) fn charge<'a>(
    catalogue: &'a Catalogue,
    visible: Visible<'a>,
) -> std::borrow::Cow<'a, [u8]> {
    match visible {
        Visible::All => std::borrow::Cow::Borrowed(&catalogue.complet[..]),
        visible => std::borrow::Cow::Owned(catalogue.reassembler(|droits| visible.shows(droits))),
    }
}

/// Enveloppe JSON-RPC d'une charge `result` **déjà sérialisée** : seules
/// `jsonrpc` et `id` sont écrites par requête.
///
/// Octets **identiques** à `Outgoing::success_bounded(&id, &value, cap)` quand
/// `resultat` vaut `serde_json::to_vec(&value)` — même ordre de champs, mêmes
/// séparateurs (vérifié octet pour octet par
/// `reponse_pre_serialisee_est_identique_a_success_bounded`). Le plafond est
/// appliqué sur la taille **totale** produite, sans matérialiser un corps
/// hors plafond.
pub(super) fn reponse_pre_serialisee(id: &Value, resultat: &[u8], cap: usize) -> BoundedReply {
    // `{"jsonrpc":"2.0","id":` … `,"result":` … `}`
    const ENTETE: &[u8] = br#"{"jsonrpc":"2.0","id":"#;
    const MILIEU: &[u8] = br#","result":"#;
    let id_octets = serde_json::to_vec(id).unwrap_or_else(|_| b"null".to_vec());
    let total = ENTETE.len() + id_octets.len() + MILIEU.len() + resultat.len() + 1;
    if cap > 0 && total > cap {
        return BoundedReply::Over { total };
    }
    let mut corps = Vec::with_capacity(total);
    corps.extend_from_slice(ENTETE);
    corps.extend_from_slice(&id_octets);
    corps.extend_from_slice(MILIEU);
    corps.extend_from_slice(resultat);
    corps.push(b'}');
    BoundedReply::Complete(corps)
}

impl ServerCore {
    /// Identité du serveur sous `_meta` (SEP-2575 : les serveurs SHOULD
    /// s'identifier dans chaque résultat).
    fn meta(&self) -> Value {
        self.meta.clone()
    }

    /// Capacités annoncées par `server/discover` et `initialize` : les trois
    /// capacités (`tools`, `resources`, `prompts`) sont toujours annoncées car
    /// leurs méthodes sont servies même à registre vide (catalogue vide).
    ///
    /// `completions` fait exception : elle n'est annoncée que si un
    /// fournisseur est enregistré — sans lui, `completion/complete` répond
    /// `-32602`, la capacité ne doit pas prétendre le contraire.
    fn capabilities(&self) -> Value {
        let mut capabilities = json!({
            "tools": {},
            "resources": {},
            "prompts": {},
        });
        if self.completions.is_some() {
            capabilities["completions"] = json!({});
        }
        capabilities
    }

    /// `server/discover` pré-sérialisé, calculé une fois puis servi tel quel.
    ///
    /// Le contenu ne dépend pas de l'appelant (les capacités sont celles du
    /// registre) : aucune variante filtrée n'existe.
    pub(super) fn discover_octets(&self) -> &[u8] {
        self.discover_cache.get_or_init(|| {
            serde_json::to_vec(&self.discover())
                .unwrap_or_default()
                .into_boxed_slice()
        })
    }

    /// `initialize` sérialisé — une entrée par révision legacy servie,
    /// calculée une fois puis servie telle quelle.
    ///
    /// Négociation (révision `2025-11-25`, « Version Negotiation ») : la
    /// révision demandée est renvoyée si le profil la sert, sinon la plus
    /// récente révision legacy servie — jamais `2026-07-28`, qui ne définit
    /// pas `initialize`.
    pub(super) fn initialize_json(&self, params: &Value) -> &Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = protocol::negotiate_legacy_version(requested);
        let results = self.initialize_cache.get_or_init(|| {
            protocol::LEGACY_SUPPORTED_VERSIONS
                .iter()
                .map(|version| (*version, self.initialize_result(version)))
                .collect()
        });
        results.get(version).unwrap_or_else(|| {
            // Invariant : `negotiate_legacy_version` ne rend qu'une révision
            // de `LEGACY_SUPPORTED_VERSIONS`, toutes matérialisées ci-dessus.
            results
                .get(protocol::LEGACY_PROTOCOL_VERSION)
                .expect("LEGACY_SUPPORTED_VERSIONS doit contenir sa révision par défaut")
        })
    }

    /// Corps d'un `InitializeResult` pour une révision legacy donnée
    /// (`resultType` est obligatoire dans CHAQUE résultat, SEP-2322).
    fn initialize_result(&self, protocol_version: &'static str) -> Value {
        json!({
            "resultType": "complete",
            "protocolVersion": protocol_version,
            "capabilities": self.capabilities(),
            "serverInfo": ServerInfo {
                name: self.name.clone(),
                version: self.version.clone(),
            },
            "_meta": self.meta(),
        })
    }

    /// `ping` sérialisé, calculé une fois puis servi tel quel.
    pub(super) fn ping_json(&self) -> &Value {
        self.ping_cache.get_or_init(|| {
            json!({
                "resultType": "complete",
                "_meta": self.meta(),
            })
        })
    }

    /// `tools/list` pré-sérialisé, calculé une fois puis servi tel quel.
    ///
    /// Chemin **sans autorisation** : la charge partielle est servie
    /// **telle quelle** (aucune resérialisation). Chemin filtré : les éléments
    /// sont réassemblés depuis les octets déjà sérialisés de ce même
    /// catalogue ([`Self::charge_outils`]) — jamais depuis un autre appelant.
    fn tools_catalogue(&self) -> &Catalogue {
        self.tools_list_cache.get_or_init(|| {
            Arc::new(Catalogue::preserialiser(
                &serde_json::to_value(self.list_tools(&Visible::All))
                    .unwrap_or_else(|_| json!({ "tools": [] })),
                "tools",
                self.tools
                    .values()
                    .map(|def| def.required_rights.as_slice()),
            ))
        })
    }

    /// `resources/list` pré-sérialisé (voir [`Self::tools_catalogue`]).
    fn resources_catalogue(&self) -> &Catalogue {
        self.resources_list_cache.get_or_init(|| {
            Arc::new(Catalogue::preserialiser(
                &serde_json::to_value(self.list_resources(&Visible::All))
                    .unwrap_or_else(|_| json!({ "resources": [] })),
                "resources",
                self.resources
                    .values()
                    .map(|def| def.required_rights.as_slice()),
            ))
        })
    }

    /// `resources/templates/list` pré-sérialisé — même sans gabarit.
    fn templates_catalogue(&self) -> &Catalogue {
        self.templates_list_cache.get_or_init(|| {
            Arc::new(Catalogue::preserialiser(
                &serde_json::to_value(self.list_resource_templates(&Visible::All))
                    .unwrap_or_else(|_| json!({ "resourceTemplates": [] })),
                "resourceTemplates",
                self.templates
                    .values()
                    .map(|def| def.required_rights.as_slice()),
            ))
        })
    }

    /// `prompts/list` pré-sérialisé (voir [`Self::tools_catalogue`]).
    fn prompts_catalogue(&self) -> &Catalogue {
        self.prompts_list_cache.get_or_init(|| {
            Arc::new(Catalogue::preserialiser(
                &serde_json::to_value(self.list_prompts(&Visible::All))
                    .unwrap_or_else(|_| json!({ "prompts": [] })),
                "prompts",
                self.prompts
                    .values()
                    .map(|def| def.required_rights.as_slice()),
            ))
        })
    }

    /// Ce que l'appelant peut voir d'un catalogue — construit à chaque
    /// requête, donc jamais emprunté au cache.
    fn visibility<'a>(&'a self, context: Option<&'a AuthContext>) -> Visible<'a> {
        match (self.auth_gate.as_ref(), context) {
            // Sans vérificateur : comportement historique, cache intact.
            (None, _) => Visible::All,
            // Autorisation active : liste propre à l'appelant.
            (Some(_), Some(context)) => Visible::Context(context),
            // Autorisation active sans identité : rien n'est visible (le
            // dispatch a déjà refusé en amont — ceinture et bretelles).
            (Some(_), None) => Visible::Nothing,
        }
    }

    /// `tools/list` pour cet appelant : charge pré-sérialisée du catalogue
    /// **sans** autorisation, réassemblage des seuls éléments visibles
    /// **avec**. Dans les deux cas, aucun schéma n'est reconstruit par
    /// requête et la charge d'un appelant n'est jamais servie à un autre.
    pub(super) fn charge_outils<'a>(
        &'a self,
        context: Option<&'a AuthContext>,
    ) -> std::borrow::Cow<'a, [u8]> {
        charge(self.tools_catalogue(), self.visibility(context))
    }

    /// `resources/list` pour cet appelant — voir [`Self::charge_outils`].
    pub(super) fn charge_ressources<'a>(
        &'a self,
        context: Option<&'a AuthContext>,
    ) -> std::borrow::Cow<'a, [u8]> {
        charge(self.resources_catalogue(), self.visibility(context))
    }

    /// `resources/templates/list` pour cet appelant — voir
    /// [`Self::charge_outils`].
    pub(super) fn charge_gabarits<'a>(
        &'a self,
        context: Option<&'a AuthContext>,
    ) -> std::borrow::Cow<'a, [u8]> {
        charge(self.templates_catalogue(), self.visibility(context))
    }

    /// `prompts/list` pour cet appelant — voir [`Self::charge_outils`].
    pub(super) fn charge_prompts<'a>(
        &'a self,
        context: Option<&'a AuthContext>,
    ) -> std::borrow::Cow<'a, [u8]> {
        charge(self.prompts_catalogue(), self.visibility(context))
    }

    /// Résultat de `server/discover` (**MUST**, SEP-2575).
    fn discover(&self) -> DiscoverResult {
        DiscoverResult {
            result_type: ResultType::Complete,
            // Mêmes indices de cache que `tools/list` : la page *Caching*
            // les exige sur `server/discover` (SEP-2549).
            ttl_ms: DEFAULT_LIST_TTL_MS,
            cache_scope: CacheScope::Private,
            supported_versions: protocol::SUPPORTED_VERSIONS.to_vec(),
            capabilities: self.capabilities(),
            meta: self.meta(),
        }
    }

    /// Noms des outils de CE snapshot, dans l'ordre du `tools/list` servi —
    /// introspection sans sérialisation (journaux, tests, gateways qui
    /// comparent leur registre à leur état détecté).
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Threads de handler encore vivants — mode borné uniquement
    /// ([`ServerBuilder::tool_timeout`]) : sans timeout, les handlers
    /// tournent dans le thread de connexion et ce compteur vaut zéro. Le
    /// module anti-zombie le lit pour son bilan de fin de vie.
    #[must_use]
    pub(crate) fn live_handler_threads(&self) -> usize {
        self.inflight_handlers
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn list_tools(&self, visible: &Visible<'_>) -> ToolsListResult {
        let tools = self
            .tools
            .iter()
            .filter(|(_, def)| visible.shows(&def.required_rights))
            .map(|(name, def)| Tool {
                name: name.clone(),
                description: def.description.clone(),
                annotations: def.annotations.clone(),
                input_schema: Some(
                    def.schema
                        .as_ref()
                        .map(|schema| schema.json())
                        .unwrap_or_else(|| json!({ "type": "object" })),
                ),
                output_schema: def.output_schema.as_ref().map(|schema| schema.json()),
            })
            .collect();
        ToolsListResult {
            result_type: ResultType::Complete,
            ttl_ms: DEFAULT_LIST_TTL_MS,
            cache_scope: CacheScope::Private,
            tools,
            meta: self.meta(),
        }
    }

    fn list_resources(&self, visible: &Visible<'_>) -> ResourcesListResult {
        let resources = self
            .resources
            .iter()
            .filter(|(_, def)| visible.shows(&def.required_rights))
            .map(|(uri, def)| Resource {
                uri: uri.clone(),
                name: def.name.clone(),
                description: def.description.clone(),
                mime_type: None,
            })
            .collect();
        ResourcesListResult {
            result_type: ResultType::Complete,
            ttl_ms: DEFAULT_LIST_TTL_MS,
            cache_scope: CacheScope::Private,
            resources,
            meta: Some(self.meta()),
        }
    }

    fn list_resource_templates(&self, visible: &Visible<'_>) -> ResourceTemplatesListResult {
        ResourceTemplatesListResult {
            result_type: ResultType::Complete,
            ttl_ms: DEFAULT_LIST_TTL_MS,
            cache_scope: CacheScope::Private,
            // Ordre déterministe : le registre est un BTreeMap clé sur
            // l'uriTemplate.
            resource_templates: self
                .templates
                .values()
                .filter(|def| visible.shows(&def.required_rights))
                .map(|def| def.template.clone())
                .collect(),
            meta: Some(self.meta()),
        }
    }

    fn list_prompts(&self, visible: &Visible<'_>) -> PromptsListResult {
        let prompts = self
            .prompts
            .iter()
            .filter(|(_, def)| visible.shows(&def.required_rights))
            .map(|(name, def)| Prompt {
                name: name.clone(),
                description: def.description.clone(),
                arguments: def
                    .schema
                    .as_ref()
                    .and_then(|schema| derive_prompt_arguments(&schema.json())),
            })
            .collect();
        PromptsListResult {
            result_type: ResultType::Complete,
            ttl_ms: DEFAULT_LIST_TTL_MS,
            cache_scope: CacheScope::Private,
            prompts,
            meta: Some(self.meta()),
        }
    }
}

#[cfg(test)]
mod tests_catalogue_preselectionne_2026_11_16 {
    use super::*;
    use serde_json::json;

    /// Droits de test — un groupe par élément.
    fn droits(groupes: &[&str]) -> Vec<String> {
        groupes.iter().map(|groupe| (*groupe).to_string()).collect()
    }

    /// Cas vide : le découpage ne perd rien et le filtre vide ne laisse que
    /// l'enveloppe.
    #[test]
    fn catalogue_vide_ne_publie_aucun_element() {
        let resultat = json!({ "ttlMs": 60000, "tools": [] });
        let catalogue = Catalogue::preserialiser(&resultat, "tools", std::iter::empty());
        assert!(catalogue.elements.is_empty());
        assert_eq!(catalogue.reassembler(|_| true), &catalogue.complet[..]);
        assert_eq!(
            String::from_utf8(catalogue.reassembler(|_| false)).expect("UTF-8"),
            r#"{"tools":[],"ttlMs":60000}"#
        );
    }

    /// Cas nominal : seuls les éléments visibles sont réassemblés, dans
    /// l'ordre du catalogue, et le résultat est un JSON valide et complet.
    #[test]
    fn catalogue_reassemble_exactement_les_elements_gardes() {
        let resultat = json!({
            "tools": [
                { "name": "a", "description": "premier" },
                { "name": "b", "description": "second" },
                { "name": "c", "description": "troisième" },
            ],
            "ttlMs": 60000,
        });
        let droits_a = droits(&["g0"]);
        let droits_b = droits(&["g1"]);
        let droits_c = droits(&["g0"]);
        let catalogue = Catalogue::preserialiser(
            &resultat,
            "tools",
            [&droits_a[..], &droits_b[..], &droits_c[..]].into_iter(),
        );
        // Découpage fidèle (invariant de la construction).
        assert_eq!(catalogue.reassembler(|_| true), &catalogue.complet[..]);
        // Filtre par droit : l'appelant ne détient que `g0` — les éléments
        // dont tous les droits sont détenus sont visibles (`shows`).
        let seulement_g0 = catalogue.reassembler(|droits| droits.iter().all(|droit| droit == "g0"));
        let value: Value = serde_json::from_slice(&seulement_g0).expect("JSON filtré");
        let noms: Vec<&str> = value["tools"]
            .as_array()
            .expect("tableau")
            .iter()
            .map(|outil| outil["name"].as_str().expect("nom"))
            .collect();
        assert_eq!(noms, ["a", "c"], "le filtre doit garder a et c");
        assert_eq!(value["ttlMs"], 60000, "l'enveloppe est conservée");
        assert!(
            !String::from_utf8_lossy(&seulement_g0).contains("second"),
            "aucun élément filtré ne doit subsister"
        );
    }

    /// Cas limite : un seul élément, et le filtre qui ne garde rien produit
    /// un tableau vide **sans** virgule orpheline.
    #[test]
    fn catalogue_a_un_seul_element_et_filtre_vide() {
        let resultat = json!({ "tools": [{ "name": "seul" }] });
        let droits_seul = droits(&["g0"]);
        let catalogue =
            Catalogue::preserialiser(&resultat, "tools", [&droits_seul[..]].into_iter());
        assert_eq!(
            String::from_utf8(catalogue.reassembler(|_| false)).expect("UTF-8"),
            r#"{"tools":[]}"#
        );
        assert_eq!(
            String::from_utf8(catalogue.reassembler(|_| true)).expect("UTF-8"),
            r#"{"tools":[{"name":"seul"}]}"#
        );
    }

    /// L'enveloppe d'une charge pré-sérialisée est **identique à l'octet** à
    /// celle du chemin classique (`Outgoing::success_bounded`) — même ordre
    /// de clés, mêmes séparateurs — pour toutes les formes d'`id`, et le
    /// plafond est appliqué de la même façon.
    #[test]
    fn reponse_pre_serialisee_est_identique_a_success_bounded() {
        let resultats = [
            json!({ "tools": [] }),
            json!({ "tools": [{ "name": "a", "description": "éàü" }], "ttlMs": 60000 }),
        ];
        let ids = [json!(1), json!(0), json!(-7), json!("abc"), Value::Null];
        for resultat in &resultats {
            let octets = serde_json::to_vec(resultat).expect("sérialisation");
            for id in &ids {
                let classique = Outgoing::success_bounded(id, resultat, 0);
                let pre = reponse_pre_serialisee(id, &octets, 0);
                assert_eq!(classique.len(), pre.len(), "taille (id {id})");
                assert_eq!(
                    classique.into_vec(),
                    pre.into_vec(),
                    "octets identiques attendus (id {id})"
                );
                // Plafond exactement à la taille : passe tel quel.
                let total = serde_json::to_vec(&Outgoing::success(id.clone(), resultat.clone()))
                    .expect("référence")
                    .len();
                assert_eq!(
                    reponse_pre_serialisee(id, &octets, total).len(),
                    total,
                    "plafond à la taille : rien ne change"
                );
                assert_eq!(
                    reponse_pre_serialisee(id, &octets, total).into_vec(),
                    serde_json::to_vec(&Outgoing::success(id.clone(), resultat.clone()))
                        .expect("référence")
                );
                // Un octet de moins : dépassement, total exact, aucun corps.
                match reponse_pre_serialisee(id, &octets, total - 1) {
                    BoundedReply::Over { total: annonce } => assert_eq!(annonce, total),
                    BoundedReply::Complete(corps) => {
                        panic!("dépassement attendu, corps rendu : {}", corps.len())
                    }
                }
            }
        }
    }
}
