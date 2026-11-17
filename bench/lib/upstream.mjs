// The public task source: a clone of the SWE-bench Pro repository at a commit the owner pins, and its checksums.
// Nothing here is copied into this repository: the clone lives under $BENCH_HOME.
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

export class UpstreamError extends Error {
  constructor(message) {
    super(message);
    this.name = 'UpstreamError';
  }
}

const git = (dir, args) => execFileSync('git', ['-C', dir, ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });

/** Parse `sha256sum` output: `<hex>  <path>` or `<hex> *<path>`. Returns Map path -> hex. Refuses unsafe paths. */
export function parseSums(text) {
  const sums = new Map();
  for (const [i, raw] of String(text).split('\n').entries()) {
    const line = raw.trimEnd();
    if (line === '') continue;
    const m = /^([0-9a-f]{64}) [ *](.+)$/.exec(line);
    if (!m) throw new UpstreamError(`SHA256SUMS line ${i + 1} is not "<sha256>  <path>"`);
    const p = m[2];
    if (p.startsWith('/') || p.includes('\0') || path.posix.normalize(p) !== p || p === '.' || p.startsWith('../')) throw new UpstreamError(`SHA256SUMS line ${i + 1} names a path that is not in its normal form`);
    if (sums.has(p)) throw new UpstreamError(`SHA256SUMS names ${p} twice`);
    sums.set(p, m[1]);
  }
  if (sums.size === 0) throw new UpstreamError('SHA256SUMS is empty');
  return sums;
}

/**
 * Check the tree against its checksums: every listed file matches, every file under `root` (`strictDirs`, relative
 * directories, when given; the whole tree otherwise) is listed apart from SHA256SUMS, and there is no symlink
 * anywhere, a parent directory included. Throws naming the first few faults.
 */
export function verifySums(root, sums, { strictDirs = null } = {}) {
  const bad = [];
  const strict = (rel) => strictDirs === null || strictDirs.some((d) => rel === d || rel.startsWith(`${d}/`));
  const walk = (dir, rel) => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      const r = rel ? `${rel}/${e.name}` : e.name;
      if (e.isSymbolicLink()) bad.push(`${r} (symlink)`);
      else if (e.isDirectory()) walk(path.join(dir, e.name), r);
      else if (e.isFile() && !sums.has(r) && r !== 'SHA256SUMS' && strict(r)) bad.push(`${r} (not listed)`);
    }
  };
  walk(root, '');
  for (const [p, hex] of sums) {
    let actual = null;
    try {
      const full = path.join(root, p);
      if (!fs.lstatSync(full).isSymbolicLink()) actual = crypto.createHash('sha256').update(fs.readFileSync(full)).digest('hex');
    } catch {
      actual = null;
    }
    if (actual !== hex) bad.push(actual === null ? `${p} (missing)` : `${p} (differs)`);
  }
  if (bad.length > 0) throw new UpstreamError(`${bad.length} fault(s) against SHA256SUMS: ${bad.slice(0, 3).join(', ')}${bad.length > 3 ? ', ...' : ''}`);
}

/** A task's checksum: the hash of the sorted `path<TAB>sha256` lines of the SHA256SUMS entries under its directory. */
export function taskChecksum(relDir, sums) {
  const prefix = `${relDir.split(path.sep).join('/')}/`;
  const lines = [...sums].filter(([p]) => p.startsWith(prefix)).map(([p, h]) => `${p}\t${h}`).sort();
  if (lines.length === 0) return null;
  return crypto.createHash('sha256').update(lines.join('\n')).digest('hex');
}

/** The arguments of the clone; `--` ends the options before the URL and the directory. */
export function cloneArgs(url, dir) {
  return ['clone', '-q', '--no-checkout', ...(isLocal(url) ? [] : ['--filter=blob:none']), '--', url, dir];
}

const isLocal = (url) => !/^[a-z][a-z0-9+.-]*:\/\//i.test(url) || url.startsWith('file://');

/**
 * Clone (or update) the upstream repository into `dir` and check out `commit`, then verify `<sumsDir>/SHA256SUMS`.
 * Returns { root, tasksRoot, sums, head }.
 */
/** The task directories under `root` (those with a task.toml, up to two levels deep), relative to it. */
function taskDirs(root) {
  const out = [];
  const walk = (dir, rel, depth) => {
    const entries = fs.readdirSync(dir, { withFileTypes: true });
    if (entries.some((e) => e.isFile() && e.name === 'task.toml')) return void out.push(rel);
    if (depth >= 2) return;
    for (const e of entries) if (e.isDirectory() && !e.name.startsWith('.')) walk(path.join(dir, e.name), rel ? `${rel}/${e.name}` : e.name, depth + 1);
  };
  walk(root, '', 0);
  return out.filter((d) => d !== '');
}

export function fetchUpstream({ url, commit, dir, sumsDir = 'v2' }) {
  if (typeof url !== 'string' || url === '' || url.startsWith('-')) throw new UpstreamError('the upstream URL must not be empty or begin with -');
  if (!/^[0-9a-f]{40}$/.test(commit ?? '')) throw new UpstreamError('tasks.upstreamCommit must be a full commit id: pin the commit in the local configuration');
  if (sumsDir.startsWith('/') || sumsDir.split('/').includes('..')) throw new UpstreamError('tasks.upstreamDir must be a relative path');
  try {
    if (!fs.existsSync(path.join(dir, '.git'))) {
      fs.mkdirSync(path.dirname(dir), { recursive: true });
      execFileSync('git', cloneArgs(url, dir), { stdio: 'pipe' });
    }
    const origin = git(dir, ['remote', 'get-url', 'origin']).trim();
    if (origin !== url) throw new UpstreamError('the existing upstream clone has another origin than the configured URL: remove it or fix tasks.upstreamUrl');
    try {
      git(dir, ['cat-file', '-e', `${commit}^{commit}`]);
    } catch {
      git(dir, ['fetch', '-q', 'origin']);
    }
    git(dir, ['checkout', '-q', '--detach', '--force', commit]);
    git(dir, ['clean', '-ffdxq']);
  } catch (e) {
    if (e instanceof UpstreamError) throw e;
    throw new UpstreamError(`could not get the upstream commit: ${String(e.stderr ?? e.message).trim().split('\n')[0]}`);
  }
  const head = git(dir, ['rev-parse', 'HEAD']).trim();
  if (head !== commit) throw new UpstreamError('the checkout is not at the pinned commit');
  const tasksRoot = path.join(dir, sumsDir);
  let text;
  try {
    text = fs.readFileSync(path.join(tasksRoot, 'SHA256SUMS'), 'utf8');
  } catch {
    throw new UpstreamError(`${sumsDir}/SHA256SUMS is missing at the pinned commit`);
  }
  const sums = parseSums(text);
  verifySums(tasksRoot, sums, { strictDirs: taskDirs(tasksRoot) });
  return { root: dir, tasksRoot, sums, head };
}
