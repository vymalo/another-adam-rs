import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { A2AClient } from '../lib/a2a.mjs';
import { AdamAgent, adamMessage, adamSettings, ranTaskImage } from '../lib/agents/adam.mjs';
import { InfraError } from '../lib/agents/common.mjs';
import { FakeContainers } from '../lib/containers.mjs';
import { runRepoName } from '../lib/gitops.mjs';
import { PINS } from '../lib/pins.mjs';
import { extractEntries, parseTar } from '../lib/tarstream.mjs';
import { loadTask } from '../lib/task.mjs';
import { TASK_ID, fakeClock, fakeCoder } from './a2afix.mjs';
import { commitFiles, git, initRepo } from './gitfix.mjs';
import { tmp } from './helpers.mjs';
import { DIGEST, IMAGE, makeTask } from './taskfix.mjs';

const BEARER = 'test-bearer-0001';
const TOKEN = 'RUNTOKEN0123456789abcdef';
const METER = `http://meter-1:8090/r/${TOKEN}/v1`;
const SLUG = 'demo-1-aaaaaa';
const LIMITS = { limitMs: 50 * 60 * 1000, steps: 150, cancelGraceMs: 30_000 };

test('the settings: the meter\'s address, the bearer, the task image by digest only, the window, which is required', () => {
  const ref = `${IMAGE}@${DIGEST}`;
  assert.deepEqual(adamSettings({ meterBaseUrl: METER, bearer: BEARER, taskImage: ref, contextWindow: 65536 }), { MODEL_BASE_URL: METER, A2A_BEARER_TOKENS: BEARER, DEVCONTAINER_DEFAULT_IMAGE: ref, MODEL_CONTEXT_WINDOW: '65536' });
  assert.throws(() => adamSettings({ meterBaseUrl: METER, bearer: BEARER, taskImage: `${IMAGE}:tag@${DIGEST}`, contextWindow: 1024 }), /digest only/);
  assert.throws(() => adamSettings({ meterBaseUrl: METER, bearer: BEARER, taskImage: ref }), /context window is required/);
  assert.throws(() => adamSettings({ meterBaseUrl: METER, bearer: BEARER, taskImage: ref, contextWindow: null }), /context window is required/);
});

test('the message is the one line adam needs, then the statement as it is', () => {
  assert.equal(adamMessage({ repoUrl: 'http://git-server:8080/bench/x-1.git', statement: 'Fix the bug.\n' }), 'In http://git-server:8080/bench/x-1.git (base branch main), do this task:\n\nFix the bug.\n');
});

test('a container of the task image ran: by the digest in its image, or by its id', () => {
  const task = { image: { digest: DIGEST } };
  assert.equal(ranTaskImage([{ image: `registry.example.test/x@${DIGEST}`, imageId: 'f'.repeat(64) }], task), true);
  assert.equal(ranTaskImage([{ image: 'x', imageId: DIGEST.slice(7) }], task), true);
  assert.equal(ranTaskImage([{ image: 'docker.io/library/debian:12', imageId: 'e'.repeat(64) }], task), false);
  assert.equal(ranTaskImage([], task), false);
});

async function setup(t, { script = [{ state: 'working' }, { state: 'completed' }], holdStream = false } = {}) {
  const home = tmp(t);
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  const calls = [];
  const containers = new FakeContainers();
  containers.listing = [{ image: `${IMAGE}@${DIGEST}`, imageId: 'f'.repeat(64), labels: {}, names: ['devc'] }];
  const state = { runs: ['0191a2b3-7c4d'], slots: { '0191a2b3-7c4d': [SLUG] } };
  const server = await fakeCoder({ script, holdStream, bearer: BEARER });
  t.after(() => server.close());
  const stack = {
    agentDir: null,
    ports: { git: 18083 },
    resetDatabase: async (slot) => calls.push(['resetDatabase', slot]),
    pushBase: async (slot, script) => calls.push(['pushBase', slot, script]),
    startCoder: async (slot, env) => calls.push(['startCoder', slot, env]),
    stopCoder: async (slot) => calls.push(['stopCoder', slot]),
    deleteRepo: async (name) => calls.push(['deleteRepo', name]),
    coderUrl: () => server.url,
    listDir: async (slot, dir) => (dir === '/work/workspaces' ? state.runs : state.slots[dir.split('/').at(-1)] ?? []),
    extractTree: async (slot, dir, dest) => {
      calls.push(['extractTree', slot, dir]);
      const src = tmp(t);
      fs.writeFileSync(path.join(src, 'README.md'), 'demo\n');
      fs.writeFileSync(path.join(src, 'hello.txt'), 'hello\n');
      extractEntries(parseTar(execFileSync('tar', ['--format=posix', '-cf', '-', '-C', src, '.'])), dest);
    },
  };
  const logs = [];
  const clock = fakeClock();
  const folders = [];
  const agent = new AdamAgent({
    pins: PINS,
    home,
    repoDir: '/repo',
    contextWindow: 131072,
    heads: async ({ url }) => {
      calls.push(['heads', url]);
      return [{ ref: 'agent/0191a2b3', sha: 'b'.repeat(40) }];
    },
    newBearer: () => BEARER,
    ensureFolder: async (o) => {
      folders.push(o);
      return o.dest;
    },
    now: clock.now,
    sleep: clock.sleep,
  });
  const ctx = {
    run: 'r1',
    attempt: 1,
    slot: 1,
    task,
    base: { dir: '/host/base', sha: 'a'.repeat(40), slug: SLUG, workdir: '/app', containerRepo: '/work/bench-base/base.git' },
    stack,
    containers,
    meter: { token: TOKEN, baseUrl: METER, refusal: () => null },
    paths: { run: tmp(t), scratch: path.join(tmp(t), 'scratch') },
    limits: LIMITS,
    contextWindow: 131072,
    log: (e) => logs.push(e),
  };
  return { agent, ctx, server, calls, logs, containers, state, clock, task, home, folders };
}

