import assert from 'node:assert/strict';
import { test } from 'node:test';
import { checkBudgetHit, checkCost, checkGrades, checkResume, report, scriptedCallMicro } from '../lib/dryrun-checks.mjs';
import { HarnessCrash, runPlan } from '../lib/runner.mjs';
import { fixture, spec } from './runnerfix.mjs';

const AGENTS = ['adam', 'opencode'];
const TASKS = ['hello-1', 'fix-add-1', 'wrong-1'];
// the dry run names its tasks hello, fix-add and wrong
const plan = (f) => TASKS.flatMap((t) => AGENTS.map((a) => ({ ...spec(f, t, a), run: `${t.replace('-1', '')}-${a}-max` })));
const failures = (results) => results.filter((r) => !r.ok).map((r) => r.what);

test('the checks pass on a ledger the runner made, and say what they proved', async (t) => {
  const f = await fixture(t);
  await runPlan({ deps: f.deps(), plan: plan(f) });
  const folded = f.ledger.fold();
  const runs = AGENTS.flatMap((agent) => ['hello', 'fix-add', 'wrong'].map((task) => ({ run: `${task}-${agent}-max`, agent })));
  const results = [...checkGrades(folded, { agents: AGENTS, arm: 'max' }), ...checkCost(folded, { runs, prices: f.config.prices, modelRequests: f.model.requests.length })];
  assert.deepEqual(failures(results), []);
  assert.ok(results.length >= 12 + 12);
  const lines = [];
  assert.equal(report(results, (l) => lines.push(l)), true);
  assert.match(lines[0], /^ok   adam on hello is graded 1/);
});

test('a task graded the wrong way round, an unfinished run and an exit that is not done are failures', async (t) => {
  const f = await fixture(t);
  const p = plan(f);
  p[0] = { ...p[0], task: f.tasks['wrong-1'] }; // adam on "hello" is run with the wrong answer
  await runPlan({ deps: f.deps(), plan: p.slice(0, 2) });
  const bad = checkGrades(f.ledger.fold(), { agents: AGENTS, arm: 'max' });
  assert.ok(failures(bad).includes('adam on hello is graded 1'));
  assert.ok(failures(bad).includes('adam on fix-add is graded 1'), 'a run that never happened is a failure');
});

test('the cost check fails when a price is not the one the ledger was made with, and when a call went round the meter', async (t) => {
  const f = await fixture(t);
  await runPlan({ deps: f.deps(), plan: [plan(f)[0]] });
  const folded = f.ledger.fold();
  const runs = [{ run: 'hello-adam-max', agent: 'adam' }];
  assert.deepEqual(failures(checkCost(folded, { runs, prices: f.config.prices, modelRequests: f.model.requests.length })), []);
  assert.ok(failures(checkCost(folded, { runs, prices: { ...f.config.prices, output: f.config.prices.output + 1 }, modelRequests: f.model.requests.length })).length > 0);
  assert.ok(failures(checkCost(folded, { runs, prices: f.config.prices, modelRequests: f.model.requests.length + 1 })).some((w) => /scripted model saw/.test(w)));
});

test('the budget check wants a refused call, a graded run and spend inside the reservation', async (t) => {
  const prices = { inputPerMTok: 3, cachedInputPerMTok: 0.3, cacheWritePerMTok: 3.75, outputPerMTok: 4000 };
  const f = await fixture(t, { capUSD: 100, perRunBudgetUSD: 34, prices });
  await runPlan({ deps: f.deps(), plan: [plan(f)[0], plan(f)[1]] });
  const runs = ['hello-adam-max', 'hello-opencode-max'];
  assert.deepEqual(failures(checkBudgetHit(f.ledger.fold(), { runs, reservedMicro: 34_000_000 })), []);
  assert.ok(failures(checkBudgetHit(f.ledger.fold(), { runs, reservedMicro: 1000 })).length > 0, 'spend over the reservation fails');
  const g = await fixture(t);
  await runPlan({ deps: g.deps(), plan: [plan(g)[0]] });
  assert.ok(failures(checkBudgetHit(g.ledger.fold(), { runs: ['hello-adam-max'], reservedMicro: 3_000_000 })).length > 0, 'a run that was never refused is not a budget hit');
});

test('the resume check wants the finished run left alone, the killed one run again with its waste kept, the rest run once', async (t) => {
  const f = await fixture(t, { agentOver: { crashAfter: { 'fix-add-opencode-max': 2 } } });
  const p = ['hello', 'fix-add', 'wrong'].map((n) => ({ ...spec(f, `${n}-1`, 'opencode'), run: `${n}-opencode-max` }));
  await assert.rejects(runPlan({ deps: f.deps(), plan: p }), HarnessCrash);
  const deps = await f.reopen();
  await runPlan({ deps, plan: p });
  const folded = f.ledger.fold();
  const ok = { finished: ['hello-opencode-max'], killed: 'fix-add-opencode-max', later: ['wrong-opencode-max'] };
  assert.deepEqual(failures(checkResume(folded, ok)), []);
  assert.ok(failures(checkResume(folded, { ...ok, killed: 'hello-opencode-max' })).length > 0, 'a run that was not interrupted is not the killed one');
  assert.ok(failures(checkResume(folded, { ...ok, finished: ['fix-add-opencode-max'] })).length > 0, 'a run that ran twice is not a finished one');
});

test('what one scripted call costs is the usage times the price, rounded up', () => {
  assert.equal(scriptedCallMicro({ input: 3_000_000, cachedInput: 300_000, cacheWrite: 3_750_000, output: 15_000_000 }), 4920);
  assert.equal(scriptedCallMicro({ input: 0, cachedInput: 0, cacheWrite: 0, output: 1 }), 1, 'rounded up to a whole micro-dollar');
});
