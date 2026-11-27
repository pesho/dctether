//! Container half: `dctether agent`, started by the host over
//! `devcontainer exec` with the frame protocol on stdin/stdout.
//!
//! It creates a private session directory, listens on Unix sockets in it,
//! and turns every accepted connection into a channel to the host. It never
//! listens on TCP or abstract sockets: under `--network=host` those would be
//! reachable by every process on the host.

use std::io::{self, Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::mux::{self, FrameReader, Out, Registry};
use crate::proto::{
    ChannelKind, Control, Frame, PROTOCOL_VERSION, WaylandStatus, X11Direct, X11Offer, X11Ready, XAuthCookie,
};
use crate::xauth;

const X11_DIR: &str = "/tmp/.X11-unix";
/// Display numbers tried for the relayed display.
const RELAY_DISPLAYS: std::ops::RangeInclusive<u32> = 10..=99;

pub async fn main() -> i32 {
    // Everything the agent creates is private to the container user.
    unsafe { libc::umask(0o077) };

    let (out, out_rx) = mpsc::channel::<Frame>(64);
    let writer = tokio::spawn(mux::write_frames(tokio::io::stdout(), out_rx));
    let mut reader = FrameReader::new(tokio::io::stdin());

    let hello = Control::Hello {
        protocol: PROTOCOL_VERSION,
        version: crate::VERSION.to_string(),
        netns: std::fs::read_link("/proc/self/ns/net")
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
        shell: login_shell(),
    };
    let _ = out.send(Frame::Control(hello)).await;

    let setup = match reader.next().await {
        Ok(Some(Frame::Control(Control::Setup {
            ssh_agent,
            browser,
            x11,
            wayland,
        }))) => (ssh_agent, browser, x11, wayland),
        Ok(None) => return 0,
        other => {
            eprintln!("dctether agent: expected setup, got {other:?}");
            return 1;
        }
    };

    let mut session = match Session::create(setup.0, setup.1, setup.2, setup.3).await {
        Ok(s) => s,
        Err(e) => {
            let _ = out
                .send(Frame::Control(Control::Error {
                    message: format!("container setup failed: {e}"),
                }))
                .await;
            drop(out);
            let _ = writer.await;
            return 1;
        }
    };
    let _ = out
        .send(Frame::Control(Control::Ready {
            env: session.env.clone(),
            x11: session.x11.clone(),
            wayland: session.wayland,
        }))
        .await;

    let reg = Arc::new(Registry::default());
    let next_id = Arc::new(AtomicU32::new(1));
    for (listener, kind) in std::mem::take(&mut session.listeners) {
        tokio::spawn(accept_loop(listener, kind, out.clone(), reg.clone(), next_id.clone()));
    }

    let mut term = signal(SignalKind::terminate()).expect("signal handler");
    let mut hup = signal(SignalKind::hangup()).expect("signal handler");
    let mut int = signal(SignalKind::interrupt()).expect("signal handler");
    let status = tokio::select! {
        s = read_loop(&mut reader, &reg) => s,
        _ = term.recv() => 0,
        _ = hup.recv() => 0,
        _ = int.recv() => 0,
    };
    drop(session); // removes the session directory and relay socket
    status
}

async fn read_loop<R: tokio::io::AsyncRead + Unpin>(reader: &mut FrameReader<R>, reg: &Registry) -> i32 {
    loop {
        match reader.next().await {
            Ok(Some(f)) => {
                if !reg.dispatch(f).await {
                    eprintln!("dctether agent: unexpected frame from host");
                    return 1;
                }
            }
            // The host went away: the session is over.
            Ok(None) => return 0,
            Err(e) => {
                eprintln!("dctether agent: {e}");
                return 1;
            }
        }
    }
}

async fn accept_loop(listener: UnixListener, kind: ChannelKind, out: Out, reg: Arc<Registry>, next_id: Arc<AtomicU32>) {
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                eprintln!("dctether agent: accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let id = loop {
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            if id != 0 && !reg.contains(id) {
                break id;
            }
        };
        let Some(inbox) = reg.insert(id) else {
            // Too many channels; refuse this connection.
            continue;
        };
        if out.send(Frame::Open { channel: id, kind }).await.is_err() {
            return;
        }
        tokio::spawn(mux::pump(id, stream, inbox, out.clone(), reg.clone(), None));
    }
}

