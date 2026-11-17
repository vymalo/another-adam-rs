import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { PINS } from '../lib/pins.mjs';
import { runCli } from '../lib/run-cli.mjs';
import { rawConfig, tmp } from './helpers.mjs';
import { PER_CALL, fixture } from './runnerfix.mjs';
import { CALLS_PER_RUN } from '../lib/agent-script.mjs';

const NAMES = ['hello-1', 'hello-2', 'hello-3', 'hello-4', 'fix-add-1', 'fix-add-2'];
const LOCKS = ['{"public":"lock"}', ''];
const ADAM = CALLS_PER_RUN.adam * PER_CALL; // 29_520 micro-dollars a run
const OPENCODE = CALLS_PER_RUN.opencode * PER_CALL; // 14_760

/** A harness on the fixture's fakes, and the files and environment the command line reads. */
async function setup(t, opts = {}) {
  const f = await fixture(t, { capUSD: 10, taskNames: NAMES, ...opts });
  const dir = tmp(t);
  const configFile = path.join(dir, 'config.json');
  const home = path.join(dir, 'home');
  fs.writeFileSync(configFile, JSON.stringify(f.raw));
  const env = { PATH: process.env.PATH, HOME: dir, BENCH_HOME: home, BENCH_CONFIG: configFile };
  const opened = [];
  const make = async () => {
    opened.push(1);
    const deps = f.deps();
    return { deps, tasks: Object.values(f.tasks), slots: 2, close: async () => {} };
  };
  const lines = [];
  const errs = [];
  const call = (command, argv = []) => runCli(command, argv, { env, make, pins: PINS, lockTexts: LOCKS, out: (l) => lines.push(l), err: (l) => errs.push(l) });
  return { f, env, home, configFile, opened, lines, errs, call, proposal: () => JSON.parse(fs.readFileSync(path.join(home, 'pilot-budget.json'), 'utf8')) };
}

test('pilot runs the first five blocks and writes the proposal, with the numbers worked out by hand', async (t) => {
  const s = await setup(t);
  assert.equal(await s.call('pilot'), 0, s.errs.join('\n'));
  const folded = s.f.ledger.fold();
  assert.equal(folded.size, 20, 'five blocks of four cells');
  assert.ok([...folded.values()].every((v) => v.state === 'Done'));
  const p = s.proposal();
  assert.equal(p.confirmed, false);
  assert.equal(p.pilotBlocks, 5);
  // each arm has ten pilot runs: five of adam (29_520) and five of opencode (14_760); the nearest-rank p95 of ten is the largest
  assert.equal(ADAM, 29_520);
  assert.equal(OPENCODE, 14_760);
  assert.deepEqual(p.perArmMicro, { max: 44_280, off: 44_280 }); // ceil(29_520 * 1.5)
  assert.equal(p.perArm.max.meanMicro, 22_140);
  // spent 10 * 29_520 + 10 * 14_760 = 442_800; left per arm 12 - 10 = 2, so 4 runs
  assert.equal(p.projection.spentMicro, 442_800);
  assert.equal(p.projection.reservedIfAllMicro, 442_800 + 4 * 44_280);
  assert.equal(p.projection.expectedMicro, 442_800 + 4 * 22_140);
  assert.equal(p.projection.fitsCap, true);
  const text = s.lines.join('\n');
  assert.match(text, /proposed budget per run \$0\.0443/);
  assert.match(text, /bench confirm/);
  assert.ok(f2(s).setups === 1, 'one setup record');
});

const f2 = (s) => ({ setups: s.f.ledger.records.filter((r) => r.kind === 'setup').length });

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

