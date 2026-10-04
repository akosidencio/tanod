import { generateToFile } from './generate-to-file.js';

/**
 * Next's own constant, inlined rather than imported.
 *
 * Importing it would mean `next.config` reaching into `next/dist/shared/lib`,
 * a private path that has moved between releases. The string has not, and a
 * wrong guess here is loud — the hook simply never fires — rather than subtle.
 */
const PHASE_PRODUCTION_BUILD = 'phase-production-build';

/**
 * Set once the hook is registered, and inherited by every child process Next
 * spawns.
 *
 * Next loads `next.config` in more than one process — the build itself, and
 * its compilation workers. Without this guard each of them would register its
 * own exit hook, and a build would generate the same file several times and
 * run `tanod check` once per worker. Same output, so it is not a
 * correctness problem, but it is noise in a build log and wasted work.
 */
const GUARD = '__TANOD_NEXT_HOOK__';

/**
 * Wrap a Next config so a production build also writes Tanod's.
 *
 * ```js
 * // next.config.mjs
 * import { withTanod } from '@tanod/next/config';
 *
 * export default withTanod(
 *   { output: 'standalone' },
 *   { out: 'tanod.yaml', upstreams: ['next-1:3000'], concurrency: 40, check: true },
 * );
 * ```
 *
 * The work happens in a `process.on('exit')` handler, which is the only hook
 * available: Next has no post-build callback. Two consequences are worth
 * knowing before relying on it.
 *
 * * **It only runs on a successful build.** A non-zero exit code means the
 *   build failed, and generating a route policy from a half-finished build
 *   would be worse than generating nothing.
 * * **It can still fail the build.** Setting `process.exitCode` from an exit
 *   handler works, so a rejected config is a failed build rather than a
 *   warning nobody reads.
 *
 * For CI, a `postbuild` script is the more predictable place — it is an
 * ordinary process with an ordinary exit code, visible in the log as its own
 * step. This wrapper is for keeping the config fresh during local development
 * without anyone having to remember a second command.
 */
export function withTanod(nextConfig = {}, options = {}) {
  return (phase, context) => {
    if (phase === PHASE_PRODUCTION_BUILD && !process.env[GUARD]) {
      process.env[GUARD] = '1';
      register(options);
    }
    return typeof nextConfig === 'function' ? nextConfig(phase, context) : nextConfig;
  };
}

function register(options) {
  const { out = 'tanod.yaml', silent = false, ...rest } = options;

  process.on('exit', (code) => {
    // A failed build has no configuration worth generating, and emitting one
    // anyway would leave a stale file that looks current.
    if (code !== 0) return;
    try {
      const result = generateToFile({ ...rest, out });
      if (!silent) {
        process.stdout.write(
          `\n@tanod/next: wrote ${result.out} — ${result.routes} routes, ` +
            `build ${result.buildId}${result.checked ? ', tanod check passed' : ''}\n`,
        );
      }
    } catch (error) {
      process.stderr.write(`\n@tanod/next: ${error?.message ?? error}\n`);
      // Verified: assigning here does change the process's exit status, so a
      // configuration Tanod would reject fails the build that produced it.
      process.exitCode = 1;
    }
  });
}