/// Per-session state in the container. Dropping it removes its files.
struct Session {
    dir: PathBuf,
    x11_socket: Option<PathBuf>,
    listeners: Vec<(UnixListener, ChannelKind)>,
    env: Vec<(String, String)>,
    x11: Option<X11Ready>,
    wayland: Option<WaylandStatus>,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(p) = &self.x11_socket {
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Session {
    async fn create(ssh_agent: bool, browser: bool, x11: Option<X11Offer>, wayland: bool) -> io::Result<Session> {
        let dir = std::env::temp_dir().join(format!("dctether-{}", hex(&random_bytes(8)?)));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let mut s = Session {
            dir,
            x11_socket: None,
            listeners: vec![],
            env: vec![],
            x11: None,
            wayland: None,
        };
        let listen = |s: &mut Session, name: &str, kind| -> io::Result<PathBuf> {
            let path = s.dir.join(name);
            let l = UnixListener::bind(&path)?;
            s.listeners.push((l, kind));
            Ok(path)
        };

        if ssh_agent {
            let p = listen(&mut s, "agent.sock", ChannelKind::SshAgent)?;
            s.set_env("SSH_AUTH_SOCK", &p);
        }

        if browser {
            let helper = std::env::current_exe()?.with_file_name("dctether-browser");
            if helper.exists() {
                listen(&mut s, "browser.sock", ChannelKind::Browser)?;
                s.set_env("BROWSER", &helper);
                let dir = s.dir.clone();
                s.set_env("DCTETHER_SESSION", &dir);
            } else {
                eprintln!("dctether agent: {} is missing; not setting BROWSER", helper.display());
            }
        }

        if let Some(offer) = x11 {
            s.setup_x11(offer)?;
        }

        if wayland {
            let (status, path) = pick_wayland(&crate::wayland::candidates(Path::new("/tmp")));
            if let Some(p) = path {
                crate::debug!("Wayland socket: {}", p.display());
                s.set_env("WAYLAND_DISPLAY", &p);
            }
            s.wayland = Some(status);
        }
        Ok(s)
    }

    fn set_env(&mut self, k: &str, v: &Path) {
        self.env.push((k.to_string(), v.to_string_lossy().into_owned()));
    }

    fn setup_x11(&mut self, offer: X11Offer) -> io::Result<()> {
        let xauthority = self.dir.join("Xauthority");

        if let Some(X11Direct { display, auth }) = offer.direct {
            match probe_x11(display, auth.as_ref()) {
                Ok(()) => {
                    let entries: Vec<_> = auth
                        .iter()
                        .map(|a| xauth::wildcard_entry(display, &a.name, &a.data))
                        .collect();
                    write_private(&xauthority, &xauth::serialize(&entries))?;
                    self.env.push(("DISPLAY".into(), format!(":{display}")));
                    self.set_env("XAUTHORITY", &xauthority);
                    self.x11 = Some(X11Ready::Direct);
                    return Ok(());
                }
                Err(e) => {
                    crate::debug!("direct X11 display :{display} not usable ({e}); relaying");
                }
            }
        }
        if !offer.relay {
            return Ok(());
        }

        ensure_x11_dir()?;
        let (display, path, listener) = bind_relay_display()?;
        self.x11_socket = Some(path);
        self.listeners.push((listener, ChannelKind::X11));
        let fake = XAuthCookie {
            name: xauth::MIT_MAGIC_COOKIE.into(),
            data: random_bytes(16)?,
        };
        write_private(
            &xauthority,
            &xauth::serialize(&[xauth::wildcard_entry(display, &fake.name, &fake.data)]),
        )?;
        self.env.push(("DISPLAY".into(), format!(":{display}")));
        self.set_env("XAUTHORITY", &xauthority);
        self.x11 = Some(X11Ready::Relay { display, fake_auth: fake });
        Ok(())
    }
}

/// Do a real X11 connection setup on the abstract socket of `display`, to
/// prove this container shares the host's X server and that the cookie
/// works. Opens no window.
fn probe_x11(display: u32, auth: Option<&XAuthCookie>) -> io::Result<()> {
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(format!("{X11_DIR}/X{display}"))?;
    let mut s = std::os::unix::net::UnixStream::connect_addr(&addr)?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    s.set_write_timeout(Some(Duration::from_secs(3)))?;
    let (name, data) = auth.map(|a| (a.name.as_bytes(), &a.data[..])).unwrap_or((b"", b""));
    let pad = |n: usize| (4 - n % 4) % 4;
    let mut req = vec![b'l', 0, 11, 0, 0, 0];
    req.extend_from_slice(&(name.len() as u16).to_le_bytes());
    req.extend_from_slice(&(data.len() as u16).to_le_bytes());
    req.extend_from_slice(&[0, 0]);
    req.extend_from_slice(name);
    req.extend(std::iter::repeat_n(0, pad(name.len())));
    req.extend_from_slice(data);
    req.extend(std::iter::repeat_n(0, pad(data.len())));
    s.write_all(&req)?;
    let mut status = [0u8; 1];
    s.read_exact(&mut status)?;
    match status[0] {
        1 => Ok(()),
        0 => Err(io::Error::other("X server refused the connection")),
        _ => Err(io::Error::other("X server requires further authentication")),
    }
}

/// Is `path` a socket someone accepts connections on? Connecting and
/// closing again is harmless for a Wayland compositor.
fn probe_wayland(path: &Path) -> WaylandStatus {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => WaylandStatus::Ready,
            Err(_) => WaylandStatus::Dead,
        },
        _ => WaylandStatus::Missing,
    }
}

