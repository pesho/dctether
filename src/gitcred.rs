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

/// Attribute names of git's credential protocol that are accepted. Array
/// attributes end in `[]`. `url` is not among them: git never sends it to
/// a helper, and it could carry a user name and password.
const KEYS: &[&str] = &[
    "protocol",
    "host",
    "path",
    "username",
    "password",
    "password_expiry_utc",
    "oauth_refresh_token",
    "authtype",
    "credential",
    "ephemeral",
    "continue",
    "wwwauth[]",
    "capability[]",
    "state[]",
];

/// The attributes a `get` may carry: what git knows when it asks. Never a
/// secret. `git credential fill` returns a credential that is already
/// complete in its input as it is, without asking a helper, so a `get`
/// carrying a password would make the host "hand out" (see [`Served`]) a
/// credential the container wrote itself.
const GET_KEYS: &[&str] = &["protocol", "host", "path", "username", "wwwauth[]", "capability[]", "state[]"];

/// Check a request from the (untrusted) container: an action line, then
/// git's `key=value` lines. Only known keys are accepted (for `get`, only
/// [`GET_KEYS`]), and only for `http`/`https`, so the container can't ask
/// the host for credentials of other kinds. Returns the action and the
/// attributes, terminated by a blank line: for `get`, what to feed to the
/// host's `git credential fill`; for the others, what [`Served`] matches.
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
        if action == Action::Get && !GET_KEYS.contains(&key) {
            return Err(format!("attribute {key:?} is not accepted in a get request"));
        }
        if value.contains('\r') {
            return Err(format!("{key}: carriage return in value"));
        }
        if key == "protocol" {
            protocol = Some(value.to_string());
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

/// What tells one credential from another, for matching a `store` or
/// `erase` request against what the host handed out.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    protocol: String,
    host: String,
    username: String,
    password: String,
}

/// The identity in a list of `key=value` lines (the last value of a key
/// wins, as in git). `None` unless all four attributes are there and the
/// password isn't empty, or if there is a `url` line: git would take the
/// identity from that URL instead, and this doesn't parse URLs.
fn identity(attrs: &str) -> Option<Identity> {
    let mut found: [Option<&str>; 4] = [None; 4];
    for line in attrs.lines().take_while(|l| !l.is_empty()) {
        let (key, value) = line.split_once('=')?;
        match key {
            "protocol" => found[0] = Some(value),
            "host" => found[1] = Some(value),
            "username" => found[2] = Some(value),
            "password" => found[3] = Some(value),
            "url" => return None,
            _ => {}
        }
    }
    let [protocol, host, username, password] = found.map(|v| v.map(str::to_string));
    Some(Identity {
        protocol: protocol?,
        host: host?,
        username: username?,
        password: password.filter(|p| !p.is_empty())?,
    })
}

/// Most credentials remembered per session; the oldest is dropped first.
const MAX_SERVED: usize = 64;

/// The credentials the host's `git credential fill` handed to the
/// container in this session. The container may have the host `approve` or
/// `reject` these and no others, and the host's git is then given the
/// host's own record of the credential, not the container's request.
///
/// Credentials without a username and password (git's `authtype` and
/// `credential` attributes) are not remembered, so `store` and `erase` do
/// nothing for them.
#[derive(Default)]
pub struct Served {
    list: Vec<(Identity, String)>,
}

impl Served {
    /// Remember the output of a successful `git credential fill` on the
    /// host. An answer without a password (nothing found, and nothing
    /// typed into the host's prompt) is not a credential.
    pub fn remember(&mut self, fill_output: &str) {
        let Some(id) = identity(fill_output) else {
            return;
        };
        self.list.retain(|(known, _)| *known != id);
        if self.list.len() >= MAX_SERVED {
            self.list.remove(0);
        }
        // As input for `git credential approve|reject`: ends with a blank line.
        let record = format!("{}\n\n", fill_output.trim_end_matches('\n'));
        self.list.push((id, record));
    }

    /// The host's record of the credential a `store` or `erase` request
    /// names, if this session handed it out.
    pub fn matching(&self, attrs: &str) -> Option<String> {
        let id = identity(attrs)?;
        self.list.iter().find(|(known, _)| *known == id).map(|(_, record)| record.clone())
    }

    /// Drop the credential a request names, once the host erased it.
    pub fn forget(&mut self, attrs: &str) {
        if let Some(id) = identity(attrs) {
            self.list.retain(|(known, _)| *known != id);
        }
    }
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

