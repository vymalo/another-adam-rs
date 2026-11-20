import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { CALLS_PER_RUN } from '../lib/agent-script.mjs';
import { LedgerError, resumeActions } from '../lib/ledger.mjs';
import { buildBlocks, specsOf } from '../lib/plan.mjs';
import { HarnessCrash, regradeInvalid, runDirName, runPlan } from '../lib/runner.mjs';
import { PermanentError } from '../lib/errors.mjs';
import { runSchedule } from '../lib/schedule.mjs';
import { PER_CALL, fixture, newTracker } from './runnerfix.mjs';

const NAMES = ['hello-1', 'hello-2', 'hello-3', 'hello-4', 'fix-add-1', 'fix-add-2'];
const plan = (f, n = NAMES.length) => specsOf(buildBlocks({ tasks: Object.values(f.tasks).slice(0, n), agents: ['adam', 'opencode'], arms: ['max', 'off'], seed: '1' }));
const USD = 1_000_000;

/** The most the cap has had to cover at any record of the ledger. */
function watchCommitted(f, book) {
  const seen = { max: 0 };
  f.hooks.afterState = () => {
    seen.max = Math.max(seen.max, book().committedMicro());
  };
  return seen;
}

test('two slots run two runs at once, never more, never two in one slot, and every run is graded', async (t) => {
  const tracker = newTracker();
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES, agentOver: { delayMs: 15, tracker } });
  const specs = plan(f);
  const res = await runSchedule({ deps: f.deps(), specs, slots: 2 });
  assert.equal(specs.length, 24);
  assert.equal(res.outcomes.length, 24);
  assert.ok(res.outcomes.every((o) => o.state === 'Done'));
  assert.equal(tracker.max, 2, 'two ran at once');
  assert.equal(res.maxActive, 2);
  assert.equal(tracker.collisions, 0, 'no slot held two runs');
  assert.equal(f.ledger.fold().get(specs[0].run).result.grades.length, 3);
});

test('one slot runs one at a time', async (t) => {
  const tracker = newTracker();
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES, agentOver: { delayMs: 5, tracker } });
  await runSchedule({ deps: f.deps(), specs: plan(f, 2), slots: 1 });
  assert.equal(tracker.max, 1);
});

test('the runs start in the order of the blocks', async (t) => {
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES, agentOver: { delayMs: 5 } });
  const specs = plan(f, 3);
  await runSchedule({ deps: f.deps(), specs, slots: 2 });
  const planned = f.ledger.records.filter((r) => r.kind === 'state' && r.state === 'Planned').map((r) => r.run);
  assert.deepEqual(planned, specs.map((s) => s.run));
});

test('R4: two slots racing for the last budget: the first block runs whole, the second is not started, not even planned, and the cap is never crossed', async (t) => {
  // a block reserves 4 runs of 3 USD; 12.05 USD holds one block and no part of a second
  const f = await fixture(t, { capUSD: 12.05, perRunBudgetUSD: 3, taskNames: NAMES, agentOver: { delayMs: 10 } });
  const deps = f.deps();
  const seen = watchCommitted(f, () => deps.book);
  const specs = plan(f, 2);
  const res = await runSchedule({ deps, specs, slots: 2 });
  const states = specs.map((s) => f.ledger.fold().get(s.run)?.state);
  assert.deepEqual(states, ['Done', 'Done', 'Done', 'Done', undefined, undefined, undefined, undefined]);
  assert.equal(res.capHit, true);
  assert.ok(seen.max <= 12.05 * USD, `the cap held ${seen.max}`);
  const spent = [...f.ledger.fold().values()].reduce((s, v) => s + v.spentMicro, 0);
  assert.ok(spent <= 12.05 * USD);
  assert.ok(specs.slice(4).every((s) => !f.agents.adam.log.concat(f.agents.opencode.log).some((l) => l[0] === 'prepare' && l[1] === s.run)), 'no run of the second block prepared');
});

test('R4: a block that does not fit even alone is not started, and nothing is written for it', async (t) => {
  const f = await fixture(t, { capUSD: 11.9, perRunBudgetUSD: 3, taskNames: NAMES });
  const res = await runSchedule({ deps: f.deps(), specs: plan(f, 2), slots: 2 });
  assert.equal(res.capHit, true);
  assert.equal(f.ledger.fold().size, 0);
});

