// shoot.mjs — diagram.html → out/kioku-flow-<lang>.png (1600x900 @2x)
import { chromium } from 'playwright';
import path from 'node:path'; import { fileURLToPath } from 'node:url';
const here = path.dirname(fileURLToPath(import.meta.url));
const browser = await chromium.launch();
for (const lang of ['ja', 'en']) {
  const page = await browser.newPage({ viewport: { width: 1600, height: 900 }, deviceScaleFactor: 2 });
  await page.goto('file://' + path.join(here, 'diagram.html') + `?lang=${lang}`);
  await page.waitForTimeout(400);
  await page.screenshot({ path: path.join(here, 'out', `kioku-flow-${lang}.png`) });
  await page.close();
}
await browser.close(); console.log('ok');
