//! dctether: give a `devcontainer exec` shell what a VS Code Dev Containers
//! terminal gets — SSH agent, X11 display and `BROWSER` — relayed over
//! `devcontainer exec` itself.

mod browser;
mod host;
mod mux;
mod proto;
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

Run COMMAND (default: the container user's shell from /etc/passwd) with `devcontainer exec`,
forwarding the host's SSH agent, X11 display and browser opener into it.

Everything before `--` that is not a dctether option is passed to
`devcontainer exec` unchanged, e.g. --workspace-folder, --container-id, --config.

Options:
  --no-ssh-agent   don't forward $SSH_AUTH_SOCK
  --no-x11         don't forward $DISPLAY
  --no-browser     don't set BROWSER
  --x11-relay      always relay X11, even if the host display is reachable directly

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
