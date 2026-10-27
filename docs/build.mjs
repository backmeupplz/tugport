// Renders the Markdown docs into tugport.com pages: `cd docs && npm ci && node build.mjs`.
// Writes website/docs/ (not in git): a page per guide, an index, and the Markdown itself for
// agents. The Pages workflow runs it on every push that touches docs/ or website/.
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { Marked } from "marked";

const REPO = "https://github.com/backmeupplz/tugport/blob/main";
const GUIDES = [
  { file: "API.md", slug: "api", title: "API reference", blurb: "Every endpoint of the panel, its roles and grants, the service spec, the CLI and the MCP endpoint for agents." },
  { file: "MIGRATION.md", slug: "migration", title: "Moving from Easypanel or Coolify", blurb: "How tugport migrate reads the old panel, what maps to what, and how the cutover and the rollback work." },
  { file: "COMPARE.md", slug: "compare", title: "Tugport compared with Coolify, Dokploy, Easypanel and CapRover", blurb: "License, price, servers, deploys, backups, AI agents and moving over, side by side, with sources, checked on October 27, 2026." },
];
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
        return `<h${depth} id="${id}"><a class="anchor" href="#${id}" aria-hidden="true">#</a>${html}</h${depth}>\n`;
      },
      link({ href: link, title, tokens }) {
        return `<a href="${esc(href(link))}"${title ? ` title="${esc(title)}"` : ""}>${this.parser.parseInline(tokens)}</a>`;
      },
    },
  });
  // Wide tables scroll inside their box instead of widening the page.
  const html = marked.parse(markdown).replace(/<table>/g, '<div class="table"><table>').replace(/<\/table>/g, "</table></div>");
  return { html, toc };
}

const page = ({ title, description, path, body }) => `<!doctype html>
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
<meta property="og:image" content="https://tugport.com/og.png">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta property="og:image:alt" content="Tugport: deploy your apps to servers you own, from one Rust binary.">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:title" content="${esc(title)}">
<meta name="twitter:description" content="${esc(description)}">
<meta name="twitter:image" content="https://tugport.com/og.png">
<link rel="icon" href="/logo.svg" type="image/svg+xml">
<link rel="icon" href="/favicon.ico" sizes="32x32">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
<meta name="theme-color" content="#0A0E14" media="(prefers-color-scheme: dark)">
<meta name="theme-color" content="#F7F6F2" media="(prefers-color-scheme: light)">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600&family=JetBrains+Mono:wght@400;500&family=Space+Grotesk:wght@500;600;700&display=swap" rel="stylesheet" media="print" onload="this.media='all'">
<noscript><link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600&family=JetBrains+Mono:wght@400;500&family=Space+Grotesk:wght@500;600;700&display=swap" rel="stylesheet"></noscript>
<link rel="stylesheet" href="/docs/docs.css">
<script defer src="https://lumen.borodutch.com/l.js"></script>
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

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
writeFileSync(new URL("docs.css", out), readFileSync(new URL("docs.css", import.meta.url)));

for (const g of GUIDES) {
  const markdown = readFileSync(new URL(g.file, import.meta.url), "utf8");
  const { html, toc } = render(markdown);
  const source = `${REPO}/docs/${g.file}`;
  const nav = toc.map((t) => `<li class="d${t.depth}"><a href="#${t.id}">${t.html.replace(/<\/?a[^>]*>/g, "")}</a></li>`).join("\n");
  const body = `<div class="wrap doc">
  <aside aria-label="On this page"><details open><summary>On this page</summary><ul>
${nav}
  </ul></details></aside>
  <main>
    <h1>${esc(g.title)}</h1>
    <p class="meta">Also as <a href="/docs/${g.slug}.md">Markdown</a> for agents, and on <a href="${source}">GitHub</a>.</p>
${html}
  </main>
</div>
<script>
  // On phones the contents start folded, above the text.
  if (matchMedia("(max-width: 860px)").matches) document.querySelector(".doc details").open = false;
</script>`;
  mkdirSync(new URL(`${g.slug}/`, out), { recursive: true });
  writeFileSync(new URL(`${g.slug}/index.html`, out), page({ title: g.title, description: g.blurb, path: `/docs/${g.slug}/`, body }));
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
writeFileSync(new URL("index.html", out), page({ title: "Documentation", description: "Guides for Tugport: the API reference and moving from Easypanel or Coolify.", path: "/docs/", body: index }));
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
