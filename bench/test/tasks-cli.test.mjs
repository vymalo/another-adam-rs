import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { ConfigError } from '../lib/config.mjs';
import { FakeContainers } from '../lib/containers.mjs';
import { runTasks } from '../lib/tasks-cli.mjs';
import { UpstreamError, fetchUpstream, parseSums, taskChecksum, verifySums } from '../lib/upstream.mjs';
import { commitFiles, git, initRepo, lines, SHA } from './gitfix.mjs';
import { rawConfig, tmp } from './helpers.mjs';
import { IMAGE, makeTask } from './taskfix.mjs';

const sha = (text) => crypto.createHash('sha256').update(text).digest('hex');

/** An upstream repository with tasks under v2/, a SHA256SUMS, and a bare copy of it standing in for the real one. */
function upstream(t, ids = ['instance_Org__Alpha-aaaaaaa1', 'instance_Org__Alpha-aaaaaaa2', 'instance_Org__Beta-bbbbbbb1']) {
  const work = initRepo(t);
  const root = path.join(work, 'v2');
  for (const id of ids) makeTask(root, id, { 'environment/Dockerfile': `FROM ${IMAGE}:${id.slice(-8)}\n` });
  const sums = [];
  const walk = (d) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true }).sort((a, b) => (a.name < b.name ? -1 : 1))) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) walk(p);
      else sums.push(`${sha(fs.readFileSync(p))}  ${path.relative(root, p)}`);
    }
  };
  walk(root);
  fs.writeFileSync(path.join(root, 'SHA256SUMS'), `${sums.join('\n')}\n`);
  const commit = commitFiles(work, {}, 'tasks');
  const bare = path.join(tmp(t), 'upstream.git');
  execFileSync('git', ['clone', '-q', '--bare', work, bare]);
  return { work, bare, commit, ids };
}

function setup(t, { tasks = {}, cfg = {} } = {}) {
  const dir = tmp(t);
  const configFile = path.join(dir, 'config.json');
  fs.writeFileSync(configFile, JSON.stringify(rawConfig({ tasks, ...cfg })));
  const env = { BENCH_CONFIG: configFile, BENCH_HOME: path.join(dir, 'home'), BENCH_TASKS_DIR: path.join(dir, 'repo-tasks') };
  const outLines = [];
  const errLines = [];
  const run = (argv, make) => runTasks(argv, { env, out: (l) => outLines.push(l), err: (l) => errLines.push(l), make });
  return { env, run, outLines, errLines, dir, home: env.BENCH_HOME, repoTasks: env.BENCH_TASKS_DIR };
}

test('SHA256SUMS is parsed strictly and checked against the files', (t) => {
  const dir = tmp(t);
  fs.mkdirSync(path.join(dir, 'a'));
  fs.writeFileSync(path.join(dir, 'a', 'x'), 'hello');
  const good = `${sha('hello')}  a/x\n`;
  const sums = parseSums(good);
  verifySums(dir, sums);
  assert.equal(parseSums(`${sha('hello')} *./a/x\n`).get('a/x'), sha('hello'));
  fs.writeFileSync(path.join(dir, 'a', 'x'), 'tampered');
  assert.throws(() => verifySums(dir, sums), /differs/);
  fs.rmSync(path.join(dir, 'a', 'x'));
  assert.throws(() => verifySums(dir, sums), /missing/);
  for (const bad of ['', 'zz  a/x\n', `${sha('a')}  ../x\n`, `${sha('a')}  /etc/x\n`, `${sha('a')}  a\n${sha('b')}  a\n`]) assert.throws(() => parseSums(bad), UpstreamError, bad);
});

test('a task checksum covers only that task\'s entries', () => {
  const sums = parseSums(`${sha('1')}  t1/a\n${sha('2')}  t1/b\n${sha('3')}  t2/a\n`);
  const a = taskChecksum('t1', sums);
  assert.match(a, /^[0-9a-f]{64}$/);
  assert.notEqual(a, taskChecksum('t2', sums));
  assert.equal(taskChecksum('t3', sums), null);
  assert.equal(taskChecksum('t1', parseSums(`${sha('1')}  t1/a\n${sha('2')}  t1/b\n${sha('9')}  t2/a\n`)), a);
  assert.notEqual(taskChecksum('t1', parseSums(`${sha('1')}  t1/a\n${sha('8')}  t1/b\n`)), a);
});

test('fetch clones a local bare repository at the pinned commit and lists the tasks', async (t) => {
  const up = upstream(t);
  const s = setup(t, { tasks: { upstreamUrl: up.bare, upstreamCommit: up.commit } });
  assert.equal(await s.run(['fetch']), 0);
  const text = s.outLines.join('\n');
  assert.match(text, /SHA256SUMS ok, 3 task\(s\), 3 without a pinned image digest/);
  assert.match(text, /instance_Org__Beta-bbbbbbb1\tOrg\/Beta/);
  assert.equal(await s.run(['fetch']), 0, 'a second fetch reuses the clone');
});