/// The first live socket among `candidates`. Otherwise `Dead` if any of
/// them exists but is dead (reported to the user), else `Missing`.
fn pick_wayland(candidates: &[PathBuf]) -> (WaylandStatus, Option<PathBuf>) {
    let mut status = WaylandStatus::Missing;
    for p in candidates {
        match probe_wayland(p) {
            WaylandStatus::Ready => return (WaylandStatus::Ready, Some(p.clone())),
            WaylandStatus::Dead => status = WaylandStatus::Dead,
            WaylandStatus::Missing => {}
        }
    }
    (status, None)
}

fn ensure_x11_dir() -> io::Result<()> {
    match std::fs::create_dir(X11_DIR) {
        Ok(()) => std::fs::set_permissions(X11_DIR, std::fs::Permissions::from_mode(0o1777)),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Bind the first free display socket in [`RELAY_DISPLAYS`]. A display is
/// free when neither its abstract socket (which clients try first) nor its
/// socket file exists; `bind` itself is the exclusive claim on the file.
fn bind_relay_display() -> io::Result<(u32, PathBuf, UnixListener)> {
    for n in RELAY_DISPLAYS {
        let abstract_name = format!("{X11_DIR}/X{n}");
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(&abstract_name)?;
        if std::os::unix::net::UnixStream::connect_addr(&addr).is_ok() {
            continue;
        }
        let path = PathBuf::from(&abstract_name);
        match UnixListener::bind(&path) {
            Ok(l) => return Ok((n, path, l)),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("no free X display number in 10..=99"))
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)
}

fn random_bytes(n: usize) -> io::Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The container user's shell from /etc/passwd (`$SHELL` is not set under
/// `devcontainer exec`).
fn login_shell() -> Option<String> {
    let uid = unsafe { libc::getuid() };
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd_shell(&passwd, uid)
}

fn passwd_shell(passwd: &str, uid: u32) -> Option<String> {
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        (f.len() >= 7 && f[2].parse() == Ok(uid) && !f[6].is_empty()).then(|| f[6].to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_wayland_socket_states() {
        let dir = std::env::temp_dir().join(format!("dctether-wl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("wl.sock");
        assert_eq!(probe_wayland(&sock), WaylandStatus::Missing);
        std::fs::write(dir.join("file"), b"").unwrap();
        assert_eq!(probe_wayland(&dir.join("file")), WaylandStatus::Missing);
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert_eq!(probe_wayland(&sock), WaylandStatus::Ready);
        // A socket file nobody accepts stream connections on, like a mount
        // of a socket from an earlier login session. (A dropped listener
        // isn't reliable here: a test spawning children concurrently can
        // briefly hold a copy of its fd between fork and exec.)
        let dead = dir.join("dead.sock");
        let _dgram = std::os::unix::net::UnixDatagram::bind(&dead).unwrap();
        assert_eq!(probe_wayland(&dead), WaylandStatus::Dead);

        // Picking: the first live one wins; a dead one is reported only if
        // nothing is live.
        let missing = dir.join("missing.sock");
        assert_eq!(
            pick_wayland(&[missing.clone(), dead.clone(), sock.clone()]),
            (WaylandStatus::Ready, Some(sock.clone()))
        );
        assert_eq!(pick_wayland(&[missing.clone(), dead.clone()]), (WaylandStatus::Dead, None));
        assert_eq!(pick_wayland(&[missing]), (WaylandStatus::Missing, None));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn finds_shell_in_passwd() {
        let p = "root:x:0:0:root:/root:/bin/bash\n\
                 vscode:x:1000:1000::/home/vscode:/usr/bin/zsh\n\
                 nologin:x:1001:1001::/home/n:\n";
        assert_eq!(passwd_shell(p, 1000).as_deref(), Some("/usr/bin/zsh"));
        assert_eq!(passwd_shell(p, 0).as_deref(), Some("/bin/bash"));
        assert_eq!(passwd_shell(p, 1001), None);
        assert_eq!(passwd_shell(p, 4242), None);
    }
}
