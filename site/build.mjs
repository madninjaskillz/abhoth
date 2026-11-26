// Builds the Abhoth website: src/pages/**.html + public/ -> dist/. No dependencies.
//
//   node build.mjs                 # base path from public/CNAME (custom domain: "/"), else "/abhoth/"
//   SITE_BASE=/ node build.mjs     # local preview at the server root
//
// Each page starts with a JSON header in an HTML comment:
//   <!--meta {"title": "...", "description": "...", "layout": "home" | "docs" | "plain"} -->
// Pretty URLs: pages/docs/install.html -> dist/docs/install/index.html.
// In page bodies, {{base}} is the site's base path and {{version}} the latest release.

import { readFileSync, writeFileSync, mkdirSync, rmSync, readdirSync, statSync, copyFileSync, existsSync } from "node:fs";
import { join, dirname, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const srcPages = join(root, "src", "pages");
const pub = join(root, "public");
const out = join(root, "dist");

const cnameFile = join(pub, "CNAME");
const cname = existsSync(cnameFile) ? readFileSync(cnameFile, "utf8").trim() : "";
const BASE = process.env.SITE_BASE ?? (cname ? "/" : "/abhoth/");
const ORIGIN = (process.env.SITE_URL ?? (cname ? `https://${cname}` : "https://madninjaskillz.github.io/abhoth")).replace(/\/$/, "");
const REPO = "https://github.com/madninjaskillz/abhoth";
const VERSION = process.env.ABHOTH_VERSION || (await latestVersion()) || "0.16.1";

async function latestVersion() {
  if (process.env.OFFLINE) return null;
  try {
    const r = await fetch("https://api.github.com/repos/madninjaskillz/abhoth/releases/latest", {
      headers: { accept: "application/vnd.github+json", "user-agent": "abhoth-site-build" },
      signal: AbortSignal.timeout(5000),
    });
    if (!r.ok) return null;
    const j = await r.json();
    return String(j.tag_name || "").replace(/^abhoth-v/, "") || null;
  } catch {
    return null;
  }
}

const DOCS_NAV = [
  ["Getting started", [["docs/", "Overview"], ["docs/install/", "Install"], ["docs/quick-start/", "Quick start"]]],
  ["Using Abhoth", [["docs/configuration/", "Configuration"], ["docs/web-ui/", "Web UI"], ["docs/desktop/", "Desktop app"]]],
  ["Running it your way", [["docs/docker/", "Self-host with Docker"], ["docs/local-models/", "Local models"], ["docs/router/", "The router"]]],
];

function walk(dir) {
  return readdirSync(dir).flatMap((n) => {
    const p = join(dir, n);
    return statSync(p).isDirectory() ? walk(p) : [p];
  });
}

const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

function urlFor(file) {
  const rel = relative(srcPages, file).split(sep).join("/").replace(/\.html$/, "");
  if (rel === "index") return "";
  if (rel === "404") return "404.html";
  if (rel.endsWith("/index")) return rel.slice(0, -5);
  return rel + "/";
}

const ICON = {
  github: '<svg viewBox="0 0 16 16" width="18" height="18" aria-hidden="true" fill="currentColor"><path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27s1.36.09 2 .27c1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8Z"/></svg>',
  sun: '<svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/></svg>',
  moon: '<svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8Z"/></svg>',
  menu: '<svg viewBox="0 0 24 24" width="22" height="22" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M4 7h16M4 12h16M4 17h16"/></svg>',
};

function header(current) {
  const link = (href, label, key) =>
    `<a href="${BASE}${href}"${current === key ? ' aria-current="page"' : ""}>${label}</a>`;
  return `<a class="skip" href="#main">Skip to content</a>
<header class="site-header">
  <div class="wrap header-inner">
    <a class="brand" href="${BASE}" aria-label="Abhoth home">
      <img src="${BASE}img/abhoth.svg" width="36" height="31" alt=""><span>Abhoth</span>
    </a>
    <button class="nav-toggle" aria-expanded="false" aria-controls="site-nav" aria-label="Menu">${ICON.menu}</button>
    <nav id="site-nav" class="site-nav" aria-label="Main">
      ${link("#models", "Features", "features")}
      ${link("#self-host", "Self-host", "selfhost")}
      ${link("#install", "Install", "install")}
      ${link("docs/", "Docs", "docs")}
      <a href="${REPO}" class="nav-gh">${ICON.github}<span>GitHub</span></a>
      <button class="theme-toggle" type="button" aria-label="Switch colour theme"><span class="i-sun">${ICON.sun}</span><span class="i-moon">${ICON.moon}</span></button>
    </nav>
  </div>
</header>`;
}

function footer() {
  return `<footer class="site-footer">
  <div class="wrap footer-inner">
    <div class="footer-brand">
      <img src="${BASE}img/abhoth.svg" width="48" height="41" alt="" loading="lazy">
      <p><strong>Abhoth</strong>: an open-source coding agent for the models you run.<br>MIT licensed.</p>
    </div>
    <nav aria-label="Footer">
      <ul>
        <li><a href="${BASE}docs/install/">Install</a></li>
        <li><a href="${BASE}docs/quick-start/">Quick start</a></li>
        <li><a href="${BASE}docs/">Docs</a></li>
        <li><a href="${REPO}/releases">Releases</a></li>
        <li><a href="${REPO}">Source on GitHub</a></li>
        <li><a href="${REPO}/issues">Issues</a></li>
      </ul>
    </nav>
    <p class="footer-note">No cookies, no analytics, no trackers on this site.</p>
  </div>
</footer>`;
}

function docsSidebar(url) {
  return `<nav class="docs-nav" aria-label="Documentation">
  ${DOCS_NAV.map(([group, items]) => `<p class="docs-nav-group">${group}</p><ul>${items
    .map(([href, label]) => `<li><a href="${BASE}${href}"${href === url ? ' aria-current="page"' : ""}>${label}</a></li>`)
    .join("")}</ul>`).join("\n  ")}
</nav>`;
}

function docsPager(url) {
  const flat = DOCS_NAV.flatMap(([, items]) => items);
  const i = flat.findIndex(([h]) => h === url);
  if (i < 0) return "";
  const prev = flat[i - 1], next = flat[i + 1];
  return `<nav class="pager" aria-label="Previous and next page">
  ${prev ? `<a class="pager-prev" href="${BASE}${prev[0]}"><small>Previous</small>${prev[1]}</a>` : "<span></span>"}
  ${next ? `<a class="pager-next" href="${BASE}${next[0]}"><small>Next</small>${next[1]}</a>` : "<span></span>"}
</nav>`;
}

function page(meta, body, url, srcRel) {
  const title = meta.title === "Abhoth" ? "Abhoth: the open-source coding agent for your own models" : `${meta.title} · Abhoth`;
  const canonical = `${ORIGIN}/${url}`;
  const layout = meta.layout || "docs";
  let main;
  if (layout === "docs") {
    main = `<main class="wrap docs" id="main" tabindex="-1">
  ${docsSidebar(url)}
  <article class="prose">
    ${body}
    <p class="edit"><a href="${REPO}/edit/main/site/src/pages/${srcRel}">Edit this page on GitHub</a></p>
    ${docsPager(url)}
  </article>
</main>`;
  } else {
    main = `<main id="main" tabindex="-1">${body}</main>`;
  }
  return `<!doctype html>
<html lang="en-GB" data-theme="dark">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(title)}</title>
<meta name="description" content="${esc(meta.description)}">
<link rel="canonical" href="${canonical}">
<meta name="color-scheme" content="dark light">
<meta name="theme-color" content="#0e0816" media="(prefers-color-scheme: dark)">
<meta name="theme-color" content="#fbf8ff" media="(prefers-color-scheme: light)">
<meta property="og:type" content="website">
<meta property="og:site_name" content="Abhoth">
<meta property="og:title" content="${esc(meta.title === "Abhoth" ? "Abhoth: the open-source coding agent for your own models" : meta.title)}">
<meta property="og:description" content="${esc(meta.description)}">
<meta property="og:url" content="${canonical}">
<meta property="og:image" content="${ORIGIN}/img/og.png">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta property="og:image:alt" content="Abhoth, a purple hooded tentacled mascot, beside the words: the coding agent for your own models">
<meta name="twitter:card" content="summary_large_image">
<link rel="icon" href="${BASE}favicon.svg" type="image/svg+xml">
<link rel="icon" href="${BASE}favicon-32.png" sizes="32x32" type="image/png">
<link rel="apple-touch-icon" href="${BASE}apple-touch-icon.png">
<link rel="stylesheet" href="${BASE}css/site.css?v=${CACHE}">
<script>document.documentElement.classList.add("js");try{var t=localStorage.getItem("theme");if(!t)t=matchMedia("(prefers-color-scheme: light)").matches?"light":"dark";document.documentElement.dataset.theme=t}catch(e){}</script>
${meta.jsonld ? `<script type="application/ld+json">${meta.jsonld}</script>` : ""}
<script src="${BASE}js/site.js?v=${CACHE}" defer></script>
</head>
<body class="layout-${layout}">
${header(meta.nav || (layout === "docs" ? "docs" : ""))}
${main}
${footer()}
</body>
</html>
`;
}

const CACHE = Date.now().toString(36);

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });

for (const f of walk(pub)) {
  const dest = join(out, relative(pub, f));
  mkdirSync(dirname(dest), { recursive: true });
  copyFileSync(f, dest);
}

const urls = [];
for (const f of walk(srcPages).filter((f) => f.endsWith(".html"))) {
  const raw = readFileSync(f, "utf8");
  const m = raw.match(/^<!--meta\s+([\s\S]*?)-->\s*/);
  if (!m) throw new Error(`${f}: missing <!--meta {...} --> header`);
  const meta = JSON.parse(m[1]);
  if (!meta.title || !meta.description) throw new Error(`${f}: title and description are required`);
  const url = urlFor(f);
  const body = raw
    .slice(m[0].length)
    .replaceAll("{{base}}", BASE)
    .replaceAll("{{version}}", VERSION)
    .replaceAll("{{repo}}", REPO);
  if (meta.jsonld) meta.jsonld = JSON.stringify(meta.jsonld).replaceAll("{{version}}", VERSION).replaceAll("{{origin}}", ORIGIN).replaceAll("</", "<\\/");
  const html = page(meta, body, url, relative(srcPages, f).split(sep).join("/"));
  const dest = url.endsWith(".html") ? join(out, url) : join(out, url, "index.html");
  mkdirSync(dirname(dest), { recursive: true });
  writeFileSync(dest, html);
  if (url !== "404.html") urls.push(url);
}

const today = new Date().toISOString().slice(0, 10);
writeFileSync(
  join(out, "sitemap.xml"),
  `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
${urls.sort().map((u) => `  <url><loc>${ORIGIN}/${u}</loc><lastmod>${today}</lastmod></url>`).join("\n")}
</urlset>
`,
);
writeFileSync(join(out, "robots.txt"), `User-agent: *\nAllow: /\n\nSitemap: ${ORIGIN}/sitemap.xml\n`);
writeFileSync(join(out, ".nojekyll"), "");

console.log(`Built ${urls.length + 1} pages to ${relative(process.cwd(), out) || "."} (base ${BASE}, origin ${ORIGIN}, version ${VERSION})`);
