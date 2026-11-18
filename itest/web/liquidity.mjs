// The Liquidity tab end to end, in headless Chromium (fresh profile), against a real agent, a real
// bipnipd instance and real LND nodes on regtest (`itest/ln/run-web.sh` sets them up). What happens
// where the agent runs (pairing, approvals, blocks) is done by `hook.sh`, called here as a step.
//
// Usage: node liquidity.mjs <page origin> <second origin> <hook.sh> <bipnip-web dir> <out dir>
//
// Prints one JSON summary on stdout; every step is logged on stderr. Exits non-zero on the first
// thing that is not as it should be.

import { chromium } from 'playwright';
import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { randomBytes } from 'node:crypto';
import http from 'node:http';

const [base, second, hook, webDir, outDir] = process.argv.slice(2);
const require = createRequire(`${webDir}/package.json`);
const { finalizeEvent, generateSecretKey, getPublicKey } = require('nostr-tools');

const started = Date.now();
const log = (message) =>
  console.error(`[web ${new Date().toISOString().slice(11, 19)} +${Math.round((Date.now() - started) / 1000)}s] ${message}`);
const step = (...args) => {
  log(`hook ${args.join(' ')}`);
  return execFileSync('bash', [hook, ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] }).trim();
};
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const fail = (message) => {
  throw new Error(message);
};

// --- what the browser does that it should not -------------------------------------------------
const seen = {};
function watch(page, origin) {
  const record = (seen[origin] = { csp: [], offOrigin: [], errors: [] });
  page.on('console', (m) => {
    const text = m.text();
    if (/CSP-VIOLATION|Content[- ]Security[- ]Policy|Refused to/i.test(text)) record.csp.push(text.slice(0, 300));
    else if (m.type() === 'error') record.errors.push(text.slice(0, 200));
  });
  page.on('pageerror', (e) => record.errors.push(String(e).slice(0, 200)));
  page.on('request', (r) => {
    const url = r.url();
    if (!url.startsWith(origin) && !url.startsWith('data:') && !url.startsWith('blob:')) {
      record.offOrigin.push(url.slice(0, 120));
    }
  });
  return page.addInitScript(() => {
    document.addEventListener('securitypolicyviolation', (e) => {
      console.error(`CSP-VIOLATION ${e.violatedDirective} ${e.blockedURI}`);
    });
  });
}

// --- requests made directly, as a hostile page or another device could ------------------------
function post(origin, headers, body) {
  return new Promise((resolve, reject) => {
    const url = new URL('/agent/call', base);
    const req = http.request(
      url,
      { method: 'POST', headers: { 'content-type': 'application/json', ...(origin ? { origin } : {}), ...headers } },
      (res) => {
        let text = '';
        res.on('data', (chunk) => (text += chunk));
        res.on('end', () => resolve({ status: res.statusCode, text }));
      },
    );
    req.on('error', reject);
    req.end(body);
  });
}

function sign(secret, method, params, at, agent) {
  const tags = [['nonce', randomBytes(16).toString('hex')]];
  if (agent) tags.push(['p', agent]);
  return JSON.stringify(
    finalizeEvent(
      { kind: 24735, created_at: at, tags, content: JSON.stringify({ v: 1, method, params }) },
      secret,
    ),
  );
}

function finalOf(text) {
  const lines = text.split('\n').filter((line) => line.trim() !== '');
  return JSON.parse(lines[lines.length - 1]);
}

const now = () => Math.floor(Date.now() / 1000);

async function ask(secret, agent, method, params = {}, at = now()) {
  const { status, text } = await post(new URL(base).origin, {}, sign(secret, method, params, at, agent));
  if (status !== 200) fail(`${method}: the host answered ${status}: ${text}`);
  return finalOf(text);
}

// --- the page ---------------------------------------------------------------------------------
async function phase(page) {
  return await page.locator('#bipnip-liquidity').getAttribute('data-phase');
}

async function until(what, seconds, test, between) {
  for (let i = 0; i < seconds; i += 2) {
    if (await test()) return;
    if (between) await between();
    await sleep(2000);
  }
  fail(`${what}: not within ${seconds} s`);
}

async function refreshBookUntil(page, selector, seconds) {
  await until(`${selector} on the book`, seconds, async () => (await page.locator(selector).count()) > 0, async () => {
    // While a read is under way the button says so and cannot be pressed: try again next round.
    await page.getByRole('button', { name: /^refresh$/i }).first().click({ timeout: 5000 }).catch(() => {});
  });
}

