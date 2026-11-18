//! Connexion servie : `TcpStream` nu ou session TLS 1.3, et ses
//! implémentations `Read`/`Write`.

use super::*;

/// Connexion servie : socket en clair, ou session **TLS 1.3** (feature
/// `tls`).
///
/// L'abstraction existe pour que `handle_connection` (lecture ligne à ligne,
/// corps par tranches, réponses, SSE) soit écrit **une seule fois** : avec
/// TLS, lectures et écritures passent par la même session `rustls`, protégée
/// par un verrou — c'est ce qui permet au canal de progression SSE d'écrire
/// depuis le fil du handler sans jamais parler en clair. Le chemin sans TLS
/// reste un `TcpStream` nu : aucune allocation, aucun verrou.
pub(crate) enum Conn {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Arc<Mutex<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>>),
}

impl Conn {
    /// Second handle sur la même connexion (lecture et écriture séparées).
    pub(super) fn try_clone(&self) -> std::io::Result<Self> {
        match self {
            Self::Plain(stream) => stream.try_clone().map(Self::Plain),
            #[cfg(feature = "tls")]
            Self::Tls(session) => Ok(Self::Tls(Arc::clone(session))),
        }
    }

    pub(super) fn local_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            Self::Plain(stream) => stream.local_addr(),
            #[cfg(feature = "tls")]
            Self::Tls(session) => lock_session(session).sock.local_addr(),
        }
    }

    pub(super) fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            Self::Plain(stream) => stream.peer_addr(),
            #[cfg(feature = "tls")]
            Self::Tls(session) => lock_session(session).sock.peer_addr(),
        }
    }

    pub(super) fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.set_read_timeout(timeout),
            #[cfg(feature = "tls")]
            Self::Tls(session) => lock_session(session).sock.set_read_timeout(timeout),
        }
    }

    /// Pose le timeout d'**écriture** de la socket sous-jacente.
    ///
    /// Sous TLS, la session `rustls` écrit elle-même sur la socket : c'est
    /// donc `session.sock` qui reçoit la borne, pas la session (qui n'expose
    /// pas de timeout). Le verrou est pris le temps du `setsockopt` — aucun
    /// échange applicatif n'a lieu ici.
    pub(super) fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.set_write_timeout(timeout),
            #[cfg(feature = "tls")]
            Self::Tls(session) => lock_session(session).sock.set_write_timeout(timeout),
        }
    }

    /// Envoie un FIN propre — `close_notify` d'abord quand la session est
    /// TLS (une fermeture sans alerte est une troncature pour le client).
    pub(super) fn shutdown_write(&mut self) {
        match self {
            Self::Plain(stream) => {
                let _ = stream.shutdown(std::net::Shutdown::Write);
            }
            #[cfg(feature = "tls")]
            Self::Tls(session) => {
                let mut session = lock_session(session);
                session.conn.send_close_notify();
                let _ = session.flush();
                let _ = session.sock.shutdown(std::net::Shutdown::Write);
            }
        }
    }
}

/// Écrivain dont **chaque tentative** d'envoi peut être bornée dans le temps.
///
/// La boucle d'écriture des réponses (`super::response::ecrire_borne`) arme
/// la borne avant chaque `write_vectored` : c'est ce qui rend l'échéance
/// globale effective quand un `send` ne rend pas la main (client qui lit au
/// compte-gouttes, corps de plusieurs Mio). Un écrivain en mémoire (tampon,
/// test) n'a rien à borner : l'implémentation est un no-op.
pub(super) trait EcritureBornee: Write {
    /// Arme la borne de la **prochaine** écriture (`SO_SNDTIMEO`).
    fn borner_prochain_envoi(&mut self, budget: Duration) -> std::io::Result<()>;
}

impl EcritureBornee for Conn {
    fn borner_prochain_envoi(&mut self, budget: Duration) -> std::io::Result<()> {
        self.set_write_timeout(Some(budget))
    }
}

impl EcritureBornee for TcpStream {
    fn borner_prochain_envoi(&mut self, budget: Duration) -> std::io::Result<()> {
        self.set_write_timeout(Some(budget))
    }
}

#[cfg(feature = "tls")]
fn lock_session<T>(session: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Read for Conn {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buffer),
            #[cfg(feature = "tls")]
            Self::Tls(session) => lock_session(session).read(buffer),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buffer),
            #[cfg(feature = "tls")]
            Self::Tls(session) => lock_session(session).write(buffer),
        }
    }

    /// Écriture **vectored** passée telle quelle à la socket : c'est elle qui
    /// envoie tête et corps en **un seul syscall** (chemin chaud inchangé —
    /// la réponse ne doit pas se scinder en deux segments). `rustls` n'expose
    /// pas d'écriture vectored : la session TLS écrit le premier tampon non
    /// vide, `ecrire_tous_vecteurs` avance ensuite les iovecs.
    fn write_vectored(&mut self, buffers: &[IoSlice<'_>]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write_vectored(buffers),
            #[cfg(feature = "tls")]
            Self::Tls(session) => {
                let mut session = lock_session(session);
                match buffers.iter().find(|tampon| !tampon.is_empty()) {
                    Some(tampon) => session.write(&tampon[..]),
                    None => Ok(0),
                }
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            #[cfg(feature = "tls")]
            Self::Tls(session) => lock_session(session).flush(),
        }
    }
}
