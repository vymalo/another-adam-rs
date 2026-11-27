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
//      docker/, dev/, tools/ or .github/; spans with a placeholder or a glob are not paths), and
//   6. the split of ADR 0079 holds: a page of .agents/docs (for agents) and the page of docs/ at the
//      same relative path (for people) link to each other, and no page for people outside the dated
//      records (docs/decisions, docs/reviews) cites a `path:line`, and
//   7. every extension URI the card builder can list (the `*EXTENSION*` constants of
//      crates/adam-a2a/src/extensions.rs, and any extension URI written as a literal in a non-test
//      Rust source) has a table row in docs/reference/extensions.md, and the constants in its agent page.
// Usage (from the repo root):  npm --prefix tools/docs-check ci && node tools/docs-check/check-docs.mjs
// Exits 1 on any failure, listing file:line for each.
import fs from 'node:fs';
import path from 'node:path';
import { JSDOM } from 'jsdom';

const root = process.cwd();
// Build output, dependencies, and the agent skills (checked by rule 5, not as docs). The agent
// pages in .agents/docs are docs: their diagrams, links and anchors are checked like any other.
const skipDirs = new Set(['.git', 'node_modules', 'target', '.claude', '.goose', '.kiro']);
const skipPaths = new Set([path.join('.agents', 'skills')]);

function* markdownFiles(dir) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (skipDirs.has(entry.name)) continue;
    const full = path.join(dir, entry.name);
    if (skipPaths.has(path.relative(root, full))) continue;
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
    if (/^(\/|(\.\.\/)*)rust\//.test(pathPart)) continue; // the rustdoc the docs-site workflow publishes; not in the tree
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

// Rule 6 (ADR 0079): docs/ is for people, .agents/docs/ for agents, at the same relative path.
const linksTo = (from, to) => {
  const src = fs.readFileSync(from, 'utf8').replace(/```[\s\S]*?```/g, '').replace(/`[^`\n]+`/g, '');
  for (const m of src.matchAll(/\]\(([^)\s#]+)(#[^)\s]*)?\)/g)) {
    if (/^[a-z][a-z0-9+.-]*:/i.test(m[1])) continue;
    // A malformed `%` is the main link loop's failure to report, not a reason to abort here.
    let resolved;
    try { resolved = path.resolve(path.dirname(from), decodeURI(m[1])); } catch { continue; }
    if (resolved === to) return true;
  }
  return false;
};
const agentDocs = path.join(root, '.agents', 'docs');
let pairs = 0;
if (fs.existsSync(agentDocs)) {
  for (const agentPage of markdownFiles(agentDocs)) {
    const relPath = path.relative(agentDocs, agentPage);
    const humanPage = path.join(root, 'docs', relPath);
    if (relPath === 'README.md' || !fs.existsSync(humanPage)) continue;
    pairs++;
    if (!linksTo(humanPage, agentPage)) failures.push(`docs/${relPath}: does not link to its agent page .agents/docs/${relPath}`);
    if (!linksTo(agentPage, humanPage)) failures.push(`.agents/docs/${relPath}: does not link back to docs/${relPath}`);
  }
}
// A page for people names files, never a `path:line`: that evidence is an agent page's.
// A page listed here mixes the two today and is split in a later change (ADR 0079, Consequences).
const mixedPages = new Set([path.join('docs', 'reference', 'parity.md')]);
const records = [path.join('docs', 'decisions'), path.join('docs', 'reviews')];
const pathLine = /`[^`\s]*\.(rs|sh|ya?ml|toml|mjs|js|json|tpl|md):[0-9]+/g;
for (const file of markdownFiles(path.join(root, 'docs'))) {
  const rel = path.relative(root, file);
  if (mixedPages.has(rel) || records.some((r) => rel.startsWith(r + path.sep))) continue;
  const src = fs.readFileSync(file, 'utf8');
  for (const m of src.matchAll(pathLine)) {
    failures.push(`${rel}:${lineOf(src, m.index)} a path:line in a page for people (move it to .agents/docs, ADR 0079): ${m[0]}`);
  }
}

// Rule 7: the extensions page lists every URI the card builder can list. The constants are the
// builder's own list; a literal URI in another non-test source is an extension written around them.
const extensionsFile = path.join('crates', 'adam-a2a', 'src', 'extensions.rs');
const extensionPages = [path.join('docs', 'reference', 'extensions.md'), path.join('.agents', 'docs', 'reference', 'extensions.md')];
let extensionUris = 0;
if (fs.existsSync(path.join(root, extensionsFile))) {
  const declared = new Map(); // uri -> where it is declared
  const constants = fs.readFileSync(path.join(root, extensionsFile), 'utf8');
  for (const m of constants.matchAll(/pub const (\w*EXTENSION\w*)\s*:\s*&str\s*=\s*"(https:\/\/[^"]+)"/g)) {
    declared.set(m[2], `${extensionsFile}: ${m[1]}`);
  }
  const literal = /https:\/\/(?:agents\.vymalo\.com\/a2a\/extensions\/[a-z0-9-]+\/v\d+|a2ui\.org\/a2a-extension\/[a-z0-9./-]+)/g;
  const literalOnly = new Set();
  const sources = function* (dir) {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      if (skipDirs.has(entry.name) || entry.name === 'tests' || entry.isSymbolicLink()) continue;
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) yield* sources(full);
      else if (entry.name.endsWith('.rs')) yield full;
    }
  };
  for (const top of crateRoots) {
    if (!fs.existsSync(path.join(root, top))) continue;
    for (const file of sources(path.join(root, top))) {
      // Code only: a `//` comment may name a URI it describes, a string literal declares one.
      const code = fs.readFileSync(file, 'utf8').split('\n').filter((l) => !/^\s*\/\//.test(l)).join('\n');
      for (const m of code.matchAll(literal)) {
        if (!declared.has(m[0]) && !literalOnly.has(m[0])) { literalOnly.add(m[0]); declared.set(m[0], `${path.relative(root, file)}: a literal`); }
      }
    }
  }
  extensionUris = declared.size;
  for (const page of extensionPages) {
    const full = path.join(root, page);
    const rows = fs.existsSync(full)
      ? fs.readFileSync(full, 'utf8').split('\n').filter((l) => l.startsWith('|')).join('\n')
      : '';
    for (const [uri, where] of declared) {
      if (page.startsWith('.agents') && literalOnly.has(uri)) continue; // the agent page lists the builder's constants
      if (!rows.includes('`' + uri + '`')) failures.push(`${page}: no row for the extension URI ${uri} (${where})`);
    }
  }
}

console.log(`${diagrams} diagrams, ${links} relative links, ${crates} crate READMEs, ${skills} first-party skills (${skillPaths} cited paths), ${pairs} people/agent page pairs checked, ${extensionUris} extension URIs`);
if (failures.length) {
  console.error(failures.join('\n'));
  console.error(`${failures.length} problem(s)`);
  process.exit(1);
}
console.log('docs OK');