async function approvalShown(page) {
  const text = page.locator('pre', { hasText: /bipnip approve [0-9a-f]+/ });
  await text.first().waitFor({ timeout: 90_000 });
  const line = await text.first().textContent();
  return line.match(/bipnip approve ([0-9a-f]+)/)[1];
}

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1366, height: 900 } });
const page = await context.newPage();
await watch(page, new URL(base).origin);
const shot = (name) => page.screenshot({ path: `${outDir}/${name}.png`, fullPage: true });
const summary = {};

try {
  // 1. Pairing: closed until the operator opens it; then the code is typed where the agent runs.
  await page.goto(base, { waitUntil: 'domcontentloaded' });
  await page.waitForTimeout(5000);
  await page.getByRole('tab', { name: /liquidity/i }).click({ timeout: 60_000 });
  await page.locator('#bipnip-liquidity').waitFor();
  await page.getByLabel(/name of this page/i).fill('browser');
  await page.getByRole('button', { name: /ask to pair/i }).click();
  await page.getByText(/pairing is not open/i).waitFor({ timeout: 30_000 });
  log('pairing refused while closed');
  step('pair-open');
  await page.getByRole('button', { name: /ask to pair/i }).click();
  await page.locator('#bipnip-pair-code').waitFor({ timeout: 30_000 });
  const code = (await page.locator('#bipnip-pair-code').textContent()).trim();
  log(`the page shows code ${code}`);
  await shot('1-pair');
  step('pair-check', code);
  step('pair-approve', code);
  await until('paired', 30, async () => (await phase(page)) === 'paired');
  const secretHex = await page.evaluate(() => localStorage.getItem('bipnip_session_key'));
  const agent = await page.evaluate(() => localStorage.getItem('bipnip_agent'));
  const secret = Uint8Array.from(Buffer.from(secretHex, 'hex'));
  summary.session = getPublicKey(secret);
  log(`paired: session ${summary.session.slice(0, 12)}…, agent ${agent.slice(0, 12)}…`);

  // 2. A lease from bob's agent, taken from the page, approved where alice's agent runs.
  const order = step('bob-offer');
  await refreshBookUntil(page, `#bipnip-leases tr[data-order="${order}"]`, 120);
  const row = page.locator(`#bipnip-leases tr[data-order="${order}"]`);
  const fresh = await row.locator('.bipnip-record').textContent();
  if (!/new/.test(fresh)) fail(`bob's record before any lease: ${fresh}`);
  await shot('2-book');
  await row.getByRole('button', { name: /^take$/i }).click();
  await page.getByLabel(/address of your node/i).fill('127.0.0.1:9735');
  await page.getByRole('button', { name: /ask to take/i }).click();
  const takeApproval = await approvalShown(page);
  log(`the take is parked as ${takeApproval}`);
  await shot('3-parked');
  step('approve', takeApproval);
  await page.getByRole('button', { name: /^close$/i }).click();
  await page.getByRole('tab', { name: /my runs/i }).click();
  const buyer = page.locator('.bipnip-run[data-run^="buyer-"]');
  await until('the lease active', 240, async () => (await buyer.getAttribute('data-state').catch(() => null)) === 'active', () => step('mine', '1'));
  log('the lease is active');
  await shot('4-active');
  await until('the lease complete', 300, async () => (await buyer.getAttribute('data-state').catch(() => null)) === 'complete', () => step('mine', '1'));
  log('the lease ran its length');

  // 3. Bob's next offer shows the lease he kept.
  const order2 = step('bob-offer');
  await page.getByRole('tab', { name: /^book$/i }).click();
  const row2 = page.locator(`#bipnip-leases tr[data-order="${order2}"]`);
  await refreshBookUntil(page, `#bipnip-leases tr[data-order="${order2}"]`, 120);
  await until('bob\'s record', 120, async () => /1 kept/.test(await row2.locator('.bipnip-record').textContent()), async () => {
    await page.getByRole('button', { name: /^refresh$/i }).first().click({ timeout: 5000 }).catch(() => {});
  });
  summary.record = await row2.locator('.bipnip-record').textContent();
  log(`bob's record: ${summary.record}`);
  await shot('5-record');

  // 4. A ring bob proposes, joined from the page.
  const ring = step('bob-ring');
  await refreshBookUntil(page, `#bipnip-rings tr[data-order="${ring}"]`, 120);
  await page.locator(`#bipnip-rings tr[data-order="${ring}"]`).getByRole('button', { name: /^join$/i }).click();
  await page.getByLabel(/address of your node/i).fill('127.0.0.1:9735');
  await page.getByRole('button', { name: /ask to join/i }).click();
  const joinApproval = await approvalShown(page);
  step('approve', joinApproval);
  await page.getByRole('button', { name: /^close$/i }).click();
  await page.getByRole('tab', { name: /my runs/i }).click();
  const member = page.locator('.bipnip-run[data-run^="ring-"]');
  await until('the ring opened', 400, async () => (await member.getAttribute('data-state').catch(() => null)) === 'success', () => step('mine', '1'));
  log('the ring opened both ways');
  await shot('6-ring');

  // 5. What is refused, asked directly as a hostile page or another device could.
  const origin = new URL(base).origin;
  const crossSite = await post('http://evil.example', {}, sign(secret, 'status', {}, now(), agent));
  if (crossSite.status !== 403) fail(`a cross-site request: ${crossSite.status}`);
  const once = sign(secret, 'status', {}, now(), agent);
  const first = finalOf((await post(origin, {}, once)).text);
  const replay = finalOf((await post(origin, {}, once)).text);
  if (first.ok !== true || replay.error?.code !== 'refused') fail(`a replay: ${JSON.stringify(replay)}`);
  const ahead = await ask(secret, agent, 'status', {}, now() + 120);
  if (ahead.error?.code !== 'stale') fail(`a request dated ahead: ${JSON.stringify(ahead)}`);
  for (const [method, params] of [
    ['approve', { approval: takeApproval }],
    ['binding.publish', {}],
    ['record', { node: '0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798' }],
    ['sessions', {}],
    ['pair.open', {}],
  ]) {
    const refused = await ask(secret, agent, method, params);
    if (refused.error?.code !== 'refused') fail(`${method} from the page: ${JSON.stringify(refused)}`);
  }
  log('cross-site, replayed, future-dated and operator requests refused');

  // A page paired to look only cannot ask for a run.
  const reader = generateSecretKey();
  step('pair-open');
  const readerAsk = await ask(reader, undefined, 'pair.request', { label: 'reader' });
  if (readerAsk.ok !== true) fail(`a second page could not ask to pair: ${JSON.stringify(readerAsk)}`);
  step('pair-approve-read', readerAsk.result.code);
  const looking = await ask(reader, agent, 'lease.take', { order_id: order2, capacity_sat: 300000 });
  if (looking.error?.code !== 'refused') fail(`a take from a read session: ${JSON.stringify(looking)}`);

  // Three requests may wait; the fourth is refused.
  for (let i = 0; i < 3; i++) {
    if (i > 0) await sleep(11_000);
    const parked = await ask(secret, agent, 'lease.take', { order_id: order2, capacity_sat: 300000 });
    if (parked.error?.code !== 'needs-approval') fail(`take ${i + 1}: ${JSON.stringify(parked)}`);
  }
  await sleep(11_000);
  const fourth = await ask(secret, agent, 'lease.take', { order_id: order2, capacity_sat: 300000 });
  if (fourth.error?.code !== 'refused' || !/waiting/.test(fourth.error.message)) {
    fail(`a fourth waiting request: ${JSON.stringify(fourth)}`);
  }
  log('a read session and a fourth waiting request refused');

  // Revoked: unpaired from then on, and what it had waiting is gone.
  step('revoke', summary.session);
  const after = await ask(secret, agent, 'status');
  if (after.error?.code !== 'unpaired') fail(`after revoke: ${JSON.stringify(after)}`);
  step('approvals-empty');
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForTimeout(3000);
  await page.getByRole('tab', { name: /liquidity/i }).click();
  await until('the page asks to pair again', 30, async () => (await phase(page)) === 'unpaired');
  log('revoked: the page is unpaired again');

  // 6. The other origin: loopback, a secure context. It is not paired (its storage is its own).
  const page2 = await context.newPage();
  await watch(page2, new URL(second).origin);
  await page2.goto(second, { waitUntil: 'domcontentloaded' });
  await page2.waitForTimeout(5000);
  await page2.getByRole('tab', { name: /liquidity/i }).click({ timeout: 60_000 });
  await page2.locator('#bipnip-liquidity[data-phase="unpaired"]').waitFor({ timeout: 30_000 });
  await page2.screenshot({ path: `${outDir}/7-loopback.png`, fullPage: true });
} catch (error) {
  log(`FAILED: ${error.message}`);
  await shot('failure').catch(() => {});
  summary.failed = error.message;
}
await browser.close();

summary.origins = seen;
summary.csp_violations = Object.values(seen).reduce((n, s) => n + s.csp.length, 0);
summary.off_origin_requests = Object.values(seen).reduce((n, s) => n + s.offOrigin.length, 0);
console.log(JSON.stringify(summary));
process.exit(summary.failed || summary.csp_violations || summary.off_origin_requests ? 1 : 0);
