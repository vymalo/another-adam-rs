import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { ConfigError } from '../lib/config.mjs';
import { FakeContainers } from '../lib/containers.mjs';
import { PINS } from '../lib/pins.mjs';
import { runTasks } from '../lib/tasks-cli.mjs';
import { UpstreamError, fetchUpstream, parseSums, taskChecksum, verifySums } from '../lib/upstream.mjs';
import { commitFiles, git, initRepo, lines, SHA } from './gitfix.mjs';
import { rawConfig, tmp } from './helpers.mjs';
import { IMAGE, makeTask } from './taskfix.mjs';

const sha = (text) => crypto.createHash('sha256').update(text).digest('hex');

/** An upstream repository with tasks under v2/, a SHA256SUMS, and a bare copy of it standing in for the real one. */
function upstream(t, ids = ['instance_Org__Alpha-aaaaaaa1', 'instance_Org__Alpha-aaaaaaa2', 'instance_Org__Beta-bbbbbbb1'], over = () => ({})) {
  const work = initRepo(t);
  const root = path.join(work, 'v2');
  for (const id of ids) makeTask(root, id, { 'environment/Dockerfile': `FROM ${IMAGE}:${id.slice(-8)}\n`, ...over(id) });
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
  assert.equal(parseSums(`${sha('hello')} *a/x\n`).get('a/x'), sha('hello'));
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
  assert.match(s2.errLines.join('\n'), /fault\(s\) against SHA256SUMS/);
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
  assert.deepEqual(Object.keys(lock.tasks[0]).sort(), ['agentPath', 'checksum', 'id', 'image', 'repo']);
  assert.equal(lock.tasks[0].agentPath, 'pass');
  assert.equal(results[`public/${up.ids[0]}`].agentPath, 'pass');
  assert.equal(results[`public/${up.ids[3]}`].agentPath, 'not-run', 'a dropped task has no agent-path run');
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
  commitFiles(clone, { 'src/app.ts': lines(5), 'package.json': '{"scripts":{"test":"vitest run"}}\n' }, 'chore: start (#1)');
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
  assert.match(s.outLines.join('\n'), /1 warning\(s\), 0 refused/);
  fs.writeFileSync(path.join(dir, 'instruction.md'), 'Add a function that computes the total.\n\n## Interface\nThe tests call `compute_total_now()`.\n');
  s.outLines.length = 0;
  await s.run(['lint']);
  assert.match(s.outLines.join('\n'), /0 warning\(s\), 0 refused/);

  const runtime = goodRuntime({ digests: {} });
  assert.equal(await s.run(['build'], () => runtime), 0);
  assert.equal(runtime.built.length, 1);
  assert.match(runtime.built[0].tag, /^adam-bench-own\/demo-2:/);
  assert.equal(fs.existsSync(path.join(s.home, 'tasks', 'contexts', 'demo-2', 'repo', 'src', 'app.ts')), true);
  assert.equal(fs.existsSync(path.join(s.home, 'tasks', 'contexts', 'demo-2', 'repo', 'src', 'app.test.ts')), false, 'the hidden test is not in the image');

  assert.equal(await s.run(['check', '--source', 'own'], () => runtime), 0);
  assert.match(s.outLines.join('\n'), /demo-2: kept/);
  assert.equal(await s.run(['select', '--source', 'own'], () => runtime), 0);
  const lock = JSON.parse(fs.readFileSync(path.join(s.repoTasks, 'own.lock.json'), 'utf8'));
  assert.deepEqual(lock.tasks.map((x) => x.id), ['demo-2']);
  assert.match(lock.tasks[0].image, /^sha256:/);
  assert.equal('upstreamCommit' in lock, false);
});

