import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { ensureAgentFolder, hostRepoUrl, lsRemoteHeads, pushBase, repoSlug, stackRepoUrl } from '../lib/gitops.mjs';
import { commitFiles, git, initRepo } from './gitfix.mjs';
import { tmp } from './helpers.mjs';

function withEnv(t, vars) {
  const before = {};
  for (const [k, v] of Object.entries(vars)) {
    before[k] = process.env[k];
    process.env[k] = v;
  }
  t.after(() => {
    for (const [k, v] of Object.entries(before)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  });
}

/** An executable that records that it ran. */
function trap(bin, marks, name) {
  const f = path.join(bin, name);
  fs.writeFileSync(f, `#!/bin/sh\ntouch ${path.join(marks, name)}\ncat >/dev/null\nexit 0\n`, { mode: 0o755 });
  return f;
}

const bare = (t) => {
  const dir = path.join(tmp(t), 'remote.git');
  execFileSync('git', ['init', '-q', '--bare', dir]);
  return dir;
};

test('repository names and addresses are plain, and unique to the task', () => {
  const a = repoSlug('instance_NodeBB__NodeBB-8168c6c0a2f0e1');
  const b = repoSlug('instance_NodeBB__NodeBB-8168c6c0a2f0e2');
  assert.match(a, /^[a-z0-9][a-z0-9-]{0,63}$/);
  assert.notEqual(a, b);
  assert.equal(repoSlug('hello'), repoSlug('hello'));
  assert.match(repoSlug('__weird..id__'), /^[a-z0-9][a-z0-9-]*$/);
  assert.equal(hostRepoUrl(8083, 'x-1a2b3c'), 'http://127.0.0.1:8083/bench/x-1a2b3c.git');
  assert.equal(stackRepoUrl('x-1a2b3c'), 'http://git-server:8080/bench/x-1a2b3c.git');
  assert.throws(() => stackRepoUrl('../x'), /plain/);
  assert.throws(() => hostRepoUrl(8083, 'a/b'), /plain/);
});

test('git call site: pushing the base runs no hook, however the environment or the repository is set up', async (t) => {
  const src = initRepo(t);
  commitFiles(src, { 'a.txt': 'one\n' }, 'base');
  const marks = tmp(t);
  const bin = tmp(t);
  trap(bin, marks, 'pre-push');
  trap(path.join(src, '.git', 'hooks'), marks, 'pre-push');
  withEnv(t, { GIT_CONFIG_COUNT: '1', GIT_CONFIG_KEY_0: 'core.hooksPath', GIT_CONFIG_VALUE_0: bin });
  const remote = bare(t);
  await pushBase({ dir: src, url: remote });
  assert.deepEqual(fs.readdirSync(marks), [], 'no hook ran');
  assert.equal(git(remote, 'rev-parse', 'refs/heads/main').trim(), git(src, 'rev-parse', 'HEAD').trim());
});

test('pushing the base replaces what the server has, and takes only a plain branch name', async (t) => {
  const src = initRepo(t);
  commitFiles(src, { 'a.txt': 'one\n' }, 'base');
  const remote = bare(t);
  await pushBase({ dir: src, url: remote });
  commitFiles(src, { 'a.txt': 'two\n' }, 'amend');
  git(src, 'commit', '-q', '--amend', '-m', 'rewritten');
  await pushBase({ dir: src, url: remote });
  assert.equal(git(remote, 'rev-parse', 'refs/heads/main').trim(), git(src, 'rev-parse', 'HEAD').trim());
  await assert.rejects(pushBase({ dir: src, url: remote, branch: '--delete' }), /plain/);
});

test('git call site: listing the pushed branches parses heads and runs no transport command', async (t) => {
  const src = initRepo(t);
  commitFiles(src, { 'a.txt': 'one\n' }, 'base');
  git(src, 'branch', 'agent/abc123');
  git(src, 'branch', 'other');
  const remote = bare(t);
  execFileSync('git', ['-C', src, 'push', '-q', remote, '--all']);
  const heads = await lsRemoteHeads({ dir: src, url: remote });
  assert.deepEqual(heads.map((h) => h.ref), ['agent/abc123']);
  assert.match(heads[0].sha, /^[0-9a-f]{40}$/);

  const marks = tmp(t);
  withEnv(t, { GIT_ALLOW_PROTOCOL: 'ext:file:http', GIT_CONFIG_COUNT: '1', GIT_CONFIG_KEY_0: 'protocol.ext.allow', GIT_CONFIG_VALUE_0: 'always' });
  await assert.rejects(lsRemoteHeads({ dir: src, url: `ext::sh -c "touch ${marks}/ext" >&2` }));
  assert.deepEqual(fs.readdirSync(marks), [], 'the ext transport did not run');
});

function repoWithAgentFolder(t) {
  const dir = initRepo(t);
  const first = commitFiles(
    dir,
    {
      'bin/adam-coder/agent/instructions.md': '# coder\n',
      'bin/adam-coder/agent/mcp.json': '{"mcpServers":{"github":{"type":"http","url":"http://127.0.0.1:8082/mcp"}}}\n',
      'bin/adam-coder/agent/skills/x/SKILL.md': 'skill\n',
      'README.md': 'not the folder\n',
    },
    'feat: the folder',
  );
  commitFiles(dir, { 'bin/adam-coder/agent/instructions.md': '# coder, later\n' }, 'feat: later');
  return { dir, first };
}

test('the agent folder is taken from the image\'s revision, not the checkout, and has no mcp.json', async (t) => {
  const { dir, first } = repoWithAgentFolder(t);
  const dest = path.join(tmp(t), 'folder');
  const got = await ensureAgentFolder({ repoDir: dir, revision: first, dest });
  assert.equal(got, dest);
  assert.equal(fs.readFileSync(path.join(dest, 'instructions.md'), 'utf8'), '# coder\n', 'the revision\'s file, not the later one');
  assert.equal(fs.readFileSync(path.join(dest, 'skills', 'x', 'SKILL.md'), 'utf8'), 'skill\n');
  assert.equal(fs.existsSync(path.join(dest, 'mcp.json')), false, 'no GitHub MCP server is connected');
  assert.equal(fs.existsSync(path.join(dest, 'README.md')), false);
  assert.ok((fs.statSync(path.join(dest, 'instructions.md')).mode & 0o444) === 0o444, 'readable by the coder\'s user');
});

test('a folder made for the revision is reused; an absent revision says how to get it', async (t) => {
  const { dir, first } = repoWithAgentFolder(t);
  const dest = path.join(tmp(t), 'folder');
  await ensureAgentFolder({ repoDir: dir, revision: first, dest });
  fs.writeFileSync(path.join(dest, 'instructions.md'), 'edited by hand\n');
  await ensureAgentFolder({ repoDir: dir, revision: first, dest });
  assert.equal(fs.readFileSync(path.join(dest, 'instructions.md'), 'utf8'), 'edited by hand\n', 'a made folder is kept as it is');
  await assert.rejects(ensureAgentFolder({ repoDir: dir, revision: 'f'.repeat(40), dest: path.join(tmp(t), 'x') }), /git fetch origin/);
  await assert.rejects(ensureAgentFolder({ repoDir: dir, revision: 'HEAD', dest: path.join(tmp(t), 'y') }), /full commit id/);
});

test('git call site: the folder is read with the harness\'s own git environment, not the process\'s (GIT_DIR, GIT_CONFIG_*)', async (t) => {
  const { dir, first } = repoWithAgentFolder(t);
  const decoy = initRepo(t);
  commitFiles(decoy, { 'x.txt': 'x\n' }, 'decoy');
  const marks = tmp(t);
  const bin = tmp(t);
  fs.appendFileSync(path.join(dir, '.git', 'config'), `[core]\n\tfsmonitor = ${trap(bin, marks, 'FSMONITOR')}\n`);
  withEnv(t, {
    GIT_DIR: path.join(decoy, '.git'),
    GIT_WORK_TREE: decoy,
    GIT_CONFIG_COUNT: '1',
    GIT_CONFIG_KEY_0: 'core.hooksPath',
    GIT_CONFIG_VALUE_0: bin,
  });
  const dest = path.join(tmp(t), 'folder');
  await ensureAgentFolder({ repoDir: dir, revision: first, dest });
  assert.equal(fs.readFileSync(path.join(dest, 'instructions.md'), 'utf8'), '# coder\n');
  assert.deepEqual(fs.readdirSync(marks), []);
});