test('R4: a block waits for the runs of the earlier block to end, then starts; the cap is never crossed', async (t) => {
  const tracker = newTracker();
  const f = await fixture(t, { capUSD: 14, perRunBudgetUSD: 3, taskNames: NAMES, agentOver: { delayMs: 10, tracker } });
  const deps = f.deps();
  const seen = watchCommitted(f, () => deps.book);
  const specs = plan(f, 2);
  const res = await runSchedule({ deps, specs, slots: 3 });
  assert.ok(res.outcomes.every((o) => o.state === 'Done'));
  assert.equal(res.capHit, false);
  assert.ok(seen.max <= 14 * USD, `the cap held ${seen.max}`);
  const recs = f.ledger.records.filter((r) => r.kind === 'state');
  const lastDoneOfFirst = Math.max(...specs.slice(0, 4).map((s) => recs.findIndex((r) => r.run === s.run && r.state === 'Done')));
  const firstPlannedOfSecond = Math.min(...specs.slice(4).map((s) => recs.findIndex((r) => r.run === s.run && r.state === 'Planned')));
  assert.ok(firstPlannedOfSecond > lastDoneOfFirst, 'the second block started after the first ended: 14 USD cannot hold a block and a part of another');
  assert.ok(tracker.max <= 3);
});

test('R5: a run the cap kept from starting is planned again by a later start with room', async (t) => {
  const f = await fixture(t, { capUSD: 3, perRunBudgetUSD: 3, taskNames: NAMES });
  const specs = plan(f, 1);
  const first = await runPlan({ deps: f.deps(), plan: specs.slice(0, 2).map((s) => ({ ...s, slot: 1 })) });
  assert.deepEqual(first.map((o) => o.state), ['Done', 'NotStarted']);
  const refused = specs[1].run;
  const deps = await f.reopen();
  deps.book.capMicro = 20 * USD; // the owner raised the cap
  const res = await runSchedule({ deps, specs, slots: 2 });
  assert.ok(res.outcomes.some((o) => o.run === refused && o.state === 'Done'));
  const folded = f.ledger.fold();
  assert.ok(specs.every((s) => folded.get(s.run).state === 'Done'));
  assert.equal(folded.get(refused).attempt, 2);
  assert.equal(folded.get(specs[0].run).attempt, 1, 'a Done run is not run again');
});

test('whatever the interleaving, the cap is never crossed at any record, with prices that make a run spend a lot', async (t) => {
  const prices = { inputPerMTok: 3, cachedInputPerMTok: 0.3, cacheWritePerMTok: 3.75, outputPerMTok: 4000 };
  const f = await fixture(t, { capUSD: 140, perRunBudgetUSD: 34, prices, taskNames: NAMES, agentOver: { delayMs: 3 } });
  const deps = f.deps();
  const seen = watchCommitted(f, () => deps.book);
  const res = await runSchedule({ deps, specs: plan(f, 3), slots: 2 });
  assert.ok(seen.max <= 140 * USD, `the cap held ${seen.max}`);
  const spent = [...f.ledger.fold().values()].reduce((s, v) => s + v.spentMicro, 0);
  assert.ok(spent <= 140 * USD, `spent ${spent}`);
  assert.equal(res.capHit, true);
  assert.ok(spent > 0, 'the first block ran');
});

