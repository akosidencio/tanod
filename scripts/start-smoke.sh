#!/bin/sh
# `tanod-next start` end to end, the way an Alpine-based app image runs it:
# a static tanod, a Node origin, one request through Tanod, then SIGTERM.
#
#   docker run --rm -v "$PWD:/src" -w /src node:22-alpine \
#     sh scripts/start-smoke.sh target/x86_64-unknown-linux-musl/release/tanod
#
# POSIX sh and BusyBox tools only, because that is what the image has.
set -eu

bin=${1:?usage: start-smoke.sh <path to a tanod binary>}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cat > "$work/origin.cjs" <<'EOF'
require('node:http')
  .createServer((req, res) => setTimeout(() => res.end('origin ok\n'), req.url === '/slow' ? 1500 : 0))
  .listen(Number(process.env.PORT), process.env.HOSTNAME);
EOF

# The listener comes from the environment, as it would on a platform that
# assigns PORT; the origin is the fixed loopback port start gives it.
cat > "$work/tanod.yaml" <<'EOF'
version: 1
server:
  listen: "127.0.0.1:${SMOKE_PORT:-18080}"
origin:
  upstreams: ["127.0.0.1:3000"]
  concurrency:
    max: 1
routes:
  - id: all
    match: "/**"
    class: private_dynamic
    concurrency:
      max: 1
      queue:
        max: 0
        timeout: 0s
EOF

TANOD_BIN="$bin" node packages/tanod-next/src/cli.js start \
  --config "$work/tanod.yaml" -- node "$work/origin.cjs" &
runner=$!

ok=
for _ in $(seq 1 100); do
  if out=$(wget -qO- http://127.0.0.1:18080/ 2>/dev/null); then ok=1; break; fi
  sleep 0.1
done
[ -n "$ok" ] || { echo "FAIL: nothing answered through tanod"; kill "$runner"; exit 1; }
[ "$out" = "origin ok" ] || { echo "FAIL: unexpected body: $out"; kill "$runner"; exit 1; }
echo "request through tanod: $out"

# With the one render slot taken, a second request is shed: Tanod is really in
# the path, not a pass-through.
wget -qO- http://127.0.0.1:18080/slow >/dev/null 2>&1 &
sleep 0.3
if wget -qO- http://127.0.0.1:18080/ >/dev/null 2>&1; then
  echo "FAIL: a request while the slot was taken was not shed"; kill "$runner"; exit 1
fi
echo "second request shed while the slot was taken"
wait $! 2>/dev/null || true

kill -TERM "$runner"
if wait "$runner"; then
  echo "PASS: start ran the origin and tanod, served through tanod, and stopped cleanly"
else
  echo "FAIL: start exited non-zero after SIGTERM"; exit 1
fi
