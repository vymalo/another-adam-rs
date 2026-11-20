// Finding 1 of the review of slice 3: the host never runs git in, or reads a file from, a place a container could
// write, and no host git sees the model's key.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { test } from 'node:test';
import { FakeContainers } from '../lib/containers.mjs';
import { gradeOnce, isolatedGit, localGit, safeEnv } from '../lib/grade.mjs';
import { loadTask } from '../lib/task.mjs';
import { makeTask } from './taskfix.mjs';
import { PUSH_SCRATCH, SAFE_DIRECTORY_LINES, lsRemoteHeads, pushScript } from '../lib/gitops.mjs';
import { commitFiles, git, initRepo } from './gitfix.mjs';
import { tmp } from './helpers.mjs';

const SHA = 'a'.repeat(40);

/** A git server's advertisement for ls-remote: the smart protocol, v0, one head. */
function fakeGitServer(t, { heads }) {
  const seen = [];
  const pkt = (s) => `${(s.length + 4).toString(16).padStart(4, '0')}${s}`;
  const server = http.createServer((req, res) => {
    seen.push(req.url);
    if (!req.url.includes('/info/refs')) {
      res.writeHead(404);
      return res.end();
    }
    res.writeHead(200, { 'content-type': 'application/x-git-upload-pack-advertisement' });
    let body = `${pkt('# service=git-upload-pack\n')}0000`;
    heads.forEach((h, i) => (body += pkt(`${SHA} ${h}${i === 0 ? '\0multi_ack side-band-64k' : ''}\n`)));
    res.end(`${body}0000`);
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => {
    t.after(() => server.close());
    resolve({ port: server.address().port, seen });
  }));
}

const withEnv = (t, vars) => {
  const before = {};
  for (const [k, v] of Object.entries(vars)) {
    before[k] = process.env[k];
    process.env[k] = v;
  }
  t.after(() => {
    for (const [k, v] of Object.entries(before)) v === undefined ? delete process.env[k] : (process.env[k] = v);
  });
};

test('the model\'s key, and everything else but a short allow-list, is not in the environment of a host git', async (t) => {
  withEnv(t, { BENCH_MODEL_API_KEY: 'the-real-model-key-0123456789', ANTHROPIC_API_KEY: 'another-secret-value-12345', HTTPS_PROXY: 'http://proxy.example.test:3128' });
  const env = safeEnv();
  assert.deepEqual(Object.keys(env).filter((k) => /KEY|TOKEN|SECRET|PROXY/i.test(k)), []);
  assert.ok(env.PATH);
  const dir = initRepo(t);
  commitFiles(dir, { a: 'a' }, 'base');
  // a git alias that prints the environment git runs its children with
  const printed = (await localGit(dir)(['-c', 'alias.e=!env', 'e'])).toString();
  assert.doesNotMatch(printed, /the-real-model-key|another-secret-value|proxy\.example/);
  assert.match(printed, /^PATH=/m);
});

test('listing the pushed branches runs in an empty directory that is not a repository, whatever the current directory holds', async (t) => {
  const { port, seen } = await fakeGitServer(t, { heads: ['refs/heads/agent/abc123', 'refs/heads/main'] });
  // The current directory is a repository a container wrote: it names a proxy, a hook and an ssh command.
  const planted = initRepo(t);
  commitFiles(planted, { a: 'a' }, 'base');
  const marks = tmp(t);
  const trap = path.join(marks, 'trap.sh');
  fs.writeFileSync(trap, `#!/bin/sh\ntouch ${marks}/ran\nexit 1\n`, { mode: 0o755 });
  fs.appendFileSync(path.join(planted, '.git', 'config'), `[core]\n\tsshCommand = ${trap}\n\tfsmonitor = ${trap}\n[http]\n\tproxy = http://127.0.0.1:1\n[url "ssh://x@example.test/"]\n\tinsteadOf = http://127.0.0.1:${port}/\n`);
  const before = process.cwd();
  process.chdir(planted);
  t.after(() => process.chdir(before));
  withEnv(t, { GIT_CONFIG_COUNT: '1', GIT_CONFIG_KEY_0: 'core.sshCommand', GIT_CONFIG_VALUE_0: trap });
  const heads = await lsRemoteHeads({ url: `http://127.0.0.1:${port}/bench/x-1.git` });
  assert.deepEqual(heads.map((h) => h.ref), ['agent/abc123']);
  assert.deepEqual(fs.readdirSync(marks), ['trap.sh'], 'nothing ran');
  assert.ok(seen.some((u) => u.startsWith('/bench/x-1.git/info/refs')), 'the server was reached directly, not through the planted proxy or rewrite');
});

test('isolatedGit runs where there is no repository and the protocols are the plain http ones', async (t) => {
  const planted = initRepo(t);
  const before = process.cwd();
  process.chdir(planted);
  t.after(() => process.chdir(before));
  const out = (await isolatedGit(['rev-parse', '--git-dir']).catch((e) => e)).message ?? '';
  assert.match(out, /not a git repository/);
  const real = initRepo(t);
  commitFiles(real, { a: 'a' }, 'base');
  await assert.rejects(isolatedGit(['ls-remote', '--', `file://${real}`]), /transport 'file' not allowed/, 'a repository that exists is still not read over file');
  await assert.rejects(isolatedGit(['ls-remote', '--', 'ext::sh -c true']), /./);
});

