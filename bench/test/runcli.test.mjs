import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { CALLS_PER_RUN } from '../lib/agent-script.mjs';
import { PINS } from '../lib/pins.mjs';
import { runCli } from '../lib/run-cli.mjs';
import { rawConfig, tmp } from './helpers.mjs';
import { PER_CALL, fixture } from './runnerfix.mjs';

const NAMES = ['hello-1', 'hello-2', 'hello-3', 'hello-4', 'fix-add-1', 'fix-add-2'];
const LOCKS = ['{"public":"lock"}', ''];
const ADAM = CALLS_PER_RUN.adam * PER_CALL; // 29_520 micro-dollars a run
const OPENCODE = CALLS_PER_RUN.opencode * PER_CALL; // 14_760

/** A harness on the fixture's fakes, and the files and environment the command line reads. */
async function setup(t, opts = {}) {
  const f = await fixture(t, { capUSD: 20, taskNames: NAMES, ...opts });
  const dir = tmp(t);
  const configFile = path.join(dir, 'config.json');
  const home = f.home; // the fixture's ledger is the one the command line reads
  fs.writeFileSync(configFile, JSON.stringify(f.raw));
  const env = { PATH: process.env.PATH, HOME: dir, BENCH_HOME: home, BENCH_CONFIG: configFile };
  const opened = [];
  const make = async () => {
    opened.push(1);
    return { deps: f.deps(), tasks: Object.values(f.tasks), slots: 2, close: async () => {} };
  };
  const lines = [];
  const errs = [];
  const call = (command, argv = [], over = {}) => runCli(command, argv, { env, make, pins: PINS, lockTexts: LOCKS, out: (l) => lines.push(l), err: (l) => errs.push(l), ...over });
  const records = () => f.ledger.records;
  return { f, env, home, configFile, opened, lines, errs, call, records, proposal: () => JSON.parse(fs.readFileSync(path.join(home, 'pilot-budget.json'), 'utf8')) };
}

const worstOf = (records, arm) => {
  const runs = new Set(records.filter((r) => r.kind === 'state' && r.state === 'Planned' && r.arm === arm && r.phase === 'pilot').map((r) => r.run));
  return Math.max(...records.filter((r) => r.kind === 'call' && r.phase === 'pending' && runs.has(r.run)).map((r) => r.worstMicro));
};

test('pilot runs the first five blocks as pilot runs and writes the proposal, with the numbers worked out by hand', async (t) => {
  const s = await setup(t);
  assert.equal(await s.call('pilot'), 0, s.errs.join('\n'));
  const folded = s.f.ledger.fold();
  assert.equal(folded.size, 20, 'five blocks of four cells');
  assert.ok([...folded.values()].every((v) => v.state === 'Done' && v.phase === 'pilot' && v.run.startsWith('pilot.')));
  const p = s.proposal();
  assert.equal(p.confirmed, false);
  assert.equal(p.pilotBlocks, 5);
  assert.equal(ADAM, 29_520);
  assert.equal(OPENCODE, 14_760);
  // each arm has ten pilot runs: five of adam (29_520) and five of opencode (14_760); nearest-rank p95 of ten is the largest
  for (const arm of ['max', 'off']) {
    const w = worstOf(s.records(), arm);
    assert.equal(p.perArm[arm].maxWorstMicro, w);
    assert.equal(p.perArm[arm].p95Micro, ADAM);
    assert.equal(p.perArm[arm].meanMicro, 22_140);
    assert.equal(p.perArmMicro[arm], Math.max(Math.ceil(ADAM * 1.5) + w, 2 * w), 'the larger of 1.5 p95 plus a worst case, and two worst cases');
  }
  // spent 10 * 29_520 + 10 * 14_760 = 442_800; the six blocks run again: 12 runs per arm
  assert.equal(p.projection.spentMicro, 442_800);
  assert.equal(p.projection.runsPerArm, 12);
  assert.equal(p.projection.expectedMicro, 442_800 + 2 * 12 * 22_140);
  assert.equal(p.projection.planningMicro, 442_800 + Math.ceil(2 * 12 * 22_140 * 1.2));
  assert.equal(p.projection.fitsCap, true);
  assert.match(s.lines.join('\n'), /bench confirm/);
  const setups = s.records().filter((r) => r.kind === 'setup');
  assert.equal(setups.length, 1);
  assert.equal(setups[0].digest, p.digest);
  assert.ok(s.records().filter((r) => r.state === 'Planned').every((r) => r.digest === p.digest));
});

