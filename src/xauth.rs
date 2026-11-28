//! `DISPLAY` parsing and the Xauthority file format.
//!
//! An Xauthority file is a sequence of entries:
//!
//! ```text
//! family u16 BE
//! address, number, name, data: each a u16 BE length followed by bytes
//! ```
//!
//! Both are implemented here so neither side needs the `xauth` program.

pub const FAMILY_LOCAL: u16 = 256;
pub const FAMILY_WILD: u16 = 0xffff;
pub const MIT_MAGIC_COOKIE: &str = "MIT-MAGIC-COOKIE-1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub family: u16,
    pub address: Vec<u8>,
    pub number: Vec<u8>,
    pub name: Vec<u8>,
    pub data: Vec<u8>,
}

/// Parse an Xauthority file. A truncated trailing entry is ignored, like
/// libXau does.
pub fn parse(mut bytes: &[u8]) -> Vec<Entry> {
    fn take_u16(b: &mut &[u8]) -> Option<u16> {
        if b.len() < 2 {
            return None;
        }
        let v = u16::from_be_bytes([b[0], b[1]]);
        *b = &b[2..];
        Some(v)
    }
    fn take_field(b: &mut &[u8]) -> Option<Vec<u8>> {
        let n = take_u16(b)? as usize;
        if b.len() < n {
            return None;
        }
        let v = b[..n].to_vec();
        *b = &b[n..];
        Some(v)
    }
    let mut out = vec![];
    loop {
        let entry = (|| {
            Some(Entry {
                family: take_u16(&mut bytes)?,
                address: take_field(&mut bytes)?,
                number: take_field(&mut bytes)?,
                name: take_field(&mut bytes)?,
                data: take_field(&mut bytes)?,
            })
        })();
        match entry {
            Some(e) => out.push(e),
            None => return out,
        }
    }
}

// Only the (Linux-only) container agent writes Xauthority files.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn serialize(entries: &[Entry]) -> Vec<u8> {
    let mut out = vec![];
    for e in entries {
        out.extend_from_slice(&e.family.to_be_bytes());
        for field in [&e.address, &e.number, &e.name, &e.data] {
            out.extend_from_slice(&(field.len() as u16).to_be_bytes());
            out.extend_from_slice(field);
        }
    }
    out
}

/// Pick the MIT-MAGIC-COOKIE-1 entry a local client of display `number`
/// would use: a wildcard entry, or a local one for this host name. Entries
/// with an empty display number match any display.
pub fn find_cookie<'a>(entries: &'a [Entry], hostname: &[u8], number: u32) -> Option<&'a Entry> {
    let num = number.to_string();
    entries.iter().find(|e| {
        e.name == MIT_MAGIC_COOKIE.as_bytes()
            && (e.number.is_empty() || e.number == num.as_bytes())
            && (e.family == FAMILY_WILD || (e.family == FAMILY_LOCAL && e.address == hostname))
    })
}

/// A wildcard-family entry for `number`, matching whatever host name the
/// client machine has.
// Only the (Linux-only) container agent writes Xauthority files.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn wildcard_entry(number: u32, name: &str, data: &[u8]) -> Entry {
    Entry {
        family: FAMILY_WILD,
        address: vec![],
        number: number.to_string().into_bytes(),
        name: name.as_bytes().to_vec(),
        data: data.to_vec(),
    }
}

/// Where the host X server is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Display {
    /// `:N`, `unix:N` — the local socket `/tmp/.X11-unix/XN`, which on Linux
    /// usually also exists as an abstract socket of the same name.
    Local { number: u32 },
    /// A full socket path, as used by XQuartz on macOS
    /// (`/private/tmp/com.apple.launchd.…/org.xquartz:0`).
    Path { path: String, number: u32 },
    /// `host:N` — TCP port 6000+N.
    Tcp { host: String, number: u32 },
}

impl Display {
    pub fn number(&self) -> u32 {
        match self {
            Display::Local { number } | Display::Path { number, .. } | Display::Tcp { number, .. } => *number,
        }
    }
}

