//! Host half: `dctether exec`.
//!
//! 1. Start the container agent over its own `devcontainer exec`, copying
//!    the right static binary into the container first if it is missing.
//! 2. Handshake: learn the container's network namespace and shell, offer
//!    forwards, receive the environment to use.
//! 3. Run the user's command in a second `devcontainer exec` with that
//!    environment, relaying channels until it exits.

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStdout, Command};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::browser;
use crate::mux::{self, ChanMsg, FrameReader, Out, Registry};
use crate::proto::{
    ChannelKind, Control, DATA_CHUNK, Frame, GpgOffer, HOST_CHANNEL_BIT, PROTOCOL_VERSION, Setup, WaylandStatus,
    X11Direct, X11Offer, X11Ready, XAuthCookie,
};
use crate::x11::ClientFilter;
use crate::xauth::{self, Display};

/// Container architectures we can ship a binary for, as `uname -m` names them.
pub const ARCHES: [&str; 2] = ["x86_64", "aarch64"];

#[derive(Debug, PartialEq, Eq)]
pub struct ExecArgs {
    /// Passed verbatim to every `devcontainer exec`.
    pub dc_args: Vec<String>,
    pub cmd: Vec<String>,
    pub ssh_agent: bool,
    /// Relay every SSH agent request, not only listing and signing.
    pub ssh_agent_unfiltered: bool,
    pub x11: bool,
    pub browser: bool,
    pub wayland: bool,
    pub force_relay: bool,
    /// Forward `forwardPorts` and `ports`.
    pub forward: bool,
    pub git_credentials: bool,
    pub gpg: bool,
    /// From `-p PORT`.
    pub ports: Vec<u16>,
}

/// `[options and devcontainer-exec options…] [-- cmd…]`. dctether's own
/// options are recognized anywhere before `--`; everything else there is
/// passed to `devcontainer exec` untouched.
pub fn parse_exec_args(args: &[String]) -> Result<ExecArgs, String> {
    let (before, cmd) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], args[i + 1..].to_vec()),
        None => (args, vec![]),
    };
    let mut a = ExecArgs {
        dc_args: vec![],
        cmd,
        ssh_agent: true,
        ssh_agent_unfiltered: false,
        x11: true,
        browser: true,
        wayland: true,
        force_relay: false,
        forward: true,
        ports: vec![],
        git_credentials: true,
        gpg: true,
    };
    let mut it = before.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--no-ssh-agent" => a.ssh_agent = false,
            "--ssh-agent-unfiltered" => a.ssh_agent_unfiltered = true,
            "--no-x11" => a.x11 = false,
            "--no-browser" => a.browser = false,
            "--no-wayland" => a.wayland = false,
            "--no-forward" => a.forward = false,
            "--no-git-credentials" => a.git_credentials = false,
            "--no-gpg" => a.gpg = false,
            "--x11-relay" => a.force_relay = true,
            "-p" => {
                let v = it.next().ok_or("-p needs a port number")?;
                a.ports.push(crate::forward::parse_port(v).ok_or_else(|| format!("-p: bad port {v:?}"))?);
            }
            _ => a.dc_args.push(arg.clone()),
        }
    }
    Ok(a)
}

pub fn exec_main(args: &[String]) -> i32 {
    let a = match parse_exec_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("dctether: {e}");
            return 2;
        }
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    match rt.block_on(run(a)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("dctether: {e}");
            1
        }
    }
}

