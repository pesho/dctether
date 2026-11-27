//! Port forwarding: one forwarder process per container, shared by every
//! `dctether exec` session on that container.
//!
//! - The forwarder (`dctether forward KEY [devcontainer-args…]`, started
//!   by `exec`, not meant to be run by hand) runs its own container agent
//!   and listens on host `127.0.0.1:<port>` for each forwarded port. Each
//!   accepted connection becomes a host-opened channel; the agent connects
//!   it to the same port on the container's loopback.
//! - Sessions find it through a control socket in a private runtime
//!   directory, named after the container. A session sends the ports it
//!   wants, reads one answer per port, then keeps the connection open for
//!   as long as it runs. The kernel closes it when the session ends, even
//!   on SIGKILL, so the forwarder counts live sessions by open connections
//!   and exits a few seconds after the last one is gone.
//! - An exclusive `flock` on a lock file makes sure only one forwarder per
//!   container runs, even when several sessions start at once.

use std::collections::HashSet;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::host;
use crate::mux::{self, Out, Registry};
use crate::proto::{ChannelKind, Control, Frame, HOST_CHANNEL_BIT, PROTOCOL_VERSION};

/// How long the forwarder stays up after its last session ends, so that a
/// session started right after another one closed finds it still running.
const GRACE: Duration = Duration::from_secs(5);
/// How long a freshly started forwarder waits for its first session.
const FIRST_SESSION: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Pure helpers

/// One port's outcome, as the forwarder reports it to a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortResult {
    /// Newly forwarded for this request.
    New(u16),
    /// Already forwarded (by an earlier session).
    Active(u16),
    Failed(u16, String),
}

pub fn format_request(ports: &[u16]) -> String {
    let list: Vec<String> = ports.iter().map(u16::to_string).collect();
    format!("forward {}\n", list.join(" "))
}

pub fn parse_request(line: &str) -> Result<Vec<u16>, String> {
    let mut words = line.split_whitespace();
    if words.next() != Some("forward") {
        return Err("expected `forward PORT…`".into());
    }
    words.map(|w| parse_port(w).ok_or_else(|| format!("bad port {w:?}"))).collect()
}

pub fn format_reply(r: &PortResult) -> String {
    match r {
        PortResult::New(p) => format!("new {p}\n"),
        PortResult::Active(p) => format!("active {p}\n"),
        // Keep the message on one line.
        PortResult::Failed(p, e) => format!("failed {p} {}\n", e.replace(['\n', '\r'], " ")),
    }
}

/// Parse one reply line; `Ok(None)` for the final `done`.
pub fn parse_reply(line: &str) -> Result<Option<PortResult>, String> {
    let line = line.trim_end();
    if line == "done" {
        return Ok(None);
    }
    let mut it = line.splitn(3, ' ');
    let (kind, port) = (it.next(), it.next().and_then(parse_port));
    match (kind, port) {
        (Some("new"), Some(p)) => Ok(Some(PortResult::New(p))),
        (Some("active"), Some(p)) => Ok(Some(PortResult::Active(p))),
        (Some("failed"), Some(p)) => Ok(Some(PortResult::Failed(p, it.next().unwrap_or("").to_string()))),
        _ => Err(format!("unexpected reply {line:?}")),
    }
}

/// A TCP port number, 1–65535.
pub fn parse_port(s: &str) -> Option<u16> {
    s.parse::<u16>().ok().filter(|p| *p != 0)
}

/// `forwardPorts` from `devcontainer read-configuration` output: the merged
/// configuration (which includes image metadata) if present, else the
/// plain one. Returns the ports plus the entries dctether can't forward
/// (such as `"db:5432"` for another Compose service).
pub fn config_ports(json: &str) -> Result<(Vec<u16>, Vec<String>), String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("reading configuration: {e}"))?;
    let list = ["mergedConfiguration", "configuration"]
        .iter()
        .find_map(|k| v.get(k)?.get("forwardPorts")?.as_array());
    let (mut ports, mut skipped) = (vec![], vec![]);
    for e in list.into_iter().flatten() {
        let port = match e {
            serde_json::Value::Number(n) => n.as_u64().and_then(|n| u16::try_from(n).ok()).filter(|p| *p != 0),
            serde_json::Value::String(s) => parse_port(s),
            _ => None,
        };
        match port {
            Some(p) if !ports.contains(&p) => ports.push(p),
            Some(_) => {}
            None => skipped.push(match e {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            }),
        }
    }
    Ok((ports, skipped))
}

