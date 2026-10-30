import { readStats } from "@/lib/render";

export const dynamic = "force-dynamic";

// The origin's own count of renders in progress, for bench/showcase. Off unless
// TANOD_FIXTURE_STATS=1, so the fixture's other suites see no new surface.
//
// `reader` names an independent peak window: the showcase sampler and a
// Prometheus scrape can both poll without resetting each other's peak.
export async function GET(request: Request) {
  if (process.env.TANOD_FIXTURE_STATS !== "1") {
    return new Response(null, { status: 404 });
  }

  const url = new URL(request.url);
  const reader = url.searchParams.get("reader") ?? "default";
  if (!/^[a-z0-9-]{1,32}$/.test(reader)) {
    return new Response("invalid reader\n", { status: 400 });
  }

  const stats = readStats(reader);
  const headers = { "Cache-Control": "private, no-store" };

  if (url.searchParams.get("format") === "prometheus") {
    const label = `instance_id="${stats.instance.replace(/[^A-Za-z0-9_.-]/g, "_")}"`;
    const body = [
      "# HELP fixture_renders_active Renders in progress inside this Next.js process.",
      "# TYPE fixture_renders_active gauge",
      `fixture_renders_active{${label}} ${stats.active}`,
      "# HELP fixture_renders_window_peak Peak renders in progress since this reader last scraped.",
      "# TYPE fixture_renders_window_peak gauge",
      `fixture_renders_window_peak{${label}} ${stats.window_peak}`,
      "# HELP fixture_renders_total Renders started by this Next.js process.",
      "# TYPE fixture_renders_total counter",
      `fixture_renders_total{${label}} ${stats.total}`,
      "",
    ].join("\n");
    return new Response(body, {
      headers: { ...headers, "Content-Type": "text/plain; version=0.0.4" },
    });
  }

  return Response.json(stats, { headers });
}