async fn run(a: ExecArgs) -> Result<i32, String> {
    let dc = std::env::var("DCTETHER_DEVCONTAINER").unwrap_or_else(|_| "devcontainer".into());
    let bins = locate_binaries()?;

    // `forwardPorts` comes from the configuration on the host; read it while
    // the agent starts.
    let config = a.forward.then(|| {
        let dc = dc.clone();
        let rc_args = crate::forward::read_configuration_args(&a.dc_args);
        tokio::spawn(async move {
            Command::new(&dc)
                .arg("read-configuration")
                .arg("--include-merged-configuration")
                .args(&rc_args)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .await
        })
    });

    // The host's gpg-agent and public keys, also while the agent starts.
    let gpg = a.gpg.then(|| tokio::task::spawn_blocking(host_gpg));

    let (mut agent, mut reader, stdin) = start_agent(&dc, &a.dc_args, &bins).await?;
    let (out, out_rx) = mpsc::channel::<Frame>(64);
    let writer = tokio::spawn(mux::write_frames(stdin, out_rx));

    // Handshake.
    let (netns, shell, container_id) = match reader.next().await {
        Ok(Some(Frame::Control(Control::Hello {
            protocol,
            version,
            netns,
            shell,
            container_id,
        }))) => {
            if protocol != PROTOCOL_VERSION {
                return Err(format!(
                    "container agent {version} speaks protocol {protocol}, expected {PROTOCOL_VERSION}"
                ));
            }
            (netns, shell, container_id)
        }
        other => return Err(format!("container agent did not start ({})", describe(other))),
    };

    let ssh_sock = a
        .ssh_agent
        .then(|| std::env::var_os("SSH_AUTH_SOCK"))
        .flatten()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let host_x11 = if a.x11 { HostX11::from_env() } else { None };
    let offer = host_x11.as_ref().map(|hx| X11Offer {
        direct: if a.force_relay { None } else { hx.direct_offer(netns.as_deref()) },
        relay: true,
    });
    crate::debug!(
        "container netns {:?}; X11 {}",
        netns,
        match &offer {
            None => "off".to_string(),
            Some(o) if o.direct.is_some() => "direct offered".to_string(),
            Some(_) => "relay only".to_string(),
        }
    );
    let gpg = match gpg {
        Some(task) => task.await.unwrap_or(None),
        None => None,
    };
    let git_credentials = a.git_credentials && which("git");
    send(
        &out,
        Frame::Control(Control::Setup(Setup {
            ssh_agent: ssh_sock.is_some(),
            browser: a.browser,
            x11: offer,
            // Only meaningful when this host has a Wayland session to share.
            wayland: a.wayland && host_wayland_socket().is_some(),
            git_credentials,
            gpg: gpg.as_ref().map(|g| GpgOffer {
                public_keys: g.public_keys.clone(),
            }),
        })),
    )
    .await?;

    let (env, x11_ready) = match reader.next().await {
        Ok(Some(Frame::Control(Control::Ready { env, x11, wayland }))) => {
            check_env(&env)?;
            report_wayland(wayland);
            (env, x11)
        }
        Ok(Some(Frame::Control(Control::Error { message }))) => {
            return Err(message.chars().filter(|c| !c.is_control()).collect());
        }
        other => return Err(format!("container agent failed during setup ({})", describe(other))),
    };
    crate::debug!(
        "X11 mode: {}",
        match &x11_ready {
            None => "off".to_string(),
            Some(X11Ready::Direct) => "direct".to_string(),
            Some(X11Ready::Relay { display, .. }) => format!("relay on :{display}"),
        }
    );

    let relay = match (x11_ready, host_x11) {
        (Some(X11Ready::Relay { fake_auth, .. }), Some(hx)) => Some(X11Relay {
            display: hx.display,
            fake: (fake_auth.name.into_bytes(), fake_auth.data),
            real: hx
                .cookie
                .map(|c| (c.name.into_bytes(), c.data))
                .unwrap_or_default(),
        }),
        _ => None,
    };
    let ctx = Arc::new(HostCtx {
        ssh_sock,
        ssh_filter: !a.ssh_agent_unfiltered,
        relay,
        browser: a.browser,
        git: git_credentials.then(Default::default),
        gpg_socket: gpg.map(|g| g.extra_socket),
    });
    let reg = Arc::new(Registry::default());
    let mut relay_task = tokio::spawn(host_read_loop(reader, reg, out.clone(), ctx));

    // Ports. Held for the whole session: the open connection is what keeps
    // the container's port forwarder alive.
    let _forwarder = match config {
        Some(config) => forward_ports(&a, config, netns.as_deref(), container_id.as_deref()).await,
        None => None,
    };

    // The user's command.
    let cmd = if a.cmd.is_empty() {
        // The shell comes from the container: only accept an absolute path,
        // so it can't be taken for a devcontainer option.
        vec![shell.filter(|s| is_safe_path(s)).unwrap_or_else(|| "/bin/sh".into())]
    } else {
        a.cmd
    };
    let mut user = Command::new(&dc);
    user.arg("exec").args(&a.dc_args);
    for (k, v) in &env {
        // One token, so the value can never be parsed as an option.
        user.arg(format!("--remote-env={k}={v}"));
    }
    user.args(&cmd);
    let mut child = user.spawn().map_err(|e| format!("{dc}: {e}"))?;
    forward_signals(child.id());

    let status = loop {
        tokio::select! {
            s = child.wait() => break s.map_err(|e| e.to_string())?,
            r = &mut relay_task, if !relay_task.is_finished() => {
                let why = r.unwrap_or_else(|e| e.to_string());
                eprintln!("\r\ndctether: forwarding stopped: {why}\r");
            }
        }
    };

    // Closing the agent's stdin ends the session in the container.
    relay_task.abort();
    writer.abort();
    drop(out);
    if tokio::time::timeout(Duration::from_secs(5), agent.wait()).await.is_err() {
        let _ = agent.kill().await;
    }
    Ok(exit_code(status))
}

/// Variables the agent may set, and what their values may look like. The
/// agent runs in the (untrusted) container and these end up on the host's
/// `devcontainer` command line.
const FORWARDED_ENV: [&str; 10] = [
    "SSH_AUTH_SOCK",
    "DISPLAY",
    "XAUTHORITY",
    "BROWSER",
    "DCTETHER_SESSION",
    "WAYLAND_DISPLAY",
    "GNUPGHOME",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_KEY_0",
    "GIT_CONFIG_VALUE_0",
];

pub fn check_env(env: &[(String, String)]) -> Result<(), String> {
    for (k, v) in env {
        if !FORWARDED_ENV.contains(&k.as_str()) {
            return Err(format!("container agent asked to set unexpected variable {k:?}"));
        }
        let ok = match k.as_str() {
            "DISPLAY" => v
                .strip_prefix(':')
                .is_some_and(|n| (1..=5).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit())),
            // Exactly the one git setting dctether adds.
            "GIT_CONFIG_COUNT" => v == "1",
            "GIT_CONFIG_KEY_0" => v == "credential.helper",
            _ => is_safe_path(v),
        };
        if !ok {
            return Err(format!("container agent sent an invalid value for {k}: {v:?}"));
        }
    }
    Ok(())
}

/// An absolute path without control characters.
fn is_safe_path(v: &str) -> bool {
    v.starts_with('/') && v.len() <= 4096 && !v.chars().any(char::is_control)
}

fn host_wayland_socket() -> Option<PathBuf> {
    crate::wayland::host_socket(
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
    )
}

fn report_wayland(status: Option<WaylandStatus>) {
    match status {
        None => {}
        Some(WaylandStatus::Ready) => crate::debug!("Wayland: mounted socket is live"),
        Some(WaylandStatus::Missing) => {
            crate::debug!("Wayland: no socket mounted (container not created by `dctether up` or VS Code)")
        }
        // The one case worth interrupting for: a Wayland socket was mounted
        // at creation (by `dctether up` or VS Code) and it broke.
        Some(WaylandStatus::Dead) => eprintln!(
            "dctether: the container's Wayland socket is from an earlier login session and no longer works; \
             not setting WAYLAND_DISPLAY. Recreate the container (e.g. \
             `dctether up --remove-existing-container`) to fix it.\r"
        ),
    }
}

// ---------------------------------------------------------------------------
// dctether up

/// `devcontainer up` with the host's Wayland socket mounted, unless
/// `--no-wayland` is given or there is no Wayland session. All other
/// arguments go to `devcontainer up` unchanged.
pub fn up_main(args: &[String]) -> i32 {
    let wayland = !args.iter().any(|a| a == "--no-wayland");
    let mut dc_args: Vec<String> = args.iter().filter(|a| *a != "--no-wayland").cloned().collect();
    if wayland {
        match host_wayland_socket() {
            None => crate::debug!("no Wayland session (WAYLAND_DISPLAY unset); not mounting a socket"),
            Some(sock) if !is_socket(&sock) => {
                eprintln!("dctether: {} is not a socket; not mounting Wayland", sock.display())
            }
            Some(sock) => match crate::wayland::mount_arg(&sock) {
                Ok(m) => {
                    crate::debug!("mounting {} at {}", sock.display(), crate::wayland::CONTAINER_SOCKET);
                    dc_args.push(m);
                }
                Err(e) => eprintln!("dctether: {e}; not mounting Wayland"),
            },
        }
    }
    let dc = std::env::var("DCTETHER_DEVCONTAINER").unwrap_or_else(|_| "devcontainer".into());
    match std::process::Command::new(&dc).arg("up").args(&dc_args).status() {
        Ok(s) => exit_code(s),
        Err(e) => {
            eprintln!("dctether: {dc}: {e}");
            1
        }
    }
}

