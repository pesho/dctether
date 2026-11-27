#!/bin/sh
# Assemble release tarballs from built binaries.
#
#   scripts/package.sh VERSION BUILD_DIR OUT_DIR
#
# BUILD_DIR must contain <target>/dctether for each target below (missing
# host targets are skipped; both Linux musl builds are required, since they
# are the container halves).
#
# Each tarball has bin/dctether plus libexec/dctether/dctether-linux-<arch>
# for every container architecture the host binary can't serve itself. A
# Linux host binary is the static build for its own architecture and copies
# itself into such containers, so its package carries only the other arch.
set -eu
version=$1 build=$2 out=$3
mkdir -p "$out"
for arch in x86_64 aarch64; do
  test -x "$build/$arch-unknown-linux-musl/dctether" || { echo "missing $arch musl build" >&2; exit 1; }
done
for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-apple-darwin aarch64-apple-darwin; do
  [ -x "$build/$target/dctether" ] || { echo "skipping $target (not built)" >&2; continue; }
  name="dctether-$version-$target"
  stage="$out/$name"
  mkdir -p "$stage/bin" "$stage/libexec/dctether"
  cp "$build/$target/dctether" "$stage/bin/dctether"
  for arch in x86_64 aarch64; do
    [ "$target" = "$arch-unknown-linux-musl" ] && continue
    cp "$build/$arch-unknown-linux-musl/dctether" "$stage/libexec/dctether/dctether-linux-$arch"
  done
  cp README.md LICENSE "$stage/"
  tar -C "$out" -czf "$out/$name.tar.gz" "$name"
done
(cd "$out" && sha256sum ./*.tar.gz | sed 's| \./| |' > SHA256SUMS)
cat "$out/SHA256SUMS"
