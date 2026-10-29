#!/usr/bin/env bash
# Parse the shipped examples with Tanod's actual strict configuration loader.
set -euo pipefail
cd "$(dirname "$0")/.."
bin=${TANOD_BIN:-target/debug/tanod}
# A fixture credential only; config checking neither binds nor contacts origin.
export TANOD_PURGE_TOKEN=production-config-validation-fixture-only
for config in ops/production/*-tanod.yaml; do
  "$bin" check --config "$config"
done