fn is_socket(p: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(p).is_ok_and(|m| m.file_type().is_socket())
}

type ConfigTask = tokio::task::JoinHandle<io::Result<std::process::Output>>;

/// Make the container's port forwarder forward `forwardPorts` plus `-p`
/// ports, and report what happened. Problems are reported, not fatal: the
/// session itself still works.
async fn forward_ports(
    a: &ExecArgs,
    config: ConfigTask,
    netns: Option<&str>,
    container_id: Option<&str>,
) -> Option<UnixStream> {
    let mut ports = vec![];
    match config.await {
        Ok(Ok(o)) if o.status.success() => match crate::forward::config_ports(&String::from_utf8_lossy(&o.stdout)) {
            Ok((p, skipped)) => {
                ports = p;
                for s in skipped {
                    eprintln!("dctether: can't forward {s:?} from forwardPorts (only port numbers are supported)");
                }
            }
            Err(e) => eprintln!("dctether: {e}"),
        },
        Ok(Ok(o)) => crate::debug!("read-configuration failed ({}); using only -p ports", o.status),
        Ok(Err(e)) => crate::debug!("read-configuration: {e}; using only -p ports"),
        Err(e) => crate::debug!("read-configuration: {e}"),
    }
    for p in &a.ports {
        if !ports.contains(p) {
            ports.push(*p);
        }
    }
    if ports.is_empty() {
        return None;
    }
    // A container in our network namespace already shares our ports;
    // listening on them here would only take them away from it.
    if netns.is_some() && netns == host_netns().as_deref() {
        let msg = "the container shares the host's network, so its ports are reachable directly; not forwarding";
        if a.ports.is_empty() {
            crate::debug!("{msg}");
        } else {
            eprintln!("dctether: {msg}");
        }
        return None;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let key = crate::forward::forwarder_key(container_id, &cwd, &a.dc_args);
    match crate::forward::attach(&key, &a.dc_args, &ports).await {
        Ok((stream, results)) => {
            for r in results {
                match r {
                    crate::forward::PortResult::New(p) => eprintln!("dctether: forwarding 127.0.0.1:{p} to the container"),
                    crate::forward::PortResult::Active(p) => crate::debug!("port {p} already forwarded"),
                    crate::forward::PortResult::Failed(p, e) => eprintln!("dctether: can't forward port {p}: {e}"),
                }
            }
            Some(stream)
        }
        Err(e) => {
            eprintln!("dctether: {e}");
            None
        }
    }
}

/// This process's network namespace, as the agent reports the container's.
fn host_netns() -> Option<String> {
    if cfg!(target_os = "linux") {
        std::fs::read_link("/proc/self/ns/net").ok()?.to_str().map(str::to_string)
    } else {
        None
    }
}

pub(crate) async fn send(out: &Out, f: Frame) -> Result<(), String> {
    out.send(f)
        .await
        .map_err(|_| "container agent pipe closed".to_string())
}

pub(crate) fn describe(r: io::Result<Option<Frame>>) -> String {
    match r {
        Ok(None) => "it exited".into(),
        Ok(Some(f)) => format!("unexpected {f:?}"),
        Err(e) => e.to_string(),
    }
}

fn exit_code(s: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
}

/// While the user's command runs, terminal signals reach it directly (it is
/// in our process group); we only make sure they don't kill the relay, and
/// pass on termination requests aimed at us.
fn forward_signals(pid: Option<u32>) {
    let (Ok(mut int), Ok(mut quit), Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::quit()),
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        return;
    };
    let kill = move |sig| {
        if let Some(pid) = pid {
            unsafe { libc::kill(pid as i32, sig) };
        }
    };
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = int.recv() => {}
                _ = quit.recv() => {}
                _ = term.recv() => kill(libc::SIGTERM),
                _ = hup.recv() => kill(libc::SIGHUP),
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Container binaries

pub struct AgentBinary {
    pub arch: &'static str,
    pub bytes: Vec<u8>,
    /// Content hash prefix; part of the cache directory name, so a changed
    /// build is never mistaken for a cached one.
    pub hash: String,
}

impl AgentBinary {
    fn cache_name(&self) -> String {
        format!("{}-{}", crate::VERSION, self.hash)
    }
}

/// Static Linux builds for each container architecture, looked up in
/// `$DCTETHER_AGENT_DIR`, then `<prefix>/libexec/dctether` next to this
/// executable; a static Linux build of the same architecture also serves
/// as its own container half.
pub(crate) fn locate_binaries() -> Result<Vec<AgentBinary>, String> {
    let exe = std::env::current_exe().and_then(|p| p.canonicalize()).ok();
    let mut dirs = vec![];
    if let Some(d) = std::env::var_os("DCTETHER_AGENT_DIR") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(prefix) = exe.as_deref().and_then(Path::parent).and_then(Path::parent) {
        dirs.push(prefix.join("libexec/dctether"));
    }
    let mut out = vec![];
    for arch in ARCHES {
        let mut path = dirs
            .iter()
            .map(|d| d.join(format!("dctether-linux-{arch}")))
            .find(|p| p.is_file());
        if path.is_none() && cfg!(all(target_os = "linux", target_env = "musl")) && std::env::consts::ARCH == arch {
            path = exe.clone();
        }
        if let Some(p) = path {
            let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let digest = Sha256::digest(&bytes);
            let hash = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
            crate::debug!("container binary for {arch}: {}", p.display());
            out.push(AgentBinary { arch, bytes, hash });
        }
    }
    if out.is_empty() {
        return Err("no static Linux build of dctether found for the container \
             (looked in $DCTETHER_AGENT_DIR and <prefix>/libexec/dctether)"
            .into());
    }
    Ok(out)
}

/// Shell snippet resolving the cache directory inside the container.
const CACHE_DIR: &str = r#""${XDG_CACHE_HOME:-$HOME/.cache}/dctether""#;

/// One `sh -c` that detects the architecture and starts the cached agent,
/// or reports that it is missing. Saves a separate probe round trip.
pub fn boot_script(bins: &[(&str, String)], debug: bool) -> String {
    let mut s = format!(
        "c={CACHE_DIR}\nm=$(uname -m)\ncase \"$m\" in arm64) m=aarch64;; esac\nb=\ncase \"$m\" in\n"
    );
    for (arch, name) in bins {
        s += &format!("  {arch}) b=\"$c/{name}/dctether\";;\n");
    }
    s += "esac\n";
    let flag = if debug { " --debug" } else { "" };
    s += &format!(
        "if [ -n \"$b\" ] && [ -x \"$b\" ]; then echo 'dctether-boot ok'; exec \"$b\" agent{flag}; fi\n\
         echo \"dctether-boot missing $m\"\n"
    );
    s
}

/// Install the binary arriving on stdin. Written under a temporary name and
/// renamed only once its size is verified, so concurrent or interrupted
/// uploads never leave a broken `dctether` behind.
pub fn upload_script(name: &str, size: usize) -> String {
    format!(
        "set -e\n\
         d={CACHE_DIR}/{name}\n\
         mkdir -p \"$d\"\n\
         t=\"$d/.upload.$$\"\n\
         trap 'rm -f \"$t\"' EXIT\n\
         cat > \"$t\"\n\
         [ \"$(wc -c < \"$t\" | tr -d ' ')\" = {size} ] || {{ echo 'dctether: upload was truncated' >&2; exit 1; }}\n\
         chmod 755 \"$t\"\n\
         ln -sf dctether \"$d/dctether-browser\"\n\
         ln -sf dctether \"$d/dctether-git-credential\"\n\
         mv -f \"$t\" \"$d/dctether\"\n"
    )
}

pub(crate) type AgentReader = FrameReader<BufReader<ChildStdout>>;

pub(crate) async fn start_agent(
    dc: &str,
    dc_args: &[String],
    bins: &[AgentBinary],
) -> Result<(Child, AgentReader, tokio::process::ChildStdin), String> {
    let names: Vec<_> = bins.iter().map(|b| (b.arch, b.cache_name())).collect();
    let script = boot_script(&names, crate::debug_enabled());
    for attempt in 0..2 {
        let mut child = Command::new(dc)
            .arg("exec")
            .args(dc_args)
            .args(["sh", "-c", &script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Keep terminal signals (Ctrl-C meant for the user's command)
            // away from the relay.
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("{dc}: {e}"))?;
        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let line = read_boot_line(&mut stdout)
            .await
            .map_err(|e| format!("reading from container: {e}"))?;
        match line.as_deref() {
            Some("ok") => return Ok((child, FrameReader::new(stdout), stdin)),
            Some(l) if l.starts_with("missing ") && attempt == 0 => {
                let arch = &l["missing ".len()..];
                drop(stdin);
                let _ = child.wait().await;
                let bin = bins
                    .iter()
                    .find(|b| b.arch == arch)
                    .ok_or_else(|| format!("no dctether build for container architecture {arch:?}"))?;
                upload(dc, dc_args, bin).await?;
            }
            _ => {
                let status = child.wait().await.map_err(|e| e.to_string())?;
                return Err(format!("could not start the container agent ({status})"));
            }
        }
    }
    Err("container agent still missing after upload".into())
}

/// The first line starting with `dctether-boot `, without that prefix.
async fn read_boot_line<R: AsyncRead + Unpin>(r: &mut BufReader<R>) -> io::Result<Option<String>> {
    let mut limited = r.take(64 * 1024);
    let mut line = vec![];
    loop {
        line.clear();
        if limited.read_until(b'\n', &mut line).await? == 0 {
            return Ok(None);
        }
        if let Some(rest) = line.strip_prefix(b"dctether-boot ") {
            return Ok(Some(String::from_utf8_lossy(rest).trim_end().to_string()));
        }
    }
}

async fn upload(dc: &str, dc_args: &[String], bin: &AgentBinary) -> Result<(), String> {
    crate::debug!("copying {} binary ({} bytes) into the container", bin.arch, bin.bytes.len());
    let mut child = Command::new(dc)
        .arg("exec")
        .args(dc_args)
        .args(["sh", "-c", &upload_script(&bin.cache_name(), bin.bytes.len())])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{dc}: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let w = stdin.write_all(&bin.bytes).await;
    drop(stdin);
    let status = child.wait().await.map_err(|e| e.to_string())?;
    match (w, status.success()) {
        (Ok(()), true) => Ok(()),
        (Err(e), _) => Err(format!("copying dctether into the container: {e}")),
        (_, false) => Err(format!("copying dctether into the container failed ({status})")),
    }
}

// ---------------------------------------------------------------------------
// Host-side endpoints

struct HostX11 {
    display: Display,
    cookie: Option<XAuthCookie>,
}

impl HostX11 {
    fn from_env() -> Option<HostX11> {
        let display = xauth::parse_display(&std::env::var("DISPLAY").ok()?)?;
        let file = std::env::var_os("XAUTHORITY")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".Xauthority")));
        let entries = file.and_then(|f| std::fs::read(f).ok()).map(|b| xauth::parse(&b)).unwrap_or_default();
        let cookie = xauth::find_cookie(&entries, &hostname(), display.number()).map(|e| XAuthCookie {
            name: String::from_utf8_lossy(&e.name).into_owned(),
            data: e.data.clone(),
        });
        crate::debug!("host DISPLAY {display:?}, cookie {}", if cookie.is_some() { "found" } else { "not found" });
        Some(HostX11 { display, cookie })
    }

    /// The direct path needs the container to share our network namespace
    /// (so it sees the same abstract sockets) and the X server to listen on
    /// an abstract socket. The agent confirms with a real connection.
    #[cfg(target_os = "linux")]
    fn direct_offer(&self, container_netns: Option<&str>) -> Option<X11Direct> {
        let Display::Local { number } = self.display else {
            return None;
        };
        let ours = std::fs::read_link("/proc/self/ns/net").ok()?;
        if container_netns? != ours.to_str()? {
            return None;
        }
        connect_abstract_std(&format!("/tmp/.X11-unix/X{number}")).ok()?;
        Some(X11Direct {
            display: number,
            auth: self.cookie.clone(),
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn direct_offer(&self, _container_netns: Option<&str>) -> Option<X11Direct> {
        None
    }
}

fn hostname() -> Vec<u8> {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return vec![];
    }
    let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf[..n].to_vec()
}

#[cfg(target_os = "linux")]
fn connect_abstract_std(name: &str) -> io::Result<std::os::unix::net::UnixStream> {
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
    std::os::unix::net::UnixStream::connect_addr(&addr)
}

trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncStream for T {}

async fn connect_x11(display: &Display) -> io::Result<Box<dyn AsyncStream>> {
    match display {
        Display::Local { number } => {
            let path = format!("/tmp/.X11-unix/X{number}");
            #[cfg(target_os = "linux")]
            if let Ok(s) = connect_abstract_std(&path) {
                s.set_nonblocking(true)?;
                return Ok(Box::new(UnixStream::from_std(s)?));
            }
            Ok(Box::new(UnixStream::connect(path).await?))
        }
        Display::Path { path, .. } => Ok(Box::new(UnixStream::connect(path).await?)),
        Display::Tcp { host, number } => {
            let port = u16::try_from(6000 + number).map_err(|_| io::Error::other("display number too large"))?;
            Ok(Box::new(tokio::net::TcpStream::connect((host.as_str(), port)).await?))
        }
    }
}

struct X11Relay {
    display: Display,
    fake: (Vec<u8>, Vec<u8>),
    real: (Vec<u8>, Vec<u8>),
}

pub(crate) struct HostCtx {
    ssh_sock: Option<PathBuf>,
    /// Pass on only the agent requests in `sshagent::allowed`.
    ssh_filter: bool,
    relay: Option<X11Relay>,
    browser: bool,
    /// `Some` when git credentials are forwarded: what this session handed
    /// out. The lock also makes requests take turns.
    git: Option<tokio::sync::Mutex<crate::gitcred::Served>>,
    /// The host gpg-agent's extra socket.
    gpg_socket: Option<PathBuf>,
}

impl HostCtx {
    /// Refuses every container-opened channel (the port forwarder's agent
    /// only carries host-opened ones).
    pub(crate) fn none() -> HostCtx {
        HostCtx {
            ssh_sock: None,
            ssh_filter: true,
            relay: None,
            browser: false,
            git: None,
            gpg_socket: None,
        }
    }
}

/// Handle frames from the agent until it goes away.
pub(crate) async fn host_read_loop(mut reader: AgentReader, reg: Arc<Registry>, out: Out, ctx: Arc<HostCtx>) -> String {
    loop {
        let frame = match reader.next().await {
            Ok(Some(f)) => f,
            Ok(None) => return "container agent exited".into(),
            Err(e) => return format!("container agent: {e}"),
        };
        match frame {
            Frame::Open { channel, kind } => open_channel(channel, kind, &reg, &out, &ctx).await,
            f => {
                if !reg.dispatch(f).await {
                    return "unexpected control frame from container agent".into();
                }
            }
        }
    }
}

async fn open_channel(id: u32, kind: ChannelKind, reg: &Arc<Registry>, out: &Out, ctx: &Arc<HostCtx>) {
    // Forwarded ports are opened by the host only, and host-chosen ids are
    // the host's: the container may use neither.
    let allowed = id & HOST_CHANNEL_BIT == 0
        && match kind {
            ChannelKind::SshAgent => ctx.ssh_sock.is_some(),
            ChannelKind::X11 => ctx.relay.is_some(),
            ChannelKind::Browser => ctx.browser,
            ChannelKind::GitCredential => ctx.git.is_some(),
            ChannelKind::GpgAgent => ctx.gpg_socket.is_some(),
            ChannelKind::Tcp(_) => false,
        };
    if reg.contains(id) {
        crate::debug!("container reused open channel id {id}; ignoring");
        return;
    }
    let inbox = match reg.insert(id) {
        Some(inbox) if allowed => inbox,
        _ => {
            reg.remove(id);
            let _ = out.send(Frame::Close { channel: id }).await;
            return;
        }
    };
    let (reg, out, ctx) = (reg.clone(), out.clone(), ctx.clone());
    tokio::spawn(async move {
        let fail = |e: io::Error| {
            crate::debug!("channel {id} ({kind:?}): {e}");
            reg.remove(id);
            let out = out.clone();
            async move {
                let _ = out.send(Frame::Close { channel: id }).await;
            }
        };
        match kind {
            ChannelKind::SshAgent => match UnixStream::connect(ctx.ssh_sock.as_ref().unwrap()).await {
                Ok(s) if ctx.ssh_filter => ssh_agent_channel(id, s, inbox, out.clone(), reg.clone()).await,
                Ok(s) => mux::pump(id, s, inbox, out.clone(), reg.clone(), None).await,
                Err(e) => fail(e).await,
            },
            ChannelKind::X11 => {
                let relay = ctx.relay.as_ref().unwrap();
                match connect_x11(&relay.display).await {
                    Ok(s) => {
                        let filter = ClientFilter::new(relay.fake.clone(), relay.real.clone());
                        mux::pump(id, s, inbox, out.clone(), reg.clone(), Some(filter)).await
                    }
                    Err(e) => fail(e).await,
                }
            }
            ChannelKind::Browser => browser_channel(id, inbox, out.clone(), reg.clone()).await,
            ChannelKind::GitCredential => git_credential_channel(id, inbox, out.clone(), reg.clone(), &ctx).await,
            ChannelKind::GpgAgent => match UnixStream::connect(ctx.gpg_socket.as_ref().unwrap()).await {
                Ok(s) => mux::pump(id, s, inbox, out.clone(), reg.clone(), None).await,
                Err(e) => fail(e).await,
            },
            ChannelKind::Tcp(_) => unreachable!("refused above"),
        }
    });
}

/// Send `bytes` on a channel, in data frames no larger than the protocol
/// allows. False when the pipe to the agent is gone.
async fn send_data(out: &Out, id: u32, bytes: &[u8]) -> bool {
    for chunk in bytes.chunks(DATA_CHUNK) {
        let data = chunk.to_vec();
        if out.send(Frame::Data { channel: id, data }).await.is_err() {
            return false;
        }
    }
    true
}

/// Relay SSH agent requests one at a time. A request the container may
/// make (`sshagent::allowed`) goes to the agent and its one reply comes
/// back. Any other is answered with a failure here and never reaches the
/// agent.
async fn ssh_agent_channel(
    id: u32,
    mut agent: UnixStream,
    mut inbox: mpsc::Receiver<ChanMsg>,
    out: Out,
    reg: Arc<Registry>,
) {
    let mut requests = crate::sshagent::Requests::default();
    let clean = 'channel: loop {
        loop {
            let result = match requests.take() {
                Ok(None) => break,
                Ok(Some(request)) if crate::sshagent::allowed(&request) => {
                    relay_agent_request(&mut agent, &request, id, &out).await
                }
                Ok(Some(request)) => {
                    crate::debug!("channel {id}: refused SSH agent request of type {}", request[4]);
                    send_data(&out, id, &crate::sshagent::FAILURE_REPLY)
                        .await
                        .then_some(())
                        .ok_or_else(|| io::ErrorKind::BrokenPipe.into())
                }
                Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
            };
            if let Err(e) = result {
                crate::debug!("channel {id} (SSH agent): {e}");
                break 'channel false;
            }
        }
        match inbox.recv().await {
            Some(ChanMsg::Data(d)) => requests.push(&d),
            // A clean end, unless it cuts a request short.
            Some(ChanMsg::Eof) => break requests.is_empty(),
            // Closed by the container.
            None => return,
        }
    };
    reg.remove(id);
    let end = if clean { Frame::Eof { channel: id } } else { Frame::Close { channel: id } };
    let _ = out.send(end).await;
}

/// Send one request to the agent and pass its one reply back.
async fn relay_agent_request(agent: &mut UnixStream, request: &[u8], id: u32, out: &Out) -> io::Result<()> {
    let gone = || io::Error::from(io::ErrorKind::BrokenPipe);
    agent.write_all(request).await?;
    let mut header = [0u8; 4];
    agent.read_exact(&mut header).await?;
    if !send_data(out, id, &header).await {
        return Err(gone());
    }
    let mut left = u32::from_be_bytes(header) as usize;
    let mut buf = vec![0u8; DATA_CHUNK.min(left)];
    while left > 0 {
        let want = left.min(buf.len());
        let n = agent.read(&mut buf[..want]).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if !send_data(out, id, &buf[..n]).await {
            return Err(gone());
        }
        left -= n;
    }
    Ok(())
}

/// Answer one request of the container's git credential helper with the
/// host's `git credential fill|approve|reject`. Terminal prompts are off
/// (they would garble the session), so credentials come from the host's
/// helpers or its askpass program.
async fn git_credential_channel(
    id: u32,
    mut inbox: mpsc::Receiver<ChanMsg>,
    out: Out,
    reg: Arc<Registry>,
    ctx: &HostCtx,
) {
    let mut req = vec![];
    let mut too_long = false;
    loop {
        match inbox.recv().await {
            Some(ChanMsg::Data(d)) => {
                if req.len() + d.len() > crate::gitcred::MAX_REQUEST {
                    too_long = true;
                } else {
                    req.extend_from_slice(&d);
                }
            }
            Some(ChanMsg::Eof) => break,
            None => return,
        }
    }
    let parsed = if too_long {
        Err("request too large".to_string())
    } else {
        String::from_utf8(req)
            .map_err(|_| "request is not valid UTF-8".to_string())
            .and_then(|r| crate::gitcred::parse_request(&r))
    };
    let reply = match parsed {
        Err(e) => format!("error: {e}\n"),
        Ok((action, attrs)) => {
            // One request at a time: each can raise a password dialog on
            // the host.
            let mut served = ctx.git.as_ref().expect("checked when the channel opened").lock().await;
            answer_git_credential(action, &attrs, &mut served).await
        }
    };
    send_data(&out, id, reply.as_bytes()).await;
    let _ = out.send(Frame::Eof { channel: id }).await;
    reg.remove(id);
}

/// `get` asks the host's git and remembers what it answered. `store` and
/// `erase` only select one of those remembered answers, which is what the
/// host's git is then given: the container can't make the host store a
/// credential of its own, or erase one it was never handed. Anything else
/// is ignored without an error, as git calls `store` after every login
/// that worked, whoever supplied the credential.
async fn answer_git_credential(
    action: crate::gitcred::Action,
    attrs: &str,
    served: &mut crate::gitcred::Served,
) -> String {
    use crate::gitcred::Action;
    let result = match action {
        Action::Get => host_git_credential(action, attrs).await.inspect(|answer| served.remember(answer)),
        Action::Store | Action::Erase => match served.matching(attrs) {
            Some(own) => {
                if action == Action::Erase {
                    served.forget(attrs);
                }
                host_git_credential(action, &own).await.map(|_| String::new())
            }
            None => {
                crate::debug!(
                    "git credential {}: not a credential this session got from the host; ignored",
                    action.git_subcommand()
                );
                Ok(String::new())
            }
        },
    };
    match result {
        Ok(answer) => format!("ok\n{answer}"),
        Err(e) => format!("error: {e}\n"),
    }
}

/// The host's `git credential SUB`. It runs in `/`, not in our working
/// directory: that is usually the workspace, which is mounted into the
/// container, and git would read the `.git/config` there. With
/// `credential.helper = !cmd` in it, the container could make the host run
/// a command.
fn git_credential_command(sub: &str) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["credential", sub])
        .current_dir("/")
        .env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

