import { chromium } from "../bench/browser/node_modules/playwright/index.mjs";

const base = process.env.GRAFANA_URL ?? "http://127.0.0.1:13000";
const dashboard = `${base}/d/tanod-overview/tanod?orgId=1&from=now-5m&to=now&refresh=5s&kiosk`;
const overview = process.env.DASHBOARD_SCREENSHOT ?? "assets/tanod-dashboard.png";
const full = process.env.DASHBOARD_FULL_SCREENSHOT ?? "assets/tanod-dashboard-full.png";

const browser = await chromium.launch({ headless: true });
try {
  const page = await browser.newPage({
    // A tall viewport keeps Grafana's lazy panels mounted for the full capture.
    viewport: { width: 1600, height: 1900 },
    deviceScaleFactor: 1,
  });
  await page.goto(dashboard, { waitUntil: "networkidle", timeout: 60_000 });
  await page.getByText("Is this fleet healthy?", { exact: true }).waitFor({ timeout: 60_000 });
  await page.waitForTimeout(10_000);
  await page.screenshot({
    path: overview,
    clip: { x: 0, y: 0, width: 1600, height: 1000 },
  });
  await page.screenshot({ path: full, fullPage: true });
  console.log(`captured ${overview} and ${full}`);
} finally {
  await browser.close();
}
