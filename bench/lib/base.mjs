// The base of a task, as both agents clone it: the repository of the task image (`/app`, or the task's working
// directory) at its base commit, plus one commit that links the ignored top-level entries (`node_modules`, a
// virtualenv, a build directory) to the image's own, so that the dependencies the image carries are found from
// any clone path in the same image. The links are tracked in that commit and so are part of the base; patch
// collection leaves them out of the diff (`adapterSymlinks` in grade.mjs).
//
// The base is made inside the task image (its git, its files, no network) into a bare repository the harness owns:
// adam gets it from the stack's git server, OpenCode clones it from the shared directory.
import fs from 'node:fs';
import path from 'node:path';
import { repoSlug } from './gitops.mjs';

const SAFE_PATH = /^\/[A-Za-z0-9._/@+=-]*$/;

/** The shell that makes `<out>/base.git` (branch main) and `<out>/base.sha` from the repository at `workdir`. Pure. */
export function baseScript({ workdir, out = '/out', scratch = '/tmp/bench-base' }) {
  for (const p of [workdir, out, scratch]) if (!SAFE_PATH.test(p)) throw new Error('a path in the base script is not a plain absolute path');
  return [
    'set -eu',
    // the image's repository may belong to another user than the one this runs as
    "export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0='*'",
    `W=${scratch}`,
    `rm -rf "$W" ${out}/base.git`,
    `git clone -q --no-hardlinks ${workdir} "$W"`,
    'cd "$W"',
    'git checkout -q -B main',
    `(cd ${workdir} && git status --porcelain --ignored | grep '^!! ' | cut -c4- || true) | while IFS= read -r p; do`,
    '  p=${p%/}',
    '  case "$p" in */*|"") continue;; esac',
    '  rm -rf "./$p"',
    `  ln -s "${workdir}/$p" "./$p"`,
    'done',
    'git add -A -f',
    'if ! git diff --cached --quiet; then',
    "  git -c user.name=bench -c user.email=bench@example.test -c commit.gpgsign=false commit -q -m 'bench: link the ignored dependency directories'",
    'fi',
    `git clone -q --bare "$W" ${out}/base.git`,
    `git -C ${out}/base.git rev-parse refs/heads/main > ${out}/base.sha`,
    `chmod -R a+rX ${out}`,
  ].join('\n');
}

/**
 * The base of a task, made once for the task's image and kept under `root`.
 * Returns { dir (the bare repository), sha, slug, workdir }.
 */
export async function prepareBase({ containers, task, root, timeoutMs = 10 * 60 * 1000 }) {
  const slug = repoSlug(task.id);
  const out = path.join(root, `${slug}-${task.image.digest.slice(7, 19)}`);
  const record = path.join(out, 'base.json');
  try {
    const kept = JSON.parse(fs.readFileSync(record, 'utf8'));
    if (/^[0-9a-f]{40}$/.test(kept.sha) && fs.existsSync(path.join(out, 'base.git'))) return { dir: path.join(out, 'base.git'), sha: kept.sha, slug, workdir: task.workdir };
  } catch {
    // not made yet
  }
  fs.rmSync(out, { recursive: true, force: true });
  fs.mkdirSync(out, { recursive: true });
  fs.chmodSync(out, 0o777); // the container's user writes here
  const r = await containers.run({
    image: task.image.ref,
    script: baseScript({ workdir: task.workdir }),
    workdir: '/',
    mounts: [{ source: out, target: '/out' }],
    network: 'none',
    timeoutMs,
  });
  if (r.exitCode !== 0) throw new Error(`the base of task ${task.id} could not be made (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-2).join(' | ')}`);
  const sha = fs.readFileSync(path.join(out, 'base.sha'), 'utf8').trim();
  if (!/^[0-9a-f]{40}$/.test(sha)) throw new Error(`the base of task ${task.id} has no commit id`);
  fs.writeFileSync(record, `${JSON.stringify({ sha, slug })}\n`);
  return { dir: path.join(out, 'base.git'), sha, slug, workdir: task.workdir };
}
