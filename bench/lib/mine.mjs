// The miner: candidate tasks from the squash commits of a full local clone.
//
// A candidate is a commit on the first-parent history of the branch that
//   * has one parent and a subject ending in `(#N)` (a squashed pull request),
//   * is not a docs, chore or revert commit,
//   * touches at least one test file and at least one source file (by path),
//   * changes at most 400 non-test lines (added plus deleted, documentation and lockfiles not counted),
//   * changes no lockfile (a new dependency would need the network),
//   * has no hunk inside a `#[cfg(test)]` item of a Rust source file (inline tests cannot be hidden).
// It writes no statement: the owner writes `instruction.md` per task.
import { execFileSync } from 'node:child_process';

export const MAX_NON_TEST_LINES = 400;
const LOCKFILES = new Set(['Cargo.lock', 'package-lock.json', 'pnpm-lock.yaml', 'yarn.lock', 'npm-shrinkwrap.json', 'bun.lockb', 'bun.lock', 'go.sum', 'poetry.lock', 'uv.lock', 'Pipfile.lock', 'Gemfile.lock', 'composer.lock', 'flake.lock']);
const TEST_DIRS = new Set(['tests', 'test', '__tests__', 'spec', 'specs', 'e2e']);
const DOC_EXT = /\.(md|mdx|txt|rst|adoc|png|jpe?g|gif|svg|ico|webp|pdf)$/i;

/** 'test' | 'lock' | 'doc' | 'source' for a repository path. */
export function classifyPath(p) {
  const parts = p.split('/');
  const name = parts[parts.length - 1];
  if (LOCKFILES.has(name)) return 'lock';
  if (parts.slice(0, -1).some((d) => TEST_DIRS.has(d))) return 'test';
  if (/\.(test|spec)\.[A-Za-z0-9]+$/.test(name) || /_test\.[A-Za-z0-9]+$/.test(name) || /^test_.*\.py$/.test(name)) return 'test';
  if (DOC_EXT.test(name) || parts[0] === 'docs') return 'doc';
  return 'source';
}

export function gitIn(dir) {
  return (args) => execFileSync('git', ['-c', 'core.quotepath=off', '-C', dir, ...args], { encoding: 'utf8', maxBuffer: 256 * 1024 * 1024, stdio: ['ignore', 'pipe', 'pipe'] });
}

/** Line ranges (1-based, inclusive) of the items that follow a `#[cfg(test)]` attribute. */
export function cfgTestRanges(text) {
  const ranges = [];
  const lines = text.split('\n');
  const lineStarts = [];
  let off = 0;
  for (const l of lines) {
    lineStarts.push(off);
    off += l.length + 1;
  }
  const lineOf = (pos) => {
    let lo = 0;
    let hi = lineStarts.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (lineStarts[mid] <= pos) lo = mid;
      else hi = mid - 1;
    }
    return lo + 1;
  };
  const re = /^[ \t]*#\[cfg\(test\)\]/gm;
  let m;
  while ((m = re.exec(text)) !== null) {
    let i = m.index + m[0].length;
    let depth = 0;
    let opened = false;
    let end = -1;
    while (i < text.length) {
      const c = text[i];
      if (c === '/' && text[i + 1] === '/') {
        while (i < text.length && text[i] !== '\n') i++;
        continue;
      }
      if (c === '/' && text[i + 1] === '*') {
        const e = text.indexOf('*/', i + 2);
        i = e < 0 ? text.length : e + 2;
        continue;
      }
      if (c === '"') {
        i++;
        while (i < text.length && text[i] !== '"') i += text[i] === '\\' ? 2 : 1;
        i++;
        continue;
      }
      if (c === "'") {
        const lit = /^'(?:[^'\\\n]|\\.[^'\n]*)'/.exec(text.slice(i, i + 12));
        if (lit) {
          i += lit[0].length;
          continue;
        }
      }
      if (c === '{') {
        depth++;
        opened = true;
      } else if (c === '}') {
        depth--;
        if (opened && depth === 0) {
          end = i;
          break;
        }
      } else if (c === ';' && !opened) {
        end = i;
        break;
      }
      i++;
    }
    ranges.push([lineOf(m.index), lineOf(end < 0 ? text.length - 1 : end)]);
  }
  return ranges;
}

/** The hunks of `git diff -U0` as { oldStart, oldLen, newStart, newLen }. */
export function parseHunks(diff) {
  const out = [];
  for (const m of diff.matchAll(/^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/gm)) {
    out.push({ oldStart: +m[1], oldLen: m[2] === undefined ? 1 : +m[2], newStart: +m[3], newLen: m[4] === undefined ? 1 : +m[4] });
  }
  return out;
}

const within = (a, b, ranges) => ranges.some(([s, e]) => a >= s && b <= e);

/** Does any hunk of the file lie inside a `#[cfg(test)]` item, in the new text or (for a pure deletion) the old? */
export function touchesInlineTests({ hunks, oldText, newText }) {
  const newRanges = newText ? cfgTestRanges(newText) : [];
  const oldRanges = oldText ? cfgTestRanges(oldText) : [];
  for (const h of hunks) {
    if (h.newLen > 0 && within(h.newStart, h.newStart + h.newLen - 1, newRanges)) return true;
    if (h.newLen === 0 && h.oldLen > 0 && within(h.oldStart, h.oldStart + h.oldLen - 1, oldRanges)) return true;
  }
  return false;
}

function subjectKind(subject) {
  const m = /^([A-Za-z]+)(\([^)]*\))?!?:/.exec(subject);
  const type = m ? m[1].toLowerCase() : null;
  if (type === 'docs' || type === 'chore' || type === 'revert' || /^revert\b/i.test(subject)) return type === 'docs' || type === 'chore' ? type : 'revert';
  return null;
}

