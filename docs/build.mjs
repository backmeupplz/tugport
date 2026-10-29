// Renders the Markdown docs into tugport.com pages: `cd docs && npm ci && node build.mjs`.
// Writes website/docs/ (not in git): a page per guide, an index, and the Markdown itself for
// agents. The Pages workflow runs it on every push that touches docs/ or website/.
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { Marked } from "marked";
import { GUIDES } from "./guides.mjs";

const REPO = "https://github.com/backmeupplz/tugport/blob/main";
const out = new URL("../website/docs/", import.meta.url);
const esc = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

/** GitHub's heading anchors: lowercase, punctuation dropped, spaces to dashes, repeats numbered. */
function slugger() {
  const seen = new Map();
  return (text) => {
    const base = text.toLowerCase().trim().replace(/<[^>]+>/g, "").replace(/[^\p{L}\p{N}\- ]/gu, "").replace(/ /g, "-");
    const n = seen.get(base) ?? 0;
    seen.set(base, n + 1);
    return n ? `${base}-${n}` : base;
  };
}

/** Links between guides go to their pages, links into the repository to GitHub. */
function href(link) {
  if (/^([a-z]+:|#|\/)/i.test(link)) return link;
  const [path, hash = ""] = link.split("#");
  const guide = GUIDES.find((g) => path.endsWith(g.file));
  if (guide) return `/docs/${guide.slug}/${hash ? `#${hash}` : ""}`;
  return `${REPO}/${new URL(path, "https://x/docs/").pathname.slice(1)}${hash ? `#${hash}` : ""}`;
}

function render(markdown) {
  const slug = slugger();
  const toc = [];
  const marked = new Marked({
    gfm: true,
    renderer: {
      heading({ tokens, depth }) {
        const html = this.parser.parseInline(tokens);
        const id = slug(html);
        if (depth === 2 || depth === 3) toc.push({ depth, id, html });
        if (depth === 1) return "";
        // tabindex -1: a mouse shortcut, kept out of the tab order and the accessibility tree
        return `<h${depth} id="${id}"><a class="anchor" href="#${id}" aria-hidden="true" tabindex="-1">#</a>${html}</h${depth}>\n`;
      },
      link({ href: link, title, tokens }) {
        return `<a href="${esc(href(link))}"${title ? ` title="${esc(title)}"` : ""}>${this.parser.parseInline(tokens)}</a>`;
      },
      // Tables in the Markdown read well on GitHub; on the site they become what reads well on a phone too:
      // Method/Path/Notes tables a list of endpoints, two-column tables a definition list, and anything wider a
      // table whose rows turn into labelled cards on narrow screens (docs.css), with Tugport's column tinted.
      table({ header, rows }) {
        const cell = (c) => this.parser.parseInline(c.tokens);
        const names = header.map((h) => h.text.trim());
        if (names[0] === "Method" && names[1] === "Path") {
          return `<dl class="endpoints">\n${rows.map((r) => `<div><dt><span class="method">${cell(r[0])}</span> ${cell(r[1])}</dt>` +
            `<dd>${r.slice(2).map(cell).join(" ")}</dd></div>`).join("\n")}\n</dl>\n`;
        }
        if (names.length === 2) {
          return `<dl class="defs">\n${rows.map((r) => `<div><dt>${cell(r[0])}</dt><dd>${cell(r[1])}</dd></div>`).join("\n")}\n</dl>\n`;
        }
        const ours = names.indexOf("Tugport");
        return `<div class="table"><table class="grid">\n<thead><tr>${header.map((h, i) => `<th scope="col"${i === ours ? ' class="ours"' : ""}>${cell(h)}</th>`).join("")}</tr></thead>\n<tbody>\n${
          rows.map((r) => `<tr>${r.map((c, i) => i === 0 ? `<th scope="row">${cell(c)}</th>` : `<td data-label="${esc(names[i])}"${i === ours ? ' class="ours"' : ""}><span>${cell(c)}</span></td>`).join("")}</tr>`).join("\n")}\n</tbody></table></div>\n`;
      },
    },
  });
  return { html: marked.parse(markdown), toc };
}

const CSS = readFileSync(new URL("docs.css", import.meta.url), "utf8");
// The @font-face rules, shared with the landing page so both use the same self-hosted files.
const FONTS = readFileSync(new URL("../website/index.html", import.meta.url), "utf8").match(/(@font-face[^<]*)<\/style>/)[1];
const page = ({ title, description, path, body, image, alt, ld }) => `<!doctype html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>${esc(title)} · Tugport</title>
<meta name="description" content="${esc(description)}">
<link rel="canonical" href="https://tugport.com${path}">
<meta property="og:type" content="article">
<meta property="og:site_name" content="Tugport">
<meta property="og:url" content="https://tugport.com${path}">
<meta property="og:title" content="${esc(title)}">
<meta property="og:description" content="${esc(description)}">
<meta property="og:image" content="https://tugport.com/og/${image}.png">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta property="og:image:alt" content="${esc(alt)}">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:title" content="${esc(title)}">
<meta name="twitter:description" content="${esc(description)}">
<meta name="twitter:image" content="https://tugport.com/og/${image}.png">
<meta name="twitter:image:alt" content="${esc(alt)}">
<link rel="icon" href="/logo.svg" type="image/svg+xml">
<link rel="icon" href="/favicon.ico" sizes="32x32">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
<meta name="theme-color" content="#0A0E14" media="(prefers-color-scheme: dark)">
<meta name="theme-color" content="#F7F6F2" media="(prefers-color-scheme: light)">
<link rel="preload" href="/fonts/inter.woff2" as="font" type="font/woff2" crossorigin>
<link rel="preload" href="/fonts/space-grotesk.woff2" as="font" type="font/woff2" crossorigin>
<!-- Self-hosted fonts (Google Fonts' latin variable files, cut to the weights and characters the site uses), preloaded
     and declared inline with font-display: optional, so they are in place for the first paint and never move the text.
     JetBrains Mono isn't preloaded: its 26 KB counted against the first paint on PageSpeed's slow phone (docs/api 99), and
     a first visit's code in the system monospace looks nearly the same. -->
<style>${FONTS}</style>
<style>${CSS}</style>
<script defer src="https://lumen.borodutch.com/l.js"></script>${ld ? `\n<script type="application/ld+json">${JSON.stringify(ld).replace(/</g, "\\u003c")}</script>` : ""}
</head>
<body>
<header class="top">
  <div class="wrap">
    <nav aria-label="Main">
      <a class="brand" href="/" aria-label="Tugport home"><img src="/logo.svg" alt="" width="30" height="30"><span>Tugport</span></a>
      <div class="links">
        <a href="/docs/">Docs</a>
        <a class="gh" href="https://github.com/backmeupplz/tugport">GitHub</a>
      </div>
    </nav>
  </div>
</header>
${body}
<footer>
  <div class="wrap">
    <span>Tugport is MIT licensed. © 2026 Nikita Kolmogorov.</span>
    <nav aria-label="Footer">
      <a href="https://github.com/backmeupplz/tugport">Source code</a>
      <a href="https://github.com/backmeupplz/tugport/releases">Releases</a>
      <a href="https://github.com/backmeupplz/tugport/issues">Issues</a>
    </nav>
  </div>
</footer>
</body>
</html>
`;

/** Plain text of a Markdown snippet: links, code marks and emphasis dropped. */
const plain = (md) => md.replace(/\[([^\]]+)\]\([^)]*\)/g, "$1").replace(/[`*]/g, "").replace(/\s+/g, " ").trim();

/** Structured data for a tested guide: TechArticle, a HowTo from its "Step N:" sections and a FAQPage from its questions. */
function guideLd(g, markdown) {
  const url = `https://tugport.com/docs/${g.slug}/`;
  const author = { "@type": "Person", name: "Nikita Kolmogorov", url: "https://borodutch.com" };
  const sections = (level) => markdown.split(new RegExp(`^${level} `, "m")).slice(1).map((chunk) => {
    const [head, ...rest] = chunk.split("\n");
    const paragraphs = rest.join("\n").replace(/```[\s\S]*?```/g, "").split(/\n\s*\n/).map((x) => x.trim()).filter((x) => x && !/^(\d+\.|-|#)/.test(x));
    return { head: head.trim(), text: plain(paragraphs[0] ?? ""), all: plain(paragraphs.join(" ")) };
  });
  const steps = sections("##").filter((x) => /^Step \d+:/.test(x.head));
  const faq = markdown.includes("## Frequently asked questions") ? sections("###").filter((x) => x.head.endsWith("?")) : [];
  const ids = markdown.match(/^## (.+)$/gm).map((h) => h.slice(3));
  const anchor = (h) => `${url}#${h.toLowerCase().replace(/[^\p{L}\p{N}\- ]/gu, "").replace(/ /g, "-")}`;
  return {
    "@context": "https://schema.org",
    "@graph": [
      { "@type": "TechArticle", headline: g.title, description: g.blurb, url, mainEntityOfPage: url, inLanguage: "en",
        datePublished: g.tested, dateModified: g.tested, author, publisher: { "@type": "Organization", name: "Tugport", url: "https://tugport.com/" },
        image: `https://tugport.com/og/${g.slug}.png`, proficiencyLevel: "Beginner" },
      { "@type": "HowTo", name: g.title, description: g.blurb,
        step: steps.map((x, i) => ({ "@type": "HowToStep", position: i + 1, name: x.head.replace(/^Step \d+:\s*/, ""), url: anchor(ids.find((h) => h === x.head)), text: x.all })) },
      ...(faq.length ? [{ "@type": "FAQPage", mainEntity: faq.map((x) => ({ "@type": "Question", name: x.head, acceptedAnswer: { "@type": "Answer", text: x.all } })) }] : []),
    ],
  };
}

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });

for (const g of GUIDES) {
  const markdown = readFileSync(new URL(g.file, import.meta.url), "utf8");
  const { html, toc } = render(markdown);
  const source = `${REPO}/docs/${g.file}`;
  const nav = toc.map((t) => `<li class="d${t.depth}"><a href="#${t.id}">${t.html.replace(/<\/?a[^>]*>/g, "")}</a></li>`).join("\n");
  // The contents: a list beside the text on wide screens, a folded block above it on phones (no script, no shift).
  const body = `<div class="wrap doc">
  <aside>
    <nav class="toc" aria-label="On this page"><p class="toc-title">On this page</p><ul>
${nav}
    </ul></nav>
    <details class="toc-fold"><summary>On this page</summary><ul>
${nav}
    </ul></details>
  </aside>
  <main>
    <h1>${esc(g.title)}</h1>
    <p class="meta">Also as <a href="/docs/${g.slug}.md">Markdown</a> for agents, and on <a href="${source}">GitHub</a>.</p>
${html}
  </main>
</div>`;
  mkdirSync(new URL(`${g.slug}/`, out), { recursive: true });
  writeFileSync(new URL(`${g.slug}/index.html`, out), page({ title: g.title, description: g.blurb, path: `/docs/${g.slug}/`, body,
    image: g.slug, alt: `${g.title}. Tugport docs.`, ld: g.tested ? guideLd(g, markdown) : undefined }));
  writeFileSync(new URL(`${g.slug}.md`, out), markdown);
}

const cards = [
  ...GUIDES.map((g) => ({ href: `/docs/${g.slug}/`, title: g.title, blurb: g.blurb })),
  { href: "/agents.md", title: "Moving a server, for AI agents", blurb: "Step-by-step instructions an agent such as Claude Code or Codex follows to move the apps on a server into Tugport." },
];
const index = `<main class="wrap index">
  <h1>Documentation</h1>
  <p class="lead">Tugport installs with one command and works from its panel. These guides cover what the panel does not explain on the page.</p>
  <ul class="cards">
${cards.map((c) => `    <li><a href="${c.href}"><h2>${esc(c.title)}</h2><p>${esc(c.blurb)}</p></a></li>`).join("\n")}
  </ul>
</main>`;
writeFileSync(new URL("index.html", out), page({ title: "Documentation", description: "Guides for Tugport: the API reference, moving from Easypanel or Coolify, the comparison with other panels and a tested Uptime Kuma deploy.", path: "/docs/", body: index,
  image: "docs", alt: "Tugport documentation: the API reference, moving from Easypanel or Coolify, and how Tugport compares." }));
console.log(`wrote ${GUIDES.length} guides and the index to website/docs/`);

// The sitemap and llms.txt follow the guide list, so a new guide shows up in both on the next deploy.
const site = new URL("../website/", import.meta.url);
const pages = ["/", "/docs/", ...GUIDES.map((g) => `/docs/${g.slug}/`)];
writeFileSync(new URL("sitemap.xml", site), `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
${pages.map((p) => `  <url><loc>https://tugport.com${p}</loc></url>`).join("\n")}
</urlset>
`);
writeFileSync(new URL("llms.txt", site), `# Tugport

> Tugport is a self-hosted deploy platform in one Rust binary. It runs next to Docker on servers you own, builds every git push, switches traffic once the new version is healthy, serves it over HTTPS, and comes with logs, charts, backups, many-server networking and scoped API keys for AI agents.

It's an alternative to panels such as Coolify, Easypanel and Dokploy, and \`tugport migrate\` moves an existing Easypanel or Coolify server over. It is MIT licensed and made by Nikita Kolmogorov.

## Docs

${GUIDES.map((g) => `- [${g.title}](https://tugport.com/docs/${g.slug}.md): ${g.blurb}`).join("\n")}
- [Moving a server to Tugport, for AI agents](https://tugport.com/agents.md): Step-by-step instructions an AI agent follows to move the apps on a Linux server into Tugport.

## Links

- [Home page](https://tugport.com/)
- [Source code](https://github.com/backmeupplz/tugport)
- [Releases](https://github.com/backmeupplz/tugport/releases)
`);
