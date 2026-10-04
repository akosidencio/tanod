import { TanodNextError } from './manifests.js';

function positiveInteger(value, name) {
  if (!Number.isSafeInteger(value) || value < 1) {
    throw new TanodNextError(`${name} must be a positive safe integer`);
  }
}

/** Resolve either a local ceiling or a conservative static group partition. */
export function resolveCapacity(options = {}, { requireExplicit = false } = {}) {
  const concurrency = options.concurrency ?? null;
  const globalMax = options.globalConcurrency ?? null;
  const replicas = options.replicas ?? null;
  const grouped = globalMax !== null || replicas !== null;

  if (grouped) {
    if (globalMax === null || replicas === null) {
      throw new TanodNextError('static partitioning requires both `globalConcurrency` and `replicas`');
    }
    if (concurrency !== null) {
      throw new TanodNextError('pass either `concurrency` or the global concurrency and replica count, not both');
    }
    positiveInteger(globalMax, 'globalConcurrency');
    positiveInteger(replicas, 'replicas');
    const localMax = Math.floor(globalMax / replicas);
    if (localMax < 1) {
      throw new TanodNextError(
        `globalConcurrency ${globalMax} cannot be partitioned across ${replicas} replicas`,
      );
    }
    return {
      concurrency: localMax,
      group: {
        globalMax,
        replicas,
        allocated: localMax * replicas,
        unallocated: globalMax - localMax * replicas,
      },
    };
  }

  if (concurrency === null) {
    if (requireExplicit) {
      throw new TanodNextError(
        'a complete configuration requires `concurrency`, or `globalConcurrency` with `replicas`',
      );
    }
    return { concurrency: 200, group: null };
  }
  positiveInteger(concurrency, 'concurrency');
  return { concurrency, group: null };
}