test('S4: lint refuses an own task whose repository has no test command a coder would find', async (t) => {
  const clone = initRepo(t);
  commitFiles(clone, { 'src/app.ts': lines(5), 'package.json': '{}\n' }, 'chore: start (#1)');
  const base = git(clone, 'rev-parse', 'HEAD').trim();
  const commit = commitFiles(clone, { 'src/app.ts': `${lines(5)}export const x = 1;\n`, 'src/app.test.ts': 'test("x", () => {});\n' }, 'feat: x (#2)');
  const s = setup(t, { tasks: { clones: { demo: clone }, selfRepo: 'demo' } });
  const dir = path.join(s.repoTasks, 'own', 'demo-2');
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'own.json'), JSON.stringify({ id: 'demo-2', repo: 'demo', clone: 'demo', commit, base, testCommand: 'npx vitest run' }));
  fs.writeFileSync(path.join(dir, 'instruction.md'), 'Add a constant.\n');
  assert.equal(await s.run(['lint']), 1);
  assert.match(s.outLines.join('\n'), /demo-2: refused: its repository at the base has no test command a coder would find/);
  assert.match(s.outLines.join('\n'), /1 refused/);
});

test('K-lint-reads-head: the lint looks at the base, not at the task\'s commit: a test script added by the commit does not count', async (t) => {
  const clone = initRepo(t);
  commitFiles(clone, { 'src/app.ts': lines(5), 'package.json': '{}\n' }, 'chore: start (#1)');
  const base = git(clone, 'rev-parse', 'HEAD').trim();
  const commit = commitFiles(clone, { 'src/app.ts': `${lines(5)}export const x = 1;\n`, 'package.json': '{"scripts":{"test":"vitest"}}\n' }, 'feat: x (#2)');
  const s = setup(t, { tasks: { clones: { demo: clone }, selfRepo: 'demo' } });
  const dir = path.join(s.repoTasks, 'own', 'demo-2');
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'own.json'), JSON.stringify({ id: 'demo-2', repo: 'demo', clone: 'demo', commit, base, testCommand: 'npx vitest run' }));
  fs.writeFileSync(path.join(dir, 'instruction.md'), 'Add a constant.\n');
  assert.equal(await s.run(['lint']), 1);
  assert.match(s.outLines.join('\n'), /demo-2: refused/);
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

// ---- `tasks check` walks the selection order and stops at the target ----

/** Tasks of several repositories: { Alpha: 12, ... } -> ids like instance_Org__Alpha-aaaa0003. A `FAIL` line in a gold patch makes its gold fail. */
const manyIds = (spec) => Object.entries(spec).flatMap(([repo, n]) => Array.from({ length: n }, (_, i) => `instance_Org__${repo}-${repo[0].toLowerCase().repeat(4)}${String(i).padStart(4, '0')}`));
const failing = (set) => (id) => ({ 'solution/solution.patch': set.has(id) ? `FAIL ${id}\n` : `diff --git a/x b/x\n# ${id}\n` });

/** A runtime whose gold passes unless the gold patch has a FAIL line; it records what is pulled and what is removed. */
function walkRuntime({ ids, digestFor = (id) => `sha256:${sha(id)}` }) {
  const digests = {};
  for (const id of ids) digests[`${IMAGE}:${id.slice(-8)}`] = digestFor(id);
  const rt = new FakeContainers({
    digests,
    onRun: (spec) => {
      if (!spec.mounts) return { exitCode: 0, stdout: 'devcontainer=no\n' };
      const model = spec.mounts.find((m) => m.target === '/patch/model.patch');
      const bad = model && fs.readFileSync(model.source, 'utf8').includes('FAIL');
      fs.writeFileSync(path.join(spec.mounts.find((m) => m.target === '/logs/verifier').source, 'reward.txt'), model && !bad ? '1\n' : '0\n');
      return { exitCode: 0 };
    },
  });
  rt.removed = [];
  rt.removeImage = async (ref) => {
    if (rt.removeFails) throw new Error('image is in use');
    rt.removed.push(ref);
  };
  // each checked task starts with one probe run (no mounts) on its image by digest
  rt.checkedIds = () => rt.runs.filter((r) => !r.mounts).map((r) => ids.find((id) => r.image === `${IMAGE}@${digestFor(id)}`));
  return rt;
}

