// Loads the bipnip-web UI through bipnip-webhost in headless Chromium, waits for the live order book
// (fetched from RoboSats coordinators over Tor by the host), and records what the browser saw:
// rows rendered, CSP violations, failed requests, page errors. Writes a screenshot.
//
// Usage: node shot.mjs <url> <png out>

import { chromium } from 'playwright';

const [url, out] = process.argv.slice(2);
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1366, height: 860 } });
const csp = [];
const failed = [];
const errors = [];
const offOrigin = [];
const non2xx = [];
const consoleAll = [];
const origin = new URL(url).origin;

page.on('response', (r) => {
  if (r.status() >= 300) non2xx.push(`${r.status()} ${new URL(r.url()).pathname}`);
});
page.on('console', (m) => {
  const t = m.text();
  consoleAll.push(`${m.type()}: ${t.slice(0, 160)}`);
  if (/Content[- ]Security[- ]Policy|Refused to/i.test(t)) csp.push(t.slice(0, 300));
  else if (m.type() === 'error') errors.push(t.slice(0, 200));
});
page.on('pageerror', (e) => errors.push(String(e).slice(0, 200)));
page.on('request', (r) => {
  const u = r.url();
  if (!u.startsWith(origin) && !u.startsWith('data:') && !u.startsWith('blob:')) offOrigin.push(u.slice(0, 120));
});
page.on('requestfailed', (r) =>
  failed.push(`${r.method()} ${new URL(r.url()).pathname} ${r.failure()?.errorText ?? ''}`),
);

await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 60_000 });
let rows = 0;
const started = Date.now();
while (Date.now() - started < 300_000) {
  rows = await page.locator('[role="row"]').count();
  if (rows > 3) break;
  await page.waitForTimeout(5_000);
}
await page.waitForTimeout(8_000);
rows = await page.locator('[role="row"]').count();
await page.screenshot({ path: out });
console.log(
  JSON.stringify({
    rows,
    seconds: Math.round((Date.now() - started) / 1000),
    csp_violations: csp.length,
    csp: csp.slice(0, 10),
    off_origin_requests: offOrigin.length,
    off_origin: offOrigin.slice(0, 10),
    failed: failed.slice(0, 20),
    errors: errors.slice(0, 10),
    non2xx: non2xx.slice(0, 20),
    console: consoleAll.slice(0, 25),
  }),
);
await browser.close();
