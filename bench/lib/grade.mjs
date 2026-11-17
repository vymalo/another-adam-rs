// Patch collection from a final tree, and grading by patch replay in a pristine container.
//
// A principle first: grading runs the agent's code (its patched sources run inside the test process), so no
// grader can be made tamper-proof against code that runs inside it. This module makes tampering hard and
// visible: it takes nothing from the agent's tree but its files (never its `.git`), strips from the patch what
// may not change (tests and the machinery that runs them), demands that every signal agree (exit status 0, a
// reward of exactly `1` written during the attempt) and flags as `suspect` every patch that touches the
// grading's own machinery, so that a person reads it. The report lists the suspect runs.
//
// collectPatch: the files of the agent's tree are copied over a clone of the base made from the harness's own
// repository, then `git add -A` and `git diff --cached --binary <base>` run there, leaving out the symlinks the
// adapter committed into the base, stopping above 5 MB.
// gradePatch: the (stripped) patch is applied in a fresh container of the task image, /tests is mounted
// read-only in that container only, `bash /tests/test.sh` runs, and /logs/verifier/reward.txt says 1 or 0.
// Three attempts; two of three pass; the three disagreeing is `flaky`; an attempt that ends in an
// infrastructure error is retried twice and then the grade is `invalid`, never a fail.
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { classifyPath } from './mine.mjs';

export const MAX_PATCH_BYTES = 5 * 1024 * 1024;
export const GRADE_ATTEMPTS = 3;
export const GRADE_PASS_AT_LEAST = 2;
export const INFRA_RETRIES = 2;

export class PatchTooLarge extends Error {
  constructor(limit) {
    super(`the patch is larger than ${limit} bytes`);
    this.name = 'PatchTooLarge';
  }
}

