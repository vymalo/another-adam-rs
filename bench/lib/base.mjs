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
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { localGit } from './grade.mjs';
import { repoSlug } from './gitops.mjs';

const SAFE_PATH = /^\/[A-Za-z0-9._/@+=-]*$/;
/** The name of the directory of a task's base in a slot's work volume (what `Stack.resetWork` keeps). */
export const BASE_NAME = 'bench-base';
export const BASE_DIR = `/work/${BASE_NAME}`;
/** The marker of the base in a slot's volume: which key the directory holds. */
export const BASE_MARKER = 'bench-key';
const SAFE_KEY = /^[A-Za-z0-9._-]+$/;

/**
 * The key of a task's base: its id and the image it is made from, the two things the base is a function of (the image is
 * pinned by digest, so the same key is the same tree). Also the directory name of its host repository.
 */
export function baseKey(task) {
  const id = String(task?.id ?? '').replace(/[^A-Za-z0-9._-]/g, '-');
  const image = String(task?.image?.ref ?? '');
  if (id === '' || image === '') throw new Error('a task base needs a task id and an image reference');
  return `${id}-${crypto.createHash('sha256').update(image).digest('hex').slice(0, 16)}`;
}

/** The shell that makes `<out>/base.git` (branch main) and `<out>/tree` from the repository at `workdir`. Pure. */
export function baseScript({ workdir, out = '/out', scratch = '/tmp/bench-base', key = null }) {
  for (const p of [workdir, out, scratch]) if (!SAFE_PATH.test(p)) throw new Error('a path in the base script is not a plain absolute path');
  if (key !== null && !SAFE_KEY.test(key)) throw new Error('the key of a base is not plain');
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
    ...(key === null ? [] : [`printf '%s\\n' '${key}' > ${out}/${BASE_MARKER}`]),
    `chmod -R a+rX ${out}`,
  ].join('\n');
}

const IDENTITY = ['-c', 'user.name=bench', '-c', 'user.email=bench@example.test', '-c', 'commit.gpgsign=false'];

/** Make the base in the slot's volume, in the task image, with no network: `base.git` and `tree`, and the key's marker when a key is given. */
export async function makeContainerBase({ containers, stack, slot, task, key, timeoutMs = 10 * 60 * 1000 }) {
  await stack.workDir(slot, BASE_DIR, { remove: true });
  await stack.workDir(slot, BASE_DIR);
  const r = await containers.run({
    image: task.image.ref,
    script: baseScript({ workdir: task.workdir, ...(key === undefined ? {} : { key }) }),
    workdir: '/',
    mounts: [{ source: BASE_DIR, target: '/out' }],
    network: 'none',
    timeoutMs,
  });
  if (r.exitCode !== 0) throw new Error(`the base of task ${task.id} could not be made (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-2).join(' | ')}`);
}

/** Rebuild the host's own repository of the base from a tar stream of the slot's `tree` (patch collection reads it). */
export async function makeHostBase({ stack, slot, task, hostRoot }) {
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
  return { dir, sha, slug: repoSlug(task.id), workdir: task.workdir, containerRepo: `${BASE_DIR}/base.git` };
}

/**
 * The base of a task for one run of `slot`: made in the slot's volume, then rebuilt on the host from a tar stream.
 * Returns { dir (the host's repository, for patch collection), sha (its commit), containerRepo, slug, workdir }.
 */
export async function prepareBase({ containers, stack, slot, task, hostRoot, timeoutMs = 10 * 60 * 1000 }) {
  await makeContainerBase({ containers, stack, slot, task, timeoutMs });
  return makeHostBase({ stack, slot, task, hostRoot });
}

/**
 * The bases of one harness, per (task, image), for its life: the slot's copy is reused while its marker names the key
 * (and its `base.git` and `tree` are there), and the host's repository is made once and shared by every slot. Without it
 * every run of a task rebuilt both, which is the same git clone, tar stream and host commit again and again (two runs per
 * task per arm).
 *
 * A slot's volume is the coder's workspace too, so an agent can remove or edit what is in it; the marker catches a
 * deleted or replaced base, not an edited one. The tasks are benchmark fixtures, not hostile, and the host's repository
 * (the patch base) is never in the volume.
 */
export class BaseCache {
  #host = new Map(); // hostRoot -> the host's base
  #pending = new Map(); // hostRoot -> the build in flight

  async prepare({ containers, stack, slot, task, hostRoot, timeoutMs = 10 * 60 * 1000 }) {
    const key = baseKey(task);
    const names = await stack.listDir(slot, BASE_DIR);
    let reuse = names.includes('base.git') && names.includes('tree') && names.includes(BASE_MARKER);
    if (reuse) {
      const { files } = await stack.readFiles(slot, BASE_DIR, [BASE_MARKER]);
      reuse = (files.get(BASE_MARKER)?.toString('utf8').trim() ?? null) === key;
    }
    if (!reuse) await makeContainerBase({ containers, stack, slot, task, key, timeoutMs });
    return this.#hostBase(hostRoot, () => makeHostBase({ stack, slot, task, hostRoot }));
  }

  #hostBase(hostRoot, make) {
    const done = this.#host.get(hostRoot);
    if (done) return done;
    let pending = this.#pending.get(hostRoot);
    if (!pending) {
      pending = make().then(
        (base) => {
          this.#host.set(hostRoot, base);
          this.#pending.delete(hostRoot);
          return base;
        },
        (e) => {
          this.#pending.delete(hostRoot);
          throw e;
        },
      );
      this.#pending.set(hostRoot, pending);
    }
    return pending;
  }
}
