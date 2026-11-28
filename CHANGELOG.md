# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `dctether exec --up` starts the dev container (with `dctether up`,
  Wayland mount included) if it isn't running or doesn't exist yet, then
  runs the command. A running container is used as it is, without
  running `up` and its `postAttachCommand` again.
- `dctether up` waits for another `dctether up` or `exec --up` of the same
  container to finish. Two `devcontainer up` at once create two containers
  for one workspace.

### Fixed

- Your command no longer outlives its session. Docker and Podman don't
  end a process started by `exec` when its client goes away, so when
  `dctether exec` was stopped (SIGTERM, `systemctl stop`) or killed, the
  command kept running in the container without its forwards, with or
  without a terminal. The command now runs through `dctether run`, which
  registers it with the container agent, and when the session ends the
  agent ends the command's process session: SIGTERM (plus SIGHUP when it
  has a terminal), then SIGKILL after 10 seconds.
- The port forwarder no longer stops with the systemd unit of the session
  that happened to start it. It shared that session's cgroup, so stopping
  the unit (or the unit's main process exiting) killed it, and the other
  sessions on the container lost their forwarded ports. On Linux it now
  starts in a systemd scope of its own (`systemd-run --user --scope`),
  or directly as before where that isn't available.
- SIGINT and SIGQUIT now reach your command. Sent to `dctether exec`
  (by a process manager, for example), they were ignored. And a Ctrl-C on
  your terminal while stdin wasn't one killed the `devcontainer exec`
  client instead, so the command got SIGTERM and dctether reported the
  client's exit status. dctether now passes both signals to the command's
  foreground process group, as Ctrl-C on its terminal would, and without a
  terminal on stdin starts the command's `devcontainer exec` in its own
  process group so that Ctrl-C reaches the command only once.
- A session whose port forwarder goes away (killed, crashed, its
  container agent gone) now asks for its ports again, which starts a new
  forwarder, instead of running on without them.

### Changed

- Background jobs that your command leaves in its session now end with
  the session, including `nohup` ones. Programs that start a session of
  their own (`setsid`, self-detaching daemons, a `tmux` server) keep
  running, as before.
- SIGTERM or SIGHUP sent to `dctether exec` now first asks the agent to
  end the command, so it shuts down with its forwards still up and its
  exit status becomes dctether's. Previously it went to the
  `devcontainer exec` client, which left the command running. A second
  signal goes to the client, as before.
- `DCTETHER_SESSION` is now always set in the command's environment.

## [0.4.1] - 2026-11-27

### Security

- Git HTTPS credentials: the host's `git credential` ran in the directory
  `dctether exec` was started from, usually the workspace that is mounted
  into the container. A process in the container could write
  `credential.helper = !command` to the workspace's `.git/config` and make
  the host run that command. The host's git now runs in `/`, so it no
  longer reads the workspace's repository configuration. Affects 0.4.0;
  there, `--no-git-credentials` avoids it.
- SSH agent: the container could send your agent any request, so besides
  signing it could add keys, remove or lock yours, and ask the agent to
  load PKCS#11/FIDO provider libraries. The host now passes on only the
  requests to list keys and to sign (and ssh's `session-bind` extension)
  and answers the rest with a failure. `ssh-add` in the container
  therefore no longer changes your host agent, unless you start the
  session with the new `--ssh-agent-unfiltered`.
- Git HTTPS credentials: the container could have your host's git store a
  credential of its choosing for any `http(s)` host, or erase a stored
  one. `store` and `erase` are now carried out only for a credential the
  host handed to the container in the same session, using the host's own
  record of it, and requests are answered one at a time. A request for a
  credential may no longer carry a password. A password typed into a
  prompt inside the container is therefore no longer saved on the host.
- X11: the host offered the direct path, and with it your real X cookie,
  to any container whose agent reported the host's network namespace. A
  modified agent in a container with its own network could claim that.
  The container now has to prove it: the agent must connect to a randomly
  named abstract socket on the host and present a one-time value, which
  only a process in the host's network namespace can do. Otherwise X11
  is relayed, as for any other container.
- Data frames larger than 32 KiB are refused. The limit was 1 MiB,
  although neither side sends more than 32 KiB, so a channel's queue could
  be made to hold 32 times more than intended.

### Changed

- README, Security section: it said the container can't get ports
  forwarded. It can't over the protocol, but it can usually edit
  `forwardPorts` in `devcontainer.json`, which the next session reads.
  Also added: the same file lets it read host environment variables
  through `remoteEnv`, and the port forwarder's name depends on the
  container ID that the agent reports.

## [0.4.0] - 2026-11-27

### Added

- Git HTTPS credentials: git in the container asks your host's git
  (`git credential fill|approve|reject`) through a credential helper that
  dctether adds with git's environment-only configuration, after any
  helpers configured in the container. Only `http` and `https` requests
  are accepted. `--no-git-credentials` turns it off.
- gpg: `GNUPGHOME` points to a private per-session directory with the
  public parts of your host's secret keys and a socket relayed to your host
  gpg-agent's restricted extra socket, so signing (for example git commit
  signing) uses your host keys. The container's `~/.gnupg` isn't touched.
  `--no-gpg` turns it off.

## [0.3.0] - 2026-11-27

### Added

- Port forwarding: `dctether exec` forwards the dev container's
  `forwardPorts`, plus ports given with `-p PORT`, so that the host's
  `127.0.0.1:PORT` reaches `PORT` on the container's loopback. All
  sessions on a container share one forwarder, which stops a few seconds
  after the last session ends. `--no-forward` turns it off. Containers on
  the host's network are not forwarded, since their ports are host ports
  already.

## [0.2.0] - 2026-11-27

### Added

- Wayland: `dctether up` runs `devcontainer up` with the host's Wayland
  socket mounted into the container, and `dctether exec` sets
  `WAYLAND_DISPLAY` to it. In containers created by VS Code, `exec` uses
  the socket VS Code mounted instead. `--no-wayland` turns this off on
  both commands.
- A warning when the container's Wayland socket stopped working because
  it is from an earlier login session.

### Changed

- Release binaries are optimized for size: about 15% smaller.
- Linux packages no longer include a second copy of the host binary as
  the container binary for the same architecture; the host binary serves
  as both.

## [0.1.0] - 2026-11-27

### Added

- `dctether exec`: runs a command (by default the container user's
  shell) through `devcontainer exec`, with:
  - SSH agent forwarding.
  - X11: directly through the host's abstract X socket when the container
    shares the host's network, otherwise through a relay that keeps the
    host's X cookie out of the container.
  - `BROWSER`, which opens `http://` and `https://` URLs on the host.
- Static Linux container binaries for x86_64 and aarch64, copied into the
  container on first use.
- Release builds for Linux and macOS, and a Homebrew tap:
  `brew install pesho/tap/dctether`.

[Unreleased]: https://github.com/pesho/dctether/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/pesho/dctether/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/pesho/dctether/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/pesho/dctether/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/pesho/dctether/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/pesho/dctether/releases/tag/v0.1.0
