import { randomUUID } from "node:crypto";
import { hostname } from "node:os";

type FixtureStats = {
  active: number;
  peak: number;
  total: number;
};

type FixtureGlobal = typeof globalThis & {
  __tanodFixtureStats?: FixtureStats;
  __tanodFixtureWindows?: Map<string, number>;
};

const fixtureGlobal = globalThis as FixtureGlobal;
const stats = (fixtureGlobal.__tanodFixtureStats ??= {
  active: 0,
  peak: 0,
  total: 0,
});

// Peak concurrency since each named reader last asked. A sampler polling every
// few hundred milliseconds would otherwise miss a peak that rose and fell
// between two polls, and the lifetime `peak` includes warmup.
const windows = (fixtureGlobal.__tanodFixtureWindows ??= new Map());

export type StatsSnapshot = FixtureStats & {
  instance: string;
  window_peak: number;
};

export function readStats(reader: string): StatsSnapshot {
  // Readers are a handful of fixed names; never let the map grow per request.
  if (!windows.has(reader) && windows.size >= 8) {
    windows.clear();
  }
  const windowPeak = Math.max(windows.get(reader) ?? 0, stats.active);
  windows.set(reader, stats.active);
  return {
    ...stats,
    instance: process.env.INSTANCE_ID || hostname(),
    window_peak: windowPeak,
  };
}

export type RenderToken = {
  id: string;
  instance: string;
  route: string;
  startedAt: number;
};

export function beginRender(route: string): RenderToken {
  stats.active += 1;
  stats.total += 1;
  stats.peak = Math.max(stats.peak, stats.active);
  for (const [reader, peak] of windows) {
    if (stats.active > peak) windows.set(reader, stats.active);
  }

  const token = {
    id: randomUUID(),
    instance: process.env.INSTANCE_ID || hostname(),
    route,
    startedAt: Date.now(),
  };

  console.log(
    JSON.stringify({
      event: "render_start",
      render_id: token.id,
      instance: token.instance,
      route,
      active: stats.active,
      peak: stats.peak,
      total: stats.total,
    }),
  );

  return token;
}

export function finishRender(token: RenderToken): void {
  stats.active = Math.max(0, stats.active - 1);
  console.log(
    JSON.stringify({
      event: "render_end",
      render_id: token.id,
      instance: token.instance,
      route: token.route,
      duration_ms: Date.now() - token.startedAt,
      active: stats.active,
      peak: stats.peak,
      total: stats.total,
    }),
  );
}

export function configuredDelay(fallback = 350): number {
  const parsed = Number(process.env.RENDER_DELAY_MS);
  return Number.isFinite(parsed) && parsed >= 0 ? parsed : fallback;
}

export async function delay(milliseconds: number): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, milliseconds));
}
