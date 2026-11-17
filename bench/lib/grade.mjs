// Patch collection from a final tree, and grading by patch replay in a pristine container.
//
// collectPatch: `git add -A` then `git diff --cached --binary <base>` in the tree, leaving out the symlinks
// the adapter committed into the base (a link whose target is under the task's /app), stopping above 5 MB.
// gradePatch: the patch is applied in a fresh container of the task image, /tests is mounted read-only in
// that container only, `bash /tests/test.sh` runs, and /logs/verifier/reward.txt says 1 or 0. Three times;
// two of three pass; the three disagreeing is `flaky`.
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

export const MAX_PATCH_BYTES = 5 * 1024 * 1024;
export const GRADE_ATTEMPTS = 3;
export const GRADE_PASS_AT_LEAST = 2;

export class PatchTooLarge extends Error {
  constructor(limit) {
    super(`the patch is larger than ${limit} bytes`);
    this.name = 'PatchTooLarge';
  }
}

/** A `git(args, { maxBytes })` that runs git in `dir` and returns stdout as a Buffer. */
export function localGit(dir) {
  return (args, { maxBytes = Infinity } = {}) =>
    new Promise((resolve, reject) => {
      const child = spawn('git', ['-c', 'core.quotepath=off', '-c', 'core.autocrlf=false', '-c', 'core.fsmonitor=false', '-C', dir, ...args], { stdio: ['ignore', 'pipe', 'pipe'] });
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
      child.on('error', reject);
      child.on('close', (code) => {
        if (over) return reject(new PatchTooLarge(maxBytes));
        if (code !== 0) return reject(new Error(`git ${args[0]} failed (${code}): ${err.trim().split('\n')[0]}`));
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

/** The diff of the final tree against `base`, untracked files and binaries included. Returns a Buffer. */
export async function collectPatch({ git, base, maxBytes = MAX_PATCH_BYTES, linkPrefix = '/app' }) {
  if (!/^[0-9a-f]{40}$/.test(base)) throw new Error('the base must be a full commit id');
  const exclude = await adapterSymlinks(git, base, linkPrefix);
  await git(['add', '-A']);
  const specs = exclude.map((p) => `:(exclude,literal)${p}`);
  return git(['diff', '--cached', '--binary', '--no-color', '--no-ext-diff', base, '--', '.', ...specs], { maxBytes });
}

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

/** One grading run. Returns { pass, reward, error, exitCode }. */
export async function gradeOnce({ containers, task, patchFile = null, cloneTo = null, workRoot = os.tmpdir(), timeoutMs = 30 * 60 * 1000 }) {
  const logs = fs.mkdtempSync(path.join(workRoot, 'grade-logs-'));
  fs.chmodSync(logs, 0o777);
  try {
    const mounts = [
      { source: path.join(task.dir, 'tests'), target: '/tests', readOnly: true },
      { source: logs, target: '/logs/verifier' },
    ];
    if (patchFile) mounts.push({ source: path.resolve(patchFile), target: '/patch/model.patch', readOnly: true });
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
    try {
      reward = fs.readFileSync(path.join(logs, 'reward.txt'), 'utf8').trim();
    } catch {
      reward = null;
    }
    let error = null;
    if (r.timedOut) error = 'timeout';
    else if (r.exitCode === 97) error = 'apply';
    else if (r.exitCode === 98 || r.exitCode === 125 || r.exitCode === 126 || r.exitCode === 127) error = 'infra';
    else if (reward === null) error = 'no-reward';
    else if (reward !== '1' && reward !== '0') error = 'bad-reward';
    return { pass: reward === '1' && error === null, reward, error, exitCode: r.exitCode };
  } finally {
    fs.rmSync(logs, { recursive: true, force: true });
  }
}

/** Grade a patch three times: pass at two of three; `flaky` when the three do not all agree. */
export async function gradePatch(opts) {
  const attempts = [];
  for (let i = 0; i < GRADE_ATTEMPTS; i++) attempts.push(await gradeOnce(opts));
  const passes = attempts.filter((a) => a.pass).length;
  return {
    pass: passes >= GRADE_PASS_AT_LEAST,
    passes,
    flaky: passes !== 0 && passes !== GRADE_ATTEMPTS,
    attempts,
  };
}
