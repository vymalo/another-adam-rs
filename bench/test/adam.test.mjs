import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { A2AClient } from '../lib/a2a.mjs';
import { AdamAgent, adamMessage, adamSettings } from '../lib/agents/adam.mjs';
import { FakeContainers } from '../lib/containers.mjs';
import { PINS } from '../lib/pins.mjs';
import { loadTask } from '../lib/task.mjs';
import { TASK_ID, fakeClock, fakeCoder } from './a2afix.mjs';
import { commitFiles, git, initRepo } from './gitfix.mjs';
import { tmp } from './helpers.mjs';
import { DIGEST, IMAGE, makeTask } from './taskfix.mjs';

const BEARER = 'test-bearer-0001';
const TOKEN = 'RUNTOKEN0123456789abcdef';
const METER = `http://meter:8090/r/${TOKEN}/v1`;
const SLUG = 'demo-1-aaaaaa';

test('the settings: the meter\'s address, the bearer, the task image by digest only, the window when the configuration has one', () => {
  const ref = `${IMAGE}@${DIGEST}`;
  assert.deepEqual(adamSettings({ meterBaseUrl: METER, bearer: BEARER, taskImage: ref }), { MODEL_BASE_URL: METER, A2A_BEARER_TOKENS: BEARER, DEVCONTAINER_DEFAULT_IMAGE: ref });
  assert.equal(adamSettings({ meterBaseUrl: METER, bearer: BEARER, taskImage: ref, contextWindow: 65536 }).MODEL_CONTEXT_WINDOW, '65536');
  assert.throws(() => adamSettings({ meterBaseUrl: METER, bearer: BEARER, taskImage: `${IMAGE}:tag@${DIGEST}` }), /digest only/);
  assert.throws(() => adamSettings({ meterBaseUrl: METER, bearer: BEARER, taskImage: `${IMAGE}:tag` }), /digest only/);
});

test('the message is the one line adam needs, then the statement as it is', () => {
  assert.equal(adamMessage({ repoUrl: 'http://git-server:8080/bench/x-1.git', statement: 'Fix the bug.\n' }), 'In http://git-server:8080/bench/x-1.git (base branch main), do this task:\n\nFix the bug.\n');
});

function setup(t, { script = [{ state: 'working' }, { state: 'completed' }], holdStream = false } = {}) {
  const home = tmp(t);
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  const stackCalls = [];
  const containers = new FakeContainers();
  const state = { runs: ['0191a2b3-7c4d'], slots: { '0191a2b3-7c4d': [SLUG] } };
  return (async () => {
    const server = await fakeCoder({ script, holdStream, bearer: BEARER });
    t.after(() => server.close());
    const stack = {
      agentDir: null,
      ports: { git: 18083 },
      resetDatabase: async (slot) => stackCalls.push(['resetDatabase', slot]),
      resetWork: async (slot) => stackCalls.push(['resetWork', slot]),
      startCoder: async (slot, env) => stackCalls.push(['startCoder', slot, env]),
      stopCoder: async (slot) => stackCalls.push(['stopCoder', slot]),
      coderUrl: () => server.url,
      listDir: async (slot, dir) => (dir === '/work/workspaces' ? state.runs : state.slots[dir.split('/').at(-1)] ?? []),
    };
    const pushes = [];
    const logs = [];
    const clock = fakeClock();
    const agent = new AdamAgent({
      pins: PINS,
      home,
      repoDir: '/repo',
      contextWindow: 131072,
      push: async (o) => pushes.push(o),
      heads: async () => [{ ref: 'agent/0191a2b3', sha: 'b'.repeat(40) }],
      newBearer: () => BEARER,
      ensureFolder: async (o) => {
        stackCalls.push(['ensureFolder', o.revision]);
        return o.dest;
      },
      now: clock.now,
      sleep: clock.sleep,
    });
    const base = { dir: '/base/base.git', sha: 'a'.repeat(40), slug: SLUG, workdir: '/app' };
    const ctx = {
      run: 'r1',
      attempt: 1,
      slot: 1,
      task,
      base,
      stack,
      containers,
      meter: { token: TOKEN, baseUrl: METER, refusal: () => null },
      paths: { run: tmp(t), in: path.join(tmp(t), 'in'), out: path.join(tmp(t), 'out') },
      limitMs: 50 * 60 * 1000,
      log: (e) => logs.push(e),
    };
    return { agent, ctx, server, stackCalls, pushes, logs, containers, state, clock, task, home };
  })();
}