test('fetch refuses without a pinned commit, and with a checksum that does not match', async (t) => {
  const up = upstream(t);
  const s = setup(t, { tasks: { upstreamUrl: up.bare } });
  await assert.rejects(s.run(['fetch']), (e) => e instanceof ConfigError && /upstreamCommit/.test(e.message));
  // a commit whose file differs from what SHA256SUMS says
  fs.appendFileSync(path.join(up.work, 'v2', up.ids[0], 'instruction.md'), 'tampered\n');
  const bad = commitFiles(up.work, {}, 'tamper');
  execFileSync('git', ['-C', up.bare, 'fetch', '-q', up.work, 'HEAD:refs/heads/tampered']);
  const s2 = setup(t, { tasks: { upstreamUrl: up.bare, upstreamCommit: bad } });
  assert.equal(await s2.run(['fetch']), 1);
  assert.match(s2.errLines.join('\n'), /do not match SHA256SUMS/);
  assert.throws(() => fetchUpstream({ url: up.bare, commit: 'abc', dir: path.join(s2.dir, 'x') }), /full commit id/);
});

/** A runtime that answers the way a good task would: the gold passes, the empty patch fails. */
function goodRuntime(extra = {}) {
  return new FakeContainers({
    digests: extra.digests,
    onRun: (spec) => {
      if (!spec.mounts) return { exitCode: 0, stdout: `devcontainer=${extra.devcontainer?.(spec) ? 'yes' : 'no'}\n` };
      const gold = spec.mounts.some((m) => m.target === '/patch/model.patch');
      fs.writeFileSync(path.join(spec.mounts.find((m) => m.target === '/logs/verifier').source, 'reward.txt'), gold ? '1\n' : '0\n');
      return { exitCode: 0 };
    },
  });
}

test('check, then select: the lock holds ids, checksums and digests only, and drops what failed or has a .devcontainer', async (t) => {
  const up = upstream(t, ['instance_Org__Alpha-aaaaaaa1', 'instance_Org__Alpha-aaaaaaa2', 'instance_Org__Alpha-aaaaaaa3', 'instance_Org__Beta-bbbbbbb1']);
  const s = setup(t, { tasks: { upstreamUrl: up.bare, upstreamCommit: up.commit, seed: 'seed-one' } });
  const digests = {};
  for (const id of up.ids) digests[`${IMAGE}:${id.slice(-8)}`] = `sha256:${sha(id)}`;
  const runtime = goodRuntime({ digests, devcontainer: (spec) => spec.image.endsWith(`sha256:${sha(up.ids[2])}`) });
  // the fourth task: its gold fails
  const original = runtime.onRun;
  runtime.onRun = (spec, i) => (spec.image.endsWith(sha(up.ids[3])) && spec.mounts?.some((m) => m.target === '/patch/model.patch') ? { exitCode: 0 } : original(spec, i));

  assert.equal(await s.run(['select', '--source', 'public']), 0);
  assert.match(s.outLines.join('\n'), /public: 0 of 0 selected/, 'nothing is selectable before the check');

  assert.equal(await s.run(['check'], () => runtime), 0);
  const results = JSON.parse(fs.readFileSync(path.join(s.home, 'tasks', 'check.json'), 'utf8'));
  assert.equal(results[`public/${up.ids[0]}`].ok, true);
  assert.equal(results[`public/${up.ids[2]}`].devcontainer, true);
  assert.equal(results[`public/${up.ids[3]}`].reason, 'gold-fails');
  const pins = JSON.parse(fs.readFileSync(path.join(s.home, 'tasks', 'pins.json'), 'utf8'));
  assert.equal(pins[`${IMAGE}:${up.ids[0].slice(-8)}`], `sha256:${sha(up.ids[0])}`);

  assert.equal(await s.run(['select', '--source', 'public', '--total', '10']), 0);
  const lock = JSON.parse(fs.readFileSync(path.join(s.repoTasks, 'public.lock.json'), 'utf8'));
  assert.deepEqual(lock.tasks.map((x) => x.id), [up.ids[0], up.ids[1]], 'the devcontainer task and the failing task are dropped');
  assert.deepEqual(Object.keys(lock).sort(), ['schema', 'seed', 'source', 'tasks', 'upstreamCommit']);
  assert.deepEqual(Object.keys(lock.tasks[0]).sort(), ['checksum', 'id', 'image', 'repo']);
  assert.equal(lock.seed, 'seed-one');
  assert.equal(lock.tasks[0].image, `sha256:${sha(up.ids[0])}`);
  const text = fs.readFileSync(path.join(s.repoTasks, 'public.lock.json'), 'utf8');
  assert.equal(text.includes('Make the thing work'), false, 'no task content in the lock');

  // the check can be limited to one task
  s.outLines.length = 0;
  assert.equal(await s.run(['check', '--only', up.ids[0]], () => runtime), 0);
  assert.match(s.outLines.join('\n'), /1 kept, 0 dropped/);
});