/// `devcontainer exec` options with a value that `read-configuration`
/// also accepts.
const RC_VALUE_OPTS: &[&str] = &[
    "--user-data-folder",
    "--docker-path",
    "--docker-arg",
    "--docker-compose-path",
    "--workspace-folder",
    "--container-id",
    "--id-label",
    "--config",
    "--override-config",
    "--log-level",
    "--log-format",
    "--terminal-columns",
    "--terminal-rows",
    "--allow-cross-origin-auth-host",
];
/// Boolean options both accept.
const RC_BOOL_OPTS: &[&str] = &[
    "--oci-auth-hardening",
    "--mount-workspace-git-root",
    "--mount-git-worktree-common-dir",
];
/// `exec`-only options with a value; dropped together with that value.
const EXEC_ONLY_VALUE_OPTS: &[&str] = &[
    "--remote-env",
    "--container-data-folder",
    "--container-system-data-folder",
    "--default-user-env-probe",
];

/// The subset of `exec`'s pass-through arguments that
/// `devcontainer read-configuration` accepts (it rejects unknown options).
pub fn read_configuration_args(dc_args: &[String]) -> Vec<String> {
    let mut out = vec![];
    let mut i = 0;
    while i < dc_args.len() {
        let a = &dc_args[i];
        let (name, inline_value) = match a.split_once('=') {
            Some((n, _)) => (n, true),
            None => (a.as_str(), false),
        };
        let next = dc_args.get(i + 1);
        if RC_VALUE_OPTS.contains(&name) {
            out.push(a.clone());
            if !inline_value && let Some(v) = next {
                out.push(v.clone());
                i += 1;
            }
        } else if RC_BOOL_OPTS.contains(&name) {
            out.push(a.clone());
            if !inline_value && let Some(v) = next.filter(|v| *v == "true" || *v == "false") {
                out.push(v.clone());
                i += 1;
            }
        } else if EXEC_ONLY_VALUE_OPTS.contains(&name) && !inline_value {
            i += 1; // skip its value too
        }
        i += 1;
    }
    out
}

/// Name for the per-container forwarder files: the container ID reported
/// by the agent if it has one, else a hash of how the container was named
/// (working directory and devcontainer arguments).
pub fn forwarder_key(container_id: Option<&str>, cwd: &Path, dc_args: &[String]) -> String {
    if let Some(id) = container_id.filter(|id| id.len() >= 12 && id.bytes().all(|b| b.is_ascii_hexdigit())) {
        return id[..id.len().min(32)].to_ascii_lowercase();
    }
    let mut h = Sha256::new();
    h.update(cwd.as_os_str().as_encoded_bytes());
    for a in dc_args {
        h.update([0]);
        h.update(a.as_bytes());
    }
    let digest = h.finalize();
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("args-{hex}")
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 40 && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

// ---------------------------------------------------------------------------
// Files

struct Paths {
    socket: PathBuf,
    lock: PathBuf,
    log: PathBuf,
}

/// A directory only this user can use: `$XDG_RUNTIME_DIR/dctether`, or
/// `$TMPDIR/dctether-<uid>` (`/tmp` by default).
fn runtime_dir() -> io::Result<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| Path::new(d).is_absolute()) {
        Some(d) => Path::new(&d).join("dctether"),
        None => std::env::temp_dir().join(format!("dctether-{uid}")),
    };
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let m = std::fs::symlink_metadata(&dir)?;
    if !m.is_dir() || m.uid() != uid || m.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} is not a private directory owned by you",
            dir.display()
        )));
    }
    Ok(dir)
}

fn paths(key: &str) -> io::Result<Paths> {
    if !valid_key(key) {
        return Err(io::Error::other(format!("invalid forwarder key {key:?}")));
    }
    let dir = runtime_dir()?;
    Ok(Paths {
        socket: dir.join(format!("forward-{key}.sock")),
        lock: dir.join(format!("forward-{key}.lock")),
        log: dir.join(format!("forward-{key}.log")),
    })
}