test('killed with two runs in flight: the next start runs those two again and no other, and never a Done run', async (t) => {
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES, agentOver: { delayMs: 5 } });
  const specs = plan(f, 3);
  let dead = false;
  // the kill: after the third settled call of any run the process is gone; what is in the other slot stops at its next record
  const wrap = (agent) => {
    const run = agent.run.bind(agent);
    agent.run = async (ctx) => {
      if (dead) throw new HarnessCrash('dead');
      return run(ctx);
    };
  };
  f.agents.adam.crashAfter = {};
  wrap(f.agents.adam);
  wrap(f.agents.opencode);
  const victim = specs[5].run;
  f.agents.adam.crashAfter[victim] = 2;
  f.agents.opencode.crashAfter[victim] = 1;
  f.hooks.afterState = () => {
    if (dead) throw new HarnessCrash('dead');
  };
  const original = f.agents.adam.run;
  f.agents.adam.run = async (ctx) => {
    try {
      return await original(ctx);
    } catch (e) {
      if (e.name === 'HarnessCrash') dead = true;
      throw e;
    }
  };
  const originalOc = f.agents.opencode.run;
  f.agents.opencode.run = async (ctx) => {
    try {
      return await originalOc(ctx);
    } catch (e) {
      if (e.name === 'HarnessCrash') dead = true;
      throw e;
    }
  };
  await assert.rejects(runSchedule({ deps: f.deps(), specs, slots: 2 }), HarnessCrash);
  const before = f.ledger.fold();
  const done = specs.filter((s) => before.get(s.run)?.state === 'Done').map((s) => s.run);
  const inFlight = specs.filter((s) => ['Reserved', 'Preparing', 'Running', 'Collecting'].includes(before.get(s.run)?.state)).map((s) => s.run);
  const untouched = specs.filter((s) => before.get(s.run) === undefined).map((s) => s.run);
  assert.ok(done.length >= 1, 'something finished before the kill');
  assert.ok(inFlight.length >= 1, 'something was in flight');
  assert.ok(untouched.length >= 1, 'something had not started');

  const deps = await f.reopen();
  delete deps.afterState;
  dead = false;
  f.agents.adam.crashAfter = {};
  f.agents.opencode.crashAfter = {};
  await runSchedule({ deps, specs, slots: 2 });
  const ran = [...f.agents.adam.log, ...f.agents.opencode.log].filter((l) => l[0] === 'run').map((l) => `${l[1]}#${l[2]}`);
  const second = ran.filter((r) => r.endsWith('#2'));
  assert.deepEqual(second.sort(), inFlight.map((r) => `${r}#2`).sort(), 'only what was in flight was run again');
  for (const run of done) assert.ok(!ran.some((r) => r === `${run}#2`), `${run} was not repeated`);
  const folded = f.ledger.fold();
  assert.ok(specs.every((s) => folded.get(s.run).state === 'Done'));
  for (const run of inFlight) assert.equal(folded.get(run).interruptions, 1);
  for (const run of untouched) assert.equal(folded.get(run).attempt, 1);
});

test('an environment that fails once is tried again by the same start, and no one run is run twice', async (t) => {
  let failing = true;
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES, agentOver: { fail: { prepare: (run) => failing && run === 'hello-1.adam.max' } } });
  const specs = plan(f, 1);
  const res = await runSchedule({
    deps: f.deps(),
    specs,
    slots: 2,
    onOutcome: (o) => {
      if (o.state === 'Failed_infra') failing = false;
    },
  });
  assert.equal(res.passes, 2);
  const v = f.ledger.fold().get('hello-1.adam.max');
  assert.equal(v.state, 'Done');
  assert.equal(v.infraFailures, 1);
  assert.equal(v.attempt, 2);
});

test('a finished schedule started again does nothing', async (t) => {
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES });
  const specs = plan(f, 2);
  await runSchedule({ deps: f.deps(), specs, slots: 2 });
  const calls = f.model.requests.length;
  const again = await runSchedule({ deps: await f.reopen(), specs, slots: 2 });
  assert.equal(again.outcomes.length, 0);
  assert.equal(f.model.requests.length, calls);
});

test('the cost of a run is still the meter\'s: eight runs of two agents add up to the scripted price', async (t) => {
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES });
  await runSchedule({ deps: f.deps(), specs: plan(f, 2), slots: 2 });
  const spent = [...f.ledger.fold().values()].reduce((s, v) => s + v.spentMicro, 0);
  assert.equal(spent, 4 * CALLS_PER_RUN.adam * PER_CALL + 4 * CALLS_PER_RUN.opencode * PER_CALL);
});

test('regrade: a Done run whose grading failed in the infrastructure is graded again from its saved patch, and the agent is not run', async (t) => {
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES });
  const specs = plan(f, 1);
  f.grader.broken = true;
  await runSchedule({ deps: f.deps(), specs, slots: 2 });
  const broken = f.ledger.fold().get(specs[0].run);
  assert.equal(broken.state, 'Done');
  assert.equal(broken.result.invalid, true, 'a grader that fails is an invalid grade, not a fail');
  assert.equal(broken.result.pass, false);
  const runsBefore = [...f.agents.adam.log, ...f.agents.opencode.log].filter((l) => l[0] === 'run').length;

  f.grader.broken = false;
  const deps = f.deps();
  for (const spec of specs) await regradeInvalid({ deps, spec });
  const v = f.ledger.fold().get(specs[0].run);
  assert.equal(v.state, 'Done');
  assert.equal(v.result.invalid, false);
  assert.equal(v.result.pass, true);
  assert.equal(v.history.filter((h) => h === 'Grading').length, 2);
  assert.equal(v.attempt, 1, 'the same attempt: the agent was not run again');
  assert.equal([...f.agents.adam.log, ...f.agents.opencode.log].filter((l) => l[0] === 'run').length, runsBefore);
  // a run that was graded fine is left alone
  const again = await regradeInvalid({ deps, spec: specs[0] });
  assert.equal(again.skipped, true);
});

