import { readFile } from "node:fs/promises";
import path from "node:path";

export const dynamic = "force-dynamic";

export async function GET() {
  try {
    const identity = JSON.parse(
      await readFile(path.join(process.cwd(), "tanod.deployment.json"), "utf8"),
    );
    return Response.json(identity, {
      headers: { "Cache-Control": "no-store" },
    });
  } catch {
    return Response.json(
      { error: "deployment identity unavailable" },
      { status: 503, headers: { "Cache-Control": "no-store" } },
    );
  }
}
