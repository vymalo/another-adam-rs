// Tests of the fix round of slice 4 (the review's findings R1 to R19 and the smoke test's S1 to S6 that are not
// in a file of their own). Each is named by its finding.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { buildBlocks, specsOf } from '../lib/plan.mjs';
import { HarnessCrash, executeRun, regradeInvalid, resumeLedger, runPlan } from '../lib/runner.mjs';
import { runSchedule } from '../lib/schedule.mjs';
import { fetchUpstream } from '../lib/upstream.mjs';
import { findCheckCommand } from '../lib/lint.mjs';
import { doctor } from '../lib/doctor.mjs';
import { PINS } from '../lib/pins.mjs';
import { FakeContainers } from '../lib/containers.mjs';
import { Ledger } from '../lib/ledger.mjs';
import { realHarness } from '../lib/run-cli.mjs';
import { normalizeConfig } from '../lib/config.mjs';
import { rawConfig } from './helpers.mjs';
import { fixture, spec } from './runnerfix.mjs';
import { commitFiles, initRepo } from './gitfix.mjs';
import { tmp } from './helpers.mjs';

test('R6: the host git of the upstream clone never sees the model key or any other variable outside the allow-list', (t) => {
  const key = 'k-for-tests-not-a-secret-0001';
  const bin = tmp(t);
  const capture = path.join(bin, 'env.txt');
  const realGit = execFileSync('sh', ['-c', 'command -v git'], { encoding: 'utf8' }).trim();
  fs.writeFileSync(path.join(bin, 'git'), `#!/bin/sh\nenv >> ${capture}\nexec ${realGit} "$@"\n`, { mode: 0o755 });
  const work = initRepo(t);
  const sum = crypto.createHash('sha256').update('x').digest('hex');
  const commit = commitFiles(work, { 'v2/one/task.toml': 'x', 'v2/SHA256SUMS': `${sum}  one/task.toml\n` }, 'tasks');
  const bare = path.join(tmp(t), 'u.git');
  execFileSync(realGit, ['clone', '-q', '--bare', work, bare]);
  const saved = { ...process.env };
  process.env.PATH = `${bin}:${process.env.PATH}`;
  process.env.BENCH_MODEL_API_KEY = key;
  process.env.HTTPS_PROXY = 'http://user:pw@proxy.example.test:3128';
  process.env.GIT_CONFIG_PARAMETERS = "'core.fsmonitor=evil'";
  t.after(() => {
    for (const k of Object.keys(process.env)) delete process.env[k];
    Object.assign(process.env, saved);
  });
  fetchUpstream({ url: bare, commit, dir: path.join(tmp(t), 'clone') });
  const seen = fs.readFileSync(capture, 'utf8');
  assert.ok(seen.includes('PATH='), 'the wrapper ran');
  assert.ok(!seen.includes(key), 'the key never reached git');
  assert.ok(!/HTTPS_PROXY|BENCH_MODEL_API_KEY|GIT_CONFIG_PARAMETERS/.test(seen));
  assert.match(seen, /GIT_CONFIG_NOSYSTEM=1/);
  assert.match(seen, /GIT_CONFIG_GLOBAL=\/dev\/null/);
  assert.match(seen, /GIT_TERMINAL_PROMPT=0/);
});

const NAMES = ['hello-1', 'hello-2', 'hello-3', 'hello-4'];

test('R1: every Planned record carries the digest of the configuration, and the pilot phase', async (t) => {
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES });
  const deps = { ...f.deps(), digest: 'abc123' };
  const specs = specsOf(buildBlocks({ tasks: Object.values(f.tasks).slice(0, 1), agents: ['adam'], arms: ['max'], seed: '1' }), { pilot: true });
  await runPlan({ deps, plan: specs.map((s) => ({ ...s, slot: 1 })) });
  const planned = f.ledger.records.filter((r) => r.kind === 'state' && r.state === 'Planned');
  assert.equal(planned.length, 1);
  assert.equal(planned[0].digest, 'abc123');
  assert.equal(planned[0].phase, 'pilot');
  assert.ok(planned[0].run.startsWith('pilot.'));
});