const LANGS = { rs: 'rust', go: 'go', ts: 'typescript', tsx: 'typescript', js: 'javascript', jsx: 'javascript', mjs: 'javascript', py: 'python' };

function languageOf(files) {
  const counts = {};
  for (const f of files) {
    const l = LANGS[f.split('.').pop()];
    if (l) counts[l] = (counts[l] ?? 0) + 1;
  }
  return Object.entries(counts).sort((a, b) => b[1] - a[1] || (a[0] < b[0] ? -1 : 1))[0]?.[0] ?? 'unknown';
}

/** A hint for the test command; the owner writes the real one in own.json. */
export function hintTestCommand(git, commit, language, testFiles) {
  const has = (p) => {
    try {
      git(['cat-file', '-e', `${commit}:${p}`]);
      return true;
    } catch {
      return false;
    }
  };
  const dirOf = (f) => f.split('/').slice(0, -1);
  const ancestorWith = (f, marker) => {
    const d = dirOf(f);
    for (let n = d.length; n >= 0; n--) {
      const base = d.slice(0, n).join('/');
      if (has(base ? `${base}/${marker}` : marker)) return base;
    }
    return null;
  };
  const f = testFiles[0];
  if (language === 'rust') {
    const dir = ancestorWith(f, 'Cargo.toml');
    return dir === null ? null : dir === '' ? 'cargo test --locked' : `cd ${dir} && cargo test --locked`;
  }
  if (language === 'go') return `go test ./${dirOf(f).join('/')}/...`;
  if (language === 'typescript' || language === 'javascript') {
    const dir = ancestorWith(f, 'package.json');
    return dir === null ? null : `${dir ? `cd ${dir} && ` : ''}npx vitest run ${testFiles.map((t) => (dir ? t.slice(dir.length + 1) : t)).join(' ')}`;
  }
  if (language === 'python') return `python -m pytest ${testFiles.join(' ')}`;
  return null;
}

/** Judge one commit. Returns { ok, reasons, candidate }. `reasons` are the rules it breaks. */
export function evaluateCommit(git, commit, { repoName = 'repo', maxNonTestLines = MAX_NON_TEST_LINES } = {}) {
  const [hash, parents, subject] = git(['log', '-1', '--format=%H%x00%P%x00%s', commit]).trim().split('\0');
  const reasons = [];
  const parentList = parents.split(' ').filter(Boolean);
  if (parentList.length !== 1) reasons.push('not-squash');
  const pr = /\(#(\d+)\)\s*$/.exec(subject);
  if (!pr) reasons.push('no-pr-number');
  const kind = subjectKind(subject);
  if (kind) reasons.push(`type-${kind}`);
  if (parentList.length !== 1) return { ok: false, reasons, candidate: null };

  const parent = parentList[0];
  const numstat = git(['diff', '--numstat', '-z', '--no-renames', parent, hash]).split('\0').filter(Boolean);
  const files = numstat.map((l) => {
    const m = /^(-|\d+)\t(-|\d+)\t(.*)$/s.exec(l);
    return { path: m[3], added: m[1] === '-' ? 0 : +m[1], deleted: m[2] === '-' ? 0 : +m[2] };
  });
  const by = { test: [], lock: [], doc: [], source: [] };
  for (const f of files) by[classifyPath(f.path)].push(f);
  if (by.test.length === 0) reasons.push('no-tests');
  if (by.source.length === 0) reasons.push('no-source');
  if (by.lock.length > 0) reasons.push('lockfile');
  const nonTestLines = by.source.reduce((s, f) => s + f.added + f.deleted, 0);
  if (nonTestLines > maxNonTestLines) reasons.push('too-large');
  if (by.source.some((f) => f.path.endsWith('.rs'))) {
    for (const f of by.source.filter((x) => x.path.endsWith('.rs'))) {
      const hunks = parseHunks(git(['diff', '-U0', '--no-renames', parent, hash, '--', f.path]));
      const show = (rev) => {
        try {
          return git(['show', `${rev}:${f.path}`]);
        } catch {
          return '';
        }
      };
      if (touchesInlineTests({ hunks, oldText: show(parent), newText: show(hash) })) {
        reasons.push('inline-tests');
        break;
      }
    }
  }
  if (reasons.length > 0) return { ok: false, reasons, candidate: null };

  const testFiles = by.test.map((f) => f.path).sort();
  const sourceFiles = by.source.map((f) => f.path).sort();
  const language = languageOf(sourceFiles);
  return {
    ok: true,
    reasons,
    candidate: {
      id: `${repoName}-${pr[1]}`,
      repo: repoName,
      pr: +pr[1],
      commit: hash,
      base: parent,
      subject,
      language,
      testFiles,
      sourceFiles,
      nonTestLines,
      testLines: by.test.reduce((s, f) => s + f.added + f.deleted, 0),
      testCommandHint: hintTestCommand(git, hash, language, testFiles),
    },
  };
}

/** Mine a clone. `branch` is the branch whose first-parent history is walked. Returns { candidates, refused }. */
export function mine({ dir, repoName, branch = 'main', limit = Infinity, maxNonTestLines }) {
  const git = gitIn(dir);
  const commits = git(['rev-list', '--first-parent', branch]).split('\n').filter(Boolean);
  const candidates = [];
  const refused = {};
  for (const c of commits) {
    const r = evaluateCommit(git, c, { repoName, maxNonTestLines });
    if (r.ok) {
      candidates.push(r.candidate);
      if (candidates.length >= limit) break;
    } else for (const reason of r.reasons) refused[reason] = (refused[reason] ?? 0) + 1;
  }
  return { candidates, refused };
}
