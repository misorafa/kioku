// record.mjs — render short.html to a 1080x1920 video (and review stills) with Playwright.
// usage: node record.mjs <ja|en> [stills]   → out/kioku-short-<lang>.webm (+ ffmpeg to mp4 outside)
import { chromium } from 'playwright';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import fs from 'node:fs';

const lang = process.argv[2] || 'ja';
const stills = process.argv[3] === 'stills';
const here = path.dirname(fileURLToPath(import.meta.url));
const url = 'file://' + path.join(here, 'short.html') + `?lang=${lang}` + (stills ? '&still=1' : '');
const out = path.join(here, 'out'); fs.mkdirSync(out, { recursive: true });
const TOTAL_MS = 58_000;

const browser = await chromium.launch();
if (stills) {
  const page = await browser.newPage({ viewport: { width: 1080, height: 1920 }, deviceScaleFactor: 1 });
  await page.goto(url); await page.waitForTimeout(500);
  for (const t of [3, 6, 11, 16, 26, 34, 42, 49, 56]) {
    await page.evaluate((t) => window.seek(t), t); await page.waitForTimeout(1600);
    await page.screenshot({ path: path.join(out, `still-${lang}-${String(t).padStart(2, '0')}.png`) });
  }
} else {
  const ctx = await browser.newContext({ viewport: { width: 1080, height: 1920 }, deviceScaleFactor: 1,
    recordVideo: { dir: out, size: { width: 1080, height: 1920 } } });
  const page = await ctx.newPage();
  await page.goto(url);
  await page.waitForTimeout(TOTAL_MS);
  const video = page.video();
  await ctx.close();
  const p = await video.path();
  fs.renameSync(p, path.join(out, `kioku-short-${lang}.webm`));
}
await browser.close();
console.log('done', lang, stills ? 'stills' : 'video');
