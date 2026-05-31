// tools/spike/bouygues-pfs-probe.mjs
// One-time RE helper. NOT shipped. Requires: npm i -g playwright && npx playwright install chrome
import { chromium } from 'playwright';
import { writeFileSync, mkdirSync } from 'node:fs';

const OUT = new URL('./out/', import.meta.url);
mkdirSync(OUT, { recursive: true });

const browser = await chromium.launch({ channel: 'chrome', headless: false });
const page = await browser.newPage();
const captured = [];

page.on('requestfinished', async (req) => {
  const url = req.url();
  if (!/kaltura\.com|bt-api-int\.bouyguestelecom\.fr/.test(url)) return;
  const res = await req.response();
  let body = null;
  try { body = await res.text(); } catch {}
  captured.push({
    url,
    method: req.method(),
    reqHeaders: req.headers(),     // includes Authorization: Basic … on bt-api-int
    reqBody: req.postData(),
    status: res.status(),
    resBody: body && body.length < 200_000 ? body : `<${body?.length} bytes>`,
  });
  writeFileSync(new URL('capture.json', OUT), JSON.stringify(captured, null, 2));
});

console.log('Log in, complete OTP, then START PLAYBACK on one channel. Ctrl+C when done.');
await page.goto('https://www.bouyguestelecom.fr/tv-direct');
await new Promise(() => {}); // keep open until Ctrl+C
