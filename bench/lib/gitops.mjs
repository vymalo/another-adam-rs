// The git the harness runs on the host, besides patch collection. Every call goes through `localGit` of
// grade.mjs (`safeEnv` and the `-c` options of slice 2): no system or global configuration, none from the
// environment, no hooks, no attributes, no fsmonitor. Nothing of an agent's tree or `.git` is read here.
import { LEAN_RECIPE, makeLeanFolder } from './variants.mjs';
import { execFile } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { isolatedGit, localGit, safeEnv } from './grade.mjs';

const REV = /^[0-9a-f]{40}$/;
const GIT_SERVER_URL = /^http:\/\/(127\.0\.0\.1|localhost):\d{1,5}\/bench\/[a-z0-9][a-z0-9-]{0,80}\.git$/;

/** The address of a task's repository on the dev git server, as the host reaches it (a published loopback port). */
export function hostRepoUrl(port, slug) {
  const url = `http://127.0.0.1:${port}/bench/${slug}.git`;
  if (!GIT_SERVER_URL.test(url)) throw new Error('the repository address is not plain');
  return url;
}

/** The address of a task's repository as the coder, inside the stack, reaches it. */
export function stackRepoUrl(slug) {
  if (!/^[a-z0-9][a-z0-9-]{0,80}$/.test(slug)) throw new Error('the repository slug is not plain');
  return `http://git-server:8080/bench/${slug}.git`;
}

const GIT_URL_RE = /^http:\/\/(127\.0\.0\.1|localhost):\d{1,5}\/bench\/[a-z0-9][a-z0-9-]{0,80}\.git$/;
const PUSH_URL_RE = /^http:\/\/git-server:8080\/bench\/[a-z0-9][a-z0-9-]{0,80}\.git$/;
/** Where the pusher makes its scratch repository: the slot's work volume, mounted by compose.yaml (`pusher-N`). */
export const PUSH_SCRATCH = '/scratch';
const PLAIN_PATH = /^\/[A-Za-z0-9._/@+=-]*$/;

/**
 * The lines that let a git in a container read a repository another user wrote (the base is written by the task image's
 * default user, often root, and read as uid 10001): `safe.directory` is honoured from a global file, which is what these
 * lines make, and not from `-c` or `GIT_CONFIG_COUNT`, which the git of a task image (2.39) ignores for the `upload-pack` a local
 * fetch or clone starts: it answers "fatal: detected dubious ownership" and the caller sees only "Could not read from remote
 * repository". Only this one setting is in the file, and the container has no secret.
 */
export const SAFE_DIRECTORY_LINES = [
  "printf '[safe]\\n\\tdirectory = *\\n' > /tmp/bench-gitconfig",
  'export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/tmp/bench-gitconfig GIT_TERMINAL_PROMPT=0 HOME=/tmp',
];

/**
 * The shell a one-shot container of the stack runs to put the base on the git server. The base repository was written
 * by a container, so the host never runs git in it: this runs in a container with no secret, on the git server's
 * network only, and takes the objects into a new repository of its own before it pushes.
 */
export function pushScript({ base, url }) {
  if (!PLAIN_PATH.test(base) || !PUSH_URL_RE.test(url)) throw new Error('the base or the address is not plain');
  return [
    'set -eu',
    ...SAFE_DIRECTORY_LINES,
    // The scratch repository is as large as the base: it goes on the disk-backed scratch volume, never in /tmp (a small tmpfs).
    `P=$(mktemp -d ${PUSH_SCRATCH}/push.XXXXXX)`,
    "trap 'rm -rf \"$P\"' EXIT",
    'git init -q "$P"',
    // The base may have been made by another uid (a task image whose default user is root): the repository is the
    // harness's own, and this git runs in a container with no secret, so its ownership is not a question here.
    `git -C "$P" fetch -q ${base} refs/heads/main`,
    `git -C "$P" push -q --force ${url} FETCH_HEAD:refs/heads/main`,
  ].join('\n');
}

/** The heads on `url` (the loopback address of a bench repository) that match `pattern`: [{ sha, ref }]. */
export async function lsRemoteHeads({ url, pattern = 'refs/heads/agent/*' }) {
  if (!GIT_URL_RE.test(url) || url.includes('..')) throw new Error('the repository address is not a bench repository on the loopback');
  const out = (await isolatedGit(['ls-remote', '--heads', '--', url, pattern])).toString('utf8');
  const heads = [];
  for (const line of out.split('\n')) {
    const m = /^([0-9a-f]{40})\trefs\/heads\/(\S+)$/.exec(line);
    if (m) heads.push({ sha: m[1], ref: m[2] });
  }
  return heads;
}

/** The preparations of one destination, in this process: a waiter is the promise of the preparation before it. */
const preparations = new Map();

