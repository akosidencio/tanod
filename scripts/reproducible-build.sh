#!/usr/bin/env bash
# Build the release binary with checkout- and Cargo-cache paths remapped to
# stable placeholders. Deriving the paths here is what lets two clean checkouts
# in different directories produce the same bytes.
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cargo_root=${CARGO_HOME:-"${HOME}/.cargo"}
remap_flags="--remap-path-prefix=${repo_root}=/tanod --remap-path-prefix=${cargo_root}/registry/src=/cargo/registry"

if [ -n "${RUSTFLAGS:-}" ]; then
  export RUSTFLAGS="${RUSTFLAGS} ${remap_flags}"
else
  export RUSTFLAGS="${remap_flags}"
fi
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-"$(git log -1 --pretty=%ct)"}

exec cargo build --release --locked --bin tanod "$@"
