# dctether

`dctether exec` runs a command in a dev container through
[`devcontainer exec`](https://github.com/devcontainers/cli), and gives it
what a VS Code Dev Containers terminal gets:

- **SSH agent**: `SSH_AUTH_SOCK` in the container reaches your host's
  agent, so `git push` over SSH and `ssh` work.
- **X11**: `DISPLAY`/`XAUTHORITY` point at your host's X server, so X
  clients open their windows on your desktop.
- **Browser**: `BROWSER` opens `http(s)` URLs in your host's browser,
  for example for OAuth logins started by CLIs in the container.

You don't need VS Code. Nothing is written to `devcontainer.json`, the
image or shell rc files. The environment is set only on dctether's own
`devcontainer exec`.

## Usage

```sh
dctether exec --workspace-folder ~/src/project              # interactive shell
dctether exec --workspace-folder ~/src/project -- git push  # one command
```

```text
dctether exec [OPTIONS] [DEVCONTAINER-EXEC-OPTIONS] [-- COMMAND [ARGS...]]

  --no-ssh-agent   don't forward $SSH_AUTH_SOCK
  --no-x11         don't forward $DISPLAY
  --no-browser     don't set BROWSER
  --x11-relay      always relay X11, even if the host display is reachable directly
```

Everything before `--` that isn't one of these options goes to
`devcontainer exec` unchanged: `--workspace-folder`, `--container-id`,
`--config`, `--remote-env` and so on. Those must all be options. The
command always goes after `--`. Without a command, dctether starts the
container user's shell from `/etc/passwd`.

The command's exit status becomes dctether's. When stdin is a terminal,
`devcontainer exec` allocates a pty as usual, and window size changes and
Ctrl-C behave as in a plain `devcontainer exec`.

Each forward is active only if the host has it: `SSH_AUTH_SOCK` must be
set and `DISPLAY` must name a reachable X server. A forward stays up
while the `dctether exec` session runs and ends with it.

| Variable | Meaning |
|---|---|
| `DCTETHER_DEVCONTAINER` | devcontainer CLI to run (default `devcontainer`) |
| `DCTETHER_AGENT_DIR` | directory with `dctether-linux-<arch>` container builds |
| `DCTETHER_DEBUG=1` | print diagnostics (paths, chosen X11 mode) to stderr |

## How it works

```text
host                                          container
────                                          ─────────
dctether exec ──devcontainer exec─────────▶ dctether agent
     │          (stdin/stdout: framed            │ listens on /tmp/dctether-XXXX/
     │           multiplex protocol)             │   agent.sock    (SSH_AUTH_SOCK)
     │                                           │   browser.sock  (BROWSER helper)
     │                                           │ and /tmp/.X11-unix/X<n> when relaying X11
     │
     ├─ $SSH_AUTH_SOCK, X server, xdg-open/open
     │
     └──devcontainer exec --remote-env …──────▶ your shell / command
```

1. dctether starts `devcontainer exec … sh -c <boot script>`. The script
   detects the container architecture and runs
   `~/.cache/dctether/<version>-<hash>/dctether agent`. If that binary is
   missing, dctether copies in the matching static Linux build (x86_64 or
   aarch64, about 1 MB) with `cat` over another `devcontainer exec`, then
   starts again. The cache key includes a content hash, so a rebuilt binary
   is never confused with an old one. The copy is checked by size before
   it is renamed into place. The container needs only a POSIX `sh` and
   `uname`, `cat`, `wc`, `tr`, `mkdir`, `chmod`, `ln`, `mv` and `rm`
   (busybox is enough).
2. The agent's stdin/stdout carry a small framed protocol: open, data,
   half-close and close per channel, plus a JSON handshake. The agent
   creates a session directory (mode 0700) and Unix sockets in it, and
   reports back the environment to use.
3. dctether runs your command in a second `devcontainer exec`, adding
   that environment with `--remote-env`. Every connection to one of the
   agent's sockets becomes a channel. On the host, the matching channel
   connects to the SSH agent or the X server, or receives a URL.
4. When your command exits, dctether closes the agent's stdin. The agent
   then removes its session directory and sockets. This also happens when
   the host side is killed (`SIGKILL` included), because the agent sees
   end-of-file on its stdin.

A session costs two `devcontainer exec` calls, about 1 s in total on the
author's machine, and a third the first time a version is used.

### X11: direct and relayed

- **Direct.** dctether uses this when the container shares the host's
  network namespace (`--network=host`) and the host X server listens on
  an abstract socket (`@/tmp/.X11-unix/X<n>`, as Xorg and Xwayland do on
  Linux). The container then reaches that socket itself. The agent
  confirms this with a real X connection handshake (it opens no window),
  then writes the host's MIT-MAGIC-COOKIE-1 into a private Xauthority
  file (`/tmp/dctether-XXXX/Xauthority`, mode 0600) as a wildcard-family
  entry, and sets `DISPLAY=:<n>`. Nothing is relayed.
- **Relay.** Otherwise the agent listens on `/tmp/.X11-unix/X<n>`, with
  `n` the first free number from 10. The socket is mode 0700, owned by the
  container user. The agent writes a fresh random cookie for that display.
  The host checks each client's connection setup for that cookie, replaces
  it with the real one and connects to the X server, so the real cookie
  never enters the container. Shared-memory and fd-passing extensions
  can't cross a byte stream, so the relay rewrites client `QueryExtension`
  requests for `MIT-SHM` and `DRI3`, and the server answers "not
  present". Clients then fall back to paths that work over a plain
  socket. `ListExtensions`
  still lists both, so a client that skips `QueryExtension` could still
  try them.

### Async runtime

dctether runs on [tokio](https://tokio.rs) with a single-threaded
(`current_thread`) runtime. The work is many small concurrent byte relays
between Unix sockets and one pipe, plus child processes and signals, and
tokio provides all of these with mature APIs. One thread is plenty for
this load.

## Security

dctether treats the container as untrusted. Even so, every forward gives
the container real power over your host session. Enable only the forwards
you need (`--no-ssh-agent`, `--no-x11`, `--no-browser`).

- **SSH agent.** While a session is open, any process in the container
  that can reach the socket can ask your agent to sign with your keys.
  That means the container user, or root. It can't read the keys, but it
  can authenticate as you anywhere those keys are accepted, for as long as
  the session lasts. This is the same trade-off as `ssh -A` and VS Code's
  own forwarding. To limit it, use keys added with confirmation
  (`ssh-add -c`), or a separate agent holding only the keys this container
  should use. The socket lives in a 0700 directory owned by the container
  user.
- **X11.** Any client on your X display can read keystrokes and screen
  contents of the other clients on that display, so granting the container
  the display grants that too. In direct mode the container gets your real
  X cookie, in a 0600 file. With `--network=host` it could already connect
  to the abstract socket, but not authenticate. In relay mode it gets only
  a per-session cookie that works through the relay.
- **Browser.** The container can ask the host to open URLs. The host
  accepts only `http://` and `https://` URLs of printable ASCII without
  spaces, at most 8 KiB long. It passes the URL to `xdg-open` (or `open` on
  macOS) as a single argument, never through a shell. A malicious container
  could still open unwanted web pages.
- **No network listeners.** The agent listens only on filesystem Unix
  sockets in the container, never on TCP or abstract sockets. Under
  `--network=host` those would be reachable by every process on the host.
  All traffic goes over the agent's own `devcontainer exec` pipe.
- The container chooses only a channel *kind* (SSH agent, X11, browser),
  never a host path or address. The host refuses kinds it didn't offer,
  caps frames at 1 MiB and allows 256 channels at once.
- The X cookie goes only through the protocol pipe, never on a command
  line. Host command lines (`ps`) show only the forwarded paths.
- What the agent reports ends up on the host's `devcontainer` command
  line: the environment for your command, and the default shell. The host
  accepts only the five variables it expects (`SSH_AUTH_SOCK`, `DISPLAY`,
  `XAUTHORITY`, `BROWSER`, `DCTETHER_SESSION`) and only absolute paths
  (`:<n>` for `DISPLAY`), each passed as a single `--remote-env=K=V`
  argument.
- The agent binary lives in the container user's home, so anything
  running as that user can replace it. The checks above don't depend on
  the agent behaving. One guarantee does: the agent reports the network
  namespace itself, so "the real cookie never enters the container" (relay
  mode) holds only for an unmodified agent. A modified one could claim to
  share the host's namespace and be offered the direct path, cookie
  included.

## Install

```sh
brew install pesho/tap/dctether
```

This also installs the `devcontainer` CLI from Homebrew. Release builds
are static, so a Linux host build doubles as the container binary for its
own architecture. The package adds the container build for the other
Linux architecture (both, on macOS) in `libexec/dctether/`.

### From source

```sh
cargo build --release                        # host binary (target/release/dctether)
scripts/build-container-binaries.sh          # static musl builds, in Docker
DCTETHER_AGENT_DIR=$PWD/target/agent target/release/dctether exec ...
```

`build-container-binaries.sh` builds the `x86_64` and `aarch64`
`*-unknown-linux-musl` binaries inside the official Rust image with
`rust-lld`. All dependencies are pure Rust, so no cross C toolchain is
needed.

## Platforms

- **Host:** Linux. The code also builds for macOS (XQuartz-style `DISPLAY`
  paths and `open`), but nobody has tested it there yet.
- **Container:** Linux on x86_64 or aarch64. Tested with an Ubuntu dev
  container, and with Alpine under qemu-aarch64.

Linux release builds target `*-unknown-linux-musl` and are statically
linked. They need no C library from the system, so they run on glibc
distributions (Fedora, Ubuntu, Debian, …) as well as on musl ones
(Alpine), whatever their glibc version. That is also why the same binary
can be copied into any container. One consequence: musl doesn't use
glibc's NSS. A TCP `DISPLAY` such as `somehost.local:0` therefore can't
be resolved through mDNS or other NSS-only resolvers. `:0`, `unix:0` and
`localhost:N` aren't affected.

## Not supported (yet)

- Wayland, git HTTPS credential helper, port forwarding, gpg agent,
  clipboard.
- Forwards end with the `dctether exec` session. Background programs that
  outlive it lose them.
- Flow control is a small bounded queue per channel. When one is full,
  reading from the pipe pauses. An X client that stops reading its socket
  can therefore stall the session's other forwards until it reads again or
  disconnects.

## License

MIT
