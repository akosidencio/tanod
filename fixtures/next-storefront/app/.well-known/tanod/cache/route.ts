import { randomUUID } from "node:crypto";
import { revalidateTag, unstable_cache } from "next/cache";

export const dynamic = "force-dynamic";

const probe = unstable_cache(
  async () => ({ generation: randomUUID(), created_at: Date.now() }),
  ["tanod-reference-cache-probe"],
  { tags: ["tanod-reference-cache-probe"], revalidate: 3600 },
);

function authorized(request: Request) {
  const expected = process.env.TANOD_DOCTOR_TOKEN;
  return expected && request.headers.get("x-tanod-doctor-token") === expected;
}

export async function GET(request: Request) {
  if (!authorized(request)) return new Response(null, { status: 404 });
  return Response.json(await probe(), { headers: { "Cache-Control": "no-store" } });
}

export async function POST(request: Request) {
  if (!authorized(request)) return new Response(null, { status: 404 });
  revalidateTag("tanod-reference-cache-probe", { expire: 0 });
  return Response.json({ invalidated: true }, { headers: { "Cache-Control": "no-store" } });
}