async fn host_git_credential(action: crate::gitcred::Action, attrs: &str) -> Result<String, String> {
    let sub = action.git_subcommand();
    let mut child = git_credential_command(sub)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(attrs.as_bytes()).await;
    drop(stdin);
    // Long enough for a password dialog on the host.
    let o = tokio::time::timeout(Duration::from_secs(300), child.wait_with_output())
        .await
        .map_err(|_| format!("git credential {sub} timed out"))?
        .map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err(format!("git credential {sub} found no credential"));
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// What gpg forwarding needs from the host.
struct HostGpg {
    extra_socket: PathBuf,
    public_keys: Vec<u8>,
}

/// The host gpg-agent's extra socket (the one meant for remote use, with
/// restricted commands) and the public parts of the host's secret keys.
/// `None` when gpg isn't installed or there are no secret keys.
fn host_gpg() -> Option<HostGpg> {
    let run = |cmd: &str, args: &[&str]| -> Option<Vec<u8>> {
        let o = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        o.status.success().then_some(o.stdout)
    };
    let socket = run("gpgconf", &["--list-dirs", "agent-extra-socket"])?;
    let extra_socket = PathBuf::from(String::from_utf8_lossy(&socket).trim());
    // Starts the agent if it isn't running, so the socket exists.
    run("gpgconf", &["--launch", "gpg-agent"])?;
    let listing = run("gpg", &["--batch", "--with-colons", "--list-secret-keys"])?;
    let fprs = secret_key_fingerprints(&String::from_utf8_lossy(&listing));
    if fprs.is_empty() {
        crate::debug!("no gpg secret keys on the host; not forwarding gpg");
        return None;
    }
    let mut args = vec!["--batch", "--export"];
    args.extend(fprs.iter().map(String::as_str));
    let public_keys = run("gpg", &args)?;
    if public_keys.is_empty() || public_keys.len() > 256 * 1024 {
        crate::debug!("unexpected gpg export size {}; not forwarding gpg", public_keys.len());
        return None;
    }
    crate::debug!("gpg: {} secret key(s), agent at {}", fprs.len(), extra_socket.display());
    Some(HostGpg {
        extra_socket,
        public_keys,
    })
}

/// Fingerprints of the primary keys in `gpg --with-colons --list-secret-keys`
/// output: the first `fpr` record after each `sec` record.
pub fn secret_key_fingerprints(colons: &str) -> Vec<String> {
    let mut out = vec![];
    let mut want = false;
    for line in colons.lines() {
        let f: Vec<&str> = line.split(':').collect();
        match f.first() {
            Some(&"sec") => want = true,
            Some(&"fpr") if want => {
                if let Some(fpr) = f.get(9).filter(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_hexdigit())) {
                    out.push(fpr.to_string());
                }
                want = false;
            }
            _ => {}
        }
    }
    out
}

