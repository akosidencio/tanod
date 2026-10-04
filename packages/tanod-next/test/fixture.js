import { existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const ROOT = fileURLToPath(new URL('../../..', import.meta.url));

/**
 * The Next build these tests read.
 *
 * Committed inside the package rather than borrowed from
 * `fixtures/next-storefront/.next`, which is `.gitignore`d. Pointing the suite
 * at build output that only exists after somebody has run `next build` makes
 * it pass locally and fail everywhere else — which is precisely how this
 * package's first CI run went.
 */
export const FIXTURE = fileURLToPath(new URL('./fixtures/next-build', import.meta.url));

/**
 * The storefront's actual build, when the machine happens to have one.
 *
 * Used only for the extra assertion that the committed copy has not drifted
 * from a real build. Absent in CI, so anything depending on it must skip
 * rather than fail.
 */
const real = path.join(ROOT, 'fixtures/next-storefront/.next');
export const REAL_BUILD = existsSync(path.join(real, 'BUILD_ID')) ? real : null;

/** The tanod binary, if this checkout has been built. */
export const BINARY =
  ['target/debug/tanod', 'target/release/tanod']
    .map((p) => path.join(ROOT, p))
    .find((p) => existsSync(p)) ?? null;

/**
 * Refuse to skip the binary-dependent tests.
 *
 * Those tests carry the package's central claim — that what it generates is a
 * configuration Tanod accepts — and they skip themselves when the binary is
 * absent so that a JavaScript package's suite does not require a Rust
 * toolchain. That convenience is a trap in CI: a `cargo build` that quietly
 * produced nothing would leave the suite green with the one assertion that
 * matters never having run.
 *
 * CI sets `TANOD_REQUIRE_BINARY=1`, which turns the missing binary from a
 * skip into an immediate, loud failure.
 */
if (process.env.TANOD_REQUIRE_BINARY === '1' && !BINARY) {
  throw new Error(
    'TANOD_REQUIRE_BINARY=1 but no tanod binary was found under target/. ' +
      'Run `cargo build` first — these tests must not skip in CI.',
  );
}