test('R9: a Done run whose saved patch is gone stays invalid, and one whose patch changed too; a good patch is graded again', async (t) => {
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES });
  const specs = specsOf(buildBlocks({ tasks: Object.values(f.tasks).slice(0, 1), agents: ['adam'], arms: ['max'], seed: '1' }));
  f.grader.broken = true;
  await runSchedule({ deps: f.deps(), specs, slots: 1 });
  const v = f.ledger.fold().get(specs[0].run);
  assert.equal(v.result.invalid, true);
  assert.ok(v.result.patchBytes > 0, 'the size of the graded patch is recorded');
  const patch = v.result.patch;
  f.grader.broken = false;
  const kept = fs.readFileSync(patch);
  fs.rmSync(patch);
  const out = await regradeInvalid({ deps: f.deps(), spec: specs[0] });
  assert.equal(out.skipped, true);
  assert.match(out.reason, /missing/);
  assert.equal(f.ledger.fold().get(specs[0].run).result.invalid, true, 'never a valid fail made of a missing patch');
  fs.writeFileSync(patch, `${kept}\n# tampered`);
  assert.equal((await regradeInvalid({ deps: f.deps(), spec: specs[0] })).skipped, true);
  fs.writeFileSync(patch, kept);
  const ok = await regradeInvalid({ deps: f.deps(), spec: specs[0] });
  assert.equal(ok.state, 'Done');
  assert.equal(f.ledger.fold().get(specs[0].run).result.invalid, false);
});

test('R10: a grading makes the grader\'s task image present first, in a run and in a regrade', async (t) => {
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES });
  const specs = specsOf(buildBlocks({ tasks: Object.values(f.tasks).slice(0, 1), agents: ['adam'], arms: ['max'], seed: '1' }));
  f.grader.broken = true;
  await runSchedule({ deps: f.deps(), specs, slots: 1 });
  assert.deepEqual(f.gradePulls, [specs[0].task.id]);
  f.grader.broken = false;
  await regradeInvalid({ deps: f.deps(), spec: specs[0] });
  assert.equal(f.gradePulls.length, 2, 'the regrade pulled it again (the grader may be new)');
});

test('R7: a grading with no network goes through the shared side of the grader, one that needs it through the window', async (t) => {
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES, verifier: 'public' });
  const specs = specsOf(buildBlocks({ tasks: Object.values(f.tasks).slice(0, 1), agents: ['adam'], arms: ['max'], seed: '1' }));
  await runSchedule({ deps: f.deps(), specs, slots: 1 });
  assert.deepEqual(f.windows, ['grader']);
  assert.deepEqual(f.shared, []);
  const g = await fixture(t, { capUSD: 100, taskNames: NAMES, verifier: 'none' });
  await runSchedule({ deps: g.deps(), specs: specsOf(buildBlocks({ tasks: Object.values(g.tasks).slice(0, 1), agents: ['adam'], arms: ['max'], seed: '1' })), slots: 1 });
  assert.deepEqual(g.windows, []);
  assert.deepEqual(g.shared, ['grader', 'grader', 'grader'].slice(0, g.shared.length));
  assert.ok(g.shared.length >= 1);
});

test('R10: a run left in Grading by a failed regrade is graded by the next regrade, with no agent run', async (t) => {
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES });
  const specs = specsOf(buildBlocks({ tasks: Object.values(f.tasks).slice(0, 1), agents: ['adam'], arms: ['max'], seed: '1' }));
  f.hooks.afterState = (run, state) => {
    if (state === 'Grading') throw new HarnessCrash('Grading');
  };
  await assert.rejects(runSchedule({ deps: f.deps(), specs, slots: 1 }), HarnessCrash);
  assert.equal(f.ledger.state(specs[0].run), 'Grading');
  const deps = await f.reopen();
  delete deps.afterState;
  const { regradeStranded } = await import('../lib/run-cli.mjs');
  const done = await regradeStranded({ deps, specs });
  assert.deepEqual(done.map((o) => o.state), ['Done']);
  assert.equal(f.agents.adam.log.filter((l) => l[0] === 'run').length, 0, 'the agent of the new start never ran');
});