test('prepare: the folder of the image\'s revision, the base pushed, a clean database and volume, then the coder with this run\'s settings, in that order', async (t) => {
  const { agent, ctx, stackCalls, pushes, task } = await setup(t);
  await agent.prepare(ctx);
  assert.deepEqual(stackCalls.map((c) => c[0]), ['ensureFolder', 'resetDatabase', 'resetWork', 'startCoder']);
  assert.equal(stackCalls[0][1], PINS.coder.revision);
  assert.deepEqual(pushes, [{ dir: '/base/base.git', url: `http://127.0.0.1:18083/bench/${SLUG}.git` }]);
  const env = stackCalls[3][2];
  assert.deepEqual(env, { MODEL_BASE_URL: METER, A2A_BEARER_TOKENS: BEARER, DEVCONTAINER_DEFAULT_IMAGE: task.image.ref, MODEL_CONTEXT_WINDOW: '131072' });
  assert.match(ctx.stack.agentDir, /agent-folder\/2d5d0b3926cb$/);
});

test('prepare: each run has its own random bearer', async (t) => {
  const { ctx, home } = await setup(t);
  const seen = new Set();
  for (let i = 0; i < 3; i++) {
    const agent = new AdamAgent({ pins: PINS, home, repoDir: '/r', push: async () => {}, ensureFolder: async (o) => o.dest });
    const calls = [];
    ctx.stack.startCoder = async (slot, env) => calls.push(env.A2A_BEARER_TOKENS);
    await agent.prepare(ctx);
    seen.add(calls[0]);
  }
  assert.equal(seen.size, 3);
  for (const b of seen) assert.match(b, /^[A-Za-z0-9_-]{32}$/);
});

test('run: the task is sent, polled to its end, and what it used is the agent\'s own figure; the bearer is in no log line', async (t) => {
  const { agent, ctx, server, logs, stackCalls } = await setup(t, { script: [{ state: 'working' }, { state: 'completed', totals: [{ inputTokens: 100, outputTokens: 20 }] }] });
  await agent.prepare(ctx);
  const r = await agent.run(ctx);
  assert.equal(r.exit, 'done');
  assert.equal(r.taskId, TASK_ID);
  assert.deepEqual(r.selfReported, { input: 100, output: 20, cached: 0, calls: 1 });
  const sent = server.of('SendStreamingMessage')[0].params.message;
  assert.equal(sent.parts[0].text, `In http://git-server:8080/bench/${SLUG}.git (base branch main), do this task:\n\nMake the thing work.\n`);
  assert.equal(sent.messageId, 'bench-r1-a1');
  assert.ok(!JSON.stringify(logs).includes(BEARER));
  const kept = fs.readFileSync(path.join(ctx.paths.run, 'adam', 'task.json'), 'utf8');
  assert.ok(!kept.includes(BEARER) && !kept.includes(TOKEN));
  assert.equal(JSON.parse(kept).exit, 'done');
  void stackCalls;
});

test('run: a task that parks twice is nudged once and cancelled; one that never ends is cancelled at the time limit', async (t) => {
  const parks = await setup(t, { script: [{ state: 'input-required', ts: 'T1' }, { state: 'working' }, { state: 'input-required', ts: 'T2' }] });
  await parks.agent.prepare(parks.ctx);
  const r1 = await parks.agent.run(parks.ctx);
  assert.deepEqual([r1.exit, r1.nudges], ['input_required', 1]);
  assert.equal(parks.server.of('CancelTask').length, 1);

  const slow = await setup(t, { script: [{ state: 'working' }] });
  slow.ctx.limitMs = 5 * 60_000;
  await slow.agent.prepare(slow.ctx);
  const r2 = await slow.agent.run(slow.ctx);
  assert.equal(r2.exit, 'timeout');
  assert.equal(slow.server.of('CancelTask').length, 1);
  assert.ok(slow.clock.elapsed() >= 5 * 60_000);
});

