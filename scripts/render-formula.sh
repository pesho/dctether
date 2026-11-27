#!/bin/sh
# Print the Homebrew formula for a release, filling in URLs and checksums.
#
#   scripts/render-formula.sh VERSION SHA256SUMS > Formula/dctether.rb
set -eu
version=$1 sums=$2
sha() {
  s=$(awk -v f="dctether-$version-$1.tar.gz" '$2 == f { print $1 }' "$sums")
  [ -n "$s" ] || { echo "no checksum for $1" >&2; exit 1; }
  echo "$s"
}
sed -e "s/@VERSION@/$version/g" \
    -e "s/@SHA_MACOS_ARM@/$(sha aarch64-apple-darwin)/" \
    -e "s/@SHA_MACOS_INTEL@/$(sha x86_64-apple-darwin)/" \
    -e "s/@SHA_LINUX_ARM@/$(sha aarch64-unknown-linux-musl)/" \
    -e "s/@SHA_LINUX_INTEL@/$(sha x86_64-unknown-linux-musl)/" \
    "$(dirname "$0")/../packaging/homebrew/dctether.rb.in"
