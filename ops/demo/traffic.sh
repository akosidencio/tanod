#!/bin/sh
# Produce a mix of reusable, private, and intentionally contended requests so
# every important Grafana section has real data without a separate load tool.
set -eu

base=http://tanod:8080
until wget -q -O /dev/null "$base/healthz"; do
  sleep 1
done

round=0
while :; do
  round=$((round + 1))

  # Repeated public URLs create misses, coalesced followers, and cache hits.
  for _ in 1 2 3 4 5 6; do
    wget -q -O /dev/null "$base/products/atlas-runner" &
  done
  wget -q -O /dev/null "$base/search?q=runner&page=$((round % 3))" &

  # Private traffic is deliberately never shared.
  wget -q -O /dev/null "$base/api/private-session" &
  wget -q -O /dev/null "$base/account" &

  # Distinct keys exceed the flash-sale route's render ceiling and exercise
  # the queue while remaining below its bounded maximum.
  for burst in 1 2 3 4 5 6 7 8; do
    wget -q -O /dev/null "$base/flash-sale?run=$round-$burst" &
  done

  wait || true
  sleep 1
done
