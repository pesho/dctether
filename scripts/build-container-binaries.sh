#!/bin/sh
# Build the static Linux (musl) binaries that dctether copies into
# containers, for x86_64 and aarch64, inside the official Rust image.
# Pure-Rust dependencies plus rust-lld mean no cross C toolchain is needed.
#
# Output: target/agent/dctether-linux-x86_64, target/agent/dctether-linux-aarch64
# Use them with:  DCTETHER_AGENT_DIR=target/agent dctether exec ...
set -eu

IMAGE=${RUST_IMAGE:-mirror.gcr.io/library/rust:1.100-slim}
cd "$(dirname "$0")/.."

docker run --rm \
  -v "$PWD:/src:z" \
  -v dctether-cargo-registry:/usr/local/cargo/registry \
  -w /src \
  -e CARGO_TARGET_DIR=/src/target/musl \
  -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  "$IMAGE" sh -euc '
    rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl >/dev/null
    for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
      RUSTFLAGS="-C linker=rust-lld" cargo build --locked --release --target "$t"
    done
    mkdir -p target/agent
    cp target/musl/x86_64-unknown-linux-musl/release/dctether target/agent/dctether-linux-x86_64
    cp target/musl/aarch64-unknown-linux-musl/release/dctether target/agent/dctether-linux-aarch64
    chown -R "$HOST_UID:$HOST_GID" target/musl target/agent
  '
ls -l target/agent
