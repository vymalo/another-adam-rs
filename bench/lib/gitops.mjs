// The git the harness runs on the host, besides patch collection. Every call goes through `localGit` of
// grade.mjs (`safeEnv` and the `-c` options of slice 2): no system or global configuration, none from the
// environment, no hooks, no attributes, no fsmonitor. Nothing of an agent's tree or `.git` is read here.
import { execFile } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { localGit, safeEnv } from './grade.mjs';

const REV = /^[0-9a-f]{40}$/;
const GIT_SERVER_URL = /^http:\/\/(127\.0\.0\.1|localhost):\d{1,5}\/bench\/[a-z0-9][a-z0-9-]{0,63}\.git$/;

/** The address of a task's repository on the dev git server, as the host reaches it (a published loopback port). */
export function hostRepoUrl(port, slug) {
  const url = `http://127.0.0.1:${port}/bench/${slug}.git`;
  if (!GIT_SERVER_URL.test(url)) throw new Error('the repository address is not plain');
  return url;
}

/** The address of a task's repository as the coder, inside the stack, reaches it. */
export function stackRepoUrl(slug) {
  if (!/^[a-z0-9][a-z0-9-]{0,63}$/.test(slug)) throw new Error('the repository slug is not plain');
  return `http://git-server:8080/bench/${slug}.git`;
}

/** Push `main` of the repository in `dir` to `url`, replacing what is there. */
export async function pushBase({ dir, url, branch = 'main' }) {
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(branch)) throw new Error('the branch is not plain');
  await localGit(dir)(['push', '-q', '--force', '--', url, `refs/heads/${branch}:refs/heads/${branch}`]);
}

/** The heads on `url` that match `pattern` (a ref glob): [{ sha, ref }]. */
export async function lsRemoteHeads({ dir, url, pattern = 'refs/heads/agent/*' }) {
  const out = (await localGit(dir)(['ls-remote', '--heads', '--', url, pattern])).toString('utf8');
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
export async function ensureAgentFolder({ repoDir, revision, dest, folder = 'bin/adam-coder/agent' }) {
  if (!REV.test(revision)) throw new Error('the revision must be a full commit id');
  const marker = path.join(dest, '.revision');
  try {
    if (fs.readFileSync(marker, 'utf8').trim() === revision) return dest;
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
  await new Promise((resolve) => execFile('chmod', ['-R', 'a+rX', dest], resolve));
  fs.writeFileSync(marker, `${revision}\n`);
  return dest;
}

/** A name for a task's repository on the git server: short, plain and unique to the task id. */
export function repoSlug(taskId) {
  const plain = String(taskId).toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 40).replace(/-+$/, '') || 't';
  return `${plain}-${crypto.createHash('sha256').update(String(taskId)).digest('hex').slice(0, 6)}`;
}
