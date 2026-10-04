import { headers } from "next/headers";

export const dynamic = "force-dynamic";

export async function GET(request: Request) {
  const expected = process.env.TANOD_DOCTOR_TOKEN;
  if (!expected || request.headers.get("x-tanod-doctor-token") !== expected) {
    return new Response(null, { status: 404 });
  }
  const incoming = await headers();
  return Response.json(
    {
      host: incoming.get("host"),
      scheme: incoming.get("x-forwarded-proto"),
    },
    { headers: { "Cache-Control": "no-store" } },
  );
}