test('S2: a run left Running by a kill does not keep its reservation: the next start frees it before anything starts', async (t) => {
  const f = await fixture(t, { capUSD: 5, perRunBudgetUSD: 3, taskNames: NAMES, agentOver: { crashAfter: { 'hello-1.adam.max': 2 } } });
  const specs = specsOf(buildBlocks({ tasks: Object.values(f.tasks).slice(0, 1), agents: ['adam'], arms: ['max'], seed: '1' }));
  await assert.rejects(runPlan({ deps: f.deps(), plan: [{ ...specs[0], slot: 1 }] }), HarnessCrash);
  assert.equal(f.ledger.state(specs[0].run), 'Running');
  const deps = await f.reopen();
  assert.equal(deps.book.runView(specs[0].run).active, true, 'rebuilt from the ledger the run still holds 3 USD of a 5 USD cap');
  assert.equal(deps.book.canReserve(3_000_000), false);
  await resumeLedger({ deps, slots: [1] });
  assert.equal(deps.book.runView(specs[0].run).active, false, 'freed');
  assert.equal(deps.book.canReserve(3_000_000), true, 'only what the killed attempt spent is left against the cap');
  const spent = deps.book.spentMicro();
  assert.ok(spent > 0 && spent < 1_000_000);
});

const files = (map) => (p) => (Object.hasOwn(map, p) ? map[p] : null);

test('S4: a repository with a test script, a manifest, a target or a configuration has a check command; one with none does not', () => {
  assert.equal(findCheckCommand(files({ 'package.json': '{"scripts":{"test":"vitest"}}' })), 'package.json scripts.test');
  assert.equal(findCheckCommand(files({ 'Cargo.toml': '[package]' })), 'Cargo.toml (cargo test)');
  assert.equal(findCheckCommand(files({ 'go.mod': 'module x' })), 'go.mod (go test)');
  assert.equal(findCheckCommand(files({ Makefile: 'build:\n\tcc\ntest:\n\t./t\n' })), 'Makefile test target');
  assert.equal(findCheckCommand(files({ 'pyproject.toml': '[tool.pytest.ini_options]\n' })), 'Python test configuration');
  assert.match(findCheckCommand(files({ 'README.md': 'Run `cargo nextest run` before you push.' })), /cargo nextest run/);
  assert.equal(findCheckCommand(files({ 'package.json': '{"scripts":{"build":"tsc"}}' })), null);
  assert.equal(findCheckCommand(files({ 'package.json': 'not json', 'README.md': 'A tool.' })), null);
  assert.equal(findCheckCommand(files({})), null);
});

test('S5: doctor warns when HTTPS_PROXY is set and the process does not use it', async () => {
  const exec = async () => ({ exitCode: 127, stdout: '', stderr: '' });
  const run = (env) => doctor({ home: '/h', pins: PINS, nodeVersion: '24.1.0', arch: 'x64', exec, readFile: () => { throw new Error('x'); }, statfs: () => ({ bavail: 1e9, bsize: 4096 }), env });
  const warn = (await run({ HTTPS_PROXY: 'http://proxy.example.test:3128' })).checks.find((c) => c.name === 'proxy');
  assert.equal(warn.status, 'warn');
  assert.match(warn.fix, /NODE_USE_ENV_PROXY=1/);
  assert.equal((await run({ https_proxy: 'http://proxy.example.test:3128' })).checks.find((c) => c.name === 'proxy').status, 'warn');
  assert.equal((await run({ HTTPS_PROXY: 'http://proxy.example.test:3128', NODE_USE_ENV_PROXY: '1' })).checks.find((c) => c.name === 'proxy').status, 'ok');
  assert.equal((await run({})).checks.find((c) => c.name === 'proxy'), undefined, 'no proxy, nothing to say');
});

test('S6: CI runs the tests on Node 22 and 24, and nothing needs more than 22', () => {
  const here = path.dirname(new URL(import.meta.url).pathname);
  const yml = fs.readFileSync(path.join(here, '..', '..', '.github', 'workflows', 'bench.yml'), 'utf8');
  assert.match(yml, /node-version: \[22, 24\]/);
  assert.match(yml, /\$\{\{ matrix\.node-version \}\}/);
  const pkg = JSON.parse(fs.readFileSync(path.join(here, '..', 'package.json'), 'utf8'));
  assert.equal(pkg.engines.node, '>=22');
  assert.match(fs.readFileSync(path.join(here, '..', 'README.md'), 'utf8'), /Node 22 or later/);
});

test('R19: the same seed and locks give the same plan after a restart, so a resume matches runs by id to the same blocks', async (t) => {
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES });
  const plan = () => specsOf(buildBlocks({ tasks: [...Object.values(f.tasks)].reverse(), agents: ['adam', 'opencode'], arms: ['max', 'off'], seed: '7' })).map((s) => [s.run, s.block]);
  assert.deepEqual(plan(), plan());
  assert.notDeepEqual(plan().map((x) => x[0]), specsOf(buildBlocks({ tasks: Object.values(f.tasks), agents: ['adam', 'opencode'], arms: ['max', 'off'], seed: '8' })).map((s) => s.run));
});

