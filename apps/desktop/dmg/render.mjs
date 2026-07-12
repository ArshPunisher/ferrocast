// Renders background.html to PNGs at 1× and 2× (render.sh joins them into one TIFF).
// Uses the Playwright that ferrocast-web already installs.
import { createRequire } from 'node:module'
import { fileURLToPath, pathToFileURL } from 'node:url'
import path from 'node:path'

const here = path.dirname(fileURLToPath(import.meta.url))
const web = path.resolve(here, '../../../../ferrocast-web/apps/dashboard/package.json')
const { chromium } = createRequire(web)('@playwright/test')
const out = process.argv[2] ?? here
const browser = await chromium.launch()
for (const scale of [1, 2]) {
  const page = await browser.newPage({ viewport: { width: 660, height: 400 }, deviceScaleFactor: scale })
  await page.goto(pathToFileURL(path.join(here, 'background.html')).href)
  // Fonts, then two frames so the script-drawn halftone has painted.
  await page.evaluate(() => document.fonts.ready.then(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))))
  await page.screenshot({ path: path.join(out, `bg${scale}.png`) })
  await page.close()
}
await browser.close()
