import { randomUUID } from "node:crypto";
import { hostname } from "node:os";

type FixtureStats = {
  active: number;
  peak: number;
  total: number;
};

type FixtureGlobal = typeof globalThis & {
  __tanodFixtureStats?: FixtureStats;
};

const fixtureGlobal = globalThis as FixtureGlobal;
const stats = (fixtureGlobal.__tanodFixtureStats ??= {
  active: 0,
  peak: 0,
  total: 0,
});

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
