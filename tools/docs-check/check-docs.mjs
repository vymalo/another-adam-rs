// Validates the repository's Markdown:
//   1. every ```mermaid block parses with the pinned Mermaid version, and
//   2. every relative Markdown link points at a file or directory that exists, and
//   3. every `#fragment` of a link to a Markdown file names a heading that exists, and
//   4. every Rust crate (a directory with a Cargo.toml under crates/ or bin/) has a README.md
//      next to it, and
//   5. every first-party skill (a directory of .agents/skills that skills-lock.json does not list)
//      has a SKILL.md whose frontmatter `name` is its directory and which has a `description`, is
//      mirrored by a symlink that resolves in .claude/, .goose/ and .kiro/, has no relative Markdown
//      link (a skill is installed into other repositories, where it would break), and cites only
//      repository paths that exist (code spans that start with crates/, bin/, docs/, deploy/,
//      docker/, dev/, tools/ or .github/; spans with a placeholder or a glob are not paths).
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
// variables or tests (see the Rules in CLAUDE.md).
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

// First-party skills: the skills the skills CLI can install from this repository into others.
// The vendored ones are pinned in skills-lock.json and belong to their authors, so they are not checked.
const skillsDir = path.join(root, '.agents', 'skills');
const mirrors = ['.claude', '.goose', '.kiro'];
const pathPrefixes = ['crates/', 'bin/', 'docs/', 'deploy/', 'docker/', 'dev/', 'tools/', '.github/'];
let skills = 0, skillPaths = 0;
if (fs.existsSync(skillsDir)) {
  let locked = new Set();
  let sources = new Set();
  const lockFile = path.join(root, 'skills-lock.json');
  if (fs.existsSync(lockFile)) {
    const lock = JSON.parse(fs.readFileSync(lockFile, 'utf8')).skills ?? {};
    locked = new Set(Object.keys(lock));
    sources = new Set(Object.values(lock).map((entry) => entry.source));
  }
  for (const entry of fs.readdirSync(skillsDir, { withFileTypes: true })) {
    if (!entry.isDirectory() || locked.has(entry.name)) continue;
    skills++;
    const name = entry.name;
    const dir = path.join(skillsDir, name);
    const rel = path.relative(root, path.join(dir, 'SKILL.md'));
    const skillFile = path.join(dir, 'SKILL.md');
    if (!fs.existsSync(skillFile)) { failures.push(`${rel}: first-party skill has no SKILL.md`); continue; }
    const src = fs.readFileSync(skillFile, 'utf8');

    // Frontmatter: the top-level `name` and `description` keys between the first two `---` lines.
    const fm = /^---\n([\s\S]*?)\n---\n/.exec(src);
    if (!fm) failures.push(`${rel}:1 no YAML frontmatter`);
    else {
      const key = (k) => {
        const m = new RegExp(`^${k}:[ \\t]*(.*)$`, 'm').exec(fm[1]);
        return m ? m[1].trim().replace(/^(["'])(.*)\1$/, '$2') : null;
      };
      if (key('name') !== name) failures.push(`${rel}:1 frontmatter name "${key('name')}" is not the directory "${name}"`);
      if (!key('description')) failures.push(`${rel}:1 frontmatter has no description`);
    }

    // Mirrors: a symlink that resolves to this skill, in every agent directory.
    for (const mirror of mirrors) {
      const link = path.join(root, mirror, 'skills', name);
      let ok = false;
      try { ok = fs.lstatSync(link).isSymbolicLink() && fs.realpathSync(link) === fs.realpathSync(dir); } catch { /* missing or dangling */ }
      if (!ok) failures.push(`${path.join(mirror, 'skills', name)}: not a symlink that resolves to .agents/skills/${name}`);
    }

    // A skill is read in other repositories: no relative link, and every repository path it cites exists.
    const body = src.replace(/```[\s\S]*?```/g, (code) => code.replace(/[^\n]/g, ' '));
    for (const m of body.matchAll(/\]\(([^)\s]+)\)/g)) {
      if (!/^(https?:|mailto:|#)/i.test(m[1])) failures.push(`${rel}:${lineOf(src, m.index)} relative link in a skill: ${m[1]}`);
    }
    for (const m of body.matchAll(/`([^`\n]+)`/g)) {
      for (const raw of m[1].split(/\s+/)) {
        // Strip quotes, trailing punctuation, a `:line` and a `#fragment`.
        const token = raw.replace(/^[("']+/, '').replace(/[)"',;.]+$/, '').replace(/[:#].*$/, '');
        if (sources.has(token)) continue; // `docker/skills` is a repository name, not a path
        if (!pathPrefixes.some((prefix) => token.startsWith(prefix))) continue;
        if (/[<>*{}$|]|\.\.\./.test(token)) continue; // a placeholder, a glob or a brace list is not a path
        skillPaths++;
        if (!fs.existsSync(path.join(root, token))) failures.push(`${rel}:${lineOf(src, m.index)} cited path does not exist: ${token}`);
      }
    }
  }
}

console.log(`${diagrams} diagrams, ${links} relative links, ${crates} crate READMEs, ${skills} first-party skills (${skillPaths} cited paths) checked`);
if (failures.length) {
  console.error(failures.join('\n'));
  console.error(`${failures.length} problem(s)`);
  process.exit(1);
}
console.log('docs OK');
