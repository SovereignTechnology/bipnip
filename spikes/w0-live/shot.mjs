// Loads the bipnip-web UI through bipnip-webhost in headless Chromium the way a user does: open "/"
// (the garage), then click the Offers tab and wait for the live order book, which the host fetches
// from RoboSats coordinators over Tor. Records what the browser saw (rows, CSP violations,
// off-origin requests, failed and non-2xx requests, console) and writes two screenshots.
//
// Usage: node shot.mjs <base url> <garage png> <offers png>

import { chromium } from 'playwright';

const [base, garagePng, offersPng] = process.argv.slice(2);
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1366, height: 860 } });
const origin = new URL(base).origin;
const csp = [];
const failed = [];
const errors = [];
const offOrigin = [];
const non2xx = [];
const consoleAll = [];

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

const started = Date.now();
await page.goto(`${origin}/`, { waitUntil: 'domcontentloaded', timeout: 60_000 });
await page.waitForTimeout(15_000);
await page.screenshot({ path: garagePng });

// RoboSats starts in Legacy garage mode, where the Offers route redirects to the garage. Switching the
// toggle to Garage Key mode is a UI mode only: no key, no robot, nothing sent to a coordinator.
let garageKeyMode = false;
try {
  await page.getByRole('button', { name: /garage key/i }).first().click({ timeout: 15_000 });
  // "Change Robot Generation Mode?" — confirming clears the (empty) local garage and switches mode.
  await page.getByRole('button', { name: /confirm and restart/i }).click({ timeout: 15_000 });
  await page.waitForTimeout(5_000);
  garageKeyMode = !(await page.getByText(/You are in Legacy mode/i).count());
} catch {
  // keep going: the Offers click below reports whether the book became reachable
}
const offersTab = page.getByRole('button', { name: /offers/i }).or(page.getByRole('tab', { name: /offers/i }));
let clicked = false;
try {
  await offersTab.first().click({ timeout: 15_000 });
  clicked = true;
} catch {
  await page.getByText(/^offers$/i).first().click({ timeout: 15_000 }).then(
    () => (clicked = true),
    () => {},
  );
}

let rows = 0;
const t0 = Date.now();
while (Date.now() - t0 < 240_000) {
  rows = await page.locator('[role="row"]').count();
  if (rows > 3) break;
  await page.waitForTimeout(5_000);
}
await page.waitForTimeout(8_000);
rows = await page.locator('[role="row"]').count();
const text = (await page.locator('body').innerText()).replace(/\s+/g, ' ').slice(0, 400);
await page.screenshot({ path: offersPng });

console.log(
  JSON.stringify({
    garage_key_mode: garageKeyMode,
    clicked_offers: clicked,
    rows,
    seconds: Math.round((Date.now() - started) / 1000),
    csp_violations: csp.length,
    csp: csp.slice(0, 10),
    off_origin_requests: offOrigin.length,
    off_origin: offOrigin.slice(0, 10),
    failed: failed.slice(0, 20),
    non2xx: non2xx.slice(0, 20),
    errors: errors.slice(0, 10),
    console: consoleAll.slice(0, 25),
    text,
  }),
);
await browser.close();