test('prepare: the folder with the configured turn limit, a clean database, the base pushed to this run\'s own repository by a container, then the coder with this run\'s settings, in that order', async (t) => {
  const { agent, ctx, calls, task, folders } = await setup(t);
  await agent.prepare(ctx);
  assert.deepEqual(calls.map((c) => c[0]), ['resetDatabase', 'pushBase', 'startCoder']);
  assert.equal(folders[0].steps, 150, 'the turn limit is the configured step limit, the one OpenCode gets');
  assert.equal(folders[0].revision, PINS.coder.revision);
  const repo = runRepoName(SLUG, 'r1', 1);
  assert.match(calls[1][2], new RegExp(`push -q --force http://git-server:8080/bench/${repo}\\.git FETCH_HEAD:refs/heads/main`));
  assert.match(calls[1][2], /fetch -q \/work\/bench-base\/base\.git refs\/heads\/main/);
  assert.deepEqual(calls[2][2], { MODEL_BASE_URL: METER, A2A_BEARER_TOKENS: BEARER, DEVCONTAINER_DEFAULT_IMAGE: task.image.ref, MODEL_CONTEXT_WINDOW: '131072' });
  assert.match(ctx.stack.agentDir, /agent-folder\/2d5d0b3926cb-t150$/);
});

test('finding 4: two runs of one task push to two repositories, and the second never sees the first\'s branches', async (t) => {
  const a = await setup(t);
  const b = await setup(t);
  b.ctx.run = 'r2';
  await a.agent.prepare(a.ctx);
  await b.agent.prepare(b.ctx);
  const repoOf = (calls) => /bench\/([a-z0-9-]+)\.git FETCH_HEAD/.exec(calls.find((c) => c[0] === 'pushBase')[2])[1];
  assert.notEqual(repoOf(a.calls), repoOf(b.calls));
  // and a retry of the same run is another repository too
  const c = await setup(t);
  c.ctx.attempt = 2;
  await c.agent.prepare(c.ctx);
  assert.notEqual(repoOf(c.calls), repoOf(a.calls));
});

test('prepare: each run has its own random bearer', async (t) => {
  const { ctx, home } = await setup(t);
  const seen = new Set();
  for (let i = 0; i < 3; i++) {
    const agent = new AdamAgent({ pins: PINS, home, repoDir: '/r', contextWindow: 1024, ensureFolder: async (o) => o.dest });
    const got = [];
    ctx.stack.startCoder = async (slot, env) => got.push(env.A2A_BEARER_TOKENS);
    await agent.prepare(ctx);
    seen.add(got[0]);
  }
  assert.equal(seen.size, 3);
  for (const b of seen) assert.match(b, /^[A-Za-z0-9_-]{32}$/);
});

