import { createHash } from 'node:crypto';
import { readFile, readFileSync } from 'node:fs';

import { TanodNextError } from './manifests.js';

export const POLICY_VERSION = 1;

const ROUTE_FIELDS = new Set(['privacy', 'weight', 'priority', 'methods', 'coalesce', 'cache']);
const CACHE_FIELDS = new Set(['ttl', 'stale_if_error', 'query', 'vary']);
const SAFE_METHODS = new Set(['GET', 'HEAD']);
const HEADER = /^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/;
const METHOD = /^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/;
const DURATION = /^(?:[1-9][0-9]*)(?:ms|s|m|h|d)$/;

function fail(file, line, message) {
  const where = line ? `${file}:${line}` : file;
  throw new TanodNextError(`${where}: ${message}`);
}

function stripComment(line) {
  let quote = null;
  let escaped = false;
  for (let i = 0; i < line.length; i += 1) {
    const char = line[i];
    if (escaped) {
      escaped = false;
      continue;
    }
    if (quote === '"' && char === '\\') {
      escaped = true;
      continue;
    }
    if (char === '"' || char === "'") {
      quote = quote === char ? null : quote ?? char;
      continue;
    }
    if (char === '#' && quote === null && (i === 0 || /\s/.test(line[i - 1]))) {
      return line.slice(0, i).trimEnd();
    }
  }
  return line.trimEnd();
}

function splitPair(text, file, line) {
  let quote = null;
  let escaped = false;
  for (let i = 0; i < text.length; i += 1) {
    const char = text[i];
    if (escaped) {
      escaped = false;
      continue;
    }
    if (quote === '"' && char === '\\') {
      escaped = true;
      continue;
    }
    if (char === '"' || char === "'") {
      quote = quote === char ? null : quote ?? char;
      continue;
    }
    if (char === ':' && quote === null) {
      return [text.slice(0, i).trim(), text.slice(i + 1).trim()];
    }
  }
  fail(file, line, 'expected `key: value`');
}

function scalar(text, file, line) {
  if (text === '') return null;
  if (text.startsWith('"')) {
    try {
      const parsed = JSON.parse(text);
      if (typeof parsed !== 'string') fail(file, line, 'expected a string');
      return parsed;
    } catch (cause) {
      if (cause instanceof TanodNextError) throw cause;
      fail(file, line, 'invalid double-quoted string');
    }
  }
  if (text.startsWith("'")) {
    if (!text.endsWith("'") || text.length < 2) fail(file, line, 'invalid quoted string');
    return text.slice(1, -1).replaceAll("''", "'");
  }
  if (text === 'true') return true;
  if (text === 'false') return false;
  if (/^[0-9]+$/.test(text)) return Number(text);
  return text;
}

function list(text, file, line) {
  if (!text.startsWith('[') || !text.endsWith(']')) {
    fail(file, line, 'expected an inline list such as `[q, page]`');
  }
  const body = text.slice(1, -1).trim();
  if (!body) return [];
  const values = [];
  let start = 0;
  let quote = null;
  let escaped = false;
  for (let i = 0; i <= body.length; i += 1) {
    const char = body[i];
    if (escaped) {
      escaped = false;
      continue;
    }
    if (quote === '"' && char === '\\') {
      escaped = true;
      continue;
    }
    if (char === '"' || char === "'") {
      quote = quote === char ? null : quote ?? char;
      continue;
    }
    if ((char === ',' && quote === null) || i === body.length) {
      const item = body.slice(start, i).trim();
      if (!item) fail(file, line, 'list contains an empty item');
      const value = scalar(item, file, line);
      if (typeof value !== 'string') fail(file, line, 'list items must be strings');
      values.push(value);
      start = i + 1;
    }
  }
  if (quote !== null) fail(file, line, 'unterminated quoted string');
  return values;
}

