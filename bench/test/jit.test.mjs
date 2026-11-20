// Just-in-time task images: pulled by digest before a task's first run, removed after its last (ADR 0048, amended 2026-11-20).
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { normalizeConfig } from '../lib/config.mjs';
import { FakeContainers, HostContainers } from '../lib/containers.mjs';
import { doctor } from '../lib/doctor.mjs';
import { buildBlocks, specsOf } from '../lib/plan.mjs';
import { PINS, coderRef } from '../lib/pins.mjs';
import { realHarness } from '../lib/run-cli.mjs';
import { runPlan } from '../lib/runner.mjs';
import { runSchedule } from '../lib/schedule.mjs';
import { rawConfig, tmp } from './helpers.mjs';
import { fixture } from './runnerfix.mjs';

const digest = (c) => `sha256:${c.repeat(64)}`;
const ref = (c) => `registry.example.test/task@${digest(c)}`;
const withRef = (task, r) => ({ ...task, image: { ...task.image, ref: r, digest: r.split('@')[1] } });

/** Specs of the given tasks (a block each, two agents, one arm) and a deps whose pulls, releases and runs are one ordered log. */
async function scheduled(t, refs, { slots = 2, releaseImage } = {}) {
  const names = refs.map((_, i) => `hello-${i + 1}`);
  const f = await fixture(t, { capUSD: 100, taskNames: names, agentOver: { delayMs: 10 } });
  const events = [];
  for (const agent of Object.values(f.agents)) {
    const run = agent.run.bind(agent);
    agent.run = async (ctx) => {
      events.push(`start ${ctx.task.image.ref}`);
      try {
        return await run(ctx);
      } finally {
        events.push(`end ${ctx.task.image.ref}`);
      }
    };
  }
  const tasks = names.map((n, i) => withRef(f.tasks[n], refs[i]));
  const specs = specsOf(buildBlocks({ tasks, agents: ['adam', 'opencode'], arms: ['max'], seed: '1' }));
  const deps = {
    ...f.deps(),
    pullImage: async (slot, task) => void events.push(`pull ${task.image.ref}`),
    releaseImage: releaseImage ?? (async (task) => void events.push(`release ${task.image.ref}`)),
  };
  return { f, events, specs, deps, slots };
}

test('a task image is released once, after the last run of the task in the block, and no run of it starts later', async (t) => {
  const s = await scheduled(t, [ref('a'), ref('b'), ref('c')]);
  const res = await runSchedule({ deps: s.deps, specs: s.specs, slots: s.slots });
  assert.ok(res.outcomes.every((o) => o.state === 'Done'));
  for (const r of [ref('a'), ref('b'), ref('c')]) {
    const at = s.events.indexOf(`release ${r}`);
    assert.ok(at > 0, `${r} is released`);
    assert.equal(s.events.filter((e) => e === `release ${r}`).length, 1, 'once');
    assert.equal(s.events.filter((e) => e === `end ${r}`).length, 2, 'both agents ran');
    assert.ok(s.events.filter((e) => e === `end ${r}`).every((e) => s.events.lastIndexOf(e) < at), 'after the last run of it ends');
    assert.equal(s.events.slice(at).includes(`start ${r}`), false, 'no run of it starts after');
    assert.ok(s.events.indexOf(`pull ${r}`) < s.events.indexOf(`start ${r}`), 'pulled before its first run');
  }
});

test('an image that another task of the plan shares is not released while a run of either is open', async (t) => {
  const shared = ref('a');
  const s = await scheduled(t, [shared, shared, ref('c')]);
  await runSchedule({ deps: s.deps, specs: s.specs, slots: s.slots });
  const releases = s.events.filter((e) => e === `release ${shared}`);
  assert.equal(releases.length, 1, 'one release for two tasks');
  const at = s.events.indexOf(`release ${shared}`);
  assert.equal(s.events.filter((e) => e === `end ${shared}`).length, 4);
  assert.ok(s.events.slice(0, at).filter((e) => e === `end ${shared}`).length === 4, 'all four runs ended first');
  assert.equal(s.events.slice(at).includes(`start ${shared}`), false);
});

test('a release that throws is harmless: the runs are Done and the schedule goes on', async (t) => {
  const s = await scheduled(t, [ref('a'), ref('b')], {
    releaseImage: async () => {
      throw new Error('image is in use');
    },
  });
  const res = await runSchedule({ deps: s.deps, specs: s.specs, slots: s.slots });
  assert.equal(res.outcomes.length, 4);
  assert.ok(res.outcomes.every((o) => o.state === 'Done'));
});

test('a plan with no releaseImage (a dry run, the old behaviour) runs as before', async (t) => {
  const s = await scheduled(t, [ref('a')]);
  delete s.deps.releaseImage;
  const res = await runSchedule({ deps: s.deps, specs: s.specs, slots: s.slots });
  assert.ok(res.outcomes.every((o) => o.state === 'Done'));
});

