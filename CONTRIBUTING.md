# Contributing to Tanod

Bug reports, fixes and documentation corrections are welcome. For a change
in behavior or a new feature, open an issue first so the approach can be
agreed before you write it.

**Security issues are not reported here.** Follow [SECURITY.md](SECURITY.md)
and use private vulnerability reporting, never a public issue or pull request.

## Prerequisites

- Linux x64. Tanod is built, tested and released for Linux only.
- Rust: `rust-toolchain.toml` pins the toolchain (rustup installs it on first
  use); `rust-version` in `Cargo.toml` is the supported minimum.
- `clang`, `cmake`, `pkg-config` and OpenSSL headers (`libssl-dev`), which
  Pingora's TLS bindings need.
- Python 3.11+ with PyYAML for the repository scripts.
- For `packages/tanod-next`: Node 22 or 24, and Bun for its Bun tests.
- Docker, only for the container and Next.js benchmarks.

## Build and test

```bash
cargo build --release --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --locked --features tls -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --lib --locked --features tls
cargo test --workspace --doc --locked
```

Repository checks that CI also runs:

```bash
cargo audit -D warnings
scripts/check-vendored-pingora.sh
python3 scripts/check-workflows.py
python3 scripts/check-repository-identity.py
```

The Next.js adapter:

```bash
cd packages/tanod-next
npm install
npm run typecheck
npm test          # Node
npm run test:bun  # Bun
```

`vendor/pingora-proxy` is Pingora 0.9.0 with one patch. Do not edit it
directly; read `vendor/pingora-proxy/TANOD-PATCH.md` first.

## Benchmarks

Every benchmark asserts its own claim and exits non-zero when it fails.
`./bench/all.sh` runs the end-to-end suite (several minutes; Docker-based
scenarios skip when no daemon is reachable). For a change to admission, the
proxy lifecycle or the cache, also run the relevant script directly, for
example `python3 bench/origin-concurrency.py` or `bench/coalesce.sh`.
`cargo bench --bench hotpath` covers in-process hot paths. A performance claim
in a pull request needs before and after numbers from the same machine; see
[performance measurement](docs/RELEASE-GATES.md#performance-measurement).

## Pull requests

- Branch from `dev` and target `dev`. `main` receives releases only.
- Keep each pull request to one change. Say what it changes, why, and how you
  tested it.
- Add or update a test for any behavior change, and update the documentation
  and `CHANGELOG.md` when users or operators would notice the change.
- Call out configuration changes, changed defaults and anything that alters
  cache sharing, admission or forwarded headers. Those need the most review.
- CI must pass. [Release gates](docs/RELEASE-GATES.md) lists every gate and
  what it fails on.

## Reporting a bug

Open an [issue](https://github.com/tanod-sh/tanod/issues/new/choose) with the
Tanod version (`tanod version`), how it is deployed, a redacted configuration,
the requests that reproduce it, and what you expected and observed. Remove
credentials, cookies, authorization headers and other users' data first.

Contributions are accepted under the [Apache License 2.0](LICENSE).
