/**
 * What this package has been built and tested against.
 *
 * Next's build manifests are internal formats with their own version numbers,
 * independent of Next's release version. They are the actual compatibility
 * surface: a Next upgrade that leaves them alone changes nothing here, and one
 * that bumps them can change everything.
 *
 * The rule mirrors Tanod's own configuration rule, and for the same reason.
 * An unknown manifest version is **refused**, never guessed at. Guessing would
 * mean generating a route policy from a format nobody verified — and a route
 * policy is what decides whether one user's page can be served to another.
 */

/** The Tanod configuration schema version this package emits. */
export const TANOD_SCHEMA_VERSION = 1;

/**
 * Manifest versions this package understands.
 *
 * `null` means the manifest carries no version field of its own, so there is
 * nothing to check and nothing to promise.
 */
export const SUPPORTED_MANIFESTS = Object.freeze({
  'routes-manifest.json': Object.freeze([3]),
  'prerender-manifest.json': Object.freeze([4]),
  'app-path-routes-manifest.json': null,
  'server/pages-manifest.json': null,
});

/**
 * Next releases actually exercised against this package, newest first.
 *
 * "Verified" means a real `next build` output was read by the generator and
 * the result passed `tanod check`. Anything not listed may well work — the
 * manifest versions are what matter — but nobody has run it.
 */
export const VERIFIED_NEXT_RELEASES = Object.freeze([
  Object.freeze({
    next: '16.3.8',
    router: 'app + pages',
    routesManifest: 3,
    prerenderManifest: 4,
  }),
  Object.freeze({
    next: '16.3.3',
    router: 'app + pages',
    routesManifest: 3,
    prerenderManifest: 4,
  }),
]);