test('run: a refusal of the meter ends the run, cancelled, as budget_hit', async (t) => {
  const { agent, ctx, server } = await setup(t, { script: [{ state: 'working' }] });
  await agent.prepare(ctx);
  let polls = 0;
  ctx.meter.refusal = () => (++polls > 1 ? 'budget' : null);
  assert.equal((await agent.run(ctx)).exit, 'budget_hit');
  assert.equal(server.of('CancelTask').length, 1);
});

// ---- collect, on a real base and a copy made by the fake helper container ----

function realBase(t) {
  const app = initRepo(t);
  commitFiles(app, { 'README.md': 'demo\n' }, 'base');
  const bare = path.join(tmp(t), 'base.git');
  execFileSync('git', ['clone', '-q', '--bare', app, bare]);
  return { dir: bare, sha: git(app, 'rev-parse', 'HEAD').trim(), slug: SLUG, workdir: '/app' };
}

test('collect: the worktree is copied out by a helper container of the task image, read-only, and becomes the patch; the pushed branch is the cross-check', async (t) => {
  const { agent, ctx, containers } = await setup(t);
  ctx.base = realBase(t);
  containers.onRun = (spec) => {
    const out = spec.mounts.find((m) => m.target === '/out').source;
    fs.writeFileSync(path.join(out, 'README.md'), 'demo\n');
    fs.writeFileSync(path.join(out, 'hello.txt'), 'hello\n');
    return { exitCode: 0 };
  };
  const r = await agent.collect(ctx);
  const spec = containers.runs[0];
  assert.equal(spec.image, ctx.task.image.ref);
  assert.deepEqual(spec.mounts.find((m) => m.target === '/tree'), { source: `/work/workspaces/0191a2b3-7c4d/${SLUG}`, target: '/tree', readOnly: true });
  assert.equal(spec.network, 'none');
  assert.match(r.patch.toString('utf8'), /\+\+\+ b\/hello\.txt/);
  assert.deepEqual(r.notes.worktree, { run: '0191a2b3-7c4d', slot: SLUG });
  assert.deepEqual(r.notes.branch, { branch: 'agent/0191a2b3', sha: 'b'.repeat(40), consistent: true, pushed: 1 });
});

test('collect: a run that made no workspace has an empty patch, and the notes say why; two runs on the volume is an error', async (t) => {
  const none = await setup(t);
  none.state.runs.length = 0;
  const r = await none.agent.collect(none.ctx);
  assert.equal(r.patch.length, 0);
  assert.equal(r.notes.worktree, null);

  const many = await setup(t);
  many.state.runs.push('another');
  await assert.rejects(many.agent.collect(many.ctx), /was not emptied/);
});

test('collect: a branch list that fails does not fail the collection', async (t) => {
  const { agent, ctx, containers } = await setup(t);
  ctx.base = realBase(t);
  agent.heads = async () => {
    throw new Error('network down');
  };
  containers.onRun = (spec) => {
    fs.writeFileSync(path.join(spec.mounts.find((m) => m.target === '/out').source, 'README.md'), 'demo\n');
    return { exitCode: 0 };
  };
  const r = await agent.collect(ctx);
  assert.deepEqual(r.notes.branch, { error: 'the branches could not be listed' });
});

test('cleanup: the coder is stopped and the scratch is removed, even after a failure', async (t) => {
  const { agent, ctx, stackCalls } = await setup(t);
  await agent.prepare(ctx);
  fs.writeFileSync(path.join(ctx.paths.out, 'x'), 'x');
  await agent.cleanup(ctx);
  assert.deepEqual(stackCalls.at(-1), ['stopCoder', 1]);
  assert.equal(fs.existsSync(ctx.paths.out), false);
  ctx.stack.stopCoder = async () => {
    throw new Error('compose failed');
  };
  fs.mkdirSync(ctx.paths.out);
  await assert.rejects(agent.cleanup(ctx), /compose failed/);
  assert.equal(fs.existsSync(ctx.paths.out), false, 'the scratch is removed even then');
});

test('a client made for the coder refuses a bearer that is not plain, and is not printable', () => {
  const c = new A2AClient({ url: 'http://127.0.0.1:1/', bearer: BEARER });
  assert.ok(!JSON.stringify(c).includes(BEARER));
  assert.ok(!String(c).includes(BEARER));
});
