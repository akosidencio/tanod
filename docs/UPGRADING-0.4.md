# Preparing an upgrade to 0.4.0

The 0.4.0 metadata in this tree is an unpublished candidate. No tag, npm package,
container push or release is implied. Review the [release gates](RELEASE-GATES.md)
and run them against the reviewed clean commit before publishing.

| Change from released 0.3.2 | Automatic behavior | Operator action |
|---|---|---|
| Proxy workers | Omitted `server.threads` uses Rust's available parallelism, respecting visible affinity and quotas, with a minimum of one. | Use `threads: 1` to retain single-worker behavior or leave CPU to a colocated Node process. Values 1–1024 are accepted; zero is rejected. Restart to change it. CPU shares alone are not a CPU quota. |
| Forwarding boundary | Untrusted forwarding fields are overwritten/removed; trusted public host remains off by default. | If the edge rewrites Host, configure its exact source CIDRs and `server.trusted_proxies.host: x_forwarded` (or `forwarded`). Ensure the edge overwrites that header and enforces public hosts. Test Server Actions and absolute redirects. |
| Cache-policy reload | Compatible operational changes retain the namespace. Sharing-policy changes retire and reclaim it before publishing a new generation. Late old writers cannot become eligible for new followers or retain content. | Expect a cold cache after incompatible edits. Purge every replica; caches and coalescing are per process. Existing pinned responses may finish. |
| Cache pressure | Registration pressure falls back uncached; a fill may finish unstored within one maximum body of headroom. Past headroom it aborts visibly. | Budget RSS beyond logical cache bytes, including detached readers, stacks and indexes. Keep large streams off cached routes if the application cannot tolerate the documented first oversized-body abort. |
| In-flight gauges | `/status` and `tanod_origin_in_flight` report actual held weighted units, which can exceed a newly lowered limit until work finishes. `tanod_upgrade_active` counts separate tunnel slots. | Interpret them as work, not request counts. Review saturation alerts after changing weights or limits. |
| Client disconnects | An admitted request whose client leaves keeps its permit while Tanod reads the origin's response to its end and discards it, up to the request's `timeouts.origin` deadline. The origin is no longer signalled to stop by a closed connection. Exempt classes and cache fills are unchanged. | Expect renders for departed clients to complete and to count against the ceiling. Keep `timeouts.origin` near your longest acceptable render. Alert on `tanod_origin_disconnect_drains_total{outcome="deadline"}`. See [what the origin ceiling guarantees](OPERATIONS.md#what-the-origin-ceiling-guarantees). |
| Operational metrics | `tanod_config_reloads_total{outcome="success"\|"failure"}` and `tanod_process_start_time_seconds` are added. Generation changes only on accepted reloads. | Import updated alert examples; keep scrape-target identity stable for timestamp-based restart alerts, or use orchestrator restart metrics. |
| Shutdown | SIGTERM withdraws readiness while serving for `drain_period`, then Pingora closes listeners and drains. SIGINT is fast shutdown. | Set the supervisor deadline above drain + shutdown timeout + teardown. The production examples use 50 seconds for 5 + 35 + 1 seconds. Streams/tunnels need reconnect or an appropriate deadline. |

Before rollout, validate configuration with the candidate binary, record the
image/binary checksum, test on a staging origin, then roll one instance while
watching readiness, origin work, queue depth, pressure and response errors.
Keep the preceding binary and configuration together for rollback. A downgrade
starts a cold cache; in-memory namespaces do not persist across process restarts.

For a fixed separate tier, count surge and terminating pods against the static
capacity partition. The shipped Kubernetes example uses ordered StatefulSet
replacement; a Deployment with zero surge can still overlap terminating pods.
Fence an old process before any forced deletion. For application autoscaling, prefer the existing per-app
process/pod topology in the [deployment guide](DEPLOYMENT-GUIDE.md) rather than
assuming the static separate-tier example coordinates capacity dynamically.