// ---------------------------------------------------------------------------
// Session side (`dctether exec`)

/// Make sure the container's forwarder runs and forwards `ports`. The
/// returned stream must stay open for as long as the session runs: it is
/// what keeps the forwarder alive.
pub async fn attach(
    key: &str,
    dc_args: &[String],
    ports: &[u16],
) -> Result<(UnixStream, Vec<PortResult>), String> {
    let p = paths(key).map_err(|e| e.to_string())?;
    let mut last_err = String::new();
    for _ in 0..3 {
        let stream = match UnixStream::connect(&p.socket).await {
            Ok(s) => s,
            Err(_) => {
                spawn_forwarder(key, dc_args, &p).map_err(|e| format!("starting the port forwarder: {e}"))?;
                match wait_for_socket(&p.socket, Duration::from_secs(30)).await {
                    Some(s) => s,
                    None => {
                        return Err(format!(
                            "the port forwarder didn't start (see {})",
                            p.log.display()
                        ));
                    }
                }
            }
        };
        match request(stream, ports).await {
            Ok(r) => return Ok(r),
            // Most likely a forwarder that was just shutting down; start over.
            Err(e) => last_err = e,
        }
    }
    Err(format!("port forwarder: {last_err} (see {})", p.log.display()))
}

async fn wait_for_socket(path: &Path, limit: Duration) -> Option<UnixStream> {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        if let Ok(s) = UnixStream::connect(path).await {
            return Some(s);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

async fn request(stream: UnixStream, ports: &[u16]) -> Result<(UnixStream, Vec<PortResult>), String> {
    let mut stream = BufReader::new(stream);
    stream
        .get_mut()
        .write_all(format_request(ports).as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut results = vec![];
    let read = async {
        loop {
            let mut line = String::new();
            if (&mut stream).take(4096).read_line(&mut line).await.map_err(|e| e.to_string())? == 0 {
                return Err("closed the connection".to_string());
            }
            match parse_reply(&line)? {
                Some(r) => results.push(r),
                None => return Ok(()),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .map_err(|_| "no answer".to_string())??;
    Ok((stream.into_inner(), results))
}

/// Start `dctether forward` detached from this session: its own session
/// (so a closed terminal doesn't kill it), output to its log file.
fn spawn_forwarder(key: &str, dc_args: &[String], p: &Paths) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&p.log)?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("forward")
        .arg(key)
        .args(dc_args)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    // Reap it if it exits while this session still runs.
    std::thread::spawn(move || child.wait());
    Ok(())
}

// ---------------------------------------------------------------------------
// Forwarder side (`dctether forward KEY [devcontainer-args…]`)

pub fn forwarder_main(args: &[String]) -> i32 {
    let Some((key, dc_args)) = args.split_first() else {
        eprintln!("dctether forward is started by `dctether exec`, not by hand");
        return 2;
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    match rt.block_on(forwarder(key, dc_args)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("dctether forward: {e}");
            1
        }
    }
}

struct Forwarder {
    ports: tokio::sync::Mutex<HashSet<u16>>,
    out: Out,
    reg: Arc<Registry>,
    next_id: AtomicU32,
}

async fn forwarder(key: &str, dc_args: &[String]) -> Result<(), String> {
    let p = paths(key).map_err(|e| e.to_string())?;

    // One forwarder per container: hold an exclusive lock for our lifetime.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&p.lock)
        .map_err(|e| format!("{}: {e}", p.lock.display()))?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(()); // another forwarder runs for this container
    }
    // Ours now: start a fresh log (stderr is the log file).
    unsafe { libc::ftruncate(2, 0) };
    eprintln!("dctether forward {} for {key}: starting", crate::VERSION);

    // Our own agent in the container, for host-opened channels only.
    let dc = std::env::var("DCTETHER_DEVCONTAINER").unwrap_or_else(|_| "devcontainer".into());
    let bins = host::locate_binaries()?;
    let (mut agent, mut reader, stdin) = host::start_agent(&dc, dc_args, &bins).await?;
    let (out, out_rx) = mpsc::channel::<Frame>(64);
    let writer = tokio::spawn(mux::write_frames(stdin, out_rx));
    match reader.next().await {
        Ok(Some(Frame::Control(Control::Hello { protocol, .. }))) if protocol == PROTOCOL_VERSION => {}
        other => return Err(format!("container agent did not start ({})", host::describe(other))),
    }
    host::send(
        &out,
        Frame::Control(Control::Setup {
            ssh_agent: false,
            browser: false,
            x11: None,
            wayland: false,
        }),
    )
    .await?;
    match reader.next().await {
        Ok(Some(Frame::Control(Control::Ready { .. }))) => {}
        other => return Err(format!("container agent failed during setup ({})", host::describe(other))),
    }
    let reg = Arc::new(Registry::default());
    let mut relay = tokio::spawn(host::host_read_loop(
        reader,
        reg.clone(),
        out.clone(),
        Arc::new(host::HostCtx::none()),
    ));

    // Only the lock holder gets here, so a socket file left behind is stale.
    let _ = std::fs::remove_file(&p.socket);
    let control = UnixListener::bind(&p.socket).map_err(|e| format!("{}: {e}", p.socket.display()))?;
    eprintln!("listening on {}", p.socket.display());

    let st = Arc::new(Forwarder {
        ports: Default::default(),
        out: out.clone(),
        reg,
        next_id: AtomicU32::new(0),
    });
    let (ended_tx, mut ended_rx) = mpsc::channel::<()>(16);
    let mut sessions = 0usize;
    let mut idle_since = Some(tokio::time::Instant::now() + FIRST_SESSION - GRACE);
    let why = loop {
        let idle = async {
            match idle_since {
                Some(t) => tokio::time::sleep_until(t + GRACE).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            r = control.accept() => if let Ok((s, _)) = r {
                sessions += 1;
                idle_since = None;
                tokio::spawn(serve_session(s, st.clone(), ended_tx.clone()));
            },
            Some(()) = ended_rx.recv() => {
                sessions -= 1;
                if sessions == 0 {
                    idle_since = Some(tokio::time::Instant::now());
                }
            }
            _ = idle => break "no sessions left".to_string(),
            r = &mut relay => break r.unwrap_or_else(|e| e.to_string()),
        }
    };
    eprintln!("stopping: {why}");

    // New sessions must not find us any more; then end the agent.
    let _ = std::fs::remove_file(&p.socket);
    drop(control);
    relay.abort();
    writer.abort();
    drop(out);
    if tokio::time::timeout(Duration::from_secs(5), agent.wait()).await.is_err() {
        let _ = agent.kill().await;
    }
    drop(lock);
    Ok(())
}

async fn serve_session(stream: UnixStream, st: Arc<Forwarder>, ended: mpsc::Sender<()>) {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    let mut line = String::new();
    let got = tokio::time::timeout(Duration::from_secs(10), (&mut rd).take(4096).read_line(&mut line)).await;
    if let Ok(Ok(n)) = got
        && n > 0
    {
        let mut reply = String::new();
        match parse_request(&line) {
            Ok(ports) => {
                for port in ports {
                    let r = st.ensure(port).await;
                    eprintln!("{r:?}");
                    reply += &format_reply(&r);
                }
            }
            Err(e) => eprintln!("bad request: {e}"),
        }
        reply += "done\n";
        let _ = wr.write_all(reply.as_bytes()).await;
    }
    // The session keeps the connection open while it runs.
    let mut sink = [0u8; 256];
    while let Ok(n) = rd.read(&mut sink).await {
        if n == 0 {
            break;
        }
    }
    let _ = ended.send(()).await;
}

impl Forwarder {
    async fn ensure(self: &Arc<Self>, port: u16) -> PortResult {
        let mut ports = self.ports.lock().await;
        if ports.contains(&port) {
            return PortResult::Active(port);
        }
        match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(l) => {
                ports.insert(port);
                tokio::spawn(self.clone().accept_loop(l, port));
                PortResult::New(port)
            }
            Err(e) => PortResult::Failed(port, e.to_string()),
        }
    }

    async fn accept_loop(self: Arc<Self>, listener: TcpListener, port: u16) {
        loop {
            let stream = match listener.accept().await {
                Ok((s, _)) => s,
                Err(e) => {
                    eprintln!("port {port}: accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let _ = stream.set_nodelay(true);
            let id = loop {
                let id = HOST_CHANNEL_BIT | (self.next_id.fetch_add(1, Ordering::Relaxed) & !HOST_CHANNEL_BIT);
                if !self.reg.contains(id) {
                    break id;
                }
            };
            let Some(inbox) = self.reg.insert(id) else {
                continue; // too many channels; refuse this connection
            };
            let kind = ChannelKind::Tcp(port);
            if self.out.send(Frame::Open { channel: id, kind }).await.is_err() {
                return;
            }
            tokio::spawn(mux::pump(id, stream, inbox, self.out.clone(), self.reg.clone(), None));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn control_messages_roundtrip() {
        assert_eq!(format_request(&[3000, 8080]), "forward 3000 8080\n");
        assert_eq!(parse_request("forward 3000 8080\n"), Ok(vec![3000, 8080]));
        assert_eq!(parse_request("forward\n"), Ok(vec![]));
        assert!(parse_request("forward 0\n").is_err());
        assert!(parse_request("forward 70000\n").is_err());
        assert!(parse_request("hello 1\n").is_err());

        for r in [
            PortResult::New(3000),
            PortResult::Active(22),
            PortResult::Failed(3775, "Address in use (os error 98)".into()),
        ] {
            assert_eq!(parse_reply(&format_reply(&r)), Ok(Some(r)));
        }
        assert_eq!(
            format_reply(&PortResult::Failed(1, "two\nlines".into())),
            "failed 1 two lines\n"
        );
        assert_eq!(parse_reply("done\n"), Ok(None));
        assert!(parse_reply("new x\n").is_err());
    }

    #[test]
    fn reads_forward_ports_from_configuration() {
        let json = r#"{"configuration":{"forwardPorts":[3000,"8080","db:5432",3000,0,true]}}"#;
        assert_eq!(
            config_ports(json).unwrap(),
            (vec![3000, 8080], vec!["db:5432".to_string(), "0".to_string(), "true".to_string()])
        );
        // The merged configuration wins: it includes image metadata.
        let json = r#"{"configuration":{"forwardPorts":[1]},"mergedConfiguration":{"forwardPorts":[2,1]}}"#;
        assert_eq!(config_ports(json).unwrap().0, vec![2, 1]);
        assert_eq!(config_ports(r#"{"configuration":{}}"#).unwrap(), (vec![], vec![]));
        assert!(config_ports("not json").is_err());
    }

    #[test]
    fn keeps_only_read_configuration_options() {
        let args = s(&[
            "--workspace-folder",
            "/w",
            "--remote-env",
            "A=B",
            "--config=/c.json",
            "--mount-workspace-git-root",
            "false",
            "--default-user-env-probe",
            "none",
            "--oci-auth-hardening",
            "--container-id",
            "abc",
            "--remote-env=C=D",
        ]);
        assert_eq!(
            read_configuration_args(&args),
            s(&[
                "--workspace-folder",
                "/w",
                "--config=/c.json",
                "--mount-workspace-git-root",
                "false",
                "--oci-auth-hardening",
                "--container-id",
                "abc",
            ])
        );
        assert!(read_configuration_args(&[]).is_empty());
    }

    #[test]
    fn derives_forwarder_key() {
        let id = "3b1c42e819173cead180a3085232101a7ee9d9602f9895631d8454ebdef96f34";
        assert_eq!(forwarder_key(Some(id), Path::new("/x"), &[]), &id[..32]);
        // Same container named differently: same key.
        assert_eq!(
            forwarder_key(Some(id), Path::new("/a"), &s(&["--workspace-folder", "/a"])),
            forwarder_key(Some(id), Path::new("/b"), &[])
        );
        // No (or a bogus) container ID: derived from how it was named.
        let a = forwarder_key(None, Path::new("/w"), &s(&["--config", "x"]));
        assert!(a.starts_with("args-") && valid_key(&a));
        assert_eq!(a, forwarder_key(Some("../../etc"), Path::new("/w"), &s(&["--config", "x"])));
        assert_ne!(a, forwarder_key(None, Path::new("/w"), &s(&["--config", "y"])));
        assert!(!valid_key("../x") && !valid_key("") && valid_key(&id[..32]));
    }
}
