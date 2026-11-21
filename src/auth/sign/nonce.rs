//! Cache de nonces **borné par clé** — voir `docs/auth.md` §2.3 et §11.7.
//!
//! Un nonce déjà vu pour un `keyid` est un rejeu. Le cache retient les nonces
//! par clé, avec deux bornes indépendantes :
//!
//! * **âge** — une entrée disparaît après `ttl` (120 s par défaut), la fenêtre
//!   d'horloge du profil étant de ±60 s ;
//! * **capacité** — `par_clé` entrées au plus (4096 par défaut). Un nonce
//!   encore dans la fenêtre n'est **jamais** évincé : un compartiment plein
//!   refuse l'insertion (échec fermé), le vérificateur rend alors
//!   `nonce_cache_full`.
//!
//! Le refus est **toujours local à une clé** : une clé qui inonde son propre
//! compartiment ne peut ni évincer les nonces d'une autre, ni faire grandir sa
//! propre table au-delà de la borne. Aucun vidage global n'existe. Le temps est
//! mesuré avec [`Instant`] (monotone) : un recul de l'horloge murale ne prolonge
//! ni ne raccourcit la rétention.
//!
//! Le cache n'est **pas** persistant : un redémarrage le vide, donc un rejeu
//! reste possible dans la fenêtre de 60 s (§9, §11.7 — assumé).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Capacité par défaut d'un compartiment de clé.
///
/// 4096 nonces pour une rétention de 120 s, soit ≈ 34 requêtes par seconde et
/// par clé en régime établi : très au-dessus du débit d'un client MCP, et
/// quatre fois l'ancienne borne (1024, franchie en ~8 s à 120 req/s, après
/// quoi un rejeu repassait). Mémoire, à pleine charge et par clé : 4096 ×
/// (16 o de nonce + 16 o d'instant) plus le surcoût des deux tables, soit
/// ≈ 0,3 Mio (estimation par le calcul, pas une mesure instrumentée).
pub const DEFAULT_PER_KEY: usize = 4096;

/// Rétention par défaut d'un nonce.
pub const DEFAULT_TTL: Duration = Duration::from_secs(120);

/// Nonce de 16 octets (128 bits) tiré du générateur du système.
pub type Nonce = [u8; 16];

/// Cache de nonces à capacité et rétention bornées, par clé.
///
/// Sûr en concurrence : un [`Mutex`] protège la table, aucune entrée de code
/// ne le tient pendant une entrée-sortie. Un verrou empoisonné (panique d'un
/// autre fil) est traité en **échec fermé** : [`NonceCache::contains`] répond
/// « déjà vu » et [`NonceCache::remember`] refuse d'enregistrer, donc la
/// requête est refusée.
pub struct NonceCache {
    per_key: usize,
    ttl: Duration,
    state: Mutex<HashMap<String, Bucket>>,
}

/// Résultat d'une réservation de nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NonceInsert {
    /// Nonce neuf, enregistré.
    Fresh,
    /// Nonce déjà retenu pour cette clé : rejeu.
    Replay,
    /// Compartiment plein de nonces **encore valides** : rien n'est enregistré.
    CacheFull,
}

/// Un compartiment par `keyid` : ensemble (consultation O(1)) et file
/// d'insertion (purge des entrées expirées, dans l'ordre d'arrivée).
#[derive(Default)]
struct Bucket {
    seen: HashSet<Nonce>,
    order: VecDeque<(Nonce, Instant)>,
}

impl Bucket {
    /// Retire les entrées plus vieilles que `ttl`.
    fn purge(&mut self, now: Instant, ttl: Duration) {
        while let Some((nonce, at)) = self.order.front() {
            if now.saturating_duration_since(*at) < ttl {
                break;
            }
            let nonce = *nonce;
            self.order.pop_front();
            self.seen.remove(&nonce);
        }
    }

    /// Enregistre un nonce neuf, **sans jamais évincer** une entrée encore
    /// valide : compartiment plein = insertion refusée ([`NonceInsert::CacheFull`]).
    fn insert(&mut self, nonce: &Nonce, now: Instant, cap: usize, ttl: Duration) -> NonceInsert {
        self.purge(now, ttl);
        if self.seen.contains(nonce) {
            return NonceInsert::Replay;
        }
        if self.order.len() >= cap {
            return NonceInsert::CacheFull;
        }
        self.seen.insert(*nonce);
        self.order.push_back((*nonce, now));
        NonceInsert::Fresh
    }
}