test('a run that fails in the infrastructure still counts as ended: the image is released', async (t) => {
  const s = await scheduled(t, [ref('a')]);
  s.f.agents.adam.fail = { prepare: () => true };
  s.f.agents.opencode.fail = { prepare: () => true };
  await runSchedule({ deps: s.deps, specs: s.specs, slots: s.slots });
  assert.ok(s.events.includes(`release ${ref('a')}`));
});

// ---- the real harness's pull and release ----

function realParts(t, { tasks, keepImages, workspaceImage } = {}) {
  const home = tmp(t);
  const notices = [];
  const slotContainers = new FakeContainers();
  const graderContainers = new FakeContainers();
  const stack = {
    up: async () => {},
    down: async () => {},
    pullWindow: async (slot, fn) => fn(),
    graderWindow: async (fn) => fn(),
    containers: () => slotContainers,
    graderContainers: () => graderContainers,
  };
  const meter = { listen: async () => ({}), close: async () => {} };
  const config = normalizeConfig(rawConfig({ concurrency: 2, ...(workspaceImage ? { tasks: { workspaceImage } } : {}) }));
  const inject = { load: () => ({ tasks }), makeStack: () => stack, makeMeter: () => meter };
  const env = { BENCH_HOME: home, HOME: home };
  const open = () => realHarness({ env, config, log: () => {}, notice: (l) => notices.push(l), keepImages, sharedPrompt: 'shared', inject });
  return { open, notices, slotContainers, graderContainers };
}
const pub = (r, id = 'demo') => ({ id, source: 'public', image: { ref: r, digest: r.split('@')[1] } });

test('release removes the image by digest and forgets that it was pulled, so the next block pulls it again', async (t) => {
  const task = pub(ref('d'));
  const p = realParts(t, { tasks: [task] });
  const h = await p.open();
  t.after(() => h.close());
  await h.deps.pullImage(1, task);
  await h.deps.pullGraderImage(task);
  assert.deepEqual(p.slotContainers.pulled, [task.image.ref], 'pulled by digest, once');
  await h.deps.releaseImage(task);
  assert.deepEqual(p.slotContainers.removedImages, [task.image.ref], 'removed by the digest reference');
  await h.deps.pullImage(1, task);
  assert.deepEqual(p.slotContainers.pulled, [task.image.ref, task.image.ref], 'pulled again: the earlier pull is forgotten');
});

test('release never removes the coder image, the workspace image or an own task image', async (t) => {
  const workspace = `registry.example.test/workspace@${digest('e')}`;
  const tasks = [pub(`${PINS.coder.image}@${PINS.coder.digest}`, 'coder-like'), pub(workspace, 'ws-like'), { id: 'own-1', source: 'own', image: { ref: 'adam-bench-own/own-1:abc' } }];
  const p = realParts(t, { tasks, workspaceImage: workspace });
  p.slotContainers.images = new Set(tasks.map((x) => x.image.ref));
  const h = await p.open();
  t.after(() => h.close());
  for (const task of tasks) await h.deps.releaseImage(task);
  assert.deepEqual(p.slotContainers.removedImages ?? [], []);
});

test('--keep-images: nothing is removed', async (t) => {
  const task = pub(ref('d'));
  const p = realParts(t, { tasks: [task], keepImages: true });
  const h = await p.open();
  t.after(() => h.close());
  await h.deps.pullImage(1, task);
  await h.deps.releaseImage(task);
  assert.deepEqual(p.slotContainers.removedImages ?? [], []);
});

test('a removal that fails is told and is harmless', async (t) => {
  const task = pub(ref('d'));
  const p = realParts(t, { tasks: [task] });
  p.slotContainers.removeImage = async () => {
    throw new Error('image is in use');
  };
  const h = await p.open();
  t.after(() => h.close());
  await h.deps.pullImage(1, task);
  await h.deps.releaseImage(task);
  assert.match(p.notices.join('\n'), /demo: image not removed \(image is in use\)/);
});

test('a pull that fails is infrastructure for that run: Failed_infra with the reason image_pull, never a fail', async (t) => {
  const task = pub(ref('d'));
  const p = realParts(t, { tasks: [task] });
  p.slotContainers.pull = async () => {
    throw new Error('pulling failed (exit 1): net/http: TLS handshake timeout');
  };
  const h = await p.open();
  t.after(() => h.close());
  await assert.rejects(h.deps.pullImage(1, task), (e) => e.reason === 'image_pull' && /TLS handshake/.test(e.message));
  const f = await fixture(t, { capUSD: 100, taskNames: ['hello-1'] });
  const spec = { run: 'hello-1.adam.max', task: f.tasks['hello-1'], agent: 'adam', arm: 'max', slot: 1 };
  const deps = { ...f.deps(), pullImage: (slot) => h.deps.pullImage(slot, task) };
  const [outcome] = await runPlan({ deps, plan: [spec] });
  assert.equal(outcome.state, 'Failed_infra');
  const reason = f.ledger.records.filter((r) => r.run === spec.run && r.state === 'Failed_infra').map((r) => r.reason);
  assert.deepEqual(reason, ['image_pull']);
});