test('run: the driver is called with the configured time limit and cancel grace; polled to its end; the bearer is in no log line and no saved file (finding 14)', async (t) => {
  const { agent, ctx, server, logs } = await setup(t, { script: [{ state: 'working' }, { state: 'completed', totals: [{ inputTokens: 100, outputTokens: 20 }] }] });
  await agent.prepare(ctx);
  const drive = agent.drive.bind(agent);
  let seen;
  agent.drive = (o) => ((seen = o), drive(o));
  const r = await agent.run(ctx);
  assert.equal(seen.limitMs, LIMITS.limitMs);
  assert.equal(seen.graceMs, LIMITS.cancelGraceMs);
  assert.equal(r.exit, 'done');
  assert.equal(r.taskId, TASK_ID);
  assert.deepEqual(r.selfReported, { input: 100, output: 20, cached: 0, calls: 1 });
  const sent = server.of('SendStreamingMessage')[0].params.message;
  assert.equal(sent.parts[0].text, `In http://git-server:8080/bench/${runRepoName(SLUG, 'r1', 1)}.git (base branch main), do this task:\n\nMake the thing work.\n`);
  assert.ok(!JSON.stringify(logs).includes(BEARER));
  const kept = fs.readFileSync(path.join(ctx.paths.run, 'adam', 'task.json'), 'utf8');
  assert.ok(!kept.includes(BEARER) && !kept.includes(TOKEN));
});

test('run: a bearer the coder itself echoed in its task is taken out of what is kept', async (t) => {
  const { agent, ctx } = await setup(t);
  await agent.prepare(ctx);
  agent.drive = async () => ({ exit: 'failed', taskId: 't', finalState: 'failed', nudges: 0, polls: 1, elapsedMs: 5, lastTask: { status: { message: { parts: [{ text: `auth failed for Bearer ${BEARER} at ${TOKEN}` }] } } }, selfReported: null });
  await agent.run(ctx);
  const kept = fs.readFileSync(path.join(ctx.paths.run, 'adam', 'task.json'), 'utf8');
  assert.ok(!kept.includes(BEARER) && !kept.includes(TOKEN));
  assert.match(kept, /\[redacted\]/);
});

test('run: the time of the run is the time to the decision, not the cancel grace (finding 19)', async (t) => {
  const { agent, ctx, clock } = await setup(t, { script: [{ state: 'working' }] });
  ctx.limits = { ...LIMITS, limitMs: 3 * 60_000, cancelGraceMs: 30_000 };
  await agent.prepare(ctx);
  const r = await agent.run(ctx);
  assert.equal(r.exit, 'timeout');
  assert.ok(r.wallMs >= 3 * 60_000 && r.wallMs < 3 * 60_000 + 15_000, `wall ${r.wallMs}`);
  assert.ok(clock.elapsed() >= r.wallMs);
});

test('run: a task that parks twice is nudged once and cancelled; a refusal of the meter ends it budget_hit', async (t) => {
  const parks = await setup(t, { script: [{ state: 'input-required', ts: 'T1' }, { state: 'working' }, { state: 'input-required', ts: 'T2' }] });
  await parks.agent.prepare(parks.ctx);
  const r1 = await parks.agent.run(parks.ctx);
  assert.deepEqual([r1.exit, r1.nudges], ['input_required', 1]);
  assert.equal(parks.server.of('CancelTask').length, 1);

  const hit = await setup(t, { script: [{ state: 'working' }] });
  await hit.agent.prepare(hit.ctx);
  let polls = 0;
  hit.ctx.meter.refusal = () => (++polls > 1 ? 'budget' : null);
  assert.equal((await hit.agent.run(hit.ctx)).exit, 'budget_hit');
});

test('finding 8: a run in which no container of the task image ran is infrastructure, with the reason', async (t) => {
  const { agent, ctx, containers } = await setup(t);
  await agent.prepare(ctx);
  containers.listing = [{ image: 'docker.io/library/debian:12', imageId: 'e'.repeat(64), labels: {}, names: [] }];
  await assert.rejects(agent.run(ctx), (e) => e instanceof InfraError && e.reason === 'no-task-container');
  containers.listing = [];
  await assert.rejects(agent.run(ctx), (e) => e.reason === 'no-task-container');
});

test('finding 11: stop removes the coder, then reaps what it started, and a failure is infrastructure', async (t) => {
  const { agent, ctx, calls, containers } = await setup(t);
  await agent.prepare(ctx);
  await agent.stop(ctx);
  assert.deepEqual(calls.at(-1), ['stopCoder', 1]);
  assert.equal(containers.reaped, 1);
  ctx.stack.stopCoder = async () => {
    throw new Error('the coder of slot 1 is not stopped');
  };
  await assert.rejects(agent.stop(ctx), (e) => e instanceof InfraError && e.reason === 'not-stopped');
});

function realBase(t) {
  const app = initRepo(t);
  commitFiles(app, { 'README.md': 'demo\n' }, 'base');
  return { dir: app, sha: git(app, 'rev-parse', 'HEAD').trim(), slug: SLUG, workdir: '/app', containerRepo: '/work/bench-base/base.git' };
}