test('run refuses to start without a confirmed pilot budget, and opens nothing', async (t) => {
  const s = await setup(t);
  assert.equal(await s.call('run'), 1);
  assert.match(s.errs.join('\n'), /no pilot budget/);
  assert.equal(s.opened.length, 0, 'the stack was not started');
  await s.call('pilot');
  const opened = s.opened.length;
  s.errs.length = 0;
  assert.equal(await s.call('run'), 1);
  assert.match(s.errs.join('\n'), /not confirmed/);
  assert.equal(s.opened.length, opened);
  assert.equal(s.f.ledger.fold().size, 20, 'no run was added');
});

test('R3: the full run runs every block again under the confirmed budget, and the pilot is reported apart and not in the rates', async (t) => {
  const s = await setup(t);
  assert.equal(await s.call('confirm'), 1);
  assert.match(s.errs.join('\n'), /run `bench pilot` first/);
  await s.call('pilot');
  assert.equal(await s.call('confirm'), 0);
  assert.equal(s.proposal().confirmed, true);
  assert.equal(await s.call('run'), 0, s.errs.join('\n'));
  const folded = s.f.ledger.fold();
  assert.equal(folded.size, 44, '20 pilot runs and 24 of the full run');
  assert.ok([...folded.values()].every((v) => v.state === 'Done'));
  const full = [...folded.values()].filter((v) => v.phase === 'run');
  assert.equal(full.length, 24);
  assert.ok(full.every((v) => !v.run.startsWith('pilot.')));
  // the full run reserved the confirmed budget of its arm, the pilot the configuration's
  const reserved = s.records().filter((r) => r.kind === 'state' && r.state === 'Reserved');
  assert.deepEqual([...new Set(reserved.slice(0, 20).map((r) => r.reservedMicro))], [3_000_000]);
  const { perArmMicro } = s.proposal();
  const byRun = new Map(s.records().filter((r) => r.state === 'Planned').map((r) => [r.run, r.arm]));
  for (const r of reserved.slice(20)) assert.equal(r.reservedMicro, perArmMicro[byRun.get(r.run)]);
  const report = fs.readFileSync(path.join(s.home, 'report.md'), 'utf8');
  assert.match(report, /Example Model/);
  assert.ok(!report.includes('example-model'), 'the model id is not shown');
  assert.match(report, /## Pilot/);
  assert.match(report, /\| adam \| max \| 6 \| 6 \| 100\.0% \|/, 'six runs of the full run, not the pilot\'s five on top');
  assert.match(s.lines.at(-1), /report\.md$/);
});

test('R1: a ledger holding runs of another configuration is refused by pilot, confirm, run and regrade, and nothing is opened', async (t) => {
  const s = await setup(t);
  await s.call('pilot');
  await s.call('confirm');
  const opened = s.opened.length;
  const changed = { ...JSON.parse(fs.readFileSync(s.configFile, 'utf8')), modelId: 'another-example-model' };
  fs.writeFileSync(s.configFile, JSON.stringify(changed));
  for (const command of ['pilot', 'confirm', 'run', 'regrade']) {
    s.errs.length = 0;
    assert.equal(await s.call(command), 1, command);
    assert.match(s.errs.join('\n'), /another configuration \(changed: model\).*new BENCH_HOME/, command);
  }
  assert.equal(s.opened.length, opened, 'nothing was started');
  assert.equal(s.f.ledger.fold().size, 20);
});

test('R1: the seed and the self repository are part of what the ledger is bound to', async (t) => {
  const s = await setup(t);
  await s.call('pilot');
  const raw = JSON.parse(fs.readFileSync(s.configFile, 'utf8'));
  fs.writeFileSync(s.configFile, JSON.stringify({ ...raw, tasks: { seed: '99' } }));
  assert.equal(await s.call('run'), 1);
  assert.match(s.errs.join('\n'), /changed: seed/);
});

test('R1: a changed lock or pin with the proposal confirmed refuses the run, and a new pilot needs a new yes', async (t) => {
  const s = await setup(t);
  await s.call('pilot');
  await s.call('confirm');
  assert.equal(await s.call('run', [], { lockTexts: ['{"public":"another"}', ''], make: async () => assert.fail('opened') }), 1);
  assert.match(s.errs.join('\n'), /changed since the pilot \(locks\)|another configuration \(changed: locks\)/);
});

test('R11: a pilot of fewer blocks than an estimate needs is refused before anything starts', async (t) => {
  const s = await setup(t);
  assert.equal(await s.call('pilot', ['--blocks', '1']), 1);
  assert.match(s.errs.join('\n'), /from 2 to 100/);
  assert.equal(s.opened.length, 0);
  assert.equal(await s.call('pilot', ['--blocks', '2']), 0, s.errs.join('\n'));
});

test('R15: a cap that cannot hold one block is refused before the pilot starts', async (t) => {
  const s = await setup(t, { capUSD: 10, perRunBudgetUSD: 3 }); // a block reserves 4 x 3 = 12 USD
  assert.equal(await s.call('pilot'), 1);
  assert.match(s.errs.join('\n'), /cannot hold one block of 4 runs/);
  assert.equal(s.opened.length, 0);
  assert.equal(s.f.ledger.fold().size, 0);
});

test('R15: a pilot whose runs could cost more than the cap says so before it starts', async (t) => {
  const s = await setup(t, { capUSD: 13, perRunBudgetUSD: 3 });
  assert.equal(await s.call('pilot', ['--blocks', '2']), 0, s.errs.join('\n'));
  assert.match(s.lines.join('\n'), /warning: if every pilot run used its whole reservation it would cost \$24\.0000, more than the cap \$13\.0000/);
});

test('a projection above the cap is not confirmed without --over-cap, and the cap is checked again at run', async (t) => {
  const s = await setup(t, { capUSD: 0.75, perRunBudgetUSD: 0.15 });
  assert.equal(await s.call('pilot', ['--blocks', '2']), 0, s.errs.join('\n'));
  assert.equal(s.proposal().projection.fitsCap, false);
  assert.match(s.lines.join('\n'), /does NOT fit/);
  assert.equal(await s.call('confirm'), 1);
  assert.match(s.errs.join('\n'), /--over-cap/);
  assert.equal(await s.call('confirm', ['--over-cap']), 0);
  assert.equal(await s.call('run'), 0, s.errs.join('\n'));
  const spent = [...s.f.ledger.fold().values()].reduce((a, v) => a + v.spentMicro, 0);
  assert.ok(spent <= 750_000, `spent ${spent}`);
});

test('R2: pilot runs that hit their budget make a censored estimate that has to be accepted with --censored', async (t) => {
  // 0.13 USD: a call's worst case is about 0.123, so the third call of a run is refused
  const s = await setup(t, { capUSD: 2, perRunBudgetUSD: 0.13 });
  assert.equal(await s.call('pilot', ['--blocks', '2']), 0, s.errs.join('\n'));
  assert.ok(s.proposal().censoredArms.length > 0);
  assert.match(s.lines.join('\n'), /--censored/);
  assert.equal(await s.call('confirm'), 1);
  assert.match(s.errs.join('\n'), /--censored/);
  assert.equal(await s.call('confirm', ['--censored']), 0);
});

test('regrade grades again the runs whose grading failed in the infrastructure, and only those', async (t) => {
  const s = await setup(t);
  s.f.grader.broken = true;
  assert.equal(await s.call('pilot', ['--blocks', '2']), 1, 'no estimate from runs that are not graded');
  const invalid = [...s.f.ledger.fold().values()].filter((v) => v.result?.invalid);
  assert.equal(invalid.length, 8);
  s.f.grader.broken = false;
  assert.equal(await s.call('regrade'), 0, s.errs.join('\n'));
  const folded = s.f.ledger.fold();
  assert.ok([...folded.values()].every((v) => v.result.invalid === false));
  assert.ok([...folded.values()].every((v) => v.attempt === 1));
  assert.match(s.lines.join('\n'), /regrade: 0 run\(s\) left in Grading, 8 run\(s\) whose grading failed/);
});

test('options that do not exist are usage errors', async (t) => {
  const s = await setup(t);
  assert.equal(await s.call('run', ['--nope']), 64);
});
