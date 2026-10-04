import { TanodNextError } from './manifests.js';

function assertValue(kind, value) {
  if (typeof value !== 'string' || value.length === 0) {
    throw new TanodNextError(`${kind} must be a non-empty string`);
  }
}

/**
 * A tag may not contain a comma, because one never could have been stored: the
 * tag header is comma-separated, so Tanod split it before indexing. Purging
 * such a tag is guaranteed to match nothing, which is exactly the silent
 * no-op this package refuses everywhere else.
 */
function assertTag(tag) {
  assertValue('tag', tag);
  if (tag.includes(',')) {
    throw new TanodNextError(
      `tag ${JSON.stringify(tag)} contains a comma. The tag header is comma-separated, so a ` +
        'tag with one in it was never stored under that name and purging it would match nothing.',
    );
  }
}

function assertPath(path) {
  assertValue('path', path);
  if (!path.startsWith('/')) {
    throw new TanodNextError(
      `path ${JSON.stringify(path)} must be absolute; Tanod stores the request path, which ` +
        'always begins with "/"',
    );
  }
}

/**
 * A client for Tanod's purge endpoint.
 *
 * `endpoint` is the **admin** listener, not the traffic one. It is usually
 * loopback or a private address, which is where the token's transport
 * protection comes from — the admin listener does not speak TLS.
 */
export function createPurger(options = {}) {
  const {
    token = process.env.TANOD_PURGE_TOKEN,
    timeoutMs = 2000,
    fetch: fetchImpl = globalThis.fetch,
  } = options;
  const configuredEndpoints = options.endpoints ?? (
    options.endpoint
      ? [options.endpoint]
      : process.env.TANOD_PURGE_URLS
        ? process.env.TANOD_PURGE_URLS.split(',').map((value) => value.trim()).filter(Boolean)
        : process.env.TANOD_PURGE_URL
          ? [process.env.TANOD_PURGE_URL]
          : []
  );

  if (!Array.isArray(configuredEndpoints) || configuredEndpoints.length === 0) {
    throw new TanodNextError(
      'no Tanod endpoint: pass `endpoint`/`endpoints`, or set TANOD_PURGE_URL(S) to the admin listener(s)',
    );
  }
  if (!token) {
    throw new TanodNextError(
      'no purge token: pass `token` or set TANOD_PURGE_TOKEN. It must match ' +
        'cache.purge.token, and without one the endpoint does not exist.',
    );
  }
  if (typeof fetchImpl !== 'function') {
    throw new TanodNextError('no fetch available; pass one explicitly');
  }

  const bases = [...new Set(configuredEndpoints.map((endpoint) => new URL('/purge', endpoint).href))]
    .map((endpoint) => new URL(endpoint));

  async function sendOne(base, params, description) {
    const url = new URL(base);
    // Tanod percent-decodes every value exactly once. URLSearchParams keeps
    // delimiters inside tags and paths from changing the shape of the query.
    // It emits form-style `+` for spaces, but Tanod deliberately implements
    // percent decoding rather than form decoding, so use `%20` instead.
    url.search = new URLSearchParams(params).toString().replaceAll('+', '%20');

    let response;
    try {
      response = await fetchImpl(url, {
        method: 'POST',
        headers: {
          // In a header, never in the URL: query strings are logged by
          // everything on the path.
          authorization: `Bearer ${token}`,
          accept: 'application/json',
        },
        // A redirect would re-send the Authorization header to whatever host
        // the redirect names. The purge endpoint never redirects, so any
        // redirect here is something to refuse rather than follow.
        redirect: 'manual',
        signal: AbortSignal.timeout(timeoutMs),
      });
    } catch (cause) {
      throw new TanodNextError(
        `purge request to ${url.origin} failed: ${cause?.message ?? cause}`,
        { cause },
      );
    }

    if (response.status >= 300 && response.status < 400) {
      throw new TanodNextError(
        `purge endpoint at ${url.origin} answered a ${response.status} redirect; refusing to re-send the token ` +
          'to another host',
      );
    }
    if (!response.ok) {
      const detail = await response.text().catch(() => '');
      throw new TanodNextError(
        `purge (${description}) at ${url.origin} failed: HTTP ${response.status} ${detail.trim()}`.trim(),
      );
    }
    let result;
    try {
      result = await response.json();
    } catch (cause) {
      throw new TanodNextError(
        `purge (${description}) at ${url.origin} returned HTTP ${response.status} with invalid JSON`,
        { cause },
      );
    }
    if (
      !result ||
      typeof result !== 'object' ||
      Array.isArray(result) ||
      result.purged !== true ||
      !Number.isSafeInteger(result.entries) ||
      result.entries < 0
    ) {
      throw new TanodNextError(
        `purge (${description}) at ${url.origin} returned HTTP ${response.status} with an invalid success body`,
      );
    }
    return result;
  }

  async function send(params, description) {
    const results = await Promise.all(
      bases.map((base) => sendOne(base, params, description)),
    );
    if (results.length === 1) return results[0];
    return {
      purged: true,
      entries: results.reduce((sum, result) => sum + result.entries, 0),
      replicas: results.length,
      results,
    };
  }

  return {
    /** Invalidate every entry carrying any of these tags. */
    async purgeTags(tags) {
      const list = [...new Set(tags)].filter(Boolean);
      list.forEach(assertTag);
      if (list.length === 0) return { purged: false, entries: 0 };
      return send(
        list.map((tag) => ['tag', tag]),
        `${list.length} tag(s)`,
      );
    },

    /** Invalidate every variant of each of these exact paths. */
    async purgePaths(paths) {
      const list = [...new Set(paths)].filter(Boolean);
      list.forEach(assertPath);
      if (list.length === 0) return { purged: false, entries: 0 };
      return send(
        list.map((path) => ['path', path]),
        `${list.length} path(s)`,
      );
    },

    /** Both at once, in a single request. */
    async purge({ tags = [], paths = [] } = {}) {
      const tagList = [...new Set(tags)].filter(Boolean);
      const pathList = [...new Set(paths)].filter(Boolean);
      tagList.forEach(assertTag);
      pathList.forEach(assertPath);
      if (tagList.length === 0 && pathList.length === 0) {
        throw new TanodNextError('purge needs at least one tag or path');
      }
      return send(
        [
          ...tagList.map((tag) => ['tag', tag]),
          ...pathList.map((path) => ['path', path]),
        ],
        `${tagList.length} tag(s), ${pathList.length} path(s)`,
      );
    },

    /**
     * Invalidate everything.
     *
     * Expect an origin load spike proportional to your traffic — every cached
     * page re-renders on its next request. This is a deploy-time or
     * incident-time tool, not a routine one.
     */
    async purgeAll() {
      return send([['all', '1']], 'everything');
    },
  };
}

