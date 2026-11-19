import assert from 'node:assert/strict';
import { execFile, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { ensureAgentFolder, hostRepoUrl, repoSlug, runRepoName, stackRepoUrl } from '../lib/gitops.mjs';
import { safeEnv } from '../lib/grade.mjs';
import { commitFiles, git, initRepo } from './gitfix.mjs';
import { tmp } from './helpers.mjs';

/** The real extraction, as the module does it: a fake `unpack` writes a half tree and then calls this. */
function realUnpack(tarball, dir, strip) {
  return new Promise((resolve, reject) => {
    const child = execFile('tar', ['-xf', '-', '-C', dir, `--strip-components=${strip}`, '--no-same-owner', '--no-same-permissions'], { env: safeEnv() }, (e) => (e ? reject(new Error('the agent folder could not be unpacked')) : resolve()));
    child.stdin.on('error', () => {});
    child.stdin.end(tarball);
  });
}

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

test('every run has a repository of its own on the git server, named by the task and the run', () => {
  const a = runRepoName('hello-1-aaaaaa', 'hello-adam-max', 1);
  const b = runRepoName('hello-1-aaaaaa', 'hello-adam-max', 2);
  const c = runRepoName('hello-1-aaaaaa', 'hello-adam-off', 1);
  assert.equal(new Set([a, b, c]).size, 3, 'two arms and two attempts of one task do not share a repository');
  for (const n of [a, b, c]) assert.match(n, /^[a-z0-9][a-z0-9-]{0,80}$/);
  assert.equal(runRepoName('hello-1-aaaaaa', 'hello-adam-max', 1), a);
  assert.match(hostRepoUrl(8083, a), /^http:\/\/127\.0\.0\.1:8083\/bench\/[a-z0-9-]+\.git$/);
});

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

function repoWithAgentFolder(t) {
  const dir = initRepo(t);
  const first = commitFiles(
    dir,
    {
      'bin/adam-coder/agent/instructions.md': '---\nlimits:\n  max_turns: 200\n  max_tool_calls: 400\n---\n# coder\n',
      'bin/adam-coder/agent/mcp.json': '{"mcpServers":{"github":{"type":"http","url":"http://127.0.0.1:8082/mcp"}}}\n',
      'bin/adam-coder/agent/skills/x/SKILL.md': 'skill\n',
      'README.md': 'not the folder\n',
    },
    'feat: the folder',
  );
  commitFiles(dir, { 'bin/adam-coder/agent/instructions.md': '---\nlimits:\n  max_turns: 200\n---\n# coder, later\n' }, 'feat: later');
  return { dir, first };
}

test('the agent folder is taken from the image\'s revision, not the checkout, and has no mcp.json', async (t) => {
  const { dir, first } = repoWithAgentFolder(t);
  const dest = path.join(tmp(t), 'folder');
  const got = await ensureAgentFolder({ repoDir: dir, revision: first, dest, steps: 150 });
  assert.equal(got, dest);
  assert.match(fs.readFileSync(path.join(dest, 'instructions.md'), 'utf8'), /# coder\n$/, 'the revision\'s file, not the later one');
  assert.match(fs.readFileSync(path.join(dest, 'instructions.md'), 'utf8'), /  max_turns: 150\n/, 'the configured step limit, the one OpenCode gets');
  assert.match(fs.readFileSync(path.join(dest, 'instructions.md'), 'utf8'), /  max_tool_calls: 400\n/);
  assert.equal(fs.readFileSync(path.join(dest, 'skills', 'x', 'SKILL.md'), 'utf8'), 'skill\n');
  assert.equal(fs.existsSync(path.join(dest, 'mcp.json')), false, 'no GitHub MCP server is connected');
  assert.equal(fs.existsSync(path.join(dest, 'README.md')), false);
  assert.ok((fs.statSync(path.join(dest, 'instructions.md')).mode & 0o444) === 0o444, 'readable by the coder\'s user');
  assert.deepEqual(fs.readdirSync(path.dirname(dest)), ['folder'], 'the folder is renamed into place: no temporary sibling is left');
});

test('two slots preparing one agent folder at the same time make it once, and both see the complete tree', async (t) => {
  const { dir, first } = repoWithAgentFolder(t);
  const home = tmp(t);
  const dest = path.join(home, 'folder');
  let made = 0;
  // A half-written tree, a pause, then the real extraction. A second preparation that emptied the destination while
  // this one was writing would run this a second time, which the count catches.
  const unpack = async (tarball, target, strip) => {
    made++;
    fs.writeFileSync(path.join(target, 'partial'), 'x');
    await new Promise((resolve) => setTimeout(resolve, 50));
    fs.rmSync(path.join(target, 'partial'), { force: true });
    await realUnpack(tarball, target, strip);
  };
  const [a, b] = await Promise.all([
    ensureAgentFolder({ repoDir: dir, revision: first, dest, steps: 150, unpack }),
    ensureAgentFolder({ repoDir: dir, revision: first, dest, steps: 150, unpack }),
  ]);
  assert.equal(a, dest);
  assert.equal(b, dest);
  assert.equal(made, 1, 'the folder is made once: the second caller waits and finds the marker');
  assert.match(fs.readFileSync(path.join(dest, 'instructions.md'), 'utf8'), /^---\nlimits:\n  max_turns: 150\n  max_tool_calls: 400\n---\n# coder\n$/);
  assert.equal(fs.readFileSync(path.join(dest, 'skills', 'x', 'SKILL.md'), 'utf8'), 'skill\n');
  assert.equal(fs.existsSync(path.join(dest, 'mcp.json')), false);
  assert.deepEqual(fs.readdirSync(home), ['folder'], 'no temporary directory is left beside the folder');
});

test('the destination appears complete: while the folder is being made it does not exist', async (t) => {
  const { dir, first } = repoWithAgentFolder(t);
  const home = tmp(t);
  const dest = path.join(home, 'folder');
  let entered;
  const reached = new Promise((resolve) => (entered = resolve));
  let release;
  const gate = new Promise((resolve) => (release = resolve));
  let seen;
  const unpack = async (tarball, target, strip) => {
    await realUnpack(tarball, target, strip);
    seen = fs.existsSync(dest);
    entered();
    await gate;
  };
  const pending = ensureAgentFolder({ repoDir: dir, revision: first, dest, steps: 150, unpack });
  const which = await Promise.race([reached.then(() => 'unpacking'), pending.then(() => 'done'), new Promise((resolve) => setTimeout(() => resolve('timeout'), 5000))]);
  try {
    assert.equal(which, 'unpacking', 'the preparation uses the unpack it was given');
    assert.equal(seen, false, 'nothing is published before the tree is complete');
  } finally {
    release();
  }
  await pending;
  assert.match(fs.readFileSync(path.join(dest, 'instructions.md'), 'utf8'), /max_turns: 150/);
  assert.deepEqual(fs.readdirSync(home), ['folder']);
});

test('a preparation that fails halfway leaves no folder and no temporary directory', async (t) => {
  const { dir, first } = repoWithAgentFolder(t);
  const home = tmp(t);
  const dest = path.join(home, 'folder');
  const unpack = async (tarball, target) => {
    fs.writeFileSync(path.join(target, 'instructions.md'), '---\nlimits:\n  max_turns: 200\n---\n# coder\n');
    throw new Error('tar failed');
  };
  await assert.rejects(ensureAgentFolder({ repoDir: dir, revision: first, dest, steps: 150, unpack }), /tar failed/);
  assert.equal(fs.existsSync(dest), false, 'no partial folder is left');
  assert.deepEqual(fs.readdirSync(home), [], 'no temporary directory is left');
});

test('a preparation that fails leaves a folder that was already complete as it was', async (t) => {
  const { dir, first } = repoWithAgentFolder(t);
  const home = tmp(t);
  const dest = path.join(home, 'folder');
  await ensureAgentFolder({ repoDir: dir, revision: first, dest, steps: 150 });
  const before = fs.readFileSync(path.join(dest, 'instructions.md'), 'utf8');
  const unpack = async () => {
    throw new Error('tar failed');
  };
  await assert.rejects(ensureAgentFolder({ repoDir: dir, revision: first, dest, steps: 90, unpack }), /tar failed/);
  assert.equal(fs.readFileSync(path.join(dest, 'instructions.md'), 'utf8'), before, 'the complete folder is untouched');
  assert.deepEqual(fs.readdirSync(home), ['folder']);
});

test('a folder is made again for another step limit, and a missing limit is refused (finding 19)', async (t) => {
  const { dir, first } = repoWithAgentFolder(t);
  const dest = path.join(tmp(t), 'folder');
  await ensureAgentFolder({ repoDir: dir, revision: first, dest, steps: 150 });
  await ensureAgentFolder({ repoDir: dir, revision: first, dest, steps: 90 });
  assert.match(fs.readFileSync(path.join(dest, 'instructions.md'), 'utf8'), /  max_turns: 90\n/);
  await assert.rejects(ensureAgentFolder({ repoDir: dir, revision: first, dest: path.join(tmp(t), 'z') }), /turn limit is required/);
  await assert.rejects(ensureAgentFolder({ repoDir: dir, revision: first, dest: path.join(tmp(t), 'z'), steps: 0 }), /turn limit is required/);
});

test('a folder made for the revision is reused; an absent revision says how to get it', async (t) => {
  const { dir, first } = repoWithAgentFolder(t);
  const dest = path.join(tmp(t), 'folder');
  await ensureAgentFolder({ repoDir: dir, revision: first, dest, steps: 150 });
  fs.writeFileSync(path.join(dest, 'instructions.md'), 'edited by hand\n');
  await ensureAgentFolder({ repoDir: dir, revision: first, dest, steps: 150 });
  assert.equal(fs.readFileSync(path.join(dest, 'instructions.md'), 'utf8'), 'edited by hand\n', 'a made folder is kept as it is');
  await assert.rejects(ensureAgentFolder({ repoDir: dir, revision: 'f'.repeat(40), dest: path.join(tmp(t), 'x'), steps: 150 }), /git fetch origin/);
  await assert.rejects(ensureAgentFolder({ repoDir: dir, revision: 'HEAD', dest: path.join(tmp(t), 'y'), steps: 150 }), /full commit id/);
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
  await ensureAgentFolder({ repoDir: dir, revision: first, dest, steps: 150 });
  assert.match(fs.readFileSync(path.join(dest, 'instructions.md'), 'utf8'), /# coder\n$/);
  assert.deepEqual(fs.readdirSync(marks), []);
});
