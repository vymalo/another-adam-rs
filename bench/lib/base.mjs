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
/** The marker of the base in a slot's volume: the bare repository's HEAD and the tree's digest, not a name. */
export const BASE_MARKER = 'base-marker';

/**
 * The key of a task's base: its id, the image it is made from and the working directory the base script clones. The image
 * is pinned by digest, so the same key is the same tree; the working directory is in the key because the script's links
 * point into it, so two tasks that differ only there are two bases. Also the directory name of its host repository.
 */
export function baseKey(task) {
  const id = String(task?.id ?? '').replace(/[^A-Za-z0-9._-]/g, '-');
  const image = String(task?.image?.ref ?? '');
  const workdir = String(task?.workdir ?? '');
  if (id === '' || image === '' || workdir === '') throw new Error('a task base needs a task id, an image reference and a working directory');
  return `${id}-${crypto.createHash('sha256').update(`${image}\0${workdir}`).digest('hex').slice(0, 16)}`;
}

/** The marker's content: the bare repository's HEAD and the digest of the tree it carries. Null when it is not that. */
export function parseBaseMarker(text) {
  const m = /^([0-9a-f]{40}) ([0-9a-f]{40})\n?$/.exec(String(text ?? ''));
  return m ? { commit: m[1], tree: m[2] } : null;
}

/** The sha a `packed-refs` file gives `name`, or ''. Peeled tag lines (`^...`) and comments are skipped. */
export function packedRef(text, name) {
  for (const line of String(text ?? '').split('\n')) {
    const trimmed = line.trim();
    if (trimmed === '' || trimmed.startsWith('#') || trimmed.startsWith('^')) continue;
    const [sha, ref] = trimmed.split(' ');
    if (ref === name && /^[0-9a-f]{40}$/.test(sha)) return sha;
  }
  return '';
}

/** The shell that makes `<out>/base.git` (branch main), `<out>/tree` and the marker, from the repository at `workdir`. Pure. */
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
    // What the cache trusts is the content, not a name: the ref a clone would read, and the tree the host rebuilds.
    `printf '%s %s\n' "$(git -C ${out}/base.git rev-parse refs/heads/main)" "$(git -C "$W" rev-parse 'HEAD^{tree}')" > ${out}/${BASE_MARKER}`,
    // The work directory is the host's, owned by the host user: a container as uid 10001 cannot chmod it.
    `chmod -R a+rX ${out} 2>/dev/null || true`,
  ].join('\n');
}

const IDENTITY = ['-c', 'user.name=bench', '-c', 'user.email=bench@example.test', '-c', 'commit.gpgsign=false'];

/** Make the base in the slot's volume, in the task image, with no network: `base.git`, `tree` and the marker. */
export async function makeContainerBase({ containers, stack, slot, task, timeoutMs = 10 * 60 * 1000 }) {
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
}

/** Rebuild the host's own repository of the base from a tar stream of the slot's `tree` (patch collection reads it). */
export async function makeHostBase({ stack, slot, task, hostRoot }) {
  const scratch = path.join(hostRoot, 'base-tree');
  const dir = path.join(hostRoot, 'base');
  fs.rmSync(dir, { recursive: true, force: true });
  await stack.extractTree(slot, `${BASE_DIR}/tree`, dir);
  fs.rmSync(scratch, { recursive: true, force: true });
  const git = localGit(dir);
  await git(['init', '-q', '-b', 'main']);
  await git(['add', '-A', '-f']);
  await git([...IDENTITY, 'commit', '-q', '--allow-empty', '-m', 'bench base']);
  const sha = (await git(['rev-parse', 'HEAD'])).toString('utf8').trim();
  const tree = (await git(['rev-parse', 'HEAD^{tree}'])).toString('utf8').trim();
  if (!/^[0-9a-f]{40}$/.test(sha) || !/^[0-9a-f]{40}$/.test(tree)) throw new Error(`the base of task ${task.id} has no commit id`);
  return { dir, sha, tree, slug: repoSlug(task.id), workdir: task.workdir, containerRepo: `${BASE_DIR}/base.git` };
}

/**
 * The bases of one harness, per (task, image, workdir), for its life: the slot's copy is reused while its marker records
 * the content that is there (the bare repository's HEAD, and the tree the host rebuilds), and the host's repository is
 * made once and shared by every slot. Without it every run of a task rebuilt both, which is the same git clone, tar
 * stream and host commit again and again (two runs per task per arm).
 *
 * A slot's volume is the coder's workspace too, so an agent can edit what is in it. The ref check catches a base whose
 * branch was changed (an object cannot be edited in place and keep its id), and the host copy's tree is compared to the
 * marker's; a `tree` edited after the host copy was made is never read again in this process, and one edited before it
 * is caught and rebuilt from the image. The host's repository (the patch base) is never in the volume.
 */
export class BaseCache {
  #host = new Map(); // hostRoot -> the host's base
  #pending = new Map(); // hostRoot -> the build in flight

  async prepare({ containers, stack, slot, task, hostRoot, timeoutMs = 10 * 60 * 1000 }) {
    let marker = await this.#markerOf(stack, slot);
    if (!marker) {
      await makeContainerBase({ containers, stack, slot, task, timeoutMs });
      marker = await this.#markerOf(stack, slot);
      if (!marker) throw new Error(`the base of task ${task.id} has no content marker`);
    }
    let base = await this.#hostBase(hostRoot, () => makeHostBase({ stack, slot, task, hostRoot }));
    if (base.tree !== marker.tree) {
      // The host copy was made from a slot whose tree is not the base (edited before it was built): remake the slot's
      // copy from the image and the host's from it, once.
      await makeContainerBase({ containers, stack, slot, task, timeoutMs });
      const fresh = await this.#markerOf(stack, slot);
      if (!fresh) throw new Error(`the base of task ${task.id} has no content marker`);
      this.#host.delete(hostRoot);
      base = await this.#hostBase(hostRoot, () => makeHostBase({ stack, slot, task, hostRoot }));
      if (base.tree !== fresh.tree) throw new Error(`the base of task ${task.id} does not match its marker`);
    }
    return base;
  }

  async #markerOf(stack, slot) {
    const names = await stack.listDir(slot, BASE_DIR);
    if (!names.includes('base.git') || !names.includes('tree') || !names.includes(BASE_MARKER)) return null;
    const { files } = await stack.readFiles(slot, BASE_DIR, [BASE_MARKER, 'base.git/refs/heads/main', 'base.git/packed-refs']);
    const marker = parseBaseMarker(files.get(BASE_MARKER)?.toString('utf8'));
    // The ref is what an agent's clone reads: a commit added there changes it, and an edited object cannot keep its id.
    // A clone reads the loose ref, or the packed one after `git pack-refs`/`git gc`; both count.
    const loose = (files.get('base.git/refs/heads/main')?.toString('utf8') ?? '').trim();
    const ref = loose || packedRef(files.get('base.git/packed-refs')?.toString('utf8'), 'refs/heads/main');
    return marker && marker.commit === ref ? marker : null;
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
