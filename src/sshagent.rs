//! Which SSH agent requests the host passes on from the container.
//!
//! The agent protocol is a sequence of messages: a `u32` big-endian length,
//! then that many bytes, the first of which is the message type. A client
//! sends one request and reads one reply.
//!
//! `git` and `ssh` need two requests: list the keys, and sign. An agent
//! offers more (add and remove keys, lock it, load PKCS#11/FIDO provider
//! libraries), and to the agent the host's relay looks like a local client,
//! so nothing on its side holds those back. The host therefore passes on
//! only what is listed in [`allowed`] and answers everything else with a
//! failure itself, unless `--ssh-agent-unfiltered` is given.
//!
//! The filter sees message types, not programs: it can't allow a request
//! for `ssh-add` and refuse the same request from something else.

/// Longest request accepted from the container (length prefix excluded).
/// OpenSSH's agent has the same limit.
pub const MAX_REQUEST: usize = 256 * 1024;

const SSH_AGENT_FAILURE: u8 = 5;
const SSH_AGENTC_REQUEST_IDENTITIES: u8 = 11;
const SSH_AGENTC_SIGN_REQUEST: u8 = 13;
const SSH_AGENTC_EXTENSION: u8 = 27;

/// The one extension passed on. `ssh` sends it on every agent connection
/// to tell the agent which server it is authenticating to; the agent needs
/// that for keys restricted to certain destinations (`ssh-add -h`). It
/// adds restrictions to the connection and changes nothing in the agent.
const SESSION_BIND: &[u8] = b"session-bind@openssh.com";

/// The reply to a refused request: a complete `SSH_AGENT_FAILURE` message.
pub const FAILURE_REPLY: [u8; 5] = [0, 0, 0, 1, SSH_AGENT_FAILURE];

/// A request whose length field is zero or over [`MAX_REQUEST`].
#[derive(Debug, PartialEq, Eq)]
pub struct BadLength(pub usize);

impl std::fmt::Display for BadLength {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SSH agent request with invalid length {}", self.0)
    }
}

impl std::error::Error for BadLength {}

/// Splits the bytes arriving from the container into requests.
#[derive(Default)]
pub struct Requests {
    buf: Vec<u8>,
}