const lockIds = (s) => JSON.parse(fs.readFileSync(path.join(s.repoTasks, 'public.lock.json'), 'utf8')).tasks.map((x) => x.id);
const checkJson = (s) => JSON.parse(fs.readFileSync(path.join(s.home, 'tasks', 'check.json'), 'utf8'));
const publicCfg = (up) => ({ tasks: { upstreamUrl: up.bare, upstreamCommit: up.commit, seed: 'seed-one' } });

test('check walks in the seeded order, stops once the target is kept, and select gives what a full check would', async (t) => {
  const ids = manyIds({ Alpha: 12, Beta: 12, Gamma: 12 });
  const bad = new Set(ids.filter((_, i) => i % 5 === 2));
  const up = upstream(t, ids, failing(bad));
  const part = setup(t, publicCfg(up));
  const rt = walkRuntime({ ids });
  assert.equal(await part.run(['check', '--total', '10'], () => rt), 0);
  const walked = rt.checkedIds();
  assert.ok(walked.length >= 10 && walked.length < ids.length, `walked ${walked.length} of ${ids.length}`);
  assert.match(part.outLines.join('\n'), /10 kept, \d+ dropped, \d+ not needed/);
  assert.equal(await part.run(['select', '--source', 'public', '--total', '10']), 0);

  const full = setup(t, publicCfg(up));
  const rt2 = walkRuntime({ ids });
  assert.equal(await full.run(['check', '--all'], () => rt2), 0);
  assert.equal(rt2.checkedIds().length, ids.length, '--all walks every task');
  assert.equal(await full.run(['select', '--source', 'public', '--total', '10']), 0);
  assert.deepEqual(lockIds(part), lockIds(full));
  assert.equal(lockIds(part).length, 10);
  // the walk went in the order select uses: what it checked is a prefix of that order
  const { selectionOrder } = await import('../lib/select.mjs');
  const order = selectionOrder(ids.map((id) => ({ id, repo: `Org/${id.split('__')[1].split('-')[0]}` })), { seed: 'seed-one', scope: 'public' }).map((x) => x.id);
  assert.deepEqual(walked, order.filter((id) => walked.includes(id)).slice(0, walked.length));
  assert.deepEqual(walked.slice(0, 3).map((id) => id.split('__')[1][0]).sort(), ['A', 'B', 'G'], 'the first round takes one task of each repository');
});

test('check keeps at most 9 per repository, skips a repository that has 9, and reports the shortfall', async (t) => {
  const ids = manyIds({ Alpha: 14, Beta: 3 });
  const up = upstream(t, ids);
  const s = setup(t, publicCfg(up));
  const rt = walkRuntime({ ids });
  assert.equal(await s.run(['check'], () => rt), 0);
  const walked = rt.checkedIds();
  assert.equal(walked.filter((id) => id.includes('Alpha')).length, 9, 'the tenth Alpha is never checked');
  assert.equal(walked.filter((id) => id.includes('Beta')).length, 3);
  const text = s.outLines.join('\n');
  assert.match(text, /12 kept, 0 dropped, 5 not needed/);
  assert.match(text, /shortfall: 12 of 80 kept/);
  assert.equal(await s.run(['select', '--source', 'public']), 0);
  assert.equal(lockIds(s).length, 12);
});

test('check resumes from check.json: a recorded verdict is not run again and the walk goes on in the same order; an infra result is', async (t) => {
  const ids = manyIds({ Alpha: 12, Beta: 12 });
  const up = upstream(t, ids, failing(new Set([ids[1], ids[14]])));
  const s = setup(t, publicCfg(up));
  const first = walkRuntime({ ids });
  assert.equal(await s.run(['check', '--total', '4'], () => first), 0);
  const done = first.checkedIds();
  assert.ok(done.length >= 4);
  // one of the recorded results is an infra failure: that is not a verdict
  const results = checkJson(s);
  const infraId = done[0];
  results[`public/${infraId}`] = { ...results[`public/${infraId}`], ok: false, reason: 'infra' };
  fs.writeFileSync(path.join(s.home, 'tasks', 'check.json'), JSON.stringify(results));
  const second = walkRuntime({ ids });
  assert.equal(await s.run(['check', '--total', '8'], () => second), 0);
  const again = second.checkedIds();
  assert.deepEqual(again.filter((id) => done.includes(id)), [infraId], 'only the infra result is run again');
  const fresh = new Set(again.filter((id) => id !== infraId));
  assert.ok(fresh.size > 0);
  // a third run with the target met runs nothing
  const third = walkRuntime({ ids });
  assert.equal(await s.run(['check', '--total', '8'], () => third), 0);
  assert.deepEqual(third.checkedIds(), []);
  assert.match(s.outLines.join('\n'), /8 kept/);
  // one walk from nothing gives the same verdicts
  const whole = setup(t, publicCfg(up));
  assert.equal(await whole.run(['check', '--total', '8'], () => walkRuntime({ ids })), 0);
  assert.deepEqual(Object.keys(checkJson(whole)).sort(), Object.keys(checkJson(s)).sort());
});


