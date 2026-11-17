// The git the harness runs on the host, besides patch collection. Every call goes through `localGit` of
// grade.mjs (`safeEnv` and the `-c` options of slice 2): no system or global configuration, none from the
// environment, no hooks, no attributes, no fsmonitor. Nothing of an agent's tree or `.git` is read here.
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
const PLAIN_PATH = /^\/[A-Za-z0-9._/@+=-]*$/;

/**
 * The shell a one-shot container of the stack runs to put the base on the git server. The base repository was written
 * by a container, so the host never runs git in it: this runs in a container with no secret, on the git server's
 * network only, and takes the objects into a new repository of its own before it pushes.
 */
export function pushScript({ base, url }) {
  if (!PLAIN_PATH.test(base) || !PUSH_URL_RE.test(url)) throw new Error('the base or the address is not plain');
  return [
    'set -eu',
    'export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 HOME=/tmp',
    'P=$(mktemp -d)',
    'git init -q "$P"',
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

/**
 * The agent folder of the coder image's revision, from this repository, without `mcp.json`: no GitHub MCP
 * server is connected, as OpenCode has none either (and the mock's answers would be sandbox data).
 * Returns the directory; it is made once for a revision and kept.
 */
export async function ensureAgentFolder({ repoDir, revision, dest, steps, folder = 'bin/adam-coder/agent' }) {
  if (!Number.isSafeInteger(steps) || steps < 1) throw new Error('the turn limit is required: both agents are given the same one');
  if (!REV.test(revision)) throw new Error('the revision must be a full commit id');
  const marker = path.join(dest, '.revision');
  try {
    if (fs.readFileSync(marker, 'utf8').trim() === `${revision}:${steps}`) return dest;
  } catch {
    // not made yet
  }
  const git = localGit(repoDir);
  try {
    await git(['cat-file', '-e', `${revision}^{commit}`]);
  } catch {
    throw new Error(`the coder image's revision ${revision.slice(0, 7)} is not in this checkout; fetch it (git fetch origin ${revision})`);
  }
  const tarball = await git(['archive', '--format=tar', revision, '--', folder]);
  const strip = folder.split('/').length;
  fs.rmSync(dest, { recursive: true, force: true });
  fs.mkdirSync(dest, { recursive: true });
  await new Promise((resolve, reject) => {
    const child = execFile('tar', ['-xf', '-', '-C', dest, `--strip-components=${strip}`, '--no-same-owner', '--no-same-permissions'], { env: safeEnv() }, (e) => (e ? reject(new Error('the agent folder could not be unpacked')) : resolve()));
    child.stdin.end(tarball);
  });
  fs.rmSync(path.join(dest, 'mcp.json'), { force: true });
  setTurnLimit(path.join(dest, 'instructions.md'), steps);
  await new Promise((resolve) => execFile('chmod', ['-R', 'a+rX', dest], resolve));
  fs.writeFileSync(marker, `${revision}:${steps}\n`);
  return dest;
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