fn which(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
}

/// Read one URL (until EOF), validate it, open it, and answer `ok` or
/// `error: …`.
async fn browser_channel(id: u32, mut inbox: mpsc::Receiver<ChanMsg>, out: Out, reg: Arc<Registry>) {
    let mut url = vec![];
    let mut too_long = false;
    loop {
        match inbox.recv().await {
            Some(ChanMsg::Data(d)) => {
                if url.len() + d.len() > browser::MAX_URL {
                    too_long = true;
                } else {
                    url.extend_from_slice(&d);
                }
            }
            Some(ChanMsg::Eof) => break,
            None => return,
        }
    }
    let reply = if too_long {
        "error: URL too long".to_string()
    } else {
        match String::from_utf8(url) {
            Err(_) => "error: URL is not valid UTF-8".to_string(),
            Ok(u) => match browser::validate_url(&u) {
                Err(e) => format!("error: {e}"),
                Ok(()) => match open_url(&u) {
                    Ok(()) => "ok".to_string(),
                    Err(e) => format!("error: {}: {e}", browser::opener()),
                },
            },
        }
    };
    send_data(&out, id, format!("{reply}\n").as_bytes()).await;
    let _ = out.send(Frame::Eof { channel: id }).await;
    reg.remove(id);
}

/// Hand a validated URL to the desktop's opener as a single argument (no
/// shell involved).
fn open_url(url: &str) -> io::Result<()> {
    let mut child = Command::new(browser::opener())
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_exec_args() {
        let a = parse_exec_args(&s(&[
            "--workspace-folder",
            "/w",
            "--no-x11",
            "--remote-env",
            "A=B",
            "--",
            "git",
            "--no-x11",
        ])).unwrap();
        assert_eq!(a.dc_args, s(&["--workspace-folder", "/w", "--remote-env", "A=B"]));
        assert_eq!(a.cmd, s(&["git", "--no-x11"]));
        assert!(!a.x11 && a.ssh_agent && a.browser && !a.force_relay);

        let a = parse_exec_args(&s(&["--x11-relay", "--no-browser", "--no-ssh-agent"])).unwrap();
        assert!(a.dc_args.is_empty() && a.cmd.is_empty());
        assert!(a.x11 && a.force_relay && !a.browser && !a.ssh_agent && a.wayland);
        assert!(!a.ssh_agent_unfiltered);
        let a = parse_exec_args(&s(&["--ssh-agent-unfiltered", "--", "--ssh-agent-unfiltered"])).unwrap();
        assert!(a.ssh_agent && a.ssh_agent_unfiltered && a.dc_args.is_empty());

        let a = parse_exec_args(&s(&["--no-wayland", "--", "--no-wayland"])).unwrap();
        assert!(!a.wayland && a.dc_args.is_empty());
        assert_eq!(a.cmd, s(&["--no-wayland"]));

        let a = parse_exec_args(&s(&["-p", "3000", "--workspace-folder", "/w", "-p", "8080"])).unwrap();
        assert_eq!(a.ports, vec![3000, 8080]);
        assert_eq!(a.dc_args, s(&["--workspace-folder", "/w"]));
        assert!(a.forward);
        assert!(!parse_exec_args(&s(&["--no-forward"])).unwrap().forward);
        let a = parse_exec_args(&s(&["--no-gpg", "--no-git-credentials"])).unwrap();
        assert!(!a.gpg && !a.git_credentials && a.dc_args.is_empty());
        assert!(parse_exec_args(&s(&["-p"])).is_err());
        assert!(parse_exec_args(&s(&["-p", "0"])).is_err());
        assert!(parse_exec_args(&s(&["-p", "http"])).is_err());
    }

    #[test]
    fn validates_agent_env() {
        let ok = |k: &str, v: &str| check_env(&[(k.to_string(), v.to_string())]).is_ok();
        assert!(ok("SSH_AUTH_SOCK", "/tmp/dctether-0123/agent.sock"));
        assert!(ok("DISPLAY", ":0"));
        assert!(ok("DISPLAY", ":10"));
        assert!(ok("XAUTHORITY", "/tmp/d/Xauthority"));
        assert!(ok("BROWSER", "/home/u/.cache/dctether/0.1.0-ab/dctether-browser"));
        assert!(ok("DCTETHER_SESSION", "/tmp/d"));
        assert!(ok("WAYLAND_DISPLAY", "/tmp/dctether-wayland.sock"));
        assert!(ok("GNUPGHOME", "/tmp/dctether-0123/gnupg"));
        assert!(ok("GIT_CONFIG_COUNT", "1"));
        assert!(ok("GIT_CONFIG_KEY_0", "credential.helper"));
        assert!(ok("GIT_CONFIG_VALUE_0", "/home/u/.cache/dctether/v/dctether-git-credential"));
        assert!(!ok("GIT_CONFIG_COUNT", "2"));
        assert!(!ok("GIT_CONFIG_KEY_0", "core.sshCommand"));
        assert!(!ok("GIT_CONFIG_VALUE_0", "!sh -c id"));
        assert!(!ok("WAYLAND_DISPLAY", "wayland-0"));
        assert!(!ok("LD_PRELOAD", "/tmp/x.so"));
        assert!(!ok("--docker-path", "/x"));
        assert!(!ok("SSH_AUTH_SOCK", "--docker-path=/x"));
        assert!(!ok("SSH_AUTH_SOCK", "relative/path"));
        assert!(!ok("BROWSER", "/bin/x\n--docker-path=/y"));
        assert!(!ok("DISPLAY", "host:0"));
        assert!(!ok("DISPLAY", ":"));
        assert!(!ok("DISPLAY", ":0.0"));
        assert!(!ok("DISPLAY", "--x"));
        assert!(is_safe_path("/bin/bash") && !is_safe_path("--docker-path=/x") && !is_safe_path("bash"));
    }

    /// The workspace is writable by the container, so the host's git must
    /// not pick up its repository configuration.
    #[test]
    fn host_git_credential_runs_outside_the_workspace() {
        for sub in ["fill", "approve", "reject"] {
            let cmd = git_credential_command(sub);
            assert_eq!(cmd.as_std().get_current_dir(), Some(Path::new("/")));
        }
    }

    #[test]
    fn finds_secret_key_fingerprints() {
        let listing = "sec:u:255:22:AAAA:1700000000::::::scESC:::+:::ed25519:::0:\n\
                       fpr:::::::::0123456789ABCDEF0123456789ABCDEF01234567:\n\
                       grp:::::::::1111111111111111111111111111111111111111:\n\
                       uid:u::::1700000000::HASH::Me <me@example.com>::::::::::0:\n\
                       ssb:u:255:18:BBBB:1700000000::::::e:::+:::cv25519::\n\
                       fpr:::::::::FEDCBA9876543210FEDCBA9876543210FEDCBA98:\n\
                       sec:e:3072:1:CCCC:1500000000:1600000000:::::sc:::+:::::0:\n\
                       fpr:::::::::AAAABBBBCCCCDDDDEEEEFFFF0000111122223333:\n";
        assert_eq!(
            secret_key_fingerprints(listing),
            vec![
                "0123456789ABCDEF0123456789ABCDEF01234567".to_string(),
                "AAAABBBBCCCCDDDDEEEEFFFF0000111122223333".to_string(),
            ]
        );
        assert!(secret_key_fingerprints("").is_empty());
        assert!(secret_key_fingerprints("sec:u:::\nfpr:::::::::--batch:\n").is_empty());
    }

    #[test]
    fn boot_script_lists_known_arches() {
        let s = boot_script(&[("x86_64", "0.1.0-abcd".into())], false);
        assert!(s.contains("x86_64) b=\"$c/0.1.0-abcd/dctether\";;"));
        assert!(!s.contains("aarch64)"));
        assert!(s.contains("exec \"$b\" agent; fi"));
        assert!(boot_script(&[], true).contains("agent --debug"));
    }

    /// Run the generated scripts with a real `sh` against a scratch HOME.
    #[test]
    fn boot_and_upload_scripts_work_in_sh() {
        use std::process::{Command, Stdio};
        let home = std::env::temp_dir().join(format!("dctether-test-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let arch = std::process::Command::new("uname").arg("-m").output().unwrap();
        let arch = String::from_utf8(arch.stdout).unwrap().trim().to_string();
        // The script normalizes macOS-style names, as a container could report them.
        let arch = if arch == "arm64" { "aarch64".to_string() } else { arch };
        let name = "0.0.0-test".to_string();
        let sh = |script: &str, input: &[u8]| {
            let mut c = Command::new("sh")
                .args(["-c", script])
                .env("HOME", &home)
                .env_remove("XDG_CACHE_HOME")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            c.stdin.take().unwrap().write_all(input).unwrap();
            c.wait_with_output().unwrap()
        };
        let boot = boot_script(&[(arch.as_str(), name.clone())], false);
        let o = sh(&boot, b"");
        assert_eq!(String::from_utf8_lossy(&o.stdout), format!("dctether-boot missing {arch}\n"));

        // A fake "binary" that answers like the agent would.
        let fake = b"#!/bin/sh\necho \"agent $1\"\n";
        let o = sh(&upload_script(&name, fake.len() + 1), fake);
        assert!(!o.status.success(), "size mismatch must fail");
        assert!(!home.join(".cache/dctether").join(&name).join("dctether").exists());

        let o = sh(&upload_script(&name, fake.len()), fake);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let dir = home.join(".cache/dctether").join(&name);
        assert_eq!(std::fs::read_link(dir.join("dctether-browser")).unwrap(), Path::new("dctether"));
        assert_eq!(std::fs::read_link(dir.join("dctether-git-credential")).unwrap(), Path::new("dctether"));

        let o = sh(&boot, b"");
        assert_eq!(String::from_utf8_lossy(&o.stdout), "dctether-boot ok\nagent agent\n");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn boot_line_skips_noise() {
        let input: &[u8] = b"motd\nmore\ndctether-boot missing aarch64\n\x01rest";
        let mut r = BufReader::new(input);
        assert_eq!(read_boot_line(&mut r).await.unwrap().as_deref(), Some("missing aarch64"));
        let mut rest = vec![];
        r.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"\x01rest");
        let mut r = BufReader::new(&b"nothing here\n"[..]);
        assert_eq!(read_boot_line(&mut r).await.unwrap(), None);
    }
}
