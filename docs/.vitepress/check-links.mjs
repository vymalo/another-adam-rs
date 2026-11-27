// Crawls the built site: every internal href must resolve to a page of dist/, and every #fragment to an id on it.
// Usage: node .vitepress/check-links.mjs [dist] (from docs/; `npm run check-links` after `npm run build`; run before the rustdoc is copied into dist/rust/, whose links it skips). Exits 1 listing every broken one.
import { readdirSync, readFileSync, existsSync, statSync } from "node:fs";
import { dirname, join, posix, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dist = resolve(process.argv[2] ?? join(here, "dist"));
const base = process.env.DOCS_BASE ?? "/another-adam-rs/";

function* htmlFiles(dir) {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) yield* htmlFiles(p);
    else if (e.name.endsWith(".html")) yield p;
  }
}
const decode = (s) => s.replace(/&amp;/g, "&").replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&lt;/g, "<").replace(/&gt;/g, ">");
const urlOf = (file) => {
  const r = relative(dist, file).split("\\").join("/");
  return base + r.replace(/(^|\/)index\.html$/, "$1");
};

const pages = new Map(); // url path -> { file, ids, hrefs }
for (const file of htmlFiles(dist)) {
  const html = readFileSync(file, "utf8");
  const ids = new Set([...html.matchAll(/\sid="([^"]*)"/g)].map((m) => decode(m[1])));
  const hrefs = [...html.matchAll(/<a\s[^>]*?href="([^"]*)"/g)].map((m) => decode(m[1]));
  pages.set(urlOf(file), { file, ids, hrefs });
}

let links = 0, fragments = 0;
const broken = [];
for (const [url, page] of pages) {
  for (const href of page.hrefs) {
    if (/^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith("//")) continue; // external, mailto:
    // /rust/ is the rustdoc the workflow copies in after the site build and after this crawl: it is not in dist/ yet.
    if (href.startsWith(`${base}rust/`) || /^(\.\.\/)*rust\//.test(href)) continue;
    links++;
    const [pathPart, frag] = href.split("#");
    const [pathOnly] = pathPart.split("?");
    const target = pathOnly === "" ? url : posix.normalize(pathOnly.startsWith("/") ? pathOnly : posix.join(posix.dirname(url + "x"), pathOnly));
    let key = target;
    let t = pages.get(key) ?? pages.get(key + ".html") ?? pages.get(key.replace(/\.html$/, "")) ?? pages.get(key.replace(/\/?$/, "/"));
    if (!t) {
      // a file in dist (an asset) is fine
      const rel = key.startsWith(base) ? key.slice(base.length) : null;
      if (rel !== null && existsSync(join(dist, rel)) && statSync(join(dist, rel)).isFile()) continue;
      broken.push(`${url}: no page for ${href}`);
      continue;
    }
    if (frag) {
      fragments++;
      let id;
      try {
        id = decodeURIComponent(frag);
      } catch {
        broken.push(`${url}: malformed fragment #${frag} (${href})`);
        continue;
      }
      if (!t.ids.has(id)) broken.push(`${url}: no id #${id} on ${key} (${href})`);
    }
  }
}
console.log(`${pages.size} pages, ${links} internal links, ${fragments} fragments checked`);
if (broken.length) {
  console.error(broken.join("\n"));
  console.error(`${broken.length} broken`);
  process.exit(1);
}
console.log("links OK");