impl NonceCache {
    /// Cache de `per_key` entrées par clé, rétention `ttl`.
    ///
    /// `per_key` est ramené à 1 au minimum : une capacité nulle refuserait
    /// toutes les requêtes, ce qui n'est jamais voulu.
    #[must_use]
    pub fn new(per_key: usize, ttl: Duration) -> Self {
        Self {
            per_key: per_key.max(1),
            ttl,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Politique du profil : 4096 entrées par clé, 120 s.
    #[must_use]
    pub fn with_default_policy() -> Self {
        Self::new(DEFAULT_PER_KEY, DEFAULT_TTL)
    }

    /// Capacité par compartiment.
    #[must_use]
    pub fn per_key(&self) -> usize {
        self.per_key
    }

    /// Rétention d'un nonce.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Ce nonce a-t-il déjà été accepté pour cette clé ?
    ///
    /// Consultation seule : n'enregistre rien, ne crée aucun compartiment (un
    /// `keyid` inconnu ne peut donc pas faire grandir le cache).
    #[must_use]
    pub fn contains(&self, keyid: &str, nonce: &Nonce) -> bool {
        match self.state.lock() {
            Ok(state) => state
                .get(keyid)
                .is_some_and(|bucket| bucket.seen.contains(nonce)),
            // Échec fermé : verrou empoisonné = « déjà vu ».
            Err(_) => true,
        }
    }

    /// Enregistre un nonce après vérification réussie.
    ///
    /// Rend `true` si le nonce est neuf et retenu, `false` s'il était déjà
    /// présent (deux requêtes identiques concurrentes : la seconde est un
    /// rejeu) **ou** si le compartiment est plein : dans les deux cas le nonce
    /// n'est pas retenu et la requête doit être refusée (échec fermé).
    pub fn remember(&self, keyid: &str, nonce: &Nonce) -> bool {
        self.insert(keyid, nonce) == NonceInsert::Fresh
    }

    /// Réserve un nonce en distinguant le rejeu du compartiment plein.
    ///
    /// Le vérificateur en tire deux motifs internes (`nonce_replay`,
    /// `nonce_cache_full`) qui partagent le même `401` au réseau.
    pub(crate) fn insert(&self, keyid: &str, nonce: &Nonce) -> NonceInsert {
        let now = Instant::now();
        let Ok(mut state) = self.state.lock() else {
            // Échec fermé : verrou empoisonné = refus.
            return NonceInsert::Replay;
        };
        if let Some(bucket) = state.get_mut(keyid) {
            return bucket.insert(nonce, now, self.per_key, self.ttl);
        }
        let mut bucket = Bucket::default();
        let recorded = bucket.insert(nonce, now, self.per_key, self.ttl);
        if recorded == NonceInsert::Fresh {
            state.insert(keyid.to_owned(), bucket);
        }
        recorded
    }

    /// Nombre total d'entrées retenues (toutes clés).
    #[must_use]
    pub fn len(&self) -> usize {
        match self.state.lock() {
            Ok(state) => state.values().map(|bucket| bucket.seen.len()).sum(),
            Err(_) => 0,
        }
    }

    /// Le cache est-il vide ?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Nombre d'entrées retenues pour une clé.
    #[must_use]
    pub fn entries_for(&self, keyid: &str) -> usize {
        match self.state.lock() {
            Ok(state) => state.get(keyid).map_or(0, |bucket| bucket.seen.len()),
            Err(_) => 0,
        }
    }

    /// Nombre de compartiments (clés distinctes retenues).
    #[must_use]
    pub fn keys(&self) -> usize {
        match self.state.lock() {
            Ok(state) => state.len(),
            Err(_) => 0,
        }
    }
}

impl Default for NonceCache {
    fn default() -> Self {
        Self::with_default_policy()
    }
}

/// base64url **sans padding** (RFC 4648 §5) — l'encodage du `nonce`.
///
/// Ce n'est pas de la cryptographie : juste la mise en forme de 128 bits en
/// 22 caractères, telle que `docs/auth.md` §2.3 l'exige.
pub(crate) fn encode_nonce(nonce: &Nonce) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(22);
    for chunk in nonce.chunks_exact(3) {
        let Some([a, b, c]) = chunk.first_chunk::<3>() else {
            break;
        };
        let triple = (u32::from(*a) << 16) | (u32::from(*b) << 8) | u32::from(*c);
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 6) as usize & 63] as char);
        out.push(ALPHABET[triple as usize & 63] as char);
    }
    // 16 octets = 5 triplets + 1 octet → deux caractères, sans padding.
    if let Some(last) = nonce.chunks_exact(3).remainder().first() {
        let triple = u32::from(*last) << 16;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
    }
    out
}

/// Nonce neuf de 16 octets, tiré du CSPRNG du **système** (`getrandom`).
pub(crate) fn system_nonce() -> Nonce {
    super::system_bytes::<16>()
}

