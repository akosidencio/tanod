#!/usr/bin/env bash
# Bounded campaigns, using existing corpora and retaining crash reproducers.
# FUZZ_SECONDS=60 is PR smoke; nightly/release runs use a longer explicit span.
set -euo pipefail
cd "$(dirname "$0")/.."
seconds=${FUZZ_SECONDS:-60}
[[ "$seconds" =~ ^[1-9][0-9]*$ ]] || { echo 'FUZZ_SECONDS must be a positive integer' >&2; exit 2; }
# cargo-fuzz has no --locked option. Resolve both graphs with Cargo's locked
# mode first and check afterward that the fuzz runner changed neither lock.
cargo metadata --locked --format-version 1 > /dev/null
cargo metadata --manifest-path fuzz/Cargo.toml --locked --format-version 1 > /dev/null
lock_before=$(sha256sum Cargo.lock fuzz/Cargo.lock)
cargo +nightly fuzz build
mapfile -t targets < <(cargo +nightly fuzz list)
[ "${#targets[@]}" -gt 0 ] || { echo 'no fuzz targets discovered' >&2; exit 1; }
for target in "${targets[@]}"; do
  mkdir -p "fuzz/corpus/$target"
  if [ -d "fuzz/seeds/$target" ]; then
    # Seed inputs contain fixtures only. Preserve discovered corpus entries.
    for seed in "fuzz/seeds/$target/"*; do
      if [ -f "$seed" ] && [ ! -e "fuzz/corpus/$target/${seed##*/}" ]; then
        cp "$seed" "fuzz/corpus/$target/"
      fi
    done
  fi
  cargo +nightly fuzz run "$target" -- \
    -max_total_time="$seconds" -timeout=10 -rss_limit_mb=2048 -max_len=65536 \
    -print_final_stats=1
done
[ "$(sha256sum Cargo.lock fuzz/Cargo.lock)" = "$lock_before" ] \
  || { echo 'fuzzing changed a dependency lockfile' >&2; exit 1; }