test('a pull goes through dockerEnv: no key and no proxy reach `docker pull` or `docker image rm`', async () => {
  const seen = [];
  const exec = (bin, args, opts, cb) => {
    seen.push({ args, env: opts.env });
    cb(null, '', '');
    return { stdin: { on() {}, end() {} } };
  };
  const saved = { ...process.env };
  process.env.BENCH_MODEL_API_KEY = 'k-not-a-secret';
  process.env.HTTPS_PROXY = 'http://proxy.example.test:3128';
  try {
    const c = new HostContainers({ exec });
    await c.pull(ref('d'));
    await c.removeImage(ref('d'));
  } finally {
    for (const k of ['BENCH_MODEL_API_KEY', 'HTTPS_PROXY']) if (saved[k] === undefined) delete process.env[k];
  }
  assert.equal(seen.length, 2);
  for (const { env } of seen) {
    assert.equal(env.BENCH_MODEL_API_KEY, undefined);
    assert.equal(env.HTTPS_PROXY, undefined);
  }
});

// ---- the doctor's floor ----

const GB = 1024 ** 3;
function machine({ freeGb, sizes = {} }) {
  const exec = async (file, args) => {
    const key = `${file} ${args.join(' ')}`;
    if (key.startsWith('docker compose version')) return { exitCode: 0, stdout: '2.40.0\n', stderr: '' };
    if (key.startsWith('docker info')) return { exitCode: 0, stdout: JSON.stringify({ ServerVersion: '29', SecurityOptions: [], CgroupVersion: '2', DockerRootDir: '/var/lib/docker' }), stderr: '' };
    if (key.includes('{{.Size}}')) {
      const target = args.at(-1);
      const gb = sizes[target];
      return gb ? { exitCode: 0, stdout: `${Math.floor(gb * GB)}\n`, stderr: '' } : { exitCode: 1, stdout: '', stderr: 'No such image' };
    }
    if (key.startsWith('docker image inspect')) return { exitCode: 0, stdout: JSON.stringify([`${PINS.coder.image}@${PINS.coder.digest}`]), stderr: '' };
    return { exitCode: 127, stdout: '', stderr: '' };
  };
  return { exec, statfs: () => ({ bavail: Math.floor((freeGb * GB) / 4096), bsize: 4096 }) };
}
const doc = (m, extra = {}) => doctor({ home: '/home/x/bench', pins: PINS, nodeVersion: '24.1.0', arch: 'x64', ...m, ...extra });
const disk = (r) => r.checks.filter((c) => c.name.startsWith('free disk')).map((c) => c.status);
const TASKS = [pub(ref('a'), 'one'), pub(ref('b'), 'two')];

test('doctor, just in time: the floor is the coder plus the largest task image plus a margin, not every kept image', async () => {
  const coder = coderRef(PINS);
  const sizes = { [coder]: 6, [ref('a')]: 3, [ref('b')]: 5 };
  // coder 6 + largest task 5 + margin 10 = 21; the coder's own 6 GB counts as present, the task images do not
  assert.deepEqual(disk(await doc(machine({ freeGb: 14, sizes }), { tasks: TASKS, justInTime: true })), ['fail', 'fail'], '14 + 6 = 20 < 21');
  assert.equal((await doc(machine({ freeGb: 15, sizes }), { tasks: TASKS, justInTime: true })).exitCode, 0, '15 + 6 = 21');
  // the whole selection at once (--keep-images): the old rule counts the task images that are present too
  assert.equal((await doc(machine({ freeGb: 8, sizes }), { tasks: TASKS, justInTime: false })).exitCode, 0, '8 + 6 + 3 + 5 = 22 >= 20');
  assert.equal((await doc(machine({ freeGb: 8, sizes }), { tasks: TASKS, justInTime: true })).exitCode, 1);
});

test('doctor, just in time: a task image that is not here is assumed 3.5 GB and the coder 12 GB, and the warning leaves room for 30 GB more', async () => {
  // nothing measured except the coder (present, size unknown to the fake): 12 + 3.5 + 10 = 25.5
  const none = machine({ freeGb: 25, sizes: {} });
  assert.deepEqual(disk(await doc(none, { tasks: TASKS, justInTime: true })), ['fail', 'fail']);
  assert.deepEqual(disk(await doc(machine({ freeGb: 26, sizes: {} }), { tasks: TASKS, justInTime: true })), ['warn', 'warn']);
  assert.deepEqual(disk(await doc(machine({ freeGb: 60, sizes: {} }), { tasks: TASKS, justInTime: true })), ['ok', 'ok']);
  const r = await doc(machine({ freeGb: 25, sizes: {} }), { tasks: TASKS, justInTime: true });
  assert.match(r.checks.find((c) => c.name.startsWith('free disk, Docker')).detail, /just in time/);
});

test('doctor: the 20 GB floor still holds when the sum would be smaller', async () => {
  const coder = coderRef(PINS);
  const r = await doc(machine({ freeGb: 5, sizes: { [coder]: 4, [ref('a')]: 1 } }), { tasks: TASKS, justInTime: true });
  assert.equal(r.exitCode, 1, '4 + 1 + 10 = 15, floored at 20: 5 + 4 = 9 fails');
});
