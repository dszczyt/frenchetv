// tools/spike/bouygues-pfs-probe.mjs
// One-time RE helper. NOT shipped. Requires: npm i -g playwright && npx playwright install chrome
import { chromium } from 'playwright';
import { writeFileSync, mkdirSync } from 'node:fs';

const OUT = new URL('./out/', import.meta.url);
mkdirSync(OUT, { recursive: true });

// Fresh context (no persisted session) so the Kaltura-KS mint happens DURING capture.
const browser = await chromium.launch({ channel: 'chrome', headless: false });
const context = await browser.newContext();
const page = await context.newPage();
const captured = [];

// Capture Kaltura, the bt-api-int gateway, AND bouyguestelecom.fr (the host that
// exchanges the OAuth/Keycloak token for the entitled Kaltura KS).
const HOSTS = /kaltura\.com|bt-api-int\.bouyguestelecom\.fr|(^|\/\/)([a-z0-9.-]*\.)?bouyguestelecom\.fr/;

page.on('requestfinished', async (req) => {
  const url = req.url();
  if (!HOSTS.test(url)) return;
  const res = await req.response();
  let body = null;
  try { body = await res.text(); } catch {}
  const hasKs = body ? /"ks"\s*:\s*"[^"]{20,}"|djJ8/.test(body) : false;
  captured.push({
    url,
    method: req.method(),
    reqHeaders: req.headers(),     // includes Authorization on bt-api-int
    reqBody: req.postData(),
    status: res.status(),
    hasKs,                          // flags the response that mints/echoes a KS
    resBody: body && body.length < 5_000_000 ? body : `<${body?.length} bytes>`,
  });
  writeFileSync(new URL('capture.json', OUT), JSON.stringify(captured, null, 2));
});

console.log('LOG OUT first if already logged in, then LOG IN fresh + OTP, then START PLAYBACK on one channel. Ctrl+C when done.');
await page.goto('https://www.bouyguestelecom.fr/tv-direct');
await new Promise(() => {}); // keep open until Ctrl+C