test('isolatedGit\'s directory is under a ceiling: a temporary directory inside a planted repository does not read its configuration', async (t) => {
  const { port, seen } = await fakeGitServer(t, { heads: ['refs/heads/agent/abc123'] });
  const planted = initRepo(t);
  commitFiles(planted, { a: 'a' }, 'base');
  fs.appendFileSync(path.join(planted, '.git', 'config'), `[http]\n\tproxy = http://127.0.0.1:1\n[url "ssh://x@example.test/"]\n\tinsteadOf = http://127.0.0.1:${port}/\n`);
  withEnv(t, { TMPDIR: planted });
  const out = (await isolatedGit(['rev-parse', '--git-dir']).catch((e) => e)).message ?? '';
  assert.match(out, /not a git repository/, 'the repository above the temporary directory is not found');
  const heads = await lsRemoteHeads({ url: `http://127.0.0.1:${port}/bench/x-1.git` });
  assert.deepEqual(heads.map((h) => h.ref), ['agent/abc123']);
  assert.ok(seen.length > 0, 'reached directly, not through the planted proxy or rewrite');
});

test('a reward the grading container made a link to a file of the host is not a reward (what a container wrote is read without following links)', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  const hostFile = path.join(tmp(t), 'one.txt');
  fs.writeFileSync(hostFile, '1\n');
  const containers = new FakeContainers({
    onRun: (spec) => {
      const logs = spec.mounts.find((m) => m.target === '/logs/verifier').source;
      fs.symlinkSync(hostFile, path.join(logs, 'reward.txt'));
      return { exitCode: 0 };
    },
  });
  const r = await gradeOnce({ containers, task, workRoot: tmp(t) });
  assert.equal(r.pass, false);
  assert.notEqual(r.reward, '1');
  // and a reward that is too long to be one is not read whole
  const big = new FakeContainers({
    onRun: (spec) => {
      fs.writeFileSync(path.join(spec.mounts.find((m) => m.target === '/logs/verifier').source, 'reward.txt'), '1'.repeat(10_000_000));
      return { exitCode: 0 };
    },
  });
  assert.equal((await gradeOnce({ containers: big, task, workRoot: tmp(t) })).pass, false);
});

test('only an http address of the bench repository on the loopback is listed', async () => {
  for (const url of ['http://example.test/bench/x.git', 'https://127.0.0.1:1/bench/x.git', 'file:///etc', 'http://127.0.0.1:8083/other/x.git', '/etc', 'http://127.0.0.1:8083/bench/../x.git']) {
    await assert.rejects(lsRemoteHeads({ url }), /address/, url);
  }
});

test('the base is pushed by a container: the script fetches into a fresh repository and pushes from it, and takes only plain names', () => {
  const script = pushScript({ base: '/work/bench-base/base.git', url: 'http://git-server:8080/bench/x-1-r1.git' });
  assert.match(script, /git init -q "\$P"/);
  assert.match(script, /git -C "\$P" fetch -q \/work\/bench-base\/base\.git refs\/heads\/main/);
  assert.match(script, /git -C "\$P" push -q --force http:\/\/git-server:8080\/bench\/x-1-r1\.git FETCH_HEAD:refs\/heads\/main/);
  assert.match(script, /GIT_CONFIG_GLOBAL=\/tmp\/bench-gitconfig/);
  for (const bad of [{ base: '/work/a b', url: 'http://git-server:8080/bench/x.git' }, { base: '/work/b.git', url: 'http://evil.example.test/bench/x.git' }, { base: '/work/b.git', url: 'http://git-server:8080/bench/x.git; rm' }]) assert.throws(() => pushScript(bad), /plain/);
  void execFileSync;
  void git;
});

test('a git in a container is told safe.directory by a global file, the one place the git of a task image reads it for a local clone or fetch', () => {
  // `-c safe.directory` and GIT_CONFIG_COUNT are ignored by the upload-pack of git 2.39 (the task images'): a base written by
  // root and read as uid 10001 failed with "Could not read from remote repository" (reproduced in a SWE-bench Pro image).
  const out = execFileSync('sh', ['-c', [...SAFE_DIRECTORY_LINES, 'git config --get-all safe.directory; echo "$GIT_CONFIG_GLOBAL"'].join('\n')], { encoding: 'utf8', env: { PATH: process.env.PATH } });
  assert.deepEqual(out.trim().split('\n'), ['*', '/tmp/bench-gitconfig']);
  const push = pushScript({ base: '/work/bench-base/base.git', url: 'http://git-server:8080/bench/x-1-r1.git' });
  assert.ok(push.includes(SAFE_DIRECTORY_LINES[0]), 'the pusher makes the file');
  assert.doesNotMatch(push, /-c safe\.directory|GIT_CONFIG_COUNT/);
});

test('the pusher\'s scratch repository is not in /tmp (a size-limited tmpfs): it is made on the scratch volume and removed on exit', () => {
  const script = pushScript({ base: '/work/bench-base/base.git', url: 'http://git-server:8080/bench/x-1-r1.git' });
  assert.ok(script.includes(`P=$(mktemp -d ${PUSH_SCRATCH}/push.XXXXXX)`));
  assert.ok(script.includes(`trap 'rm -rf "$P"' EXIT`));
  for (const line of script.split('\n')) if (line.includes('/tmp')) assert.match(line, /bench-gitconfig/);
});
