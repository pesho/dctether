//! Git HTTPS credentials: a credential helper in the container that asks
//! the host's git.
//!
//! The container side is `dctether-git-credential ACTION` (a symlink to
//! the binary), which git runs as a credential helper. It sends the action
//! and git's request over the session's `git-credential.sock`; the host
//! answers with `git credential fill|approve|reject`, so whatever helpers
//! and prompts the host has configured are used.

/// Largest request accepted from the container.
pub const MAX_REQUEST: usize = 64 * 1024;

/// What git asks a helper to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `get`: look up a credential (host: `git credential fill`).
    Get,
    /// `store`: a credential worked (host: `git credential approve`).
    Store,
    /// `erase`: a credential was rejected (host: `git credential reject`).
    Erase,
}

impl Action {
    pub fn git_subcommand(self) -> &'static str {
        match self {
            Action::Get => "fill",
            Action::Store => "approve",
            Action::Erase => "reject",
        }
    }
}

/// Attribute names of git's credential protocol that are passed on. Array
/// attributes end in `[]`.
const KEYS: &[&str] = &[
    "protocol",
    "host",
    "path",
    "username",
    "password",
    "password_expiry_utc",
    "oauth_refresh_token",
    "url",
    "authtype",
    "credential",
    "ephemeral",
    "continue",
    "wwwauth[]",
    "capability[]",
    "state[]",
];

/// Check a request from the (untrusted) container: an action line, then
/// git's `key=value` lines. Only known keys are accepted, and only for
/// `http`/`https`, so the container can't ask the host for credentials of
/// other kinds. Returns the action and the attributes to feed to the
/// host's `git credential`, terminated by a blank line.
pub fn parse_request(req: &str) -> Result<(Action, String), String> {
    if req.len() > MAX_REQUEST {
        return Err("request too large".into());
    }
    if req.contains('\0') {
        return Err("request contains NUL".into());
    }
    let mut lines = req.lines();
    let action = match lines.next() {
        Some("get") => Action::Get,
        Some("store") => Action::Store,
        Some("erase") => Action::Erase,
        Some(other) => return Err(format!("unknown action {other:?}")),
        None => return Err("empty request".into()),
    };
    let mut out = String::new();
    let mut protocol = None;
    for line in lines {
        if line.is_empty() {
            break; // end of git's attribute list
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("malformed line {line:?}"))?;
        if !KEYS.contains(&key) {
            return Err(format!("unsupported attribute {key:?}"));
        }
        if value.contains('\r') {
            return Err(format!("{key}: carriage return in value"));
        }
        match key {
            "protocol" => protocol = Some(value.to_string()),
            "url" => {
                let lower = value.to_ascii_lowercase();
                if !(lower.starts_with("https://") || lower.starts_with("http://")) {
                    return Err("only http and https URLs are supported".into());
                }
            }
            _ => {}
        }
        out.push_str(line);
        out.push('\n');
    }
    match protocol.as_deref() {
        Some("https" | "http") => {}
        Some(p) => return Err(format!("protocol {p:?} not forwarded (only http and https)")),
        None => return Err("request has no protocol".into()),
    }
    out.push('\n');
    Ok((action, out))
}

/// Container side: `dctether-git-credential ACTION`, run by git with the
/// request on stdin. Prints the host's answer for `get`.
#[cfg(target_os = "linux")]
pub fn helper_main(args: &[String]) -> i32 {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let Some(action) = args.first() else {
        eprintln!("usage: dctether-git-credential get|store|erase");
        return 2;
    };
    let mut input = String::new();
    if std::io::stdin().take(MAX_REQUEST as u64).read_to_string(&mut input).is_err() {
        return 1;
    }
    // Outside a dctether session (no socket), stay quiet: git moves on to
    // its next helper or prompts as usual.
    let Some(session) = std::env::var_os("DCTETHER_SESSION") else {
        return 1;
    };
    let sock = std::path::Path::new(&session).join("git-credential.sock");
    let result = (|| -> std::io::Result<String> {
        let mut s = UnixStream::connect(&sock)?;
        s.write_all(format!("{action}\n{input}").as_bytes())?;
        s.shutdown(std::net::Shutdown::Write)?;
        let mut reply = String::new();
        s.take(MAX_REQUEST as u64).read_to_string(&mut reply)?;
        Ok(reply)
    })();
    match result {
        Ok(reply) => match reply.split_once('\n') {
            Some(("ok", body)) => {
                print!("{body}");
                0
            }
            _ => {
                let msg = reply.strip_prefix("error: ").unwrap_or(&reply);
                eprintln!("dctether-git-credential: {}", msg.trim_end());
                1
            }
        },
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_git_requests() {
        let (a, body) = parse_request("get\nprotocol=https\nhost=github.com\n\n").unwrap();
        assert_eq!(a, Action::Get);
        assert_eq!(body, "protocol=https\nhost=github.com\n\n");

        let req = "store\nprotocol=https\nhost=example.com\npath=org/repo.git\nusername=u\npassword=p=q\n\
                   capability[]=authtype\nwwwauth[]=Basic realm=\"x\"\n";
        let (a, body) = parse_request(req).unwrap();
        assert_eq!(a, Action::Store);
        assert!(body.contains("password=p=q\n") && body.ends_with("\n\n"));

        let (a, _) = parse_request("erase\nprotocol=http\nhost=h\n").unwrap();
        assert_eq!((a, a.git_subcommand()), (Action::Erase, "reject"));
        assert_eq!(Action::Get.git_subcommand(), "fill");
        assert!(parse_request("get\nprotocol=https\nurl=https://h/x\n").is_ok());
    }

    #[test]
    fn rejects_everything_else() {
        for bad in [
            "",
            "list\nprotocol=https\nhost=h\n",
            "get\nhost=h\n",
            "get\nprotocol=smtp\nhost=mail\n",
            "get\nprotocol=cert\npath=/home/u/cert.p12\n",
            "get\nprotocol=https\nurl=file:///etc/passwd\n",
            "get\nprotocol=https\nhost\n",
            "get\nprotocol=https\nhelper=!sh -c id\n",
            "get\nprotocol=https\nhost=a\rb\n",
            "get\nprotocol=https\nhost=a\0b\n",
        ] {
            assert!(parse_request(bad).is_err(), "{bad:?}");
        }
        let huge = format!("get\nprotocol=https\nhost={}\n", "a".repeat(MAX_REQUEST));
        assert!(parse_request(&huge).is_err());
    }

    #[test]
    fn stops_at_blank_line() {
        let (_, body) = parse_request("get\nprotocol=https\nhost=h\n\nignored=1\n").unwrap();
        assert_eq!(body, "protocol=https\nhost=h\n\n");
    }
}