impl Requests {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// The next complete request, length prefix included, or `None` if
    /// more bytes are needed. After an error the connection must be closed.
    pub fn take(&mut self) -> Result<Option<Vec<u8>>, BadLength> {
        let Some(header) = self.buf.first_chunk::<4>() else {
            return Ok(None);
        };
        let len = u32::from_be_bytes(*header) as usize;
        if len == 0 || len > MAX_REQUEST {
            return Err(BadLength(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let rest = self.buf.split_off(4 + len);
        Ok(Some(std::mem::replace(&mut self.buf, rest)))
    }

    /// No bytes of an unfinished request are buffered.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// May the container make this request? `request` is one complete message
/// as returned by [`Requests::take`].
pub fn allowed(request: &[u8]) -> bool {
    match request.get(4) {
        Some(&SSH_AGENTC_REQUEST_IDENTITIES | &SSH_AGENTC_SIGN_REQUEST) => true,
        Some(&SSH_AGENTC_EXTENSION) => extension_name(request) == Some(SESSION_BIND),
        _ => false,
    }
}

/// The name of an extension request: a length-prefixed string after the
/// type byte.
fn extension_name(request: &[u8]) -> Option<&[u8]> {
    let len = u32::from_be_bytes(*request.get(5..9)?.first_chunk::<4>()?) as usize;
    request.get(9..9usize.checked_add(len)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(body: &[u8]) -> Vec<u8> {
        let mut m = (body.len() as u32).to_be_bytes().to_vec();
        m.extend_from_slice(body);
        m
    }

    fn extension(name: &[u8]) -> Vec<u8> {
        let mut body = vec![SSH_AGENTC_EXTENSION];
        body.extend_from_slice(&(name.len() as u32).to_be_bytes());
        body.extend_from_slice(name);
        body.extend_from_slice(b"contents");
        message(&body)
    }

    #[test]
    fn splits_requests_however_they_arrive() {
        let a = message(&[SSH_AGENTC_REQUEST_IDENTITIES]);
        let b = message(&[SSH_AGENTC_SIGN_REQUEST, 1, 2, 3, 4, 5]);
        let wire: Vec<u8> = [a.clone(), b.clone(), a.clone()].concat();
        for chunk in [1, 2, 3, 5, 7, wire.len()] {
            let mut r = Requests::default();
            let mut got = vec![];
            for c in wire.chunks(chunk) {
                r.push(c);
                while let Some(m) = r.take().unwrap() {
                    got.push(m);
                }
            }
            assert_eq!(got, vec![a.clone(), b.clone(), a.clone()], "chunk {chunk}");
            assert!(r.is_empty());
        }
    }

    #[test]
    fn partial_request_is_kept() {
        let mut r = Requests::default();
        assert_eq!(r.take(), Ok(None));
        assert!(r.is_empty());
        r.push(&[0, 0, 0]);
        assert_eq!(r.take(), Ok(None));
        r.push(&[2, SSH_AGENTC_SIGN_REQUEST]);
        assert_eq!(r.take(), Ok(None));
        assert!(!r.is_empty());
        r.push(&[9]);
        assert_eq!(r.take(), Ok(Some(vec![0, 0, 0, 2, SSH_AGENTC_SIGN_REQUEST, 9])));
        assert!(r.is_empty());
    }

    #[test]
    fn rejects_bad_lengths_from_the_header_alone() {
        let mut r = Requests::default();
        r.push(&[0, 0, 0, 0]);
        assert_eq!(r.take(), Err(BadLength(0)));

        let mut r = Requests::default();
        r.push(&((MAX_REQUEST + 1) as u32).to_be_bytes());
        assert_eq!(r.take(), Err(BadLength(MAX_REQUEST + 1)));

        let mut r = Requests::default();
        r.push(&(MAX_REQUEST as u32).to_be_bytes());
        assert_eq!(r.take(), Ok(None));
    }

    #[test]
    fn allows_only_listing_signing_and_session_bind() {
        assert!(allowed(&message(&[SSH_AGENTC_REQUEST_IDENTITIES])));
        assert!(allowed(&message(&[SSH_AGENTC_SIGN_REQUEST, 0, 0, 0, 0])));
        assert!(allowed(&extension(b"session-bind@openssh.com")));

        // Add, remove, remove all, smartcard add/remove, lock, unlock, and
        // the constrained variants of add.
        for ty in [17u8, 18, 19, 20, 21, 22, 23, 25, 26] {
            assert!(!allowed(&message(&[ty, 0, 0, 0, 0])), "type {ty}");
        }
        // Every other type, known or not, including agent replies.
        for ty in (0..=255u8).filter(|t| ![11, 13, 27].contains(t)) {
            assert!(!allowed(&message(&[ty])), "type {ty}");
        }
        assert!(!allowed(&extension(b"query")));
        assert!(!allowed(&extension(b"session-bind@openssh.com.evil")));
        assert!(!allowed(&extension(b"")));
        // An extension request too short to hold a name, or whose name
        // length runs past the message.
        assert!(!allowed(&message(&[SSH_AGENTC_EXTENSION])));
        assert!(!allowed(&message(&[SSH_AGENTC_EXTENSION, 0, 0, 0, 200, b's'])));
        assert!(!allowed(&message(&[SSH_AGENTC_EXTENSION, 0xff, 0xff, 0xff, 0xff, b's'])));
        assert!(!allowed(&[]));
    }

    #[test]
    fn failure_reply_is_one_message() {
        assert_eq!(FAILURE_REPLY.to_vec(), message(&[SSH_AGENT_FAILURE]));
    }
}
