//! Proof that the container shares the host's network namespace.
//!
//! The host hands its real X cookie to the container only on the direct
//! X11 path, which is for containers on the host's network. The agent
//! reports its network namespace, but the agent runs in the container, so
//! that is only a hint. Before the cookie is sent, the host makes it prove
//! the claim:
//!
//! 1. The host listens on an abstract Unix socket with a random name.
//!    Abstract sockets exist per network namespace, so only a process in
//!    the host's namespace can connect to it.
//! 2. The host sends the socket's name and a random nonce over the agent
//!    pipe. The agent connects, writes the nonce, closes, and then answers
//!    on the pipe.
//! 3. After that answer the host looks at what is already queued on the
//!    socket. Nothing waits and no timing is involved.
//!
//! What this shows is that the agent, or something the agent could pass the
//! nonce to, is in the host's network namespace. The socket has no access
//! control and its name is visible to every process in that namespace,
//! which is why a connection counts only if it presents the nonce.

use std::io::{self, Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::time::Duration;

const NONCE_LEN: usize = 32;
/// Queued connections examined before giving up, so strays from other
/// processes on the host can't keep the check busy.
const MAX_CONNECTIONS: usize = 64;

/// Host side: the listening socket and what the agent must present.
pub struct Probe {
    listener: UnixListener,
    socket: String,
    nonce: Vec<u8>,
}

impl Probe {
    /// Listen on a fresh, randomly named abstract socket. Do this before
    /// telling the agent about it.
    pub fn new() -> io::Result<Probe> {
        let socket = format!("dctether-netns-{}", crate::agent::hex(&crate::agent::random_bytes(16)?));
        let nonce = crate::agent::random_bytes(NONCE_LEN)?;
        let listener = UnixListener::bind_addr(&SocketAddr::from_abstract_name(&socket)?)?;
        listener.set_nonblocking(true)?;
        Ok(Probe { listener, socket, nonce })
    }

    /// The abstract socket's name, for the agent.
    pub fn socket(&self) -> &str {
        &self.socket
    }

    /// The bytes the agent must write to the socket.
    pub fn nonce(&self) -> &[u8] {
        &self.nonce
    }

    /// Has a connection presented the nonce? Call this once the agent has
    /// answered on the pipe: by then it has connected, written and closed,
    /// so everything is queued and nothing here waits.
    pub fn satisfied(&self) -> bool {
        for _ in 0..MAX_CONNECTIONS {
            // Would block: no more connections are queued.
            let Ok((stream, _)) = self.listener.accept() else {
                return false;
            };
            if read_queued(stream).is_some_and(|got| crate::x11::ct_eq(&got, &self.nonce)) {
                return true;
            }
        }
        false
    }
}

/// What a connection has written so far, without waiting for more. At most
/// one byte longer than a nonce, so that anything longer doesn't match.
fn read_queued(mut stream: UnixStream) -> Option<Vec<u8>> {
    // An accepted socket doesn't inherit the listener's non-blocking mode.
    stream.set_nonblocking(true).ok()?;
    let mut got = vec![0u8; NONCE_LEN + 1];
    let mut n = 0;
    while n < got.len() {
        match stream.read(&mut got[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // Would block: the peer is connected but has nothing more queued.
            Err(_) => break,
        }
    }
    got.truncate(n);
    Some(got)
}

/// Container side: connect to the host's abstract socket and write the
/// nonce. In a container with its own network namespace the socket doesn't
/// exist and nothing happens; the host finds that out by itself.
pub fn answer(socket: &str, nonce: &[u8]) {
    let Ok(addr) = SocketAddr::from_abstract_name(socket) else {
        return;
    };
    let Ok(mut stream) = UnixStream::connect_addr(&addr) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    let _ = stream.write_all(nonce);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connect(probe: &Probe) -> UnixStream {
        UnixStream::connect_addr(&SocketAddr::from_abstract_name(probe.socket()).unwrap()).unwrap()
    }

    #[test]
    fn the_nonce_satisfies_the_probe() {
        let probe = Probe::new().unwrap();
        assert_eq!(probe.nonce().len(), NONCE_LEN);
        answer(probe.socket(), probe.nonce());
        assert!(probe.satisfied());
        // Each connection counts once.
        assert!(!probe.satisfied());
    }

    #[test]
    fn nothing_else_does() {
        // Nobody connected (a container in another namespace can't).
        let probe = Probe::new().unwrap();
        assert!(!probe.satisfied());

        // The wrong value, a prefix, and the nonce with something added.
        let probe = Probe::new().unwrap();
        answer(probe.socket(), &[0; NONCE_LEN]);
        answer(probe.socket(), &probe.nonce()[..NONCE_LEN - 1]);
        answer(probe.socket(), &[probe.nonce(), &b"x"[..]].concat());
        answer(probe.socket(), b"");
        assert!(!probe.satisfied());

        // Connected, but silent and still open.
        let probe = Probe::new().unwrap();
        let _silent = connect(&probe);
        assert!(!probe.satisfied());

        // The nonce of another probe.
        let (a, b) = (Probe::new().unwrap(), Probe::new().unwrap());
        assert_ne!(a.socket(), b.socket());
        answer(a.socket(), b.nonce());
        assert!(!a.satisfied());
    }

    #[test]
    fn strays_ahead_of_the_agent_are_skipped() {
        let probe = Probe::new().unwrap();
        let _silent = connect(&probe);
        answer(probe.socket(), b"junk");
        // The nonce written in two pieces before closing.
        let mut split = connect(&probe);
        split.write_all(&probe.nonce()[..10]).unwrap();
        split.write_all(&probe.nonce()[10..]).unwrap();
        drop(split);
        assert!(probe.satisfied());
    }

    #[test]
    fn answering_a_missing_socket_does_nothing() {
        answer("dctether-netns-no-such-socket", b"nonce");
        answer("", b"nonce");
    }
}