/// Parse a `DISPLAY` value: `[host]:number[.screen]`.
pub fn parse_display(s: &str) -> Option<Display> {
    let colon = s.rfind(':')?;
    let (host, rest) = (&s[..colon], &s[colon + 1..]);
    let num_str = rest.split('.').next()?;
    if num_str.is_empty() || !num_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number: u32 = num_str.parse().ok()?;
    if host.starts_with('/') {
        // The socket path is the whole value, screen suffix included.
        return Some(Display::Path {
            path: s.to_string(),
            number,
        });
    }
    if host.is_empty() || host == "unix" {
        return Some(Display::Local { number });
    }
    // `host::N` is DECnet; not supported.
    if host.ends_with(':') {
        return None;
    }
    let host = host.trim_start_matches('[').trim_end_matches(']');
    Some(Display::Tcp {
        host: host.to_string(),
        number,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(addr: &str, num: &str, data: &[u8]) -> Entry {
        Entry {
            family: FAMILY_LOCAL,
            address: addr.as_bytes().to_vec(),
            number: num.as_bytes().to_vec(),
            name: MIT_MAGIC_COOKIE.as_bytes().to_vec(),
            data: data.to_vec(),
        }
    }

    #[test]
    fn roundtrips_entries() {
        let entries = vec![
            local("myhost", "0", &[1; 16]),
            wildcard_entry(0, MIT_MAGIC_COOKIE, &[2; 16]),
            wildcard_entry(1, MIT_MAGIC_COOKIE, &[3; 16]),
        ];
        let bytes = serialize(&entries);
        assert_eq!(parse(&bytes), entries);
    }

    #[test]
    fn matches_xauth_nlist_layout() {
        // `xauth nlist` prints each field as <len><hex>, in file order.
        let e = wildcard_entry(0, MIT_MAGIC_COOKIE, &[0xab; 2]);
        let hex: String = serialize(&[e]).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "ffff00000001300012\
             4d49542d4d414749432d434f4f4b49452d31\
             0002abab"
        );
    }

    #[test]
    fn ignores_truncated_tail() {
        let mut bytes = serialize(&[wildcard_entry(0, MIT_MAGIC_COOKIE, &[1; 16])]);
        let good = parse(&bytes);
        bytes.extend_from_slice(&[0xff, 0xff, 0, 5, b'a']);
        assert_eq!(parse(&bytes), good);
        assert!(parse(&[]).is_empty());
    }

    #[test]
    fn finds_cookie_like_a_local_client() {
        let entries = vec![
            local("otherhost", "0", &[9; 16]),
            local("myhost", "1", &[8; 16]),
            local("myhost", "0", &[7; 16]),
            wildcard_entry(0, MIT_MAGIC_COOKIE, &[6; 16]),
        ];
        assert_eq!(find_cookie(&entries, b"myhost", 0).unwrap().data, vec![7; 16]);
        assert_eq!(find_cookie(&entries, b"myhost", 1).unwrap().data, vec![8; 16]);
        assert_eq!(find_cookie(&entries, b"elsewhere", 0).unwrap().data, vec![6; 16]);
        assert!(find_cookie(&entries, b"elsewhere", 1).is_none());

        let any_display = vec![local("myhost", "", &[5; 16])];
        assert_eq!(find_cookie(&any_display, b"myhost", 3).unwrap().data, vec![5; 16]);

        let other_proto = vec![Entry {
            name: b"XDM-AUTHORIZATION-1".to_vec(),
            ..wildcard_entry(0, MIT_MAGIC_COOKIE, &[1; 16])
        }];
        assert!(find_cookie(&other_proto, b"h", 0).is_none());
    }

    #[test]
    fn parses_displays() {
        assert_eq!(parse_display(":0"), Some(Display::Local { number: 0 }));
        assert_eq!(parse_display(":1.0"), Some(Display::Local { number: 1 }));
        assert_eq!(parse_display("unix:12"), Some(Display::Local { number: 12 }));
        assert_eq!(
            parse_display("localhost:10.0"),
            Some(Display::Tcp {
                host: "localhost".into(),
                number: 10
            })
        );
        assert_eq!(
            parse_display("[::1]:2"),
            Some(Display::Tcp {
                host: "::1".into(),
                number: 2
            })
        );
        assert_eq!(
            parse_display("/private/tmp/com.apple.launchd.abc/org.xquartz:0"),
            Some(Display::Path {
                path: "/private/tmp/com.apple.launchd.abc/org.xquartz:0".into(),
                number: 0
            })
        );
        assert_eq!(parse_display(""), None);
        assert_eq!(parse_display("wayland-0"), None);
        assert_eq!(parse_display(":"), None);
        assert_eq!(parse_display(":x"), None);
        assert_eq!(parse_display("decnet::0"), None);
    }
}