impl std::fmt::Debug for NonceCache {
    /// Jamais de nonce dans un journal : seulement les bornes et le volume.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NonceCache")
            .field("per_key", &self.per_key)
            .field("ttl", &self.ttl)
            .field("entries", &self.len())
            .field("keys", &self.keys())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn nonce(value: u8) -> Nonce {
        [value; 16]
    }

    #[test]
    fn un_nonce_neuf_puis_le_meme_est_un_rejeu() {
        let cache = NonceCache::with_default_policy();
        assert!(!cache.contains("client-a", &nonce(1)));
        assert!(cache.remember("client-a", &nonce(1)));
        assert!(cache.contains("client-a", &nonce(1)));
        // Second enregistrement : refusé, la première décision n'est pas
        // effacée.
        assert!(!cache.remember("client-a", &nonce(1)));
        assert!(cache.contains("client-a", &nonce(1)));
        // Un autre keyid a son propre compartiment.
        assert!(!cache.contains("client-b", &nonce(1)));
        assert!(cache.remember("client-b", &nonce(1)));
    }

    #[test]
    fn le_cache_est_borne_par_cle() {
        let cache = NonceCache::new(16, DEFAULT_TTL);
        for value in 0..10_000u32 {
            let mut n = [0u8; 16];
            n[..4].copy_from_slice(&value.to_le_bytes());
            assert_eq!(cache.remember("clé", &n), value < 16, "entrée {value}");
            assert!(cache.entries_for("clé") <= 16, "borne dépassée à {value}");
        }
        assert_eq!(cache.entries_for("clé"), 16);
        assert_eq!(cache.keys(), 1);
        // Les seize premiers nonces sont toujours retenus : aucune éviction.
        for value in 0..16u32 {
            let mut n = [0u8; 16];
            n[..4].copy_from_slice(&value.to_le_bytes());
            assert!(cache.contains("clé", &n), "nonce {value} évincé");
        }
    }

    /// Saturation (P2 #1) : le rejeu de la première requête reste refusé.
    #[test]
    fn la_saturation_ne_rouvre_pas_le_rejeu() {
        let cache = NonceCache::new(8, DEFAULT_TTL);
        for value in 0..8u8 {
            assert_eq!(cache.insert("clé", &nonce(value)), NonceInsert::Fresh);
        }
        assert_eq!(cache.entries_for("clé"), 8);
        // Le nonce de la première requête est encore retenu : rejeu.
        assert_eq!(cache.insert("clé", &nonce(0)), NonceInsert::Replay);
        // Requête valide au-delà de la capacité : refus, jamais d'éviction.
        assert_eq!(cache.insert("clé", &nonce(99)), NonceInsert::CacheFull);
        assert_eq!(cache.entries_for("clé"), 8, "le compartiment a grandi");
        assert!(!cache.contains("clé", &nonce(99)));
        for value in 0..8u8 {
            assert!(cache.contains("clé", &nonce(value)), "nonce {value} évincé");
        }
    }

    /// Une entrée expirée libère sa place ; une entrée valide, jamais.
    #[test]
    fn les_entrees_expirees_rendent_la_capacite() {
        let cache = NonceCache::new(1, Duration::from_millis(30));
        assert_eq!(cache.insert("clé", &nonce(1)), NonceInsert::Fresh);
        assert_eq!(cache.insert("clé", &nonce(2)), NonceInsert::CacheFull);
        std::thread::sleep(Duration::from_millis(45));
        assert_eq!(cache.insert("clé", &nonce(2)), NonceInsert::Fresh);
        assert_eq!(cache.entries_for("clé"), 1);
        assert!(!cache.contains("clé", &nonce(1)), "entrée expirée retenue");
    }

    /// Inondation d'une clé : les nonces des autres ne bougent pas, et le
    /// compartiment de l'inondatrice refuse au-delà de sa capacité.
    #[test]
    fn une_inondation_n_evince_que_sa_propre_cle() {
        let cache = NonceCache::new(8, DEFAULT_TTL);
        for value in 0..8u8 {
            assert!(cache.remember("victime", &nonce(value)));
        }
        let mut refuses = 0usize;
        for round in 0..5_000u32 {
            let mut n = [0u8; 16];
            n[..4].copy_from_slice(&round.to_le_bytes());
            if !cache.remember("inondatrice", &n) {
                refuses += 1;
            }
        }
        // Le compartiment de la victime est intact.
        assert_eq!(cache.entries_for("victime"), 8);
        for value in 0..8u8 {
            assert!(
                cache.contains("victime", &nonce(value)),
                "nonce {value} évincé par une autre clé"
            );
            assert!(!cache.remember("victime", &nonce(value)), "rejeu accepté");
        }
        // Celui de l'inondatrice est resté borné, sans éviction.
        assert_eq!(cache.entries_for("inondatrice"), 8);
        assert_eq!(refuses, 5_000 - 8);
        // Le total reste borné : 8 + 8.
        assert_eq!(cache.len(), 16);
    }

