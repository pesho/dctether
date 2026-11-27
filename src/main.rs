//! dctether: give a `devcontainer exec` shell what a VS Code Dev Containers
//! terminal gets — SSH agent, X11 display and `BROWSER` — relayed over
//! `devcontainer exec` itself.

mod browser;
mod host;
mod mux;
mod proto;
mod wayland;
mod x11;
mod xauth;

#[cfg(target_os = "linux")]
mod agent;

use std::sync::atomic::{AtomicBool, Ordering};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

static DEBUG: AtomicBool = AtomicBool::new(false);

pub fn debug_enabled() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

#[macro_export]
macro_rules! debug {
    ($($t:tt)*) => {
        if $crate::debug_enabled() {
            eprintln!("dctether: {}\r", format_args!($($t)*));
        }
    };
}

const USAGE: &str = "\
usage: dctether exec [OPTIONS] [DEVCONTAINER-EXEC-OPTIONS] [-- COMMAND [ARGS...]]
       dctether up [--no-wayland] [DEVCONTAINER-UP-OPTIONS]

exec  Run COMMAND (default: the container user's shell from /etc/passwd) with
      `devcontainer exec`, forwarding the host's SSH agent, X11 display, browser
      opener and (if the container was created by `dctether up`) Wayland into it.
up    Run `devcontainer up`, also mounting the host's Wayland socket into the
      container. Mounts are fixed at creation: an existing container is reused
      as it is (add --remove-existing-container to recreate it).

Everything that is not a dctether option (before `--`, for exec) is passed to
devcontainer unchanged, e.g. --workspace-folder, --container-id, --config.
Without them, devcontainer uses the dev container of the current directory.

exec options:
  --no-ssh-agent   don't forward $SSH_AUTH_SOCK
  --no-x11         don't forward $DISPLAY
  --no-browser     don't set BROWSER
  --no-wayland     don't set WAYLAND_DISPLAY
  --x11-relay      always relay X11, even if the host display is reachable directly

up options:
  --no-wayland     don't mount the Wayland socket

Environment:
  DCTETHER_DEVCONTAINER  devcontainer CLI to run (default: devcontainer)
  DCTETHER_AGENT_DIR     directory with dctether-linux-<arch> container builds
  DCTETHER_DEBUG=1       print diagnostics to stderr
";
fn main() {
    if std::env::var_os("DCTETHER_DEBUG").is_some_and(|v| !v.is_empty() && v != "0") {
        DEBUG.store(true, Ordering::Relaxed);
    }
    let args: Vec<String> = std::env::args().collect();
    let argv0 = args
        .first()
        .and_then(|a| std::path::Path::new(a).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");

    let code = if argv0 == "dctether-browser" {
        browser_helper(&args[1..])
    } else {
        match args.get(1).map(String::as_str) {
            Some("exec") => host::exec_main(&args[2..]),
            Some("up") => host::up_main(&args[2..]),
            Some("agent") => agent_main(&args[2..]),
            Some("browser") => browser_helper(&args[2..]),
            Some("--version" | "-V") => {
                println!("dctether {VERSION}");
                0
            }
            Some("--help" | "-h" | "help") => {
                print!("{USAGE}");
                0
            }
            _ => {
                eprint!("{USAGE}");
                2
            }
        }
    };
    std::process::exit(code);
}

#[cfg(target_os = "linux")]
fn agent_main(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--debug") {
        DEBUG.store(true, Ordering::Relaxed);
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(agent::main())
}

#[cfg(not(target_os = "linux"))]
fn agent_main(_: &[String]) -> i32 {
    eprintln!("dctether agent runs inside Linux containers only");
    2
}

#[cfg(target_os = "linux")]
fn browser_helper(args: &[String]) -> i32 {
    browser::helper_main(args)
}

#[cfg(not(target_os = "linux"))]
fn browser_helper(_: &[String]) -> i32 {
    eprintln!("dctether-browser runs inside Linux containers only");
    2
}
