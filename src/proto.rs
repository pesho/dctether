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
pub const PROTOCOL_VERSION: u32 = 1;

pub const HEADER_LEN: usize = 9;
/// Largest payload either side accepts. Data frames are produced in chunks of
/// at most [`DATA_CHUNK`] bytes; anything larger is a protocol violation.
pub const MAX_PAYLOAD: usize = 1 << 20;
/// Size of the read buffer used when relaying a socket into data frames.
pub const DATA_CHUNK: usize = 32 * 1024;

const T_CONTROL: u8 = 1;
const T_OPEN: u8 = 2;
const T_DATA: u8 = 3;
const T_EOF: u8 = 4;
const T_CLOSE: u8 = 5;

/// What a channel forwards. The container may only ask for one of these;
/// it never names a host path or address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    SshAgent,
    X11,
    Browser,
}

impl ChannelKind {
    fn to_byte(self) -> u8 {
        match self {
            ChannelKind::SshAgent => 1,
            ChannelKind::X11 => 2,
            ChannelKind::Browser => 3,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(ChannelKind::SshAgent),
            2 => Some(ChannelKind::X11),
            3 => Some(ChannelKind::Browser),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Handshake message on channel 0.
    Control(Control),
    /// The container accepted a local connection and asks the host to
    /// connect the matching host-side endpoint.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    Hello {
        protocol: u32,
        version: String,
        /// `readlink /proc/self/ns/net` inside the container.
        netns: Option<String>,
        /// Login shell of the container user, from the passwd database.
        shell: Option<String>,
    },
    Setup {
        ssh_agent: bool,
        browser: bool,
        x11: Option<X11Offer>,
    },
    Ready {
        /// Environment for the user's `devcontainer exec`.
        env: Vec<(String, String)>,
        x11: Option<X11Ready>,
    },
    /// Fatal setup error; the sender exits after sending it.
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct X11Offer {
    /// Offered only when the container shares the host's network namespace
    /// and the host X server listens on an abstract socket.
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
        Frame::Open { channel, kind } => (T_OPEN, *channel, vec![kind.to_byte()].into()),
        Frame::Data { channel, data } => (T_DATA, *channel, data.as_slice().into()),
        Frame::Eof { channel } => (T_EOF, *channel, (&[][..]).into()),
        Frame::Close { channel } => (T_CLOSE, *channel, (&[][..]).into()),
    };
    debug_assert!(payload.len() <= MAX_PAYLOAD);
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
    if len > MAX_PAYLOAD {
        return Err(DecodeError::TooLarge(len));
    }
    if !matches!(ty, T_CONTROL | T_OPEN | T_DATA | T_EOF | T_CLOSE) {
        return Err(DecodeError::UnknownType(ty));
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
            if payload.len() != 1 || channel == 0 {
                return Err(DecodeError::BadPayload("open"));
            }
            let kind = ChannelKind::from_byte(payload[0])
                .ok_or(DecodeError::UnknownChannelKind(payload[0]))?;
            Frame::Open { channel, kind }
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
        roundtrip(Frame::Eof { channel: 3 });
        roundtrip(Frame::Close { channel: 4 });
        roundtrip(Frame::Control(Control::Hello {
            protocol: PROTOCOL_VERSION,
            version: "0.1.0".into(),
            netns: Some("net:[4026531833]".into()),
            shell: None,
        }));
        roundtrip(Frame::Control(Control::Setup {
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
        }));
        roundtrip(Frame::Control(Control::Ready {
            env: vec![("DISPLAY".into(), ":10".into())],
            x11: Some(X11Ready::Relay {
                display: 10,
                fake_auth: XAuthCookie {
                    name: "MIT-MAGIC-COOKIE-1".into(),
                    data: vec![9; 16],
                },
            }),
        }));
        roundtrip(Frame::Control(Control::Error {
            message: "nope".into(),
        }));
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
        let mut buf = BytesMut::new();
        buf.put_u8(T_DATA);
        buf.put_u32(1);
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
