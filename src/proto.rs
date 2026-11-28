//! Wire protocol between the host half and the container agent.
//!
//! The agent's stdin/stdout (a `devcontainer exec` pipe) carry a sequence of
//! frames:
//!
//! ```text
//! +------+------------+------------+-----------------+
//! | type | channel    | length     | payload         |
//! | u8   | u32 BE     | u32 BE     | `length` bytes  |
//! +------+------------+------------+-----------------+
//! ```
//!
//! Channel 0 carries JSON control messages (handshake). Every forwarded
//! connection gets its own channel id, chosen by the container side, which is
//! the only side that opens channels.
//!
//! Encoding and decoding are pure functions so they can be unit-tested
//! without any I/O.

use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};

/// Bumped whenever the frame layout or the control messages change
/// incompatibly. Both halves are always the same build in practice (the
/// container half is copied by content hash), so this is a safety net.
pub const PROTOCOL_VERSION: u32 = 3;

pub const HEADER_LEN: usize = 9;
/// Largest payload either side accepts in a control frame (the handshake
/// can carry a gpg key export).
pub const MAX_PAYLOAD: usize = 1 << 20;
/// Largest payload of a data frame, and the size of the read buffer used
/// when relaying a socket into data frames. A larger data frame is a
/// protocol violation, which bounds what a channel's queue can hold.
pub const DATA_CHUNK: usize = 32 * 1024;

/// Largest payload accepted for a frame of type `ty`.
fn max_payload(ty: u8) -> usize {
    if ty == T_DATA { DATA_CHUNK } else { MAX_PAYLOAD }
}

const T_CONTROL: u8 = 1;
const T_OPEN: u8 = 2;
const T_DATA: u8 = 3;
const T_EOF: u8 = 4;
const T_CLOSE: u8 = 5;

/// What a channel forwards. The container may open all but `Tcp`;
/// it never names a host path or address. `Tcp` is opened by the host only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    SshAgent,
    X11,
    Browser,
    /// A forwarded port: the agent connects to this port on the
    /// container's loopback.
    Tcp(u16),
    /// One request from the git credential helper.
    GitCredential,
    /// A connection to gpg-agent (the host's restricted extra socket).
    GpgAgent,
}

/// Channel ids chosen by the host have this bit set, ids chosen by the
/// container don't, so the two sides never pick the same id.
pub const HOST_CHANNEL_BIT: u32 = 0x8000_0000;

impl ChannelKind {
    fn encode(self) -> Vec<u8> {
        match self {
            ChannelKind::SshAgent => vec![1],
            ChannelKind::X11 => vec![2],
            ChannelKind::Browser => vec![3],
            ChannelKind::Tcp(port) => {
                let [hi, lo] = port.to_be_bytes();
                vec![4, hi, lo]
            }
            ChannelKind::GitCredential => vec![5],
            ChannelKind::GpgAgent => vec![6],
        }
    }

    fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        match b {
            [1] => Ok(ChannelKind::SshAgent),
            [2] => Ok(ChannelKind::X11),
            [3] => Ok(ChannelKind::Browser),
            [4, hi, lo] => match u16::from_be_bytes([*hi, *lo]) {
                0 => Err(DecodeError::BadPayload("open")),
                port => Ok(ChannelKind::Tcp(port)),
            },
            [5] => Ok(ChannelKind::GitCredential),
            [6] => Ok(ChannelKind::GpgAgent),
            [k, ..] if !(1..=6).contains(k) => Err(DecodeError::UnknownChannelKind(*k)),
            _ => Err(DecodeError::BadPayload("open")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Handshake message on channel 0.
    Control(Control),
    /// One side accepted a local connection and asks the other to connect
    /// the matching endpoint: the container for SSH agent, X11 and browser
    /// channels, the host for forwarded ports.
    Open { channel: u32, kind: ChannelKind },
    /// Bytes for a channel.
    Data { channel: u32, data: Vec<u8> },
    /// The sender will send no more data on this channel (half close).
    Eof { channel: u32 },
    /// The channel is gone (error or full close); drop all state for it.
    Close { channel: u32 },
}

/// Control messages, exchanged once at startup:
/// agent → host `Hello`, host → agent `Setup`, agent → host `Ready`.
/// Before `Setup` the host may send `NetnsProbe`, which the agent answers
/// with `NetnsProbed`. After `Ready` the host may send `Stop`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    Hello {
        protocol: u32,
        version: String,
        /// `readlink /proc/self/ns/net` inside the container. The agent's
        /// word only: where it matters (the X cookie), the host checks it
        /// with `NetnsProbe`.
        netns: Option<String>,
        /// Login shell of the container user, from the passwd database.
        shell: Option<String>,
        /// Identifies the container (its runtime's container ID), so
        /// sessions on the same container can share one port forwarder.
        #[serde(default)]
        container_id: Option<String>,
    },
    /// Host → agent: prove that you are in my network namespace by
    /// connecting to this abstract Unix socket and writing `nonce` to it
    /// (see `netns.rs`).
    NetnsProbe { socket: String, nonce: Vec<u8> },
    /// Agent → host: done trying. Says nothing about the outcome; the
    /// host looks at its socket.
    NetnsProbed,
    Setup(Setup),
    Ready {
        /// Environment for the user's `devcontainer exec`.
        env: Vec<(String, String)>,
        x11: Option<X11Ready>,
        /// `None` when Wayland was not asked for.
        #[serde(default)]
        wayland: Option<WaylandStatus>,
        /// The agent's own binary: the host runs the user's command as
        /// `<launcher> run COMMAND...`, which ties it to the session (see
        /// `procs.rs`). Ends up on the host's command line, so the host
        /// checks it like the shell path.
        #[serde(default)]
        launcher: Option<String>,
    },
    /// Host → agent: end the user's command (its session) now, while the
    /// forwards are still up. The agent keeps running until the host
    /// closes its stdin.
    Stop,
    /// Fatal setup error; the sender exits after sending it.
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct X11Offer {
    /// Offered only when the container has shown that it shares the
    /// host's network namespace (`NetnsProbe`) and the host X server
    /// listens on an abstract socket.
    pub direct: Option<X11Direct>,
    /// Whether the agent may fall back to a relayed display.
    pub relay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct X11Direct {
    pub display: u32,
    pub auth: Option<XAuthCookie>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XAuthCookie {
    pub name: String,
    pub data: Vec<u8>,
}

/// What the host asks the agent to set up.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setup {
    pub ssh_agent: bool,
    pub browser: bool,
    pub x11: Option<X11Offer>,
    /// Look for the Wayland socket `dctether up` (or VS Code) mounted.
    #[serde(default)]
    pub wayland: bool,
    /// Install the git credential helper.
    #[serde(default)]
    pub git_credentials: bool,
    /// Relay gpg-agent through a private GNUPGHOME.
    #[serde(default)]
    pub gpg: Option<GpgOffer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpgOffer {
    /// `gpg --export` of the host's secret keys: their public parts, which
    /// gpg in the container needs to know what the agent can sign with.
    pub public_keys: Vec<u8>,
}

/// State of the mounted Wayland socket in the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaylandStatus {
    /// Mounted and the compositor accepts connections.
    Ready,
    /// Not mounted (the container wasn't created by `dctether up`).
    Missing,
    /// Mounted, but nobody listens: the mount pins a socket from an
    /// earlier login session.
    Dead,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum X11Ready {
    Direct,
    /// The agent listens on a local display and relays it. Clients must
    /// present `fake_auth`; the host swaps in the real cookie.
    Relay { display: u32, fake_auth: XAuthCookie },
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    UnknownType(u8),
    UnknownChannelKind(u8),
    TooLarge(usize),
    BadPayload(&'static str),
    BadControl(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::UnknownType(t) => write!(f, "unknown frame type {t}"),
            DecodeError::UnknownChannelKind(k) => write!(f, "unknown channel kind {k}"),
            DecodeError::TooLarge(n) => write!(f, "frame payload of {n} bytes exceeds limit"),
            DecodeError::BadPayload(what) => write!(f, "malformed {what} frame"),
            DecodeError::BadControl(e) => write!(f, "malformed control message: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Append the encoding of `frame` to `out`.
pub fn encode(frame: &Frame, out: &mut BytesMut) {
    let (ty, channel, payload): (u8, u32, std::borrow::Cow<'_, [u8]>) = match frame {
        Frame::Control(c) => (
            T_CONTROL,
            0,
            serde_json::to_vec(c).expect("control messages always serialize").into(),
        ),
        Frame::Open { channel, kind } => (T_OPEN, *channel, kind.encode().into()),
        Frame::Data { channel, data } => (T_DATA, *channel, data.as_slice().into()),
        Frame::Eof { channel } => (T_EOF, *channel, (&[][..]).into()),
        Frame::Close { channel } => (T_CLOSE, *channel, (&[][..]).into()),
    };
    debug_assert!(payload.len() <= max_payload(ty));
    out.reserve(HEADER_LEN + payload.len());
    out.put_u8(ty);
    out.put_u32(channel);
    out.put_u32(payload.len() as u32);
    out.put_slice(&payload);
}

/// Decode one frame from the front of `buf`, consuming its bytes.
///
/// Returns `Ok(None)` when `buf` does not yet hold a complete frame; nothing
/// is consumed in that case.
pub fn decode(buf: &mut BytesMut) -> Result<Option<Frame>, DecodeError> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    let ty = buf[0];
    let channel = u32::from_be_bytes(buf[1..5].try_into().unwrap());
    let len = u32::from_be_bytes(buf[5..9].try_into().unwrap()) as usize;
    if !matches!(ty, T_CONTROL | T_OPEN | T_DATA | T_EOF | T_CLOSE) {
        return Err(DecodeError::UnknownType(ty));
    }
    // Checked from the header alone, before any of the payload is buffered.
    if len > max_payload(ty) {
        return Err(DecodeError::TooLarge(len));
    }
    if buf.len() < HEADER_LEN + len {
        return Ok(None);
    }
    buf.advance(HEADER_LEN);
    let payload = buf.split_to(len);
    let frame = match ty {
        T_CONTROL => {
            if channel != 0 {
                return Err(DecodeError::BadPayload("control"));
            }
            let c = serde_json::from_slice(&payload)
                .map_err(|e| DecodeError::BadControl(e.to_string()))?;
            Frame::Control(c)
        }
        T_OPEN => {
            if channel == 0 {
                return Err(DecodeError::BadPayload("open"));
            }
            Frame::Open {
                channel,
                kind: ChannelKind::decode(&payload)?,
            }
        }
        T_DATA => Frame::Data {
            channel,
            data: payload.to_vec(),
        },
        T_EOF | T_CLOSE => {
            if !payload.is_empty() {
                return Err(DecodeError::BadPayload("eof/close"));
            }
            if ty == T_EOF {
                Frame::Eof { channel }
            } else {
                Frame::Close { channel }
            }
        }
        _ => unreachable!(),
    };
    Ok(Some(frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(f: Frame) {
        let mut buf = BytesMut::new();
        encode(&f, &mut buf);
        let got = decode(&mut buf).unwrap().unwrap();
        assert_eq!(got, f);
        assert!(buf.is_empty());
    }

    #[test]
    fn roundtrips_every_frame_type() {
        roundtrip(Frame::Open {
            channel: 7,
            kind: ChannelKind::SshAgent,
        });
        roundtrip(Frame::Open {
            channel: 8,
            kind: ChannelKind::X11,
        });
        roundtrip(Frame::Open {
            channel: 9,
            kind: ChannelKind::Browser,
        });
        roundtrip(Frame::Data {
            channel: 1,
            data: (0..=255).collect(),
        });
        roundtrip(Frame::Data {
            channel: u32::MAX,
            data: vec![],
        });
        roundtrip(Frame::Open {
            channel: HOST_CHANNEL_BIT | 1,
            kind: ChannelKind::Tcp(3000),
        });
        roundtrip(Frame::Open {
            channel: 10,
            kind: ChannelKind::Tcp(65535),
        });
        roundtrip(Frame::Open {
            channel: 11,
            kind: ChannelKind::GitCredential,
        });
        roundtrip(Frame::Open {
            channel: 12,
            kind: ChannelKind::GpgAgent,
        });
        roundtrip(Frame::Eof { channel: 3 });
        roundtrip(Frame::Close { channel: 4 });
        roundtrip(Frame::Control(Control::Hello {
            protocol: PROTOCOL_VERSION,
            version: "0.1.0".into(),
            netns: Some("net:[4026531833]".into()),
            shell: None,
            container_id: Some("3b1c42e81917".into()),
        }));
        roundtrip(Frame::Control(Control::Setup(Setup {
            ssh_agent: true,
            browser: false,
            x11: Some(X11Offer {
                direct: Some(X11Direct {
                    display: 0,
                    auth: Some(XAuthCookie {
                        name: "MIT-MAGIC-COOKIE-1".into(),
                        data: vec![1, 2, 3],
                    }),
                }),
                relay: true,
            }),
            wayland: true,
            git_credentials: true,
            gpg: Some(GpgOffer {
                public_keys: vec![0x99, 1, 2],
            }),
        })));
        roundtrip(Frame::Control(Control::Ready {
            env: vec![("DISPLAY".into(), ":10".into())],
            x11: Some(X11Ready::Relay {
                display: 10,
                fake_auth: XAuthCookie {
                    name: "MIT-MAGIC-COOKIE-1".into(),
                    data: vec![9; 16],
                },
            }),
            wayland: Some(WaylandStatus::Dead),
            launcher: Some("/home/dev/.cache/dctether/x/dctether".into()),
        }));
        roundtrip(Frame::Control(Control::Stop));
        roundtrip(Frame::Control(Control::Error {
            message: "nope".into(),
        }));
        roundtrip(Frame::Control(Control::NetnsProbe {
            socket: "dctether-netns-0123".into(),
            nonce: vec![0, 1, 255],
        }));
        roundtrip(Frame::Control(Control::NetnsProbed));
    }

    #[test]
    fn wayland_fields_default_when_absent() {
        let setup: Control =
            serde_json::from_str(r#"{"type":"setup","ssh_agent":true,"browser":true,"x11":null}"#).unwrap();
        assert_eq!(
            setup,
            Control::Setup(Setup {
                ssh_agent: true,
                browser: true,
                ..Setup::default()
            })
        );
        let ready: Control = serde_json::from_str(r#"{"type":"ready","env":[],"x11":null}"#).unwrap();
        assert!(matches!(ready, Control::Ready { wayland: None, launcher: None, .. }));
    }

    #[test]
    fn wire_layout_is_stable() {
        let mut buf = BytesMut::new();
        encode(
            &Frame::Data {
                channel: 0x01020304,
                data: b"hi".to_vec(),
            },
            &mut buf,
        );
        assert_eq!(&buf[..], &[3, 1, 2, 3, 4, 0, 0, 0, 2, b'h', b'i']);
    }

    #[test]
    fn partial_input_is_not_consumed() {
        let mut full = BytesMut::new();
        encode(
            &Frame::Data {
                channel: 5,
                data: vec![42; 100],
            },
            &mut full,
        );
        for cut in 0..full.len() {
            let mut part = BytesMut::from(&full[..cut]);
            assert_eq!(decode(&mut part), Ok(None), "cut at {cut}");
            assert_eq!(part.len(), cut);
        }
    }

    #[test]
    fn decodes_back_to_back_frames_byte_by_byte() {
        let frames = vec![
            Frame::Open {
                channel: 1,
                kind: ChannelKind::X11,
            },
            Frame::Data {
                channel: 1,
                data: b"abc".to_vec(),
            },
            Frame::Eof { channel: 1 },
            Frame::Close { channel: 1 },
        ];
        let mut wire = BytesMut::new();
        for f in &frames {
            encode(f, &mut wire);
        }
        let mut buf = BytesMut::new();
        let mut got = vec![];
        for b in wire.iter() {
            buf.put_u8(*b);
            while let Some(f) = decode(&mut buf).unwrap() {
                got.push(f);
            }
        }
        assert_eq!(got, frames);
    }

    #[test]
    fn rejects_oversized_frame_from_header_alone() {
        // Data frames: at most DATA_CHUNK.
        let mut buf = BytesMut::new();
        buf.put_u8(T_DATA);
        buf.put_u32(1);
        buf.put_u32((DATA_CHUNK + 1) as u32);
        assert_eq!(decode(&mut buf), Err(DecodeError::TooLarge(DATA_CHUNK + 1)));
        roundtrip(Frame::Data {
            channel: 1,
            data: vec![7; DATA_CHUNK],
        });

        // Control frames may be larger (the gpg key export), up to MAX_PAYLOAD.
        roundtrip(Frame::Control(Control::Error {
            message: "x".repeat(DATA_CHUNK + 1),
        }));
        let mut buf = BytesMut::new();
        buf.put_u8(T_CONTROL);
        buf.put_u32(0);
        buf.put_u32((MAX_PAYLOAD + 1) as u32);
        assert_eq!(decode(&mut buf), Err(DecodeError::TooLarge(MAX_PAYLOAD + 1)));
    }

    #[test]
    fn rejects_unknown_type_and_kind() {
        let mut buf = BytesMut::from(&[99u8, 0, 0, 0, 1, 0, 0, 0, 0][..]);
        assert_eq!(decode(&mut buf), Err(DecodeError::UnknownType(99)));

        let mut buf = BytesMut::from(&[T_OPEN, 0, 0, 0, 1, 0, 0, 0, 1, 77][..]);
        assert_eq!(decode(&mut buf), Err(DecodeError::UnknownChannelKind(77)));
    }

    #[test]
    fn tcp_open_carries_port() {
        let mut buf = BytesMut::new();
        encode(
            &Frame::Open {
                channel: 7,
                kind: ChannelKind::Tcp(0x1234),
            },
            &mut buf,
        );
        assert_eq!(&buf[..], &[T_OPEN, 0, 0, 0, 7, 0, 0, 0, 3, 4, 0x12, 0x34]);
        // Port 0, a truncated port, and a port on a non-TCP kind are errors.
        for bad in [&[4u8, 0, 0][..], &[4, 1], &[1, 0, 80]] {
            let mut buf = BytesMut::from(&[T_OPEN, 0, 0, 0, 7, 0, 0, 0, bad.len() as u8][..]);
            buf.put_slice(bad);
            assert_eq!(decode(&mut buf), Err(DecodeError::BadPayload("open")), "{bad:?}");
        }
    }

    #[test]
    fn rejects_malformed_payloads() {
        // Open on channel 0.
        let mut buf = BytesMut::from(&[T_OPEN, 0, 0, 0, 0, 0, 0, 0, 1, 1][..]);
        assert_eq!(decode(&mut buf), Err(DecodeError::BadPayload("open")));
        // Eof with a payload.
        let mut buf = BytesMut::from(&[T_EOF, 0, 0, 0, 1, 0, 0, 0, 1, 0][..]);
        assert_eq!(decode(&mut buf), Err(DecodeError::BadPayload("eof/close")));
        // Control on a non-zero channel.
        let mut buf = BytesMut::from(&[T_CONTROL, 0, 0, 0, 1, 0, 0, 0, 2, b'{', b'}'][..]);
        assert_eq!(decode(&mut buf), Err(DecodeError::BadPayload("control")));
        // Control that is not JSON.
        let mut buf = BytesMut::from(&[T_CONTROL, 0, 0, 0, 0, 0, 0, 0, 1, b'x'][..]);
        assert!(matches!(decode(&mut buf), Err(DecodeError::BadControl(_))));
    }
}