test('collect: the worktree comes out as a tar stream into a directory the harness owns and becomes the patch; the cross-check lists this run\'s own repository', async (t) => {
  const { agent, ctx, calls } = await setup(t);
  ctx.base = realBase(t);
  await agent.prepare(ctx);
  const r = await agent.collect(ctx);
  assert.deepEqual(calls.find((c) => c[0] === 'extractTree').slice(1), [1, `/work/workspaces/0191a2b3-7c4d/${SLUG}`]);
  assert.match(r.patch.toString('utf8'), /\+\+\+ b\/hello\.txt/);
  assert.deepEqual(r.notes.worktree, { run: '0191a2b3-7c4d', slot: SLUG });
  assert.deepEqual(r.notes.branch, { branch: 'agent/0191a2b3', sha: 'b'.repeat(40), consistent: true, pushed: 1 });
  assert.equal(calls.find((c) => c[0] === 'heads')[1], `http://127.0.0.1:18083/bench/${runRepoName(SLUG, 'r1', 1)}.git`);
  assert.equal(fs.existsSync(path.join(ctx.paths.scratch, 'tree')), false, 'the tree is removed once the patch is made');
});

test('collect: a run that made no workspace has an empty patch, and the notes say why; two runs on the volume is an error', async (t) => {
  const none = await setup(t);
  none.state.runs.length = 0;
  await none.agent.prepare(none.ctx);
  const r = await none.agent.collect(none.ctx);
  assert.equal(r.patch.length, 0);
  assert.equal(r.notes.worktree, null);
  const many = await setup(t);
  many.state.runs.push('another');
  await many.agent.prepare(many.ctx);
  await assert.rejects(many.agent.collect(many.ctx), /was not emptied/);
});

test('collect: a branch list that fails does not fail the collection', async (t) => {
  const { agent, ctx } = await setup(t);
  ctx.base = realBase(t);
  await agent.prepare(ctx);
  agent.heads = async () => {
    throw new Error('network down');
  };
  assert.deepEqual((await agent.collect(ctx)).notes.branch, { error: 'the branches could not be listed' });
});

test('finding 4 and 9: cleanup deletes the run\'s repository, stops the coder, reaps the service and removes the scratch, even after a failure', async (t) => {
  const { agent, ctx, calls, containers } = await setup(t);
  await agent.prepare(ctx);
  fs.writeFileSync(path.join(ctx.paths.scratch, 'x'), 'x');
  await agent.cleanup(ctx);
  assert.deepEqual(calls.filter((c) => c[0] === 'deleteRepo'), [['deleteRepo', runRepoName(SLUG, 'r1', 1)]]);
  assert.ok(calls.some((c) => c[0] === 'stopCoder'));
  assert.equal(containers.reaped, 1);
  assert.equal(fs.existsSync(ctx.paths.scratch), false);

  const failing = await setup(t);
  await failing.agent.prepare(failing.ctx);
  failing.ctx.stack.stopCoder = async () => {
    throw new Error('compose failed');
  };
  await assert.rejects(failing.agent.cleanup(failing.ctx), /compose failed/);
  assert.ok(failing.calls.some((c) => c[0] === 'deleteRepo'), 'the repository is deleted even when the coder could not be stopped');
  assert.equal(fs.existsSync(failing.ctx.paths.scratch), false);
});

test('a client made for the coder is not printable', () => {
  const c = new A2AClient({ url: 'http://127.0.0.1:1/', bearer: BEARER });
  assert.ok(!JSON.stringify(c).includes(BEARER));
  assert.ok(!String(c).includes(BEARER));
});

test('M-adam-external-ignored: a failed run whose last message says the model endpoint failed is an external failure; one that says the tests fail, or that finished, is not', async (t) => {
  const { agent, ctx } = await setup(t);
  await agent.prepare(ctx);
  const say = (exit, text) => async () => ({ exit, taskId: 't', finalState: exit, nudges: 0, polls: 1, elapsedMs: 5, lastTask: { status: { message: { parts: [{ text }] } } }, selfReported: null });
  agent.drive = say('failed', 'the model endpoint is unavailable (HTTP 503 upstream)');
  assert.equal((await agent.run(ctx)).externalFailure, true);
  agent.drive = say('failed', 'the tests still fail');
  assert.equal((await agent.run(ctx)).externalFailure, false);
  agent.drive = say('done', 'the model endpoint is unavailable (HTTP 503 upstream)');
  assert.equal((await agent.run(ctx)).externalFailure, false);
});
