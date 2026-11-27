# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/pesho/dctether/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/pesho/dctether/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/pesho/dctether/releases/tag/v0.1.0