/**
 * The agent folder of the coder image's revision, from this repository, without `mcp.json`: no GitHub MCP
 * server is connected, as OpenCode has none either (and the mock's answers would be sandbox data).
 * Returns the directory; it is made once for a revision and kept.
 *
 * Two slots of a run prepare the same folder at the same time (one home, one coder revision). They are
 * serialised per destination in this process, and each preparation is built in a temporary sibling and renamed
 * into place: the lock makes the second caller wait and find the first one's marker (renaming its own copy over a
 * folder the first slot's coder has mounted would delete it), and the rename means a failed or killed preparation
 * never leaves a half-written folder at the destination. `unpack` is the extraction, so a test can fail it halfway.
 */
export async function ensureAgentFolder({ repoDir, revision, dest, steps, folder = 'bin/adam-coder/agent', unpack = unpackAgentFolder, variant = 'full', reshape = makeLeanFolder }) {
  if (!['full', 'lean'].includes(variant)) throw new Error(`no agent folder variant ${variant}`);
  const mark = variant === 'full' ? `${revision}:${steps}` : `${revision}:${steps}:${variant}-${LEAN_RECIPE}`;
  if (!Number.isSafeInteger(steps) || steps < 1) throw new Error('the turn limit is required: both agents are given the same one');
  if (!REV.test(revision)) throw new Error('the revision must be a full commit id');
  // One spelling of the destination for the marker, the lock, the temporary sibling and the rename: `dest/` and
  // `dest` are one folder, and `${dest}/` + `.tmp` would put the temporary inside the folder it must replace.
  const target = path.resolve(dest);
  if (folderIsMade(target, mark)) return dest;
  return serialized(target, async () => {
    if (folderIsMade(target, mark)) return dest;
    const git = localGit(repoDir);
    try {
      await git(['cat-file', '-e', `${revision}^{commit}`]);
    } catch {
      throw new Error(`the coder image's revision ${revision.slice(0, 7)} is not in this checkout; fetch it (git fetch origin ${revision})`);
    }
    const tarball = await git(['archive', '--format=tar', revision, '--', folder]);
    const strip = folder.split('/').length;
    const temp = `${target}.tmp-${process.pid}-${crypto.randomBytes(6).toString('hex')}`;
    try {
      fs.mkdirSync(temp, { recursive: true });
      await unpack(tarball, temp, strip);
      fs.rmSync(path.join(temp, 'mcp.json'), { force: true });
      setTurnLimit(path.join(temp, 'instructions.md'), steps);
      if (variant === 'lean') reshape(temp);
      await new Promise((resolve) => execFile('chmod', ['-R', 'a+rX', temp], resolve));
      fs.writeFileSync(path.join(temp, '.revision'), `${mark}\n`);
      fs.rmSync(target, { recursive: true, force: true });
      fs.renameSync(temp, target);
      return dest;
    } catch (e) {
      fs.rmSync(temp, { recursive: true, force: true });
      throw e;
    }
  });
}

/** The tar of the agent folder into `dir`, without the archive's leading components. */
async function unpackAgentFolder(tarball, dir, strip) {
  await new Promise((resolve, reject) => {
    const child = execFile('tar', ['-xf', '-', '-C', dir, `--strip-components=${strip}`, '--no-same-owner', '--no-same-permissions'], { env: safeEnv() }, (e) => (e ? reject(new Error('the agent folder could not be unpacked')) : resolve()));
    // A tar that exits early closes the pipe: that is the callback's error, not an unhandled one on the socket.
    child.stdin.on('error', () => {});
    child.stdin.end(tarball);
  });
}

/** Is the folder at `dest` the one this revision and step limit make? */
function folderIsMade(dest, mark) {
  try {
    return fs.readFileSync(path.join(dest, '.revision'), 'utf8').trim() === mark;
  } catch {
    return false;
  }
}

/** Run `fn` after the preparation of the same destination that came before it. A failed one does not block the next. */
function serialized(key, fn) {
  const run = (preparations.get(key) ?? Promise.resolve()).then(fn);
  const tail = run.catch(() => {});
  preparations.set(key, tail);
  tail.then(() => {
    if (preparations.get(key) === tail) preparations.delete(key);
  });
  return run;
}

/** The repository a run has on the git server: the task's slug, the run and the attempt, so no run sees another's branches. */
export function runRepoName(slug, run, attempt) {
  const tail = crypto.createHash('sha256').update(`${run}#${attempt}`).digest('hex').slice(0, 8);
  return `${slug}-${tail}`.slice(0, 80);
}

/** The turn limit of adam's agent folder is the configured step limit, the same value OpenCode's `steps` has. */
export function setTurnLimit(file, steps) {
  const text = fs.readFileSync(file, 'utf8');
  if (!/^(\s+)max_turns:\s*\d+/m.test(text)) throw new Error('the agent folder has no max_turns to set');
  fs.writeFileSync(file, text.replace(/^(\s+)max_turns:\s*\d+/m, `$1max_turns: ${steps}`));
}

/** A name for a task's repository on the git server: short, plain and unique to the task id. */
export function repoSlug(taskId) {
  const plain = String(taskId).toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 40).replace(/-+$/, '') || 't';
  return `${plain}-${crypto.createHash('sha256').update(String(taskId)).digest('hex').slice(0, 6)}`;
}
