## What and why

<!-- What this changes and the problem it solves. Link the issue if there is one. -->

## Testing

<!-- Commands you ran and what they showed. For performance claims, before and after numbers from the same machine. -->

## Checklist

- [ ] One change, targeting `dev`
- [ ] `cargo fmt`, `clippy -D warnings` (with and without `--features tls`) and `cargo test` pass locally
- [ ] Tests added or updated for any behavior change
- [ ] Documentation and `CHANGELOG.md` updated where users or operators would notice
- [ ] **Security:** effect on cache sharing, cache keys, admission, forwarded headers or the admin API, if any:
- [ ] **Performance:** expected effect on throughput, latency or memory, if any:
- [ ] **Breaking:** configuration, CLI, metric or default changes, if any:
