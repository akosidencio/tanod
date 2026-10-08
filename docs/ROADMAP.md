# Roadmap

Remaining work for Tanod's SSR and dynamic origin protection. Tanod is pre-1.0; sustained production validation remains open. Release history belongs in the [changelog](../CHANGELOG.md).

## Validate deployments

Before relying on Tanod in production:

- Validate static replica budgets and path-stable ingress in production-shaped staging
- Exercise release binaries, generated configuration, systemd, domain setup, upgrades, and rollback on a clean server
- Obtain an independent cache-key and response-shareability review
- Record sustained production behavior under representative traffic

See the [production reference](./NEXTJS-PRODUCTION-REFERENCE.md), [standalone guide](./STANDALONE.md), and [release gates](./RELEASE-GATES.md) for validation requirements.

## Improve overload handling and visibility

The next visitor-facing improvements are:

- Add opt-in, per-route stale responses when admission sheds a request; currently stale content is served during revalidation or upstream failure, not overload shedding
- Estimate distinct client addresses over one- and five-minute windows without exporting addresses, and report open downstream connections

## Expand integrations and distribution

After replica validation, expand support based on demonstrated self-hosting demand:

- Define a versioned framework-adapter contract and shared conformance tests
- Choose and implement the next framework integration
- Add Linux ARM64 artifacts and package-manager installation when demand justifies them

Revisit capacity leases, adaptive limits, and distributed reuse only if staging or production evidence shows fixed partitions and local reuse are insufficient.

## Current boundaries

Account for these limits when deploying:

- Cache and coalescing are local; replica capacity is statically partitioned, and purges must reach every cache-holding instance
- Path purges match exact paths, without dynamic route-pattern invalidation
- Slow readers can retain origin capacity; bounded response spooling trades progressive rendering for earlier capacity release
- An edge server, CDN, authentication, and client rate limiting remain separate responsibilities

See [operations](./OPERATIONS.md) for deployment and monitoring guidance.
