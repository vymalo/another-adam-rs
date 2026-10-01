// Validates the repository's Markdown:
//   1. every ```mermaid block parses with the pinned Mermaid version, and
//   2. every relative Markdown link points at a file or directory that exists, and
//   3. every `#fragment` of a link to a Markdown file names a heading that exists, and
//   4. every Rust crate (a directory with a Cargo.toml under crates/ or bin/) has a README.md
//      next to it.
// Usage (from the repo root):  npm --prefix tools/docs-check ci && node tools/docs-check/check-docs.mjs
// Exits 1 on any failure, listing file:line for each.
import fs from 'node:fs';
import path from 'node:path';
import { JSDOM } from 'jsdom';

const root = process.cwd();
// Build output, dependencies, and the vendored agent skills (not this repository's docs).
const skipDirs = new Set(['.git', 'node_modules', 'target', '.claude', '.agents', '.goose', '.kiro']);

function* markdownFiles(dir) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (skipDirs.has(entry.name)) continue;
    const full = path.join(dir, entry.name);
    if (entry.isSymbolicLink()) continue; // symlinks are checked at their target
    if (entry.isDirectory()) yield* markdownFiles(full);
    else if (entry.name.endsWith('.md')) yield full;
  }
}

// Mermaid needs a DOM to parse.
const dom = new JSDOM('<!doctype html><html><body></body></html>');
globalThis.window = dom.window;
globalThis.document = dom.window.document;
Object.defineProperty(globalThis, 'navigator', { value: dom.window.navigator, configurable: true });
globalThis.DOMParser = dom.window.DOMParser;
globalThis.Element = dom.window.Element;
const { default: mermaid } = await import('mermaid');
mermaid.initialize({ startOnLoad: false });

const lineOf = (src, index) => src.slice(0, index).split('\n').length;

// GitHub's heading slug: lowercase, drop punctuation, spaces to hyphens, `-1`, `-2` for repeats.
function anchors(file) {
  const seen = new Map();
  const out = new Set();
  const text = fs.readFileSync(file, 'utf8').replace(/```[\s\S]*?```/g, '');
  for (const m of text.matchAll(/^ {0,3}#{1,6}[ \t]+(.+?)[ \t]*#*[ \t]*$/gm)) {
    const base = m[1]
      .replace(/`/g, '')
      .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
      .toLowerCase()
      .replace(/[^\p{L}\p{N}\s_-]/gu, '')
      .trim()
      .replace(/\s/g, '-');
    const n = seen.get(base) ?? 0;
    seen.set(base, n + 1);
    out.add(n === 0 ? base : `${base}-${n}`);
  }
  return out;
}
const anchorCache = new Map();
const anchorsOf = (file) => {
  if (!anchorCache.has(file)) anchorCache.set(file, anchors(file));
  return anchorCache.get(file);
};
let diagrams = 0, links = 0;
const failures = [];

for (const file of markdownFiles(root)) {
  const rel = path.relative(root, file);
  const src = fs.readFileSync(file, 'utf8');

  for (const m of src.matchAll(/```mermaid\n([\s\S]*?)```/g)) {
    diagrams++;
    try { await mermaid.parse(m[1]); }
    catch (e) { failures.push(`${rel}:${lineOf(src, m.index)} mermaid: ${String(e.message).split('\n')[0]}`); }
  }

  // A malformed `%` in a link is reported as that link's failure instead of aborting the run.
  const decode = (s, fn) => { try { return fn(s); } catch { return null; } };

  // Blank out fenced blocks and inline code spans (keeping line numbers) so
  // example links inside code are not checked.
  const blank = (code) => code.replace(/[^\n]/g, ' ');
  const prose = src
    .replace(/```[\s\S]*?```/g, blank)
    .replace(/`[^`\n]+`/g, blank);
  for (const m of prose.matchAll(/\]\(([^)\s]+)\)/g)) {
    const [pathPart, fragment] = m[1].split('#');
    if (/^[a-z][a-z0-9+.-]*:/i.test(pathPart)) continue; // http(s):, mailto:
    if (!pathPart && !fragment) continue;
    links++;
    const decodedPath = pathPart ? decode(pathPart, decodeURI) : '';
    const decodedFragment = fragment ? decode(fragment, decodeURIComponent) : '';
    if (decodedPath === null || decodedFragment === null) {
      failures.push(`${rel}:${lineOf(src, m.index)} malformed percent-encoding: ${m[1]}`);
      continue;
    }
    const target = pathPart ? path.resolve(path.dirname(file), decodedPath) : file;
    if (!fs.existsSync(target)) {
      failures.push(`${rel}:${lineOf(src, m.index)} broken link: ${m[1]}`);
      continue;
    }
    if (fragment && target.endsWith('.md') && fs.statSync(target).isFile()
        && !anchorsOf(target).has(decodedFragment.toLowerCase())) {
      failures.push(`${rel}:${lineOf(src, m.index)} no such heading: ${m[1]}`);
    }
  }
}

// Every crate documents itself: a directory under these roots with a Cargo.toml needs a
// README.md, updated in the same change as any change to its public API, environment
// variables or tests (see "Development" in README.md).
const crateRoots = ['crates', 'bin'];
let crates = 0;
for (const crateRoot of crateRoots) {
  const dir = path.join(root, crateRoot);
  if (!fs.existsSync(dir)) continue;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const crateDir = path.join(dir, entry.name);
    if (!fs.existsSync(path.join(crateDir, 'Cargo.toml'))) continue;
    crates++;
    if (!fs.existsSync(path.join(crateDir, 'README.md'))) {
      failures.push(`${path.relative(root, crateDir)}: crate has a Cargo.toml but no README.md`);
    }
  }
}

console.log(`${diagrams} diagrams, ${links} relative links, ${crates} crate READMEs checked`);
if (failures.length) {
  console.error(failures.join('\n'));
  console.error(`${failures.length} problem(s)`);
  process.exit(1);
}
console.log('docs OK');
