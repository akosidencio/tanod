#!/usr/bin/env bash
# Prove vendor/pingora-proxy is the published pingora-proxy 0.9.0 plus
# vendor/pingora-proxy/tanod.patch, and nothing else.
#
# The published crate is checked against the checksum crates.io recorded for it
# (the one Cargo.lock carried before the path patch). The local Cargo cache is
# used when it holds that crate; otherwise it is downloaded.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENDOR="$ROOT/vendor/pingora-proxy"
VERSION=0.9.0
SHA256=2467265ad39b56230b56149f1c03ca510f46c4bf4a25d6d4b7345e0f06cc46e7

WORK=$(mktemp -d "${TMPDIR:-/tmp}/tanod-vendor-XXXXXX")
trap 'rm -rf "$WORK"' EXIT

CRATE="$WORK/pingora-proxy-$VERSION.crate"
CACHED=$(ls "${CARGO_HOME:-$HOME/.cargo}"/registry/cache/*/pingora-proxy-$VERSION.crate 2>/dev/null | head -n 1 || true)
if [ -n "$CACHED" ]; then
  cp "$CACHED" "$CRATE"
else
  curl -fsSL --max-time 60 -o "$CRATE" \
    "https://static.crates.io/crates/pingora-proxy/pingora-proxy-$VERSION.crate"
fi
echo "$SHA256  $CRATE" | sha256sum -c --quiet - \
  || { echo "published pingora-proxy $VERSION does not match its recorded checksum" >&2; exit 1; }

tar xzf "$CRATE" -C "$WORK"
TREE="$WORK/pingora-proxy-$VERSION"
git -C "$TREE" init -q
git -C "$TREE" apply -p1 "$VENDOR/tanod.patch"
rm -rf "$TREE/.git"
cp "$VENDOR/tanod.patch" "$VENDOR/TANOD-PATCH.md" "$TREE/"

if diff -r "$TREE" "$VENDOR"; then
  echo "vendor/pingora-proxy = pingora-proxy $VERSION ($SHA256) + tanod.patch"
else
  echo "vendor/pingora-proxy differs from the published crate plus tanod.patch" >&2
  exit 1
fi
