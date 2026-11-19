// The base of a task, as both agents clone it: the repository of the task image (`/app`, or the task's working
// directory) at its base commit, plus one commit that links the ignored top-level entries (`node_modules`, a
// virtualenv, a build directory) to the image's own, so that the dependencies the image carries are found from
// any clone path in the same image. The links are tracked in that commit and so are part of the base; patch
// collection leaves them out of the diff (`adapterSymlinks` in grade.mjs).
//
// Two copies, because the host runs no git in a place a container could write:
//   * in the slot's work volume, made inside the task image with no network: `base.git` (what adam's pusher and
//     OpenCode's clone read) and `tree` (the same files, without `.git`);
//   * on the host, made by the harness from a tar stream of `tree`: a repository the harness owns, which patch
//     collection clones. Its commit is not the container's; the trees are the same.
import fs from 'node:fs';
import path from 'node:path';
import { localGit } from './grade.mjs';
import { repoSlug } from './gitops.mjs';

const SAFE_PATH = /^\/[A-Za-z0-9._/@+=-]*$/;
export const BASE_DIR = '/work/bench-base';

/** The shell that makes `<out>/base.git` (branch main) and `<out>/tree` from the repository at `workdir`. Pure. */
export function baseScript({ workdir, out = '/out', scratch = '/tmp/bench-base' }) {
  for (const p of [workdir, out, scratch]) if (!SAFE_PATH.test(p)) throw new Error('a path in the base script is not a plain absolute path');
  return [
    'set -eu',
    // the image's repository may belong to another user than the one this runs as
    "export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0='*'",
    `W=${scratch}`,
    `rm -rf "$W" ${out}/base.git ${out}/tree`,
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
    `mkdir ${out}/tree`,
    `tar --exclude=./.git -cf - . | tar -xf - -C ${out}/tree`,
    // The work directory is the host's, owned by the host user: a container as uid 10001 cannot chmod it.
    `chmod -R a+rX ${out} 2>/dev/null || true`,
  ].join('\n');
}

const IDENTITY = ['-c', 'user.name=bench', '-c', 'user.email=bench@example.test', '-c', 'commit.gpgsign=false'];

/**
 * The base of a task for one run of `slot`: made in the slot's volume, then rebuilt on the host from a tar stream.
 * Returns { dir (the host's repository, for patch collection), sha (its commit), containerRepo, slug, workdir }.
 */
export async function prepareBase({ containers, stack, slot, task, hostRoot, timeoutMs = 10 * 60 * 1000 }) {
  const slug = repoSlug(task.id);
  await stack.workDir(slot, BASE_DIR, { remove: true });
  await stack.workDir(slot, BASE_DIR);
  const r = await containers.run({
    image: task.image.ref,
    script: baseScript({ workdir: task.workdir }),
    workdir: '/',
    mounts: [{ source: path.join(stack.workRoot(slot), 'bench-base'), target: '/out' }],
    network: 'none',
    timeoutMs,
  });
  if (r.exitCode !== 0) throw new Error(`the base of task ${task.id} could not be made (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-2).join(' | ')}`);
  const tree = path.join(hostRoot, 'base-tree');
  const dir = path.join(hostRoot, 'base');
  fs.rmSync(dir, { recursive: true, force: true });
  await stack.extractTree(slot, `${BASE_DIR}/tree`, dir);
  fs.rmSync(tree, { recursive: true, force: true });
  const git = localGit(dir);
  await git(['init', '-q', '-b', 'main']);
  await git(['add', '-A', '-f']);
  await git([...IDENTITY, 'commit', '-q', '--allow-empty', '-m', 'bench base']);
  const sha = (await git(['rev-parse', 'HEAD'])).toString('utf8').trim();
  if (!/^[0-9a-f]{40}$/.test(sha)) throw new Error(`the base of task ${task.id} has no commit id`);
  return { dir, sha, slug, workdir: task.workdir, containerRepo: `${BASE_DIR}/base.git` };
}