test('check --keep-images removes the image of a dropped task right after its verdict, and never one a kept task or the workspace image uses', async (t) => {
  const ids = ['instance_Org__Alpha-aaaaaaa1', 'instance_Org__Alpha-aaaaaaa2', 'instance_Org__Beta-bbbbbbb1', 'instance_Org__Beta-bbbbbbb2'];
  const bad = new Set([ids[0], ids[2], ids[3]]);
  const shared = `sha256:${sha('shared')}`;
  const workspace = `sha256:${sha('workspace')}`;
  // ids[1] (kept) and ids[0] (dropped) share an image; ids[3] (dropped) has the workspace image's digest
  const digestFor = (id) => (id === ids[0] || id === ids[1] ? shared : id === ids[3] ? workspace : `sha256:${sha(id)}`);
  const up = upstream(t, ids, failing(bad));
  const s = setup(t, { tasks: { ...publicCfg(up).tasks, workspaceImage: `registry.example.test/workspace@${workspace}` } });
  const rt = walkRuntime({ ids, digestFor });
  assert.equal(await s.run(['check', '--all', '--keep-images'], () => rt), 0);
  const removed = rt.removed.join('\n');
  assert.match(removed, new RegExp(`${IMAGE}@sha256:${sha(ids[2])}`), 'a dropped task\'s image is removed by digest');
  assert.match(removed, new RegExp(`${IMAGE}:${ids[2].slice(-8)}`), 'and by tag');
  assert.equal(removed.includes(shared), false, 'an image a kept task shares stays');
  assert.equal(removed.includes(`:${ids[0].slice(-8)}`), false);
  assert.equal(removed.includes(workspace), false, 'the workspace (coder) image stays');
  assert.equal(removed.includes(`:${ids[1].slice(-8)}`), false, 'a kept task\'s image stays');
  assert.equal(removed.includes(`:${ids[3].slice(-8)}`), false);
});

test('check removes the image of a kept task too, once its digest is in check.json, never the workspace image or the coder\'s', async (t) => {
  const ids = ['instance_Org__Alpha-aaaaaaa1', 'instance_Org__Alpha-aaaaaaa2', 'instance_Org__Beta-bbbbbbb1', 'instance_Org__Beta-bbbbbbb2'];
  const workspace = `sha256:${sha('workspace')}`;
  const digestFor = (id) => (id === ids[3] ? workspace : id === ids[2] ? PINS.coder.digest : `sha256:${sha(id)}`);
  const up = upstream(t, ids, failing(new Set([ids[0]])));
  const s = setup(t, { tasks: { ...publicCfg(up).tasks, workspaceImage: `registry.example.test/workspace@${workspace}` } });
  const rt = walkRuntime({ ids, digestFor });
  const recordedAtRemoval = {};
  const remove = rt.removeImage;
  rt.removeImage = async (ref) => {
    const digest = /@(sha256:[0-9a-f]{64})$/.exec(ref)?.[1];
    if (digest) recordedAtRemoval[digest] = Object.values(checkJson(s)).some((r) => r.imageDigest === digest && r.ok !== undefined);
    return remove(ref);
  };
  assert.equal(await s.run(['check', '--all'], () => rt), 0);
  const removed = rt.removed.join('\n');
  assert.match(removed, new RegExp(`${IMAGE}@sha256:${sha(ids[1])}`), 'a kept task\'s image is removed by digest');
  assert.match(removed, new RegExp(`${IMAGE}:${ids[1].slice(-8)}`), 'and by tag');
  assert.match(removed, new RegExp(`${IMAGE}@sha256:${sha(ids[0])}`), 'and a dropped task\'s, as before');
  assert.equal(removed.includes(workspace), false, 'the workspace image stays');
  assert.equal(removed.includes(PINS.coder.digest), false, 'the coder image stays');
  assert.equal(recordedAtRemoval[`sha256:${sha(ids[1])}`], true, 'the digest was in check.json before the image went');
  const kept = checkJson(s)[`public/${ids[1]}`];
  assert.equal(kept.ok, true);
  assert.equal(kept.imageDigest, `sha256:${sha(ids[1])}`);
});

