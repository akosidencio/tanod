# Validation evidence — 2026-10-29

**Release decision: NOT READY.** Both hour-long release soaks failed the origin
concurrency gate (10 observed, 8 configured). Successful shorter checks do not
supersede that result. See the two root engineering reports for scope and limits.

## Performance

`phase31/summary/trials-current.csv` contains 1,092 current samples. The original
Phase 2/candidate comparison uses matching release builds compiled alongside
`--bench hotpath`, which unifies development features such as Tokio test-util.
The ordinary-release graph comparison is separate; binary hashes and exact
build commands distinguish them. Microbenchmarks and allocation profiles are
separate experiments. CPU columns monitor Tanod for proxy cases and the origin
for direct-origin demo cases. These short desktop-host trials are not isolated
machine precision claims; medians and full ranges are retained.

`phase31/perf/demonstration-clock` and `phase31/demo-superseded` retain an earlier
demonstration with cross-case outstanding-request carryover. Use
`demonstration-isolated-clock` and `phase31/demo` for the final ordinary-release
demo. These start fresh processes per case, separate successful latency from
intentional 503 responses, and label warmup-inclusive origin/metric counters.
No extra uncached render capacity is claimed. The all-trial CSV retains the
36 superseded rows in addition to the 1,092 current rows.

The Phase 2 worktree is eed23e1 with only the recorded Cargo benchmark target
adaptation and the matching current bench/hotpath.rs. No baseline runtime Rust
was edited. Manifests preserve the adapted input hash and binary SHA.

## Functional and stability checks

`phase4/checks.tsv` is an append-only execution history, not an all-green verdict.
The initial dirty-source refusal was expected; later clean preflights passed.
The first performance failure was the origin fixture's backlog overflow; the
fresh final run passed. Trace-export synchronization and disconnect coverage
fixtures were corrected and rerun. Concurrent local validation also exposed a
500ms connect timeout, a short drain timing miss, and fixed-port collisions.
The final isolated all.sh run and three isolated short TinyUFO soaks passed.
The failed hour-long CLOCK/TinyUFO results remain the blocking verdict.

Use `phase4/all-isolated/` for the final complete suite, `compat-corrected` for
Next 16.3.8/Node 22, and `compat` for the other three successful declared pairs.
Raw hour-long samples and complete outcome counts are in `phase4/soak-raw/`.
The diagnostic in `phase4/disconnect-boundary` is a counterexample, not a passing
release gate. It deliberately models backend work that ignores disconnection;
it does not fully attribute the long-soak peaks.

TLS byte reproduction used the frozen clean 32cafb5 source. Its input hashes,
lockfile and result are retained. Later scripts/docs do not change compiled
Rust/Cargo/embedded inputs. Review archives and binaries remain local; their
checksums, smoke output, npm dry-run contents and CycloneDX inventories are
included here. The SBOM is a resolved dependency inventory, not a linked-only
runtime inventory. No tag, registry publication, signed builder identity or
independent security audit is claimed.

`SHA256SUMS` covers these evidence files. Verify from this directory with
`sha256sum -c SHA256SUMS`.
Captured logs, CSVs, SVGs, text output and the baseline patch retain their
producer's whitespace and line endings; local Git attributes preserve those
bytes and exclude their formatting from source whitespace checks.
