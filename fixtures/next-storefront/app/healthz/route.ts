export async function GET() {
  return new Response(null, {
    status: 204,
    headers: {
      "Cache-Control": "no-store",
      "X-Tanod-Build-Id": process.env.TANOD_BUILD_ID ?? "unknown",
    },
  });
}
