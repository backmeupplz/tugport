// Share cards (1200×630) for the docs pages, rendered with the Chrome on this machine into website/og/<slug>.png:
// `node docs/og.mjs` after adding or renaming a guide (CHROME=<path> if Chrome lives elsewhere). The cards are
// committed, so the Pages build needs no browser.
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { GUIDES } from "./guides.mjs";

const chrome = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const site = new URL("../website/", import.meta.url).pathname;
const esc = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
const cards = [
  { slug: "docs", path: "/docs/", title: "Tugport documentation", blurb: "The API reference, moving from Easypanel or Coolify, and how Tugport compares with other panels." },
  ...GUIDES.map((g) => ({ slug: g.slug, path: `/docs/${g.slug}/`, title: g.title, blurb: g.blurb })),
];
const card = (c) => `<!doctype html><html><head><meta charset="utf-8">
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500&family=JetBrains+Mono:wght@500&family=Space+Grotesk:wght@600;700&display=block" rel="stylesheet">
<style>
* { margin: 0; padding: 0; box-sizing: border-box; }
html, body { width: 1200px; height: 630px; overflow: hidden; }
body { display: flex; flex-direction: column; padding: 64px 72px; color: #E8EDF4; font-family: Inter, sans-serif;
  background: radial-gradient(760px 520px at 1080px 40px, rgba(255, 106, 61, 0.2), transparent 70%), #0A0E14; }
.brand { display: flex; align-items: center; gap: 14px; font: 700 34px/1 "Space Grotesk", sans-serif; letter-spacing: -0.02em; }
.brand img { width: 52px; height: 52px; }
.brand span.tag { margin-left: 8px; font: 500 22px/1 Inter, sans-serif; color: #FF8A64; padding: 8px 14px; border: 1.5px solid #5A2A1C; border-radius: 999px; }
h1 { margin-top: auto; font: 600 64px/1.06 "Space Grotesk", sans-serif; letter-spacing: -0.03em; text-wrap: balance; max-width: 1000px; }
p { margin-top: 22px; font-size: 26px; line-height: 1.42; color: #A2AEBE; text-wrap: balance; max-width: 980px; }
.url { margin-top: 30px; font: 500 22px/1 "JetBrains Mono", monospace; color: #FF8A64; }
</style></head><body>
<div class="brand"><img src="file://${site}logo.svg" alt=""><span>Tugport</span><span class="tag">Docs</span></div>
<h1>${esc(c.title)}</h1><p>${esc(c.blurb)}</p><div class="url">tugport.com${c.path}</div>
</body></html>`;

const tmp = mkdtempSync(join(tmpdir(), "tugport-og-"));
mkdirSync(join(site, "og"), { recursive: true });
for (const c of cards) {
  const html = join(tmp, `${c.slug}.html`), png = join(site, "og", `${c.slug}.png`);
  writeFileSync(html, card(c));
  execFileSync(chrome, ["--headless=new", "--disable-gpu", "--hide-scrollbars", "--force-device-scale-factor=1", "--window-size=1200,630",
    "--allow-file-access-from-files", "--virtual-time-budget=3000", `--screenshot=${png}`, `file://${html}`], { stdio: "ignore" });
  console.log(`og/${c.slug}.png ${Math.round(statSync(png).size / 1024)} KB`);
}
rmSync(tmp, { recursive: true, force: true });