/** Parse the deliberately small, versioned tanod.next.yaml format. */
export function parsePolicy(raw, file = 'tanod.next.yaml') {
  const document = { version: null, routes: {} };
  let section = null;
  let route = null;
  let subsection = null;
  const seen = new Set();

  for (const [index, original] of String(raw).replaceAll('\r\n', '\n').split('\n').entries()) {
    const line = index + 1;
    if (original.includes('\t')) fail(file, line, 'tabs are not allowed; use two-space indentation');
    const clean = stripComment(original);
    if (!clean.trim()) continue;
    const indent = clean.length - clean.trimStart().length;
    if (indent % 2 !== 0) fail(file, line, 'indentation must use multiples of two spaces');
    const [keyText, valueText] = splitPair(clean.trim(), file, line);
    const key = scalar(keyText, file, line);
    if (typeof key !== 'string' || !key) fail(file, line, 'keys must be non-empty strings');

    if (indent === 0) {
      route = null;
      subsection = null;
      if (key === 'version') {
        if (seen.has('version')) fail(file, line, 'duplicate `version`');
        document.version = scalar(valueText, file, line);
        seen.add('version');
      } else if (key === 'routes') {
        if (valueText) fail(file, line, '`routes` must contain an indented mapping');
        if (seen.has('routes')) fail(file, line, 'duplicate `routes`');
        section = 'routes';
        seen.add('routes');
      } else {
        fail(file, line, `unknown top-level field \`${key}\``);
      }
      continue;
    }

    if (section !== 'routes') fail(file, line, 'only `routes` may contain nested fields');
    if (indent === 2) {
      if (valueText) fail(file, line, `route \`${key}\` must contain an indented mapping`);
      route = key;
      subsection = null;
      if (Object.hasOwn(document.routes, route)) fail(file, line, `duplicate route \`${route}\``);
      document.routes[route] = {};
      continue;
    }
    if (!route) fail(file, line, 'route field appears before a route name');

    if (indent === 4) {
      if (!ROUTE_FIELDS.has(key)) fail(file, line, `unknown route field \`${key}\``);
      if (Object.hasOwn(document.routes[route], key)) fail(file, line, `duplicate field \`${key}\``);
      if (key === 'cache') {
        if (valueText) fail(file, line, '`cache` must contain an indented mapping');
        document.routes[route].cache = {};
        subsection = 'cache';
      } else {
        document.routes[route][key] = key === 'methods' ? list(valueText, file, line) : scalar(valueText, file, line);
        subsection = null;
      }
      continue;
    }

    if (indent === 6 && subsection === 'cache') {
      if (!CACHE_FIELDS.has(key)) fail(file, line, `unknown cache field \`${key}\``);
      const cache = document.routes[route].cache;
      if (Object.hasOwn(cache, key)) fail(file, line, `duplicate cache field \`${key}\``);
      cache[key] = key === 'query' || key === 'vary' ? list(valueText, file, line) : scalar(valueText, file, line);
      continue;
    }
    fail(file, line, 'unexpected indentation or nesting');
  }

  return validatePolicy(document, undefined, file);
}

export function normalizeRouteName(name) {
  if (typeof name !== 'string' || !name.startsWith('/') || name.includes('?') || name.includes('#')) {
    throw new TanodNextError(`policy route ${JSON.stringify(name)} must be an absolute Next.js route name without a query or fragment`);
  }
  return name === '/' ? name : name.replace(/\/+$/, '');
}

function assertUnique(values, label, route) {
  const normalized = values.map((value) => value.toLowerCase());
  if (new Set(normalized).size !== normalized.length) {
    throw new TanodNextError(`policy route \`${route}\` repeats a ${label}`);
  }
}

export function buildRouteNames(build) {
  const handlers = new Set(
    Object.entries(build.appPaths)
      .filter(([file]) => file.endsWith('/route'))
      .map(([, name]) => normalizeRouteName(name)),
  );
  const names = new Set(
    [...build.staticRoutes, ...build.dynamicRoutes, ...(build.pagePaths ?? []).map((page) => ({ page }))]
      .map((entry) => entry.page)
      .filter((name) => name && !name.startsWith('/_'))
      .map(normalizeRouteName),
  );
  for (const handler of handlers) names.delete(handler);
  const prerendered = new Set(
    Object.keys(build.prerendered)
      .filter((name) => !name.startsWith('/_'))
      .map(normalizeRouteName),
  );
  for (const name of Object.keys(build.prerendered)) {
    if (!name.startsWith('/_')) names.add(normalizeRouteName(name));
  }
  return { pages: names, handlers, prerendered };
}

