// record.mjs — render short.html to a 1080x1920 video (and review stills) with Playwright.
// usage: node record.mjs <ja|en> [stills] [starts=0,7,…] [total=53]
import { chromium } from 'playwright';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import fs from 'node:fs';

const lang = process.argv[2] || 'ja';
const stills = process.argv.includes('stills');
const kv = Object.fromEntries(process.argv.slice(3).filter(a => a.includes('=')).map(a => a.split('=')));
const starts = (kv.starts || '0,7,11,15.5,21,28.5,36.5,41,46.5');
const total = Number(kv.total || 53);
const here = path.dirname(fileURLToPath(import.meta.url));
const url = 'file://' + path.join(here, 'short.html') + `?lang=${lang}&starts=${starts}&total=${total}` + (stills ? '&still=1' : '');
const out = path.join(here, 'out'); fs.mkdirSync(out, { recursive: true });

const browser = await chromium.launch();
if (stills) {
  const page = await browser.newPage({ viewport: { width: 1080, height: 1920 }, deviceScaleFactor: 1 });
  await page.goto(url); await page.waitForTimeout(500);
  const ts = starts.split(',').map(Number).map(s => s + 3.5);
  for (const t of ts) {
    await page.evaluate((t) => window.seek(t), t); await page.waitForTimeout(1800);
    await page.screenshot({ path: path.join(out, `still-${lang}-${String(Math.round(t)).padStart(2, '0')}.png`) });
  }
} else {
  const ctx = await browser.newContext({ viewport: { width: 1080, height: 1920 }, deviceScaleFactor: 1,
    recordVideo: { dir: out, size: { width: 1080, height: 1920 } } });
  const page = await ctx.newPage();
  await page.goto(url);
  await page.waitForTimeout(total * 1000 + 400);
  const video = page.video();
  await ctx.close();
  fs.renameSync(await video.path(), path.join(out, `kioku-short-${lang}.webm`));
}
await browser.close();
console.log('done', lang, stills ? 'stills' : 'video', 'total', total);
