# AGENTS.md

Guidance for coding agents working on dctether. The README describes what
the tool does and how it works; read its "How it works" and "Security"
sections before changing behavior.

## What this is

A Rust CLI wrapping `devcontainer exec` (the `@devcontainers/cli`) that
gives the shell what a VS Code Dev Containers terminal gets: SSH agent,
X11, `BROWSER`, Wayland and port forwarding. One binary has two halves:
the host side (`exec`, `up`, `forward`) and the container side (`agent`,
`dctether-browser`), talking over a framed multiplex protocol on the
agent's `devcontainer exec` stdin/stdout.

## Layout

| File | What |
|---|---|
| `src/main.rs` | CLI dispatch, help text, `debug!` macro |
| `src/proto.rs` | Frame and control-message encoding (pure, unit-tested) |
| `src/mux.rs` | Channel registry, frame reader/writer, per-channel pump |
| `src/host.rs` | `exec` and `up`: agent bootstrap/upload, handshake, host endpoints |
| `src/agent.rs` | Container half (Linux only): session dir, sockets, X11/Wayland probing |
| `src/forward.rs` | Port forwarding: per-container `dctether forward` process, control socket |
| `src/x11.rs` | X11 client-stream filter: cookie swap, MIT-SHM/DRI3 hiding (pure) |
| `src/xauth.rs` | Xauthority format and `DISPLAY` parsing (pure) |
| `src/wayland.rs` | Wayland socket paths and mount argument (pure) |
| `src/browser.rs` | URL validation (host) and the `BROWSER` helper (container) |
| `scripts/` | Static builds, packaging, formula rendering, changelog extraction |
| `.github/workflows/release.yml` | Tag-triggered release and Homebrew tap update |
| `packaging/homebrew/dctether.rb.in` | Formula template rendered at release time |

## Build and test

```sh
cargo build
cargo test                                  # unit tests; run it more than once after concurrency changes
scripts/build-container-binaries.sh         # static musl x86_64 + aarch64 into target/agent/, in Docker
DCTETHER_AGENT_DIR=$PWD/target/agent DCTETHER_DEBUG=1 target/debug/dctether exec -- true
```

- The host's Rust (Fedora package) has no musl target and no rustup, and
  no clippy. Static builds, clippy and the macOS type-check run in the
  official Rust image, as in `build-container-binaries.sh`:
  `cargo clippy --all-targets` and
  `cargo clippy --target aarch64-apple-darwin` (after
  `rustup target add aarch64-apple-darwin` inside the container).
- Keep dependencies pure Rust (no `cc`, `ring`, OpenSSL): the aarch64
  musl build links with `rust-lld` and has no cross C toolchain.
- Keep Linux-only code behind `cfg(target_os = "linux")`. The host half
  must still compile for macOS; it just hasn't been run there.
- Rebuild `target/agent/` after changing anything the agent runs, or the
  live test exercises the old container half. The cache key is a content
  hash, so a rebuilt agent is always copied in fresh.

## Rules

- **The container is untrusted.** Everything the agent sends is checked
  on the host: channel kinds, frame sizes, environment variables
  (`check_env`), shell path, URLs. Anything from the container that ends
  up on a host command line must be validated and passed as a single
  argument (for example `--remote-env=K=V`).
- **No listeners in the container except Unix sockets in the 0700
  session directory**, never TCP or abstract sockets: under
  `--network=host` those would be reachable by every host process.
  Secrets (X cookies) travel only inside protocol frames, never in argv.
- **Never write the user's configuration**: no `devcontainer.json`,
  `containerEnv`, images or shell rc files. Environment goes on dctether's
  own `devcontainer exec` only; mounts only through `dctether up` flags.
- **Pure logic stays pure and unit-tested** (protocol, X11 filter,
  Xauthority, URL checks, argument handling). Tests must not depend on
  timing or on fds being closed promptly: another test spawning children
  can hold a copy of an fd between fork and exec.
- **No unsubstantiated claims** in code comments, the README or the
  changelog. State what was tested, and say "untested" otherwise.
- **Prefer simple, unhacky solutions**; when one depends on another
  tool's undocumented behavior (like VS Code's socket names), document it
  as best effort and make it fail safe.

## Live testing

- The user's real dev containers (`bridge-devcontainer` on the host
  network, `engine-devcontainer` on the bridge network) are not test
  fixtures. Read-only checks are fine. Never stop, rebuild or recreate
  them, and never modify `~/devel/bridge` or `~/devel/engine` without
  asking.
- For anything else, create a throwaway dev container from a minimal
  image in a scratch workspace (for example Debian slim plus a UID-1000
  user), and remove the container and its image afterwards. Don't reuse
  the bridge image: its metadata mounts the user's home volume and runs
  install hooks.
- Nothing may open a window on the user's screen without asking. Check
  X11 with `xkbcomp -xkb :0 FILE` and Wayland with `wayland-info` or a raw
  registry request.
- When testing BROWSER, put a fake `xdg-open` that only logs its
  arguments first on `PATH`.

## Changelog and releases

- Add every user-visible change to `## [Unreleased]` in `CHANGELOG.md`
  ([Keep a Changelog](https://keepachangelog.com/en/1.1.0/)) in the same
  commit as the change.
- Release steps are in the README ("Changelog and releases"). The
  workflow takes the notes from the version's changelog section and fails
  if the tag, `Cargo.toml` and the changelog disagree.
- Tagging and releasing need the user's go-ahead. So do changes to the
  Homebrew tap (`pesho/homebrew-tap`) other than the release workflow's.