test('own tasks: mine, lint, build, check and select, end to end on fixtures', async (t) => {
  const clone = initRepo(t);
  commitFiles(clone, { 'src/app.ts': lines(5), 'package.json': '{}\n' }, 'chore: start (#1)');
  const base = git(clone, 'rev-parse', 'HEAD').trim();
  const commit = commitFiles(clone, { 'src/app.ts': `${lines(5)}export function compute_total_now() {}\n`, 'src/app.test.ts': 'test("x", () => {});\n' }, 'feat: total (#2)');
  const ws = `registry.example.test/workspace@sha256:${SHA('d')}`;
  const s = setup(t, { tasks: { clones: { demo: clone }, workspaceImage: ws, selfRepo: 'demo' } });

  assert.equal(await s.run(['mine']), 0);
  const cands = JSON.parse(fs.readFileSync(path.join(s.home, 'tasks', 'own-candidates.json'), 'utf8')).candidates;
  assert.equal(cands.length, 1);
  assert.deepEqual([cands[0].commit, cands[0].base, cands[0].testFiles], [commit, base, ['src/app.test.ts']]);
  assert.equal(JSON.stringify(cands).includes('compute_total_now'), false, 'no source or test content in a candidate');

  const dir = path.join(s.repoTasks, 'own', 'demo-2');
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'own.json'), JSON.stringify({ id: 'demo-2', repo: 'demo', clone: 'demo', commit, base, testCommand: 'npx vitest run src/app.test.ts' }));
  fs.writeFileSync(path.join(dir, 'instruction.md'), 'Add a function compute_total_now.\n');
  assert.equal(await s.run(['lint']), 0);
  assert.match(s.outLines.join('\n'), /warning: the statement names the identifier "compute_total_now"/);
  assert.match(s.outLines.join('\n'), /1 warning\(s\)/);
  fs.writeFileSync(path.join(dir, 'instruction.md'), 'Add a function that computes the total.\n\n## Interface\nThe tests call `compute_total_now()`.\n');
  s.outLines.length = 0;
  await s.run(['lint']);
  assert.match(s.outLines.join('\n'), /0 warning\(s\)/);

  const runtime = goodRuntime({ digests: {} });
  assert.equal(await s.run(['build'], () => runtime), 0);
  assert.equal(runtime.built.length, 1);
  assert.match(runtime.built[0].tag, /^adam-bench-own\/demo-2:/);
  assert.equal(fs.existsSync(path.join(s.home, 'tasks', 'materialized', 'demo-2', 'environment', 'repo', 'src', 'app.ts')), true);
  assert.equal(fs.existsSync(path.join(s.home, 'tasks', 'materialized', 'demo-2', 'environment', 'repo', 'src', 'app.test.ts')), false, 'the hidden test is not in the image');

  assert.equal(await s.run(['check', '--source', 'own'], () => runtime), 0);
  assert.match(s.outLines.join('\n'), /demo-2: kept/);
  assert.equal(await s.run(['select', '--source', 'own'], () => runtime), 0);
  const lock = JSON.parse(fs.readFileSync(path.join(s.repoTasks, 'own.lock.json'), 'utf8'));
  assert.deepEqual(lock.tasks.map((x) => x.id), ['demo-2']);
  assert.match(lock.tasks[0].image, /^sha256:/);
  assert.equal('upstreamCommit' in lock, false);
});

test('the local configuration refuses a bad tasks block, and an unknown subcommand is a usage error', async (t) => {
  for (const tasks of [{ upstreamCommit: 'abc' }, { workspaceImage: 'x/y:latest' }, { clones: { a: 'relative/path' } }, { nope: 1 }, { upstreamDir: '../x' }]) {
    const s = setup(t, { tasks });
    await assert.rejects(s.run(['fetch']), ConfigError, JSON.stringify(tasks));
  }
  const s = setup(t);
  assert.equal(await s.run(['frobnicate']), 64);
  await assert.rejects(s.run(['mine']), (e) => e instanceof ConfigError && /tasks\.clones/.test(e.message));
  await assert.rejects(s.run(['build']), (e) => e instanceof ConfigError && /workspaceImage/.test(e.message));
});