    #[test]
    fn l_age_evince_et_ne_touche_qu_aux_entrees_vieilles() {
        let cache = NonceCache::new(1024, Duration::from_millis(30));
        assert!(cache.remember("clé", &nonce(1)));
        assert!(cache.contains("clé", &nonce(1)));
        std::thread::sleep(Duration::from_millis(45));
        // Purge à l'écriture suivante : l'ancien nonce sort, le neuf entre.
        assert!(cache.remember("clé", &nonce(2)));
        assert_eq!(cache.entries_for("clé"), 1);
        assert!(!cache.contains("clé", &nonce(1)));
        assert!(cache.contains("clé", &nonce(2)));
    }

    #[test]
    fn la_consultation_ne_cree_pas_de_compartiment() {
        let cache = NonceCache::with_default_policy();
        for round in 0..1_000u32 {
            let mut n = [0u8; 16];
            n[..4].copy_from_slice(&round.to_le_bytes());
            assert!(!cache.contains("inconnu", &n));
        }
        assert_eq!(cache.keys(), 0, "un keyid inconnu ne doit rien allouer");
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
    }

    #[test]
    fn la_capacite_nulle_est_ramenee_a_un() {
        let cache = NonceCache::new(0, DEFAULT_TTL);
        assert_eq!(cache.per_key(), 1);
        assert!(cache.remember("clé", &nonce(1)));
        assert!(!cache.remember("clé", &nonce(2)), "capacité 1 déjà pleine");
        assert_eq!(cache.entries_for("clé"), 1);
    }

    #[test]
    fn le_nonce_s_encode_sur_22_caracteres_base64url() {
        // Vecteur de `docs/auth.md` §2.5 : les octets 00 01 … 0f.
        let nonce = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        assert_eq!(encode_nonce(&nonce), "AAECAwQFBgcICQoLDA0ODw");
        assert_eq!(encode_nonce(&[0u8; 16]), "AAAAAAAAAAAAAAAAAAAAAA");
        assert_eq!(encode_nonce(&[0xff; 16]), "_____________________w");
        // 22 caractères, alphabet URL, aucun padding.
        for round in 0..64u8 {
            let texte = encode_nonce(&[round; 16]);
            assert_eq!(texte.len(), 22);
            assert!(
                texte
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "{texte}"
            );
        }
    }

    #[test]
    fn le_nonce_du_systeme_change_a_chaque_tirage() {
        let a = system_nonce();
        let b = system_nonce();
        assert_ne!(a, b);
        assert_eq!(encode_nonce(&a).len(), 22);
    }

    #[test]
    fn le_cache_est_sur_en_concurrence() {
        // Capacité très au-dessus du volume inséré : aucune éviction, donc le
        // rejeu immédiat est toujours détecté (la borne est testée à part).
        let cache = Arc::new(NonceCache::new(64 * 1024, DEFAULT_TTL));
        let mut threads = Vec::new();
        for worker in 0..8u8 {
            let cache = Arc::clone(&cache);
            threads.push(std::thread::spawn(move || {
                for round in 0..2_000u32 {
                    let mut n = [worker; 16];
                    n[8..12].copy_from_slice(&round.to_le_bytes());
                    assert!(cache.remember("partagée", &n), "première insertion");
                    // Rejeu immédiat : le second enregistrement échoue.
                    assert!(!cache.remember("partagée", &n), "rejeu admis");
                }
            }));
        }
        for thread in threads {
            thread.join().expect("fil de test");
        }
        assert_eq!(cache.entries_for("partagée"), 8 * 2_000);
        assert_eq!(cache.keys(), 1);
    }

    #[test]
    fn la_borne_tient_sous_concurrence() {
        let cache = Arc::new(NonceCache::new(64, DEFAULT_TTL));
        let mut threads = Vec::new();
        for worker in 0..8u8 {
            let cache = Arc::clone(&cache);
            threads.push(std::thread::spawn(move || {
                for round in 0..2_000u32 {
                    let mut n = [worker; 16];
                    n[8..12].copy_from_slice(&round.to_le_bytes());
                    cache.remember("partagée", &n);
                }
            }));
        }
        for thread in threads {
            thread.join().expect("fil de test");
        }
        assert!(cache.entries_for("partagée") <= 64, "borne dépassée");
        assert_eq!(
            cache.entries_for("partagée"),
            64,
            "aucune éviction attendue"
        );
        assert_eq!(cache.keys(), 1);
    }
}