test('check --keep-images keeps the images of the kept tasks', async (t) => {
  const ids = ['instance_Org__Alpha-aaaaaaa1', 'instance_Org__Alpha-aaaaaaa2'];
  const up = upstream(t, ids);
  const s = setup(t, publicCfg(up));
  const rt = walkRuntime({ ids });
  assert.equal(await s.run(['check', '--all', '--keep-images'], () => rt), 0);
  assert.deepEqual(rt.removed, []);
  assert.match(s.outLines.join('\n'), /2 kept, 0 dropped/);
});

test('a failed image removal is logged and does not change the verdict', async (t) => {
  const ids = ['instance_Org__Alpha-aaaaaaa1', 'instance_Org__Alpha-aaaaaaa2'];
  const up = upstream(t, ids, failing(new Set([ids[0]])));
  const s = setup(t, publicCfg(up));
  const rt = walkRuntime({ ids });
  rt.removeFails = true;
  assert.equal(await s.run(['check', '--all'], () => rt), 0);
  assert.equal(checkJson(s)[`public/${ids[0]}`].reason, 'gold-fails');
  assert.equal(checkJson(s)[`public/${ids[1]}`].ok, true);
  assert.match(s.outLines.join('\n'), new RegExp(`${ids[0]}: image not removed \\(image is in use\\)`));
});

test('check --all does not stop at the target and does not skip a full repository; it still resumes', async (t) => {
  const ids = manyIds({ Alpha: 12, Beta: 12 });
  const up = upstream(t, ids);
  const s = setup(t, publicCfg(up));
  const rt = walkRuntime({ ids });
  assert.equal(await s.run(['check', '--all', '--total', '5'], () => rt), 0);
  assert.equal(rt.checkedIds().length, 24);
  assert.doesNotMatch(s.outLines.join('\n'), /shortfall/);
  const rt2 = walkRuntime({ ids });
  assert.equal(await s.run(['check', '--all'], () => rt2), 0);
  assert.deepEqual(rt2.checkedIds(), []);
});

test('check --only runs exactly the named tasks, again, whatever the target', async (t) => {
  const ids = manyIds({ Alpha: 4 });
  const up = upstream(t, ids);
  const s = setup(t, publicCfg(up));
  await s.run(['check', '--all'], () => walkRuntime({ ids }));
  const rt = walkRuntime({ ids });
  assert.equal(await s.run(['check', '--only', ids[2]], () => rt), 0);
  assert.deepEqual(rt.checkedIds(), [ids[2]]);
});

// ---- the grading path is /app; the clone path is an attribute (owner's decision, 2026-11-20) ----

/** Gold and empty answer as a good task does at /app; the gold's clone-path run answers `atClone`. */
function pathRuntime({ ids, atClone = () => '0' }) {
  const rt = walkRuntime({ ids });
  rt.onRun = (spec) => {
    if (!spec.mounts) return { exitCode: 0, stdout: 'devcontainer=no\n' };
    const model = spec.mounts.find((m) => m.target === '/patch/model.patch');
    const id = ids.find((x) => spec.image.endsWith(sha(x)));
    const clone = spec.script.includes('/work/workspaces/bench/slot');
    const bad = model && fs.readFileSync(model.source, 'utf8').includes('FAIL');
    const reward = clone ? atClone(id) : model && !bad ? '1' : '0';
    fs.writeFileSync(path.join(spec.mounts.find((m) => m.target === '/logs/verifier').source, 'reward.txt'), `${reward}\n`);
    return { exitCode: 0 };
  };
  return rt;
}

