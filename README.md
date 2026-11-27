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
- **Wayland**: `WAYLAND_DISPLAY` points at your host's compositor, if the
  container was created with `dctether up` (or by VS Code).
- **Ports**: the container's `forwardPorts` (and any `-p PORT`) are
  reachable on your host's `127.0.0.1` while a session runs.
- **Git HTTPS credentials**: git in the container gets credentials from
  your host's git (its credential helpers, or its password prompt).
- **gpg**: signing and decryption use your host's gpg-agent, so commit
  signing works with your host keys.

You don't need VS Code. Nothing is written to `devcontainer.json`, the
image or shell rc files. The environment is set only on dctether's own
`devcontainer exec`.

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

## Usage

```sh
cd ~/src/project            # a folder with a dev container configuration
dctether up                 # start its container (optional, see below)
dctether exec               # interactive shell
dctether exec -- git push   # one command
```

Like `devcontainer`, dctether finds the container from the current
directory, so no options are needed. To pick another container, add
`--workspace-folder DIR` or `--container-id ID`.

`dctether exec` works with a running container however it was started
(`devcontainer up`, VS Code, `dctether up`). `dctether up` is
`devcontainer up` plus a mount of your Wayland socket. Wayland needs a
mount made when the container is created, by `dctether up` or by VS Code
(see [Wayland](#wayland-mounted-at-creation)).

```text
dctether exec [OPTIONS] [DEVCONTAINER-EXEC-OPTIONS] [-- COMMAND [ARGS...]]

  --no-ssh-agent   don't forward $SSH_AUTH_SOCK
  --ssh-agent-unfiltered  let the container also add, remove and lock keys in
                   your SSH agent (default: it can only list keys and sign)
  --no-x11         don't forward $DISPLAY
  --no-browser     don't set BROWSER
  --no-wayland     don't set WAYLAND_DISPLAY
  --no-git-credentials  don't answer git's HTTPS credential requests from the host
  --no-gpg         don't relay gpg-agent (and don't set GNUPGHOME)
  --x11-relay      always relay X11, even if the host display is reachable directly
  -p PORT          also forward host 127.0.0.1:PORT to the container's PORT
  --no-forward     don't forward any ports

dctether up [--no-wayland] [DEVCONTAINER-UP-OPTIONS]

  --no-wayland     don't mount the Wayland socket
```

For `exec`, everything before `--` that isn't one of these options goes to
`devcontainer exec` unchanged, for example `--workspace-folder`,
`--container-id`, `--config` or `--remote-env`. Those must all be options. The
command always goes after `--`. Without a command, dctether starts the
container user's shell from `/etc/passwd`. For `up`, every argument other
than `--no-wayland` goes to `devcontainer up`.

The command's exit status becomes dctether's. When stdin is a terminal,
`devcontainer exec` allocates a pty as usual, and window size changes and
Ctrl-C behave as in a plain `devcontainer exec`.

Each forward is active only if the host has it: `SSH_AUTH_SOCK` must be
set, `DISPLAY` must name a reachable X server, and for Wayland
`WAYLAND_DISPLAY` must be set (and the container created by `dctether
up` or VS Code). A forward stays up while the `dctether exec` session runs and ends
with it; the Wayland mount lasts as long as the container, and forwarded
ports last while any session on the container runs.

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
  Linux). The container then reaches that socket itself. Before the
  host hands over its cookie, the container has to show that it really is
  on the host's network: the host listens on a randomly named abstract
  socket, and the agent must connect to it and present a one-time value
  sent over the pipe. Abstract sockets exist per network namespace, so a
  container with its own network can't. The agent then confirms the
  display with a real X connection handshake (it opens no window),
  and writes the host's MIT-MAGIC-COOKIE-1 into a private Xauthority
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

### Wayland: mounted at creation

Wayland clients pass file descriptors (window buffers, keymaps) over
their socket, and a byte relay can't carry those. So instead of relaying,
`dctether up` bind-mounts the compositor's socket
(`$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`) into the container at
`/tmp/dctether-wayland.sock`, as VS Code does with its own mount. The
container shares the host's kernel, so clients talk to the compositor
directly. `dctether exec` then checks that the socket accepts connections
and sets `WAYLAND_DISPLAY=/tmp/dctether-wayland.sock`. An absolute path,
so the container needs no `XDG_RUNTIME_DIR`.

If that socket isn't there, `dctether exec` falls back to the one VS
Code mounts in containers it creates, `/tmp/vscode-wayland-<uuid>.sock`,
with the same check. That name is VS Code's internal naming, not a
documented interface. If it changes, the fallback finds nothing and
`WAYLAND_DISPLAY` stays unset. dctether only points `WAYLAND_DISPLAY` at
a socket that is already in the container, so this adds no exposure.

Caveats, and why it can be turned off (`--no-wayland` on `up` or `exec`):

- **Only at creation.** Docker can't add mounts to a running container,
  and `devcontainer up` reuses an existing container as it is. To add the
  mount to an existing container, recreate it with
  `dctether up --remove-existing-container`. Containers started any other
  way (other than by VS Code) simply don't get Wayland.
- **Breaks after a new login.** The mount pins the socket that existed
  when the container was created. After you log out and back in, the
  compositor listens on a new socket at the same path, and the container's
  copy goes dead. `dctether exec` notices, says so, and doesn't set
  `WAYLAND_DISPLAY`. Recreating the container fixes it.
- **Same user ID.** Compositor sockets are usually accessible only to
  their owner, so the container user needs your host user ID (or root).

### Git HTTPS credentials

dctether adds one credential helper to git through git's environment-only
configuration (`GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_0=credential.helper`,
`GIT_CONFIG_VALUE_0`), so no git config file changes. It comes after any
helpers configured in the container, which are still asked first. The
helper (`dctether-git-credential`, next to the container binary) sends
git's request to the host, which answers with `git credential
fill|approve|reject`: your host's helpers answer, or your host's askpass
program asks you. Terminal prompts are off on the host side, so they
can't interfere with your session. The host's git runs in `/`, not in
your workspace: the workspace is mounted into the container, so its
`.git/config` could name a helper for the host to run. Credential
settings in a repository's own configuration on the host are therefore
not used.

Only `http` and `https` requests and git's documented attributes are
accepted. A request for a credential may carry only what git knows when
it asks (host, path, user name, the server's challenge), never a
password. Requests are answered one at a time.

Git also tells its helpers when a credential worked (`store`) or was
rejected (`erase`). The host acts on that only for a credential it handed
to the container in the same session, and passes its own record of that
credential to `git credential approve|reject`, not the container's
request. Anything else is ignored. So a password typed into a prompt
inside the container is not saved on the host; one typed into the host's
prompt is.

### gpg: a private GNUPGHOME

`dctether exec` sets `GNUPGHOME` to a private directory in the session
(`/tmp/dctether-XXXX/gnupg`) containing:

- `S.gpg-agent`, relayed to your host gpg-agent's *extra* socket
  (`gpgconf --list-dirs agent-extra-socket`), the socket gpg-agent
  provides for forwarding to other machines. The agent runs connections
  on it in restricted mode (see `--extra-socket` in gpg-agent's manual
  for what that excludes). Signing and decryption work, with passphrases
  asked for by your host's pinentry. Tested with `gpg --clearsign` and
  `git commit -S`. gpg may print `problem with fast path key listing:
  Forbidden - ignored`: a shortcut the restricted socket refuses, after
  which gpg carries on normally.
- `pubring.gpg`, the public parts of your host's secret keys (from
  `gpg --export`), which gpg needs to know which keys the agent holds.
- `common.conf`, so that gpg uses that keyring file rather than keyboxd.

The container's own `~/.gnupg` isn't touched, and your host keyring
isn't reachable from the container. Inside a dctether session, gpg
therefore doesn't see the container's own keys or trust settings. This is
skipped when the container has no gpg (`gpgconf`), the host has no
secret keys, or `--no-gpg` is given.

### Clipboard

There is no separate clipboard forward: programs in the container reach
your clipboard through their display connection, X11 or Wayland. Tested
in a host-network dev container on KDE Plasma (Wayland): text copied with
`xclip -selection clipboard` (X11) and with `wl-copy` (Wayland) shows up
in the host clipboard, and host clipboard contents paste into apps in the
container.

Copies from Playwright's Chromium paste fine but don't appear in Klipper's
history. Chromium marks them as secret (`x-kde-passwordManagerHint:
secret`), the KDE convention for passwords, and Klipper honours that.

### Ports: one forwarder per container

`dctether exec` forwards the `forwardPorts` of the dev container
configuration (read with `devcontainer read-configuration`), plus any
ports given with `-p`. Host `127.0.0.1:<port>` then reaches port `<port>`
on the container's own loopback, so services that listen only on the
container's `127.0.0.1` work too. Entries naming another service, such as
`"db:5432"`, are skipped with a message.

All sessions on a container share one forwarder process
(`dctether forward`, started by `exec`; you don't run it yourself):

1. A session asks the forwarder for its ports through a control socket in
   `$XDG_RUNTIME_DIR/dctether/`, named after the container's ID. If none
   answers, the session starts it in the background, detached from the
   terminal. A lock file ensures there is only ever one per container.
2. The forwarder runs its own container agent and listens on the host
   ports. Each connection becomes a channel to the agent, which connects
   to the same port inside the container. The session prints which ports
   it newly forwarded, and which it couldn't (for example because the
   host port is already in use).
3. Each session keeps its control connection open while it runs. The
   kernel closes it when the session ends, however it ends. Five seconds
   after the last session is gone, the forwarder stops and the ports
   close.

Ports are only added, never removed: a port one session added with `-p`
stays forwarded until the last session ends. A container on the host's
network (`--network=host`) already shares the host's ports, so dctether
doesn't forward any there. Its log is
`$XDG_RUNTIME_DIR/dctether/forward-<id>.log`.

### Async runtime

dctether runs on [tokio](https://tokio.rs) with a single-threaded
(`current_thread`) runtime. The work is many small concurrent byte relays
between Unix sockets and one pipe, plus child processes and signals, and
tokio provides all of these with mature APIs. One thread is plenty for
this load.

## Security

dctether treats the container as untrusted. Even so, every forward gives
the container real power over your host session. Enable only the forwards
you need (`--no-ssh-agent`, `--no-x11`, `--no-browser`, `--no-wayland`,
`--no-git-credentials`, `--no-gpg`).

- **SSH agent.** While a session is open, any process in the container
  that can reach the socket can ask your agent to sign with your keys.
  That means the container user, or root. It can't read the keys, but it
  can authenticate as you anywhere those keys are accepted, for as long as
  the session lasts. This is the same trade-off as `ssh -A` and VS Code's
  own forwarding. To limit it, use keys added with confirmation
  (`ssh-add -c`), or a separate agent holding only the keys this container
  should use. The socket lives in a 0700 directory owned by the container
  user.

  The host passes on only the requests to list keys and to sign, and the
  `session-bind@openssh.com` extension that `ssh` sends to say which
  server it is talking to. Requests to add, remove or lock keys, or to
  load PKCS#11/FIDO provider libraries, are answered with a failure on the
  host and never reach your agent. Your agent can't hold those back
  itself: it sees dctether as a local client (see `allow-remote-pkcs11`
  in ssh-agent's manual). `--ssh-agent-unfiltered` relays everything, for
  example to run `ssh-add` in the container. The filter goes by the kind
  of request, so it can't allow one for `ssh-add` and refuse the same one
  from another program. Tested with OpenSSH 10.0 and 10.2 in the
  container: `ssh` logs in with a key from the host agent (binding the
  session first), `git push` over SSH and `ssh -A` to a further host
  work, as do `ssh-add -l` and `ssh-keygen -Y sign`; `ssh-add KEY`, `-d`,
  `-D` and `-x` report that the agent refused.
- **X11.** Any client on your X display can read keystrokes and screen
  contents of the other clients on that display, so granting the container
  the display grants that too. In direct mode the container gets your real
  X cookie, in a 0600 file. With `--network=host` it could already connect
  to the abstract socket, but not authenticate. In relay mode it gets only
  a per-session cookie that works through the relay.
- **Wayland.** For as long as the container exists, anything in it that
  can reach the mounted socket is a client of your compositor, with the
  same access as your own apps: it can open windows and use the clipboard.
  The core Wayland protocol doesn't let clients read each other's input or
  screen contents, unlike X11. Compositors add extensions on top (screen
  capture, input emulation, clipboard managers), and which clients may use
  them depends on the compositor. The mount is the compositor's real
  socket, not a relay, and dctether doesn't filter it.
- **Git credentials.** The container can ask your host's git for
  credentials of any `http(s)` host, and gets whatever your host's
  helpers have stored for it (or what you type into the host's prompt).
  That is the same trade-off as VS Code's credential forwarding. It can
  also make your host forget a credential, but only one it was handed in
  the same session, and it can't make your host store one of its own
  choosing. Use `--no-git-credentials` for containers you don't trust
  with them.
- **gpg.** While a session is open, anything in the container that can
  reach the session's socket can ask your gpg-agent to sign or decrypt
  with your keys. Your host's pinentry asks for the passphrase unless
  it is cached. The agent serves this socket in restricted mode.
- **Browser.** The container can ask the host to open URLs. The host
  accepts only `http://` and `https://` URLs of printable ASCII without
  spaces, at most 8 KiB long. It passes the URL to `xdg-open` (or `open` on
  macOS) as a single argument, never through a shell. A malicious container
  could still open unwanted web pages.
- **No network listeners in the container.** The agent listens only on
  filesystem Unix sockets in the container, never on TCP or abstract
  sockets. Under `--network=host` those would be reachable by every
  process on the host. All traffic goes over the agent's own
  `devcontainer exec` pipe.
- **Forwarded ports** listen on the host's `127.0.0.1`, so every local
  process on the host can reach those container services, the same as with
  VS Code's forwarding or Docker's `127.0.0.1` publishing. Only ports from
  your configuration or your `-p` flags are forwarded, and the container
  can't ask for any over the protocol. But see the next point: the
  configuration is usually a file the container can edit.
- **The workspace is writable by the container**, and that is where
  `devcontainer.json` normally is. The `devcontainer` CLI reads it on the
  host for every `exec`, with or without dctether. A container that adds
  a port to `forwardPorts` there gets it forwarded on your host's
  `127.0.0.1` by the next session (dctether prints each port it newly
  forwards; `--no-forward` turns forwarding off). And a `remoteEnv` entry
  written as `${localEnv:NAME}` copies that variable of your host
  environment into the container on the next `exec`. Both tested with
  devcontainer CLI 0.91.0.
- The container chooses only a channel *kind* (SSH agent, X11, browser,
  git credential, gpg-agent), never a host path or address. Port channels
  are opened by the host only. The host refuses kinds it didn't offer,
  caps data frames at 32 KiB and handshake messages at 1 MiB, and allows
  256 channels at once.
- The X cookie goes only through the protocol pipe, never on a command
  line. Host command lines (`ps`) show only the forwarded paths.
- What the agent reports ends up on the host's `devcontainer` command
  line: the environment for your command, and the default shell. The host
  accepts only the variables it expects (`SSH_AUTH_SOCK`, `DISPLAY`,
  `XAUTHORITY`, `BROWSER`, `DCTETHER_SESSION`, `WAYLAND_DISPLAY`,
  `GNUPGHOME`, and the three `GIT_CONFIG_*` for the credential helper),
  only absolute paths (`:<n>` for `DISPLAY`, the exact values for
  `GIT_CONFIG_COUNT` and `GIT_CONFIG_KEY_0`), each passed as a single
  `--remote-env=K=V` argument.
- The agent binary lives in the container user's home, so anything
  running as that user can replace it. The checks above don't depend on
  the agent behaving. "The real cookie never enters the container" (relay
  mode) doesn't either: the agent reports its network namespace, but the
  host offers the direct path, cookie included, only after the check
  described under [X11](#x11-direct-and-relayed). What that check shows
  is that the agent, or something it could pass the one-time value to, is
  in the host's network namespace. One thing does depend on the agent:
  it reports the container's ID, which names the port forwarder that
  sessions on a container share. A modified agent that reported another
  container's ID could get that container's sessions to use its
  forwarder, so that their forwarded ports lead to it instead (from
  reading the code; not tested).

## Platforms

- **Host:** Linux. The code also builds for macOS (XQuartz-style `DISPLAY`
  paths and `open`), but nobody has tested it there yet.
- **Container:** Linux on x86_64 or aarch64. Tested with Ubuntu and
  Debian dev containers, and with Alpine under qemu-aarch64.

Linux release builds target `*-unknown-linux-musl` and are statically
linked. They need no C library from the system, so they run on glibc
distributions (Fedora, Ubuntu, Debian, …) as well as on musl ones
(Alpine), whatever their glibc version. That is also why the same binary
can be copied into any container. One consequence: musl doesn't use
glibc's NSS. A TCP `DISPLAY` such as `somehost.local:0` therefore can't
be resolved through mDNS or other NSS-only resolvers. `:0`, `unix:0` and
`localhost:N` aren't affected.

## Not supported (yet)

- Automatic port detection (as VS Code does), and ports of other Compose
  services (`"db:5432"`).
- Forwards end with the `dctether exec` session. Background programs that
  outlive it lose them.
- Flow control is a small bounded queue per channel. When one is full,
  reading from the pipe pauses. An X client that stops reading its socket
  can therefore stall the session's other forwards until it reads again or
  disconnects.

## Changelog and releases

Changes are listed in [CHANGELOG.md](CHANGELOG.md), in
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format. To
release version `X.Y.Z`:

1. In `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - DATE`,
   add a new empty `## [Unreleased]` above it, and update the links at
   the bottom.
2. Set `version = "X.Y.Z"` in `Cargo.toml` and commit.
3. Tag `vX.Y.Z` and push the tag. The release workflow builds the
   packages, publishes the GitHub release with that changelog section as
   its notes, and updates the Homebrew tap. It refuses to release when the
   tag, `Cargo.toml` and the changelog disagree.

## License

MIT