        // What git sends when it asks, challenge and capabilities included.
        let req = "get\nprotocol=https\nhost=h\npath=o/r.git\nusername=u\ncapability[]=authtype\n\
                   capability[]=state\nwwwauth[]=Basic realm=\"x\"\nstate[]=helper.k=v\n";
        assert!(parse_request(req).is_ok());
    }

    /// The host's `git credential fill` returns a credential that is
    /// complete in its input unchanged, so a `get` must not carry one.
    #[test]
    fn get_carries_no_secrets() {
        for attr in [
            "password=planted",
            "credential=planted",
            "authtype=Bearer",
            "oauth_refresh_token=planted",
            "password_expiry_utc=1",
            "ephemeral=1",
            "continue=1",
            "url=https://planted:planted@h/",
        ] {
            let req = format!("get\nprotocol=https\nhost=h\nusername=u\n{attr}\n");
            assert!(parse_request(&req).is_err(), "{attr}");
        }
        // `store` and `erase` describe a credential, so they do carry one.
        for action in ["store", "erase"] {
            let req = format!("{action}\nprotocol=https\nhost=h\nusername=u\npassword=p\npassword_expiry_utc=1\n");
            assert!(parse_request(&req).is_ok(), "{action}");
            // But never as a URL.
            assert!(parse_request(&format!("{action}\nprotocol=https\nurl=https://u:p@h/\n")).is_err());
        }
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
    fn store_and_erase_select_only_what_the_host_handed_out() {
        // What `git credential fill` printed on the host, extras included.
        let fill = "protocol=https\nhost=git.example\nusername=me\npassword=s3cret\npassword_expiry_utc=1900000000\n";
        let mut served = Served::default();
        served.remember(fill);

        // git in the container reports the same credential back, with a
        // path the host never printed and attributes of its own.
        let (_, same) = parse_request(
            "store\nprotocol=https\nhost=git.example\npath=org/repo.git\nusername=me\npassword=s3cret\n\
             password_expiry_utc=1\noauth_refresh_token=planted\n",
        )
        .unwrap();
        // The host's git gets the host's own record, not the request.
        assert_eq!(served.matching(&same).as_deref(), Some(&*format!("{fill}\n")));

        // Anything that differs in protocol, host, user or password names
        // no credential, and neither does a request with a `url` line.
        for other in [
            "protocol=https\nhost=git.example\nusername=me\npassword=other\n\n",
            "protocol=https\nhost=other.example\nusername=me\npassword=s3cret\n\n",
            "protocol=http\nhost=git.example\nusername=me\npassword=s3cret\n\n",
            "protocol=https\nhost=git.example\nusername=you\npassword=s3cret\n\n",
            "protocol=https\nhost=git.example\nusername=me\n\n",
            "protocol=https\nhost=git.example\n\n",
            "protocol=https\nhost=git.example\nusername=me\npassword=s3cret\nurl=https://git.example/\n\n",
            // The last value of a repeated key counts, as in git.
            "protocol=https\nhost=git.example\nusername=me\npassword=s3cret\nhost=other.example\n\n",
            "",
        ] {
            assert_eq!(served.matching(other), None, "{other:?}");
        }

        served.forget(&same);
        assert_eq!(served.matching(&same), None);
    }

    #[test]
    fn empty_answers_are_not_credentials() {
        let mut served = Served::default();
        // `fill` exits 0 with these when nothing is stored and the prompt
        // returns nothing.
        served.remember("protocol=https\nhost=h\nusername=\npassword=\n");
        served.remember("protocol=https\nhost=h\n");
        assert_eq!(served.matching("protocol=https\nhost=h\nusername=\npassword=\n\n"), None);
        assert!(served.list.is_empty());
    }

    #[test]
    fn remembers_a_bounded_number_without_duplicates() {
        let cred = |n: usize| format!("protocol=https\nhost=h{n}\nusername=u\npassword=p\n");
        let mut served = Served::default();
        for n in 0..MAX_SERVED + 5 {
            served.remember(&cred(n));
            served.remember(&cred(n));
        }
        assert_eq!(served.list.len(), MAX_SERVED);
        assert_eq!(served.matching(&cred(0)), None);
        assert!(served.matching(&cred(MAX_SERVED + 4)).is_some());
    }

    #[test]
    fn stops_at_blank_line() {
        let (_, body) = parse_request("get\nprotocol=https\nhost=h\n\nignored=1\n").unwrap();
        assert_eq!(body, "protocol=https\nhost=h\n\n");
    }
}