test('confirm with no pilot says so; confirm then lets run start, and the full run does not repeat the pilot', async (t) => {
  const s = await setup(t);
  assert.equal(await s.call('confirm'), 1);
  assert.match(s.errs.join('\n'), /run `bench pilot` first/);
  await s.call('pilot');
  assert.equal(await s.call('confirm'), 0);
  assert.equal(s.proposal().confirmed, true);
  const runsBefore = [...s.f.agents.adam.log, ...s.f.agents.opencode.log].filter((l) => l[0] === 'run').length;
  assert.equal(runsBefore, 20);
  assert.equal(await s.call('run'), 0, s.errs.join('\n'));
  const folded = s.f.ledger.fold();
  assert.equal(folded.size, 24);
  assert.ok([...folded.values()].every((v) => v.state === 'Done'));
  const runsAfter = [...s.f.agents.adam.log, ...s.f.agents.opencode.log].filter((l) => l[0] === 'run');
  assert.equal(runsAfter.length, 24, 'only the four runs that were left');
  assert.ok(runsAfter.every((l) => l[2] === 1), 'no run had a second attempt');
  // the four new runs reserved the confirmed budget, not the pre-pilot one
  const reserved = s.f.ledger.records.filter((r) => r.kind === 'state' && r.state === 'Reserved').map((r) => r.reservedMicro);
  assert.deepEqual([...new Set(reserved.slice(0, 20))], [3_000_000]);
  assert.deepEqual([...new Set(reserved.slice(20))], [44_280]);
  // the report is written from the ledger
  const report = fs.readFileSync(path.join(s.home, 'report.md'), 'utf8');
  assert.match(report, /Example Model/);
  assert.ok(!report.includes('example-model'), 'the model id is not shown');
  assert.match(s.lines.at(-1), /report\.md$/);
});

test('a pilot again removes the earlier confirmation; a changed configuration or lock refuses the run', async (t) => {
  const s = await setup(t);
  await s.call('pilot');
  await s.call('confirm');
  const changed = { ...JSON.parse(fs.readFileSync(s.configFile, 'utf8')), arms: { max: { reasoning_effort: 'high' }, off: { reasoning_effort: 'low' } } };
  fs.writeFileSync(s.configFile, JSON.stringify(changed));
  assert.equal(await s.call('run'), 1);
  assert.match(s.errs.join('\n'), /changed since the pilot/);
  fs.writeFileSync(s.configFile, JSON.stringify(s.f.raw));
  assert.equal(await runCli('run', [], { env: s.env, make: async () => assert.fail('opened'), pins: PINS, lockTexts: ['{"public":"another"}', ''], out: () => {}, err: (l) => s.errs.push(l) }), 1);
  assert.match(s.errs.join('\n'), /task locks changed/);
  await s.call('pilot');
  assert.equal(s.proposal().confirmed, false, 'a new pilot needs a new yes');
});

test('a projection above the cap is not confirmed without --over-cap', async (t) => {
  const s = await setup(t, { capUSD: 0.7, perRunBudgetUSD: 0.2 });
  assert.equal(await s.call('pilot', ['--blocks', '3']), 0, s.errs.join('\n'));
  // 12 pilot runs spent 6 * 29_520 + 6 * 14_760 = 265_680; 6 runs of each arm are left at 44_280: 265_680 + 12 * 44_280 = 797_040 > 700_000
  assert.equal(s.proposal().projection.reservedIfAllMicro, 797_040);
  assert.equal(s.proposal().projection.fitsCap, false);
  assert.match(s.lines.join('\n'), /do NOT all fit/);
  assert.equal(await s.call('confirm'), 1);
  assert.match(s.errs.join('\n'), /--over-cap/);
  assert.equal(await s.call('confirm', ['--over-cap']), 0);
  assert.equal(await s.call('run'), 0, s.errs.join('\n'));
  const spent = [...s.f.ledger.fold().values()].reduce((a, v) => a + v.spentMicro, 0);
  assert.ok(spent <= 700_000, `spent ${spent}`);
  assert.match(s.lines.join('\n'), /ended: Done 24/);
});

test('regrade grades again the runs whose grading failed in the infrastructure, and only those', async (t) => {
  const s = await setup(t);
  s.f.grader.broken = true;
  await s.call('pilot', ['--blocks', '3']).catch(() => {});
  const invalid = [...s.f.ledger.fold().values()].filter((v) => v.result?.invalid);
  assert.ok(invalid.length > 0);
  s.f.grader.broken = false;
  assert.equal(await s.call('regrade'), 0, s.errs.join('\n'));
  const folded = s.f.ledger.fold();
  assert.ok([...folded.values()].every((v) => v.result.invalid === false));
  assert.ok([...folded.values()].every((v) => v.attempt === 1));
  assert.match(s.lines.join('\n'), /regrade: 12 run/);
});

test('options that do not exist are usage errors', async (t) => {
  const s = await setup(t);
  assert.equal(await s.call('run', ['--nope']), 64);
  assert.equal(await s.call('pilot', ['--blocks', '0']).catch(() => 'threw'), 1);
});