/** Validate a parsed or programmatically supplied policy, optionally against a build. */
export function validatePolicy(input, build, file = 'tanod.next.yaml') {
  if (!input || typeof input !== 'object' || Array.isArray(input)) fail(file, 0, 'policy must be a mapping');
  if (input.version !== POLICY_VERSION) {
    fail(file, 0, `policy schema version ${String(input.version)} is not supported; expected ${POLICY_VERSION}`);
  }
  if (!input.routes || typeof input.routes !== 'object' || Array.isArray(input.routes)) {
    fail(file, 0, '`routes` must be a mapping');
  }

  const output = { version: POLICY_VERSION, routes: {}, fingerprint: input.fingerprint ?? null };
  const normalizedNames = new Set();
  const catalog = build ? buildRouteNames(build) : null;
  for (const [originalName, source] of Object.entries(input.routes)) {
    const name = normalizeRouteName(originalName);
    if (normalizedNames.has(name)) fail(file, 0, `duplicate route after normalization: \`${name}\``);
    normalizedNames.add(name);
    if (!source || typeof source !== 'object' || Array.isArray(source)) fail(file, 0, `route \`${name}\` must be a mapping`);
    for (const field of Object.keys(source)) {
      if (!ROUTE_FIELDS.has(field)) fail(file, 0, `route \`${name}\` has unknown field \`${field}\``);
    }
    if (!['public', 'private'].includes(source.privacy)) {
      fail(file, 0, `route \`${name}\` must set privacy to \`public\` or \`private\``);
    }
    const route = { privacy: source.privacy };
    if (source.weight !== undefined) {
      if (!Number.isInteger(source.weight) || source.weight < 1 || source.weight > 65535) {
        fail(file, 0, `route \`${name}\` weight must be an integer from 1 to 65535`);
      }
      route.weight = source.weight;
    }
    if (source.priority !== undefined) {
      if (!['high', 'normal', 'low'].includes(source.priority)) fail(file, 0, `route \`${name}\` priority must be high, normal, or low`);
      route.priority = source.priority;
    }
    if (source.coalesce !== undefined) {
      if (typeof source.coalesce !== 'boolean') fail(file, 0, `route \`${name}\` coalesce must be true or false`);
      route.coalesce = source.coalesce;
    }
    if (source.methods !== undefined) {
      if (!Array.isArray(source.methods) || source.methods.length === 0) fail(file, 0, `route \`${name}\` methods must be a non-empty list`);
      route.methods = source.methods.map((method) => String(method).toUpperCase());
      if (route.methods.some((method) => !METHOD.test(method))) fail(file, 0, `route \`${name}\` contains an invalid HTTP method`);
      assertUnique(route.methods, 'method', name);
    }
    if (source.cache !== undefined) {
      if (!source.cache || typeof source.cache !== 'object' || Array.isArray(source.cache)) fail(file, 0, `route \`${name}\` cache must be a mapping`);
      for (const field of Object.keys(source.cache)) {
        if (!CACHE_FIELDS.has(field)) fail(file, 0, `route \`${name}\` cache has unknown field \`${field}\``);
      }
      const cache = {};
      for (const field of ['ttl', 'stale_if_error']) {
        if (source.cache[field] !== undefined) {
          if (typeof source.cache[field] !== 'string' || !DURATION.test(source.cache[field])) {
            fail(file, 0, `route \`${name}\` cache.${field} must be a positive duration such as \`2s\``);
          }
          cache[field] = source.cache[field];
        }
      }
      for (const field of ['query', 'vary']) {
        if (source.cache[field] !== undefined) {
          if (!Array.isArray(source.cache[field]) || source.cache[field].length === 0 || source.cache[field].some((value) => typeof value !== 'string' || !value)) {
            fail(file, 0, `route \`${name}\` cache.${field} must be a non-empty string list`);
          }
          cache[field] = [...source.cache[field]];
          assertUnique(cache[field], `cache.${field} value`, name);
        }
      }
      if (cache.vary?.some((header) => !HEADER.test(header) || ['cookie', 'authorization', 'user-agent', '*'].includes(header.toLowerCase()))) {
        fail(file, 0, `route \`${name}\` cache.vary contains an invalid, credential, or unbounded header`);
      }
      route.cache = cache;
    }
    if (route.privacy === 'private' && (route.cache !== undefined || route.coalesce === true)) {
      fail(file, 0, `route \`${name}\` is private and cannot enable cache or coalescing settings`);
    }
    if (route.privacy === 'public') {
      if (!route.cache?.ttl) fail(file, 0, `public route \`${name}\` must set cache.ttl`);
      if (route.methods?.some((method) => !SAFE_METHODS.has(method))) {
        fail(file, 0, `public route \`${name}\` declares an unsafe cacheable method`);
      }
    }
    if (catalog) {
      const matches = Number(catalog.pages.has(name)) + Number(catalog.handlers.has(name));
      if (matches !== 1) fail(file, 0, `route \`${name}\` must match exactly one build route; found ${matches}`);
      if (catalog.prerendered.has(name)) {
        fail(file, 0, `route \`${name}\` is already public because the build prerendered it; remove the redundant assertion`);
      }
      if (catalog.handlers.has(name) && route.privacy === 'public') {
        fail(file, 0, `route handler \`${name}\` cannot be declared public because its methods and data access are not proven by the build`);
      }
    }
    output.routes[name] = route;
  }
  return output;
}

function withFingerprint(policy, raw) {
  return { ...policy, fingerprint: createHash('sha256').update(raw).digest('hex') };
}

export async function readPolicy(file, build) {
  let raw;
  try {
    raw = await new Promise((resolve, reject) => readFile(file, 'utf8', (error, data) => error ? reject(error) : resolve(data)));
  } catch (cause) {
    throw new TanodNextError(`could not read policy ${file}`, { cause });
  }
  return validatePolicy(withFingerprint(parsePolicy(raw, file), raw), build, file);
}

export function readPolicySync(file, build) {
  let raw;
  try {
    raw = readFileSync(file, 'utf8');
  } catch (cause) {
    throw new TanodNextError(`could not read policy ${file}`, { cause });
  }
  return validatePolicy(withFingerprint(parsePolicy(raw, file), raw), build, file);
}

export function policyFingerprint(policy) {
  if (!policy) return 'none';
  return policy.fingerprint ?? createHash('sha256').update(JSON.stringify(policy)).digest('hex');
}
