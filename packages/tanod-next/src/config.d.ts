import type { GenerateOptions } from './index.js';

export interface WithTanodOptions extends GenerateOptions {
  /** Where to write the generated config. Default `tanod.yaml`. */
  out?: string;
  /** Next build output directory. Default `.next`. */
  distDir?: string;
  /** Run `tanod check` on the result; a rejection fails the build. */
  check?: boolean;
  /** Path to the tanod binary. Default `$TANOD_BIN`, else `tanod` on PATH. */
  tanodBin?: string;
  /** Assertion file read after the build. */
  policyFile?: string;
  /** Deployment identity artifact path, or false to omit it. */
  identityOut?: string | false;
  /** Suppress the success line. Failures are always reported. */
  silent?: boolean;
}

type NextConfigInput = Record<string, unknown> | ((phase: string, context: unknown) => unknown);

/** Wrap a Next config so a production build also writes Tanod's. */
export function withTanod(
  nextConfig?: NextConfigInput,
  options?: WithTanodOptions,
): (phase: string, context: unknown) => unknown;