/**
 * `revalidateTag()` from `next/cache`, and then the same tag in Tanod.
 *
 * Both are needed and neither is redundant: Next invalidates its own
 * incremental cache inside the server, Tanod invalidates the shared copy in
 * front of it. Doing only the first leaves Tanod serving the old page until
 * its TTL expires, which is the failure this function exists to prevent.
 *
 * Tanod's purge is attempted **after** Next's, and a failure throws. Stale
 * content served silently is worse than a failed deploy hook: one is visible
 * immediately, the other is discovered by a customer.
 */
export async function revalidateTag(tag, options = {}) {
  const { nextProfile = { expire: 0 }, ...purgerOptions } = options;
  const { revalidateTag: nextRevalidateTag } = await importNextCache();
  // Immediate expiry is deliberate. With stale-while-revalidate, the first
  // Tanod miss could fetch stale HTML from Next and cache it again.
  nextRevalidateTag(tag, nextProfile);
  return createPurger(purgerOptions).purgeTags([tag]);
}

/** The same, for `revalidatePath()`. */
export async function revalidatePath(path, options) {
  const { revalidatePath: nextRevalidatePath } = await importNextCache();
  nextRevalidatePath(path);
  return createPurger(options).purgePaths([path]);
}

async function importNextCache() {
  try {
    // @ts-ignore -- `next` is an optional peer dependency, so this module is
    // resolvable in a Next app and absent everywhere else. That is exactly why
    // the import is dynamic and inside a try.
    return await import('next/cache');
  } catch (cause) {
    throw new TanodNextError(
      'could not import `next/cache`. The revalidate helpers only work inside a Next.js ' +
        'server; from anywhere else use createPurger().purgeTags() / .purgePaths() directly.',
      { cause },
    );
  }
}
