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
    const p = m[2].replace(/^\.\//, '');
    if (p.startsWith('/') || p.split('/').includes('..') || p.includes('\0')) throw new UpstreamError(`SHA256SUMS line ${i + 1} names an unsafe path`);
    if (sums.has(p)) throw new UpstreamError(`SHA256SUMS names ${p} twice`);
    sums.set(p, m[1]);
  }
  if (sums.size === 0) throw new UpstreamError('SHA256SUMS is empty');
  return sums;
}

/** Check every listed file against its checksum. Throws naming the first few that differ or are missing. */
export function verifySums(root, sums) {
  const bad = [];
  for (const [p, hex] of sums) {
    const full = path.join(root, p);
    let actual = null;
    try {
      if (!fs.lstatSync(full).isSymbolicLink()) actual = crypto.createHash('sha256').update(fs.readFileSync(full)).digest('hex');
    } catch {
      actual = null;
    }
    if (actual !== hex) bad.push(actual === null ? `${p} (missing)` : `${p} (differs)`);
  }
  if (bad.length > 0) throw new UpstreamError(`${bad.length} file(s) do not match SHA256SUMS: ${bad.slice(0, 3).join(', ')}${bad.length > 3 ? ', ...' : ''}`);
}

/** A task's checksum: the hash of the sorted `path<TAB>sha256` lines of the SHA256SUMS entries under its directory. */
export function taskChecksum(relDir, sums) {
  const prefix = `${relDir.split(path.sep).join('/')}/`;
  const lines = [...sums].filter(([p]) => p.startsWith(prefix)).map(([p, h]) => `${p}\t${h}`).sort();
  if (lines.length === 0) return null;
  return crypto.createHash('sha256').update(lines.join('\n')).digest('hex');
}

const isLocal = (url) => !/^[a-z][a-z0-9+.-]*:\/\//i.test(url) || url.startsWith('file://');

/**
 * Clone (or update) the upstream repository into `dir` and check out `commit`, then verify `<sumsDir>/SHA256SUMS`.
 * Returns { root, tasksRoot, sums, head }.
 */
export function fetchUpstream({ url, commit, dir, sumsDir = 'v2' }) {
  if (!/^[0-9a-f]{40}$/.test(commit ?? '')) throw new UpstreamError('tasks.upstreamCommit must be a full commit id: pin the commit in the local configuration');
  if (sumsDir.startsWith('/') || sumsDir.split('/').includes('..')) throw new UpstreamError('tasks.upstreamDir must be a relative path');
  try {
    if (!fs.existsSync(path.join(dir, '.git'))) {
      fs.mkdirSync(path.dirname(dir), { recursive: true });
      execFileSync('git', ['clone', '-q', '--no-checkout', ...(isLocal(url) ? [] : ['--filter=blob:none']), url, dir], { stdio: 'pipe' });
    }
    try {
      git(dir, ['cat-file', '-e', `${commit}^{commit}`]);
    } catch {
      git(dir, ['fetch', '-q', 'origin']);
    }
    git(dir, ['checkout', '-q', '--detach', '--force', commit]);
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
  verifySums(tasksRoot, sums);
  return { root: dir, tasksRoot, sums, head };
}