test('a task that fails only at the clone path is kept, recorded agentPath fail and carried into the lock; the others say pass or not-run', async (t) => {
  const ids = ['instance_Org__Alpha-aaaaaaa1', 'instance_Org__Alpha-aaaaaaa2', 'instance_Org__Alpha-aaaaaaa3'];
  const up = upstream(t, ids, failing(new Set([ids[2]])));
  const s = setup(t, publicCfg(up));
  const rt = pathRuntime({ ids, atClone: (id) => (id === ids[1] ? '0' : '1') });
  assert.equal(await s.run(['check'], () => rt), 0);
  const r = checkJson(s);
  assert.deepEqual([r[`public/${ids[0]}`].ok, r[`public/${ids[0]}`].agentPath], [true, 'pass']);
  assert.deepEqual([r[`public/${ids[1]}`].ok, r[`public/${ids[1]}`].agentPath, r[`public/${ids[1]}`].agentPathReason], [true, 'fail', 'tests-fail']);
  assert.deepEqual([r[`public/${ids[2]}`].ok, r[`public/${ids[2]}`].reason, r[`public/${ids[2]}`].agentPath], [false, 'gold-fails', 'not-run']);
  assert.match(s.outLines.join('\n'), new RegExp(`${ids[1]}: kept \\(agent path: fail, tests-fail\\)`));
  assert.match(s.outLines.join('\n'), /2 kept, 1 dropped/);
  assert.equal(await s.run(['select', '--source', 'public']), 0);
  const lock = JSON.parse(fs.readFileSync(path.join(s.repoTasks, 'public.lock.json'), 'utf8'));
  assert.deepEqual(Object.fromEntries(lock.tasks.map((x) => [x.id, x.agentPath])), { [ids[0]]: 'pass', [ids[1]]: 'fail' });
});

test('resume: a verdict from before the rule (no agentPath) is checked again; a verdict with one is not', async (t) => {
  const ids = ['instance_Org__Alpha-aaaaaaa1', 'instance_Org__Alpha-aaaaaaa2'];
  const up = upstream(t, ids);
  const s = setup(t, publicCfg(up));
  assert.equal(await s.run(['check'], () => pathRuntime({ ids, atClone: () => '1' })), 0);
  const results = checkJson(s);
  // as an old check wrote them: the clone path was the rule, and a clone-path failure was a drop
  delete results[`public/${ids[0]}`].agentPath;
  delete results[`public/${ids[0]}`].agentPathReason;
  results[`public/${ids[1]}`] = { ...results[`public/${ids[1]}`], ok: false, reason: 'gold-fails' };
  delete results[`public/${ids[1]}`].agentPath;
  delete results[`public/${ids[1]}`].agentPathReason;
  fs.writeFileSync(path.join(s.home, 'tasks', 'check.json'), JSON.stringify(results));
  assert.equal(await s.run(['select', '--source', 'public']), 0);
  assert.equal(JSON.parse(fs.readFileSync(path.join(s.repoTasks, 'public.lock.json'), 'utf8')).tasks.length, 0, 'an old verdict is not selectable');
  const again = pathRuntime({ ids, atClone: () => '1' });
  assert.equal(await s.run(['check'], () => again), 0);
  assert.deepEqual(again.checkedIds().sort(), [...ids].sort(), 'both are run again: the old data has no empty run at /app');
  assert.equal(checkJson(s)[`public/${ids[1]}`].ok, true, 'the task dropped for a clone-path reason is now kept');
  const third = pathRuntime({ ids });
  assert.equal(await s.run(['check'], () => third), 0);
  assert.deepEqual(third.checkedIds(), [], 'a new verdict is resumed');
});