test('the ledger lets a Done run be graded again only by a regrade of an invalid grade', async (t) => {
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES });
  const specs = plan(f, 1);
  await runSchedule({ deps: f.deps(), specs, slots: 2 });
  const run = specs[0].run;
  assert.throws(() => f.ledger.append({ kind: 'state', run, state: 'Grading' }), LedgerError);
  assert.throws(() => f.ledger.append({ kind: 'state', run, state: 'Grading', regrade: true }), /invalid/, 'a valid grade is final');
  assert.throws(() => f.ledger.append({ kind: 'state', run, state: 'Planned' }), LedgerError);
});

test('M-fits-strict: a block that needs exactly what is left under the cap starts (exactly at the cap is allowed)', async (t) => {
  const f = await fixture(t, { capUSD: 12, perRunBudgetUSD: 3, taskNames: NAMES });
  const specs = plan(f, 1);
  const res = await runSchedule({ deps: f.deps(), specs, slots: 2 });
  assert.equal(res.capHit, false);
  assert.ok(specs.every((s) => f.ledger.fold().get(s.run)?.state === 'Done'));
});

test('H-run-never-pulls: every run makes its task image present on its own slot, once, before its base is prepared', async (t) => {
  const f = await fixture(t, { capUSD: 100, taskNames: NAMES });
  const order = [];
  const deps = { ...f.deps(), pullImage: async (slot, task) => void order.push(`pull ${slot} ${task.id}`), prepareBase: async () => (order.push('base'), { dir: '/base/base.git', sha: 'a'.repeat(40), slug: 'demo', workdir: '/app' }) };
  const specs = specsOf(buildBlocks({ tasks: Object.values(f.tasks).slice(0, 1), agents: ['adam'], arms: ['max'], seed: '1' }));
  await runSchedule({ deps, specs, slots: 1 });
  assert.deepEqual(order, [`pull 1 ${specs[0].task.id}`, 'base']);
});

test('a prepare failure that a retry cannot change is tried once and is final; a transient one is tried again', async (t) => {
  const stderr = 'fatal: detected dubious ownership in repository at \'/work/bench-base/base.git\'\nfatal: Could not read from remote repository.\n';
  let transient = 0;
  const f = await fixture(t, { capUSD: 100, taskNames: ['hello-1', 'fix-add-1'], agentOver: { fail: { prepare: (run) => (run.startsWith('hello-1') ? new PermanentError('the clone for OpenCode failed (exit 128): fatal: detected dubious ownership', { stderr: `${stderr}token=stack-secret-value\n` }) : run.startsWith('fix-add-1') && ++transient <= 1) } } });
  const specs = plan(f).filter((s) => ['hello-1', 'fix-add-1'].includes(s.task.id) && s.agent === 'opencode');
  const res = await runSchedule({ deps: f.deps(), specs, slots: 1 });
  const folded = f.ledger.fold();
  const hello = folded.get(specs.find((s) => s.task.id === 'hello-1').run);
  const fix = folded.get(specs.find((s) => s.task.id === 'fix-add-1').run);
  assert.equal(hello.state, 'Failed_infra');
  assert.equal(hello.final, true);
  assert.equal(hello.attempt, 1, 'the permanent failure was not attempted again');
  assert.equal(fix.state, 'Done', 'a transient failure is retried and succeeds');
  assert.equal(fix.attempt, 2);
  assert.ok(res.passes >= 2);
  assert.equal(resumeActions(folded).find((a) => a.run === hello.run).action, 'give-up');
  // the whole of what the command said is kept in the run's directory, with the key taken out
  const kept = fs.readFileSync(path.join(f.home, 'runs', runDirName(hello.run, 1), 'prepare-stderr.txt'), 'utf8');
  assert.match(kept, /fatal: detected dubious ownership in repository at '\/work\/bench-base\/base\.git'/);
  assert.match(kept, /Could not read from remote repository/);
  assert.ok(!kept.includes('stack-secret-value'));
  assert.match(kept, /token=\[redacted\]/);
});