/** What git runs with on the harness's side: no system or global configuration, no hooks, attributes, fsmonitor or textconv. */
const SAFE_C = ['-c', 'core.quotepath=off', '-c', 'core.autocrlf=false', '-c', 'core.fsmonitor=false', '-c', 'core.hooksPath=/dev/null', '-c', 'core.attributesFile=/dev/null', '-c', 'protocol.ext.allow=never'];
const safeEnv = () => ({ ...process.env, GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null', GIT_TERMINAL_PROMPT: '0', GIT_DIR: undefined, GIT_WORK_TREE: undefined, GIT_INDEX_FILE: undefined });

/** A `git(args, { maxBytes })` that runs git in `dir`, safely, and returns stdout as a Buffer. */
export function localGit(dir) {
  return (args, { maxBytes = Infinity, input } = {}) =>
    new Promise((resolve, reject) => {
      const child = spawn('git', [...SAFE_C, '-C', dir, ...args], { stdio: ['pipe', 'pipe', 'pipe'], env: safeEnv() });
      const chunks = [];
      let size = 0;
      let err = '';
      let over = false;
      child.stdout.on('data', (c) => {
        size += c.length;
        if (size > maxBytes) {
          over = true;
          child.kill('SIGKILL');
        } else chunks.push(c);
      });
      child.stderr.on('data', (c) => {
        if (err.length < 2000) err += c;
      });
      child.stdin.on('error', () => {});
      child.stdin.end(input ?? '');
      child.on('error', reject);
      child.on('close', (code) => {
        if (over) return reject(new PatchTooLarge(maxBytes));
        if (code !== 0) {
          const e = new Error(`git ${args[0]} failed (${code}): ${err.trim().split('\n')[0]}`);
          e.code = code;
          return reject(e);
        }
        resolve(Buffer.concat(chunks));
      });
    });
}

/** The tracked symlinks of `base` whose target is under `linkPrefix`: paths the patch must not carry. */
export async function adapterSymlinks(git, base, linkPrefix = '/app') {
  const tree = (await git(['ls-tree', '-r', '-z', base])).toString('utf8').split('\0').filter(Boolean);
  const out = [];
  for (const entry of tree) {
    const m = /^120000 blob ([0-9a-f]+)\t(.*)$/s.exec(entry);
    if (!m) continue;
    const target = (await git(['cat-file', 'blob', m[1]])).toString('utf8').trim();
    if (target === linkPrefix || target.startsWith(`${linkPrefix}/`)) out.push(m[2]);
  }
  return out;
}

/**
 * Copy the files of `tree` over the checked-out clone `work`: never the agent's `.git`, never into a directory the
 * base ignores, never through a symlink (links are copied as links), never a nested repository, never a
 * `.gitignore` (the base's own stay). Tracked files the tree no longer has are removed.
 */
async function copyTree(tree, work, git) {
  let level = [''];
  while (level.length > 0) {
    const dirs = [];
    for (const rel of level) {
      const from = path.join(tree, rel);
      const to = path.join(work, rel);
      fs.mkdirSync(to, { recursive: true });
      for (const e of fs.readdirSync(from, { withFileTypes: true })) {
        if (e.name === '.git' && rel === '') continue;
        const r = rel === '' ? e.name : `${rel}/${e.name}`;
        const src = path.join(tree, r);
        const dest = path.join(work, r);
        if (e.isDirectory()) {
          if (fs.existsSync(path.join(src, '.git'))) continue;
          dirs.push(r);
        } else if (e.isSymbolicLink()) {
          fs.rmSync(dest, { recursive: true, force: true });
          fs.symlinkSync(fs.readlinkSync(src), dest);
        } else if (e.isFile()) {
          if (e.name === '.gitignore') continue;
          fs.rmSync(dest, { recursive: true, force: true });
          fs.copyFileSync(src, dest);
          fs.chmodSync(dest, fs.lstatSync(src).mode & 0o777);
        }
      }
    }
    let ignored = new Set();
    if (dirs.length > 0) {
      try {
        const out = await git(['check-ignore', '-z', '--stdin'], { input: dirs.map((d) => `${d}/\0`).join('') });
        ignored = new Set(out.toString('utf8').split('\0').filter(Boolean).map((p) => p.replace(/\/$/, '')));
      } catch (e) {
        if (e.code !== 1) throw e;
      }
    }
    level = dirs.filter((d) => !ignored.has(d));
    for (const d of level) {
      const dest = path.join(work, d);
      let st = null;
      try {
        st = fs.lstatSync(dest);
      } catch {
        st = null;
      }
      if (st && !st.isDirectory()) fs.rmSync(dest, { force: true });
    }
  }
  const tracked = (await git(['ls-files', '-z'])).toString('utf8').split('\0').filter(Boolean);
  for (const p of tracked) {
    if (path.posix.basename(p) === '.gitignore') continue;
    try {
      fs.lstatSync(path.join(tree, p));
    } catch {
      fs.rmSync(path.join(work, p), { force: true });
    }
  }
}

/**
 * The diff of the agent's final tree against `base`, untracked files and binaries included. Returns a Buffer.
 * `baseRepo` is a repository the harness owns that holds `base`; the agent's `.git` is never read or run.
 * The base's `.gitignore` decides what is left out (the agent's changes to it are dropped); nested repositories
 * are not entered.
 */
export async function collectPatch({ tree, baseRepo, base, maxBytes = MAX_PATCH_BYTES, linkPrefix = '/app', workRoot = os.tmpdir() }) {
  if (!/^[0-9a-f]{40}$/.test(base)) throw new Error('the base must be a full commit id');
  const scratch = fs.mkdtempSync(path.join(workRoot, 'collect-'));
  try {
    const work = path.join(scratch, 'w');
    execFileSync('git', [...SAFE_C, '-c', 'protocol.file.allow=always', 'clone', '-q', '--no-checkout', '--no-hardlinks', '--', baseRepo, work], { env: safeEnv(), stdio: 'pipe' });
    const git = localGit(work);
    await git(['checkout', '-q', '--detach', base]);
    const exclude = await adapterSymlinks(git, base, linkPrefix);
    await copyTree(tree, work, git);
    await git(['add', '-A']);
    const specs = exclude.map((p) => `:(exclude,literal)${p}`);
    return await git(['diff', '--cached', '--binary', '--no-color', '--no-ext-diff', '--no-textconv', base, '--', '.', ...specs], { maxBytes });
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
}

// ---- what the agent's patch may not change ----

const PROTECTED_NAMES = new Set(['conftest.py', 'pytest.ini', 'tox.ini', 'noxfile.py', '.gitattributes', 'mocha.opts']);
const PROTECTED_PATTERNS = [/^(jest|vitest)\.(config|setup|workspace)\.[A-Za-z0-9.]+$/, /^\.mocharc\.[A-Za-z0-9.]+$/, /^karma\.conf\.[A-Za-z0-9.]+$/];
const CONFIG_FILES = new Set(['pyproject.toml', 'setup.cfg', 'Cargo.toml', 'package.json', 'go.mod']);
const TEST_CONFIG_LINE = /\[tool\.pytest|\[tool:pytest\]|\[pytest\]|\[\[test\]\]|\[\[bench\]\]|\bharness\b|"test"\s*:|"jest"\s*:|"vitest"\s*:|"mocha"\s*:/;
const MENTIONS = /reward\.txt|\/logs\/verifier|\/tests\/|test\.sh/;

const isProtected = (p, hidden) => hidden.has(p) || classifyPath(p) === 'test' || PROTECTED_NAMES.has(path.posix.basename(p)) || PROTECTED_PATTERNS.some((re) => re.test(path.posix.basename(p)));

function sectionPaths(section) {
  const paths = new Set();
  const head = section.slice(0, section.indexOf('\n') < 0 ? undefined : section.indexOf('\n')).replace(/^diff --git /, '');
  const quoted = /^"a\/(.*)" "b\/(.*)"$/.exec(head);
  if (quoted) {
    paths.add(quoted[1]);
    paths.add(quoted[2]);
  } else {
    for (let i = head.indexOf(' b/'); i >= 0; i = head.indexOf(' b/', i + 1)) {
      const left = head.slice(2, i);
      const right = head.slice(i + 3);
      if (head.startsWith('a/') && left === right) {
        paths.add(left);
        break;
      }
    }
  }
  for (const m of section.matchAll(/^(?:rename|copy) (?:from|to) (.+)$/gm)) paths.add(m[1]);
  for (const m of section.matchAll(/^(?:---|\+\+\+) [ab]\/(.+)$/gm)) paths.add(m[1]);
  return [...paths];
}

/** Paths a task's hidden tests change, read from its `tests/test_patch.patch` if it has one. */
export function hiddenTestPaths(task) {
  try {
    const text = fs.readFileSync(path.join(task.dir, 'tests', 'test_patch.patch'), 'utf8');
    return text.split(/^(?=diff --git )/m).filter((s) => s.startsWith('diff --git ')).flatMap(sectionPaths);
  } catch {
    return [];
  }
}

/**
 * Remove from a patch every change to the tests and to the machinery that runs them. Keeps but flags
 * (`suspect`) changes that configure the tests and any patch that mentions the grading's own files.
 * Returns { patch, removed, suspect }.
 */
export function sanitizePatch(patch, { hiddenTests = [] } = {}) {
  const hidden = new Set(hiddenTests);
  const text = String(patch);
  const sections = text.split(/^(?=diff --git )/m);
  const kept = [];
  const removed = [];
  const suspect = [];
  for (const section of sections) {
    if (!section.startsWith('diff --git ')) {
      kept.push(section);
      continue;
    }
    const paths = sectionPaths(section);
    const hit = paths.filter((p) => isProtected(p, hidden));
    if (hit.length > 0) {
      for (const p of new Set(hit)) removed.push(p);
      continue;
    }
    kept.push(section);
    for (const p of paths) {
      if (!CONFIG_FILES.has(path.posix.basename(p))) continue;
      const added = section.split('\n').filter((l) => l.startsWith('+') && !l.startsWith('+++')).join('\n');
      if (TEST_CONFIG_LINE.test(added) && !suspect.includes(`test-configuration:${p}`)) suspect.push(`test-configuration:${p}`);
    }
  }
  const added = text.split('\n').filter((l) => l.startsWith('+') && !l.startsWith('+++')).join('\n');
  if (MENTIONS.test(added)) suspect.push('mentions-grading');
  return { patch: kept.join(''), removed: [...new Set(removed)].sort(), suspect };
}

// ---- grading ----

/** The network a task's verifier asks for, as a RunSpec network. */
export function verifierNetwork(task) {
  const n = task.verifierNetwork;
  return n === 'public' || n === 'default' ? 'default' : n === 'none' || n === 'no-network' ? 'none' : n;
}

/**
 * The shell of one grading run. Pristine: the patch is applied in the task's own working directory.
 * With `cloneTo` (the environment check): the base is cloned to the path the agents will use, the
 * top-level ignored entries are linked to the original's, the patch is applied there and the test
 * script runs with the working directory rewritten to the clone.
 */
export function gradeScript({ workdir = '/app', cloneTo = null } = {}) {
  const apply = `if [ -s /patch/model.patch ]; then git apply --whitespace=nowarn /patch/model.patch || { echo 'bench: the patch does not apply' >&2; exit 97; }; fi`;
  if (cloneTo === null) {
    return ['mkdir -p /logs/verifier', `cd ${workdir} || exit 98`, apply, 'bash /tests/test.sh'].join('\n');
  }
  return [
    'mkdir -p /logs/verifier',
    `rm -rf ${cloneTo} && mkdir -p "$(dirname ${cloneTo})" || exit 98`,
    `git clone -q --no-hardlinks ${workdir} ${cloneTo} || exit 98`,
    `(cd ${workdir} && git status --porcelain --ignored | grep '^!! ' | cut -c4-) | while IFS= read -r p; do p=\${p%/}; case "$p" in */*) continue;; esac; ln -s ${workdir}/"$p" ${cloneTo}/"$p"; done`,
    `cd ${cloneTo} || exit 98`,
    apply,
    `sed 's#${workdir}\\([^A-Za-z0-9_]\\|$\\)#${cloneTo}\\1#g' /tests/test.sh > /tmp/bench-test.sh`,
    'bash /tmp/bench-test.sh',
  ].join('\n');
}

const STALE_SLACK_MS = 1000;

/**
 * One grading run, in a directory of its own that is empty when the container starts and is read only after
 * the container has ended. A pass needs exit status 0, a reward file of exactly `1` written during the
 * attempt, and no error. A reward of 1 with another exit status is a fail and `suspect`.
 * Returns { pass, reward, error, exitCode, suspect }.
 */
export async function gradeOnce({ containers, task, patchFile = null, cloneTo = null, workRoot = os.tmpdir(), timeoutMs = 30 * 60 * 1000 }) {
  const logs = fs.mkdtempSync(path.join(workRoot, 'grade-logs-'));
  fs.chmodSync(logs, 0o777);
  try {
    const mounts = [
      { source: path.join(task.dir, 'tests'), target: '/tests', readOnly: true },
      { source: logs, target: '/logs/verifier' },
    ];
    if (patchFile) mounts.push({ source: path.resolve(patchFile), target: '/patch/model.patch', readOnly: true });
    const startedMs = Date.now();
    const r = await containers.run({
      image: task.image.ref,
      script: gradeScript({ workdir: task.workdir, cloneTo }),
      workdir: '/',
      mounts,
      network: verifierNetwork(task),
      cpus: task.cpus,
      memoryMb: task.memoryMb,
      timeoutMs,
    });
    let reward = null;
    let stale = false;
    try {
      const file = path.join(logs, 'reward.txt');
      stale = fs.statSync(file).mtimeMs < startedMs - STALE_SLACK_MS;
      reward = fs.readFileSync(file, 'utf8').trim();
    } catch {
      reward = null;
    }
    const suspect = [];
    let error = null;
    if (r.timedOut) error = 'timeout';
    else if (r.exitCode === 97) error = 'apply';
    else if (r.exitCode === 98 || r.exitCode === 125 || r.exitCode === 126 || r.exitCode === 127) error = 'infra';
    else if (reward === null) error = 'no-reward';
    else if (stale) error = 'stale-reward';
    else if (reward !== '1' && reward !== '0') error = 'bad-reward';
    else if (r.exitCode !== 0) {
      error = 'exit-nonzero';
      if (reward === '1') suspect.push('reward-with-failed-exit');
    }
    return { pass: error === null && reward === '1', reward, error, exitCode: r.exitCode, suspect };
  } finally {
    fs.rmSync(logs, { recursive: true, force: true });
  }
}

/** One grading attempt, tried again (at most twice) after an infrastructure error; still one: `invalid`. */
export async function gradeAttempt(opts) {
  let r;
  for (let i = 0; i <= INFRA_RETRIES; i++) {
    r = await gradeOnce(opts);
    if (r.error !== 'infra' && r.error !== 'timeout') return { ...r, invalid: false };
  }
  return { ...r, invalid: true };
}

/**
 * Grade a patch three times: pass at two of three; `flaky` when the three do not all agree; `invalid` (and
 * never a fail) when an attempt stays an infrastructure error. The patch is stripped first (`sanitizePatch`);
 * `removed` and `suspect` say what was taken out and what a person should read.
 */
export async function gradePatch({ containers, task, patchFile = null, workRoot = os.tmpdir(), hiddenTests, timeoutMs }) {
  let mounted = patchFile;
  let removed = [];
  let suspect = [];
  const scratch = fs.mkdtempSync(path.join(workRoot, 'patch-'));
  try {
    if (patchFile) {
      const s = sanitizePatch(fs.readFileSync(patchFile, 'utf8'), { hiddenTests: hiddenTests ?? hiddenTestPaths(task) });
      removed = s.removed;
      suspect = [...s.suspect];
      mounted = s.patch.trim() === '' ? null : path.join(scratch, 'model.patch');
      if (mounted) fs.writeFileSync(mounted, s.patch);
    }
    const attempts = [];
    for (let i = 0; i < GRADE_ATTEMPTS; i++) attempts.push(await gradeAttempt({ containers, task, patchFile: mounted, workRoot, timeoutMs }));
    for (const a of attempts) for (const s of a.suspect) if (!suspect.includes(s)) suspect.push(s);
    const invalid = attempts.some((a) => a.invalid);
    const passes = attempts.filter((a) => a.pass).length;
    return {
      pass: !invalid && passes >= GRADE_PASS_AT_LEAST,
      passes,
      flaky: !invalid && passes !== 0 && passes !== GRADE_ATTEMPTS,
      invalid,
      attempts,
      removed,
      suspect,
      patch: patchFile,
    };
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
}
