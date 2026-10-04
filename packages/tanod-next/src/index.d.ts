/** Everything @tanod/next reads out of a Next.js build. */
export interface NextBuild {
  /** The Next build id, from `.next/BUILD_ID`. Becomes `deployment.id`. */
  buildId: string;
  deploymentId: string | null;
  identity: string;
  basePath: string;
  staticRoutes: Array<{ page: string }>;
  dynamicRoutes: Array<{ page: string }>;
  dataRoutes: Array<{ page?: string }>;
  prerendered: Record<string, { initialRevalidateSeconds?: number | false }>;
  appPaths: Record<string, string>;
  pagePaths: string[];
  manifestVersions: { routes: number; prerender: number | null };
  fingerprint: string;
}

export interface RouteAssertion {
  privacy: 'public' | 'private';
  weight?: number;
  priority?: 'high' | 'normal' | 'low';
  methods?: string[];
  coalesce?: boolean;
  cache?: {
    ttl?: string;
    stale_if_error?: string;
    query?: string[];
    vary?: string[];
  };
}

export interface NextPolicy {
  version: 1;
  routes: Record<string, RouteAssertion>;
  fingerprint?: string | null;
}

export interface GenerateOptions {
  /** Emit a `deployment:` block. Default true. */
  includeDeployment?: boolean;
  /** TTL suggested in the comment for opting a dynamic route into caching. */
  defaultTtl?: string;
  /** `stale_if_error` for prerendered routes. Default `1m`. */
  staleIfError?: string;
  /** Origin addresses. With at least one, a complete config is emitted. */
  upstreams?: string[];
  /** `origin.concurrency.max`. Required when writing a complete config. */
  concurrency?: number;
  /** One origin-work budget partitioned across the Tanod replica group. */
  globalConcurrency?: number;
  /** Maximum Tanod replicas sharing `globalConcurrency`. */
  replicas?: number;
  /** `origin.priorities.low`, which reserves the rest for page renders. */
  lowPriorityPercent?: number;
  /** Checked-in operator assertions for dynamic routes. */
  policy?: NextPolicy | null;
  /** Safe rollout stage. Default `cache`. */
  rollout?: 'observe' | 'protect' | 'coalesce' | 'cache';
}

export interface PurgeResult {
  purged: boolean;
  scope?: 'selective' | 'all';
  tags?: number;
  paths?: number;
  entries: number;
  bytes?: number;
  remaining_entries?: number;
  /** Number of Tanod replicas reached by a fan-out purge. */
  replicas?: number;
  /** Per-replica responses when more than one endpoint is configured. */
  results?: PurgeResult[];
}

export interface PurgerOptions {
  /** Tanod's **admin** listener, e.g. `http://127.0.0.1:9091`. */
  endpoint?: string;
  /** Every Tanod admin listener in a replica group. */
  endpoints?: readonly string[];
  /** Must match `cache.purge.token`. */
  token?: string;
  timeoutMs?: number;
  fetch?: typeof globalThis.fetch;
}

export interface RevalidateTagOptions extends PurgerOptions {
  /** Next cache-life profile. Defaults to `{ expire: 0 }` for immediate invalidation. */
  nextProfile?: string | { expire?: number };
}

export interface Purger {
  purgeTags(tags: readonly string[]): Promise<PurgeResult>;
  purgePaths(paths: readonly string[]): Promise<PurgeResult>;
  purge(what: { tags?: readonly string[]; paths?: readonly string[] }): Promise<PurgeResult>;
  purgeAll(): Promise<PurgeResult>;
}

export class TanodNextError extends Error {}

export function resolveCapacity(
  options?: Pick<GenerateOptions, 'concurrency' | 'globalConcurrency' | 'replicas'>,
  settings?: { requireExplicit?: boolean },
): {
  concurrency: number;
  group: null | {
    globalMax: number;
    replicas: number;
    allocated: number;
    unallocated: number;
  };
};

export function readBuild(distDir: string): Promise<NextBuild>;
export function generateConfig(build: NextBuild, options?: GenerateOptions): string;
export function inspectRoutes(build: NextBuild, policy?: NextPolicy | null): Array<Record<string, unknown>>;
export const POLICY_VERSION: 1;
export function parsePolicy(raw: string, file?: string): NextPolicy;
export function readPolicy(file: string, build?: NextBuild): Promise<NextPolicy>;
export function readPolicySync(file: string, build?: NextBuild): NextPolicy;
export function validatePolicy(policy: NextPolicy, build?: NextBuild, file?: string): NextPolicy;
export function formatInspection(build: NextBuild, policy?: NextPolicy | null, options?: { json?: boolean }): string;
export function explainRequest(build: NextBuild, policy: NextPolicy | null, options: { url?: string; path?: string; method?: string; headers?: string[] }): Record<string, unknown>;
export function doctor(build: NextBuild, options?: Record<string, unknown>): Promise<Record<string, unknown>>;
export function calibrate(options?: Record<string, unknown>): Promise<Record<string, unknown>>;
export function toGlob(page: string): string;
export function routeId(page: string, taken: Set<string>): string;
export function createPurger(options?: PurgerOptions): Purger;

/** `revalidateTag()` from `next/cache`, then the same tag in Tanod. */
export function revalidateTag(tag: string, options?: RevalidateTagOptions): Promise<PurgeResult>;
/** `revalidatePath()` from `next/cache`, then the same path in Tanod. */
export function revalidatePath(path: string, options?: PurgerOptions): Promise<PurgeResult>;

export const TANOD_SCHEMA_VERSION: number;
export const SUPPORTED_MANIFESTS: Readonly<Record<string, readonly number[] | null>>;
export const VERIFIED_NEXT_RELEASES: ReadonlyArray<{
  next: string;
  router: string;
  routesManifest: number;
  prerenderManifest: number;
}>;