test('R19: a pilot killed after it removed the old proposal and before it wrote the new one leaves no proposal, so run refuses', async (t) => {
  const { runCli } = await import('../lib/run-cli.mjs');
  const { rawConfig } = await import('./helpers.mjs');
  const f = await fixture(t, { capUSD: 20, taskNames: NAMES, agentOver: { crashAfter: Object.fromEntries(NAMES.flatMap((n) => ['adam', 'opencode'].flatMap((a) => ['max', 'off'].map((arm) => [`pilot.${n}.${a}.${arm}`, 1])))) } });
  const dir = tmp(t);
  const configFile = path.join(dir, 'config.json');
  fs.writeFileSync(configFile, JSON.stringify(f.raw));
  const env = { PATH: process.env.PATH, HOME: dir, BENCH_HOME: f.home, BENCH_CONFIG: configFile };
  fs.writeFileSync(path.join(f.home, 'pilot-budget.json'), JSON.stringify({ schema: 2, confirmed: true }));
  const make = async () => ({ deps: f.deps(), tasks: Object.values(f.tasks), slots: 1, close: async () => {} });
  const lockTexts = ['{"public":1}', ''];
  const errs = [];
  await assert.rejects(runCli('pilot', ['--blocks', '2'], { env, make, lockTexts, out: () => {}, err: (l) => errs.push(l) }));
  assert.equal(fs.existsSync(path.join(f.home, 'pilot-budget.json')), false, 'the old yes is gone');
  const second = await runCli('run', [], { env, make: async () => assert.fail('opened'), lockTexts, out: () => {}, err: (l) => errs.push(l) });
  assert.equal(second, 1);
  assert.match(errs.join('\n'), /another configuration|no pilot budget/);
  assert.ok(rawConfig);
});

/** What realHarness needs, with every outside thing a fake that records what was done to it. */
function realParts(t, { tasks, upFails = false, origin = new FakeContainers() } = {}) {
  const home = tmp(t);
  const log = [];
  const slotContainers = new FakeContainers();
  const graderContainers = new FakeContainers();
  const stack = {
    up: async () => {
      if (upFails) throw new Error('docker compose up failed');
    },
    down: async () => log.push('stack down'),
    pullWindow: async (slot, fn) => fn(),
    graderWindow: async (fn) => fn(),
    containers: () => slotContainers,
    graderContainers: () => graderContainers,
    loadImage: async (service) => log.push(`load ${service}`),
  };
  const meter = { listen: async () => ({}), close: async () => log.push('meter closed') };
  const config = normalizeConfig(rawConfig({ concurrency: 2 }));
  const inject = { load: () => ({ tasks }), makeStack: () => stack, makeMeter: () => meter, makeOrigin: () => origin };
  const env = { BENCH_HOME: home, HOME: home };
  return { env, config, inject, log, home, slotContainers, graderContainers };
}

test('R14: a failure after the meter listens closes the meter, brings the stack down and releases the ledger', async (t) => {
  const p = realParts(t, { tasks: [], upFails: true });
  await assert.rejects(realHarness({ env: p.env, config: p.config, log: () => {}, inject: p.inject }), /compose up failed/);
  assert.deepEqual(p.log, ['meter closed', 'stack down']);
  Ledger.open(path.join(p.home, 'ledger.jsonl')).close(); // the lock was released
});

test('R14: every own image is put on every slot and on the grader before anything is spent, and a missing one stops the start', async (t) => {
  const own = { id: 'demo-2', source: 'own', image: { ref: 'adam-bench-own/demo-2:abc' } };
  const origin = new FakeContainers();
  origin.images = new Set([own.image.ref]);
  const p = realParts(t, { tasks: [own], origin });
  const h = await realHarness({ env: p.env, config: p.config, log: () => {}, inject: p.inject });
  assert.deepEqual(p.log.filter((l) => l.startsWith('load')).sort(), ['load grader', 'load podman-1', 'load podman-2']);
  assert.equal(origin.saved.length, 1, 'saved once, however many services load it');
  await h.close();
  const q = realParts(t, { tasks: [own], origin: new FakeContainers() });
  await assert.rejects(realHarness({ env: q.env, config: q.config, log: () => {}, inject: q.inject }), /on no service/);
  assert.ok(q.log.includes('meter closed') && q.log.includes('stack down'));
});
