import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { CALLS_PER_RUN } from '../lib/agent-script.mjs';
import { hostGradeIO } from '../lib/grade.mjs';
import { HarnessCrash, executeRun, runDirName, runPlan } from '../lib/runner.mjs';
import { MODEL_ID, TEST_KEY } from './helpers.mjs';
import { PER_CALL, fixture, history, spec } from './runnerfix.mjs';

test('a run goes through every state, with the patch saved before it is graded, and ends Done', async (t) => {
  const f = await fixture(t);
  let patchAtGrading = null;
  f.hooks.afterState = (run, state) => {
    if (state === 'Grading') patchAtGrading = fs.existsSync(path.join(f.home, 'runs', runDirName(run, 1), 'model.patch'));
  };
  const out = await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'opencode')] });
  assert.deepEqual(out.map((o) => o.state), ['Done']);
  assert.deepEqual(history(f, 'hello-1-opencode-max'), ['Planned', 'Reserved', 'Preparing', 'Running', 'Collecting', 'Grading', 'Done']);
  assert.equal(patchAtGrading, true);
  const v = f.ledger.fold().get('hello-1-opencode-max');
  assert.equal(v.result.pass, true);
  assert.deepEqual(v.result.grades, [1, 1, 1]);
  assert.equal(v.result.exit, 'done');
  assert.deepEqual([v.task, v.agent, v.arm], ['hello-1', 'opencode', 'max']);
  assert.equal(v.reservationActive, false);
});

test('the grades are 1, 1, 0: a wrong answer fails, the right ones pass, for both agents', async (t) => {
  const f = await fixture(t);
  const plan = ['hello-1', 'fix-add-1', 'wrong-1'].flatMap((n) => [spec(f, n, 'adam'), spec(f, n, 'opencode')]);
  await runPlan({ deps: f.deps(), plan });
  const folded = f.ledger.fold();
  for (const agent of ['adam', 'opencode']) {
    assert.deepEqual(['hello-1', 'fix-add-1', 'wrong-1'].map((n) => folded.get(`${n}-${agent}-max`).result.pass), [true, true, false], agent);
  }
});

test('a plan folds the ledger once, not once per run and twice per new run', async (t) => {
  const f = await fixture(t);
  let folds = 0;
  const fold = f.ledger.fold.bind(f.ledger);
  f.ledger.fold = () => {
    folds += 1;
    return fold();
  };
  const plan = [spec(f, 'hello-1', 'adam'), spec(f, 'hello-1', 'opencode'), spec(f, 'fix-add-1', 'adam')];
  const out = await runPlan({ deps: f.deps(), plan });
  assert.deepEqual(out.map((o) => o.state), ['Done', 'Done', 'Done']);
  // resumeLedger reads the ledger once, and runPlan once after the resume records; advance uses that map and folds the
  // one Planned record of a new run instead of replaying the ledger, which made a plan O(n^2) in its records.
  assert.equal(folds, 2);
});

test('the ledger cost is the scripted usage times the price, for each run and in total, and the model saw exactly those calls', async (t) => {
  const f = await fixture(t);
  await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'adam'), spec(f, 'hello-1', 'opencode')] });
  const folded = f.ledger.fold();
  assert.equal(folded.get('hello-1-adam-max').spentMicro, CALLS_PER_RUN.adam * PER_CALL);
  assert.equal(folded.get('hello-1-opencode-max').spentMicro, CALLS_PER_RUN.opencode * PER_CALL);
  assert.equal(f.model.requests.length, CALLS_PER_RUN.adam + CALLS_PER_RUN.opencode);
  assert.equal(PER_CALL, 4920);
  assert.deepEqual(folded.get('hello-1-adam-max').tokens, { input: 6 * 1000, cached: 6 * 400, cacheWrite: 0, output: 6 * 200, reasoning: 6 * 50 });
});

test('a run over its budget ends budget_hit, is graded, and has spent no more than its reservation', async (t) => {
  // output at 4000 USD per million: a call's worst case is about $32.8 and its actual cost about $0.8
  const prices = { inputPerMTok: 3, cachedInputPerMTok: 0.3, cacheWritePerMTok: 3.75, outputPerMTok: 4000 };
  const f = await fixture(t, { capUSD: 100, perRunBudgetUSD: 34, prices });
  await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'adam'), spec(f, 'hello-1', 'opencode')] });
  for (const agent of ['adam', 'opencode']) {
    const v = f.ledger.fold().get(`hello-1-${agent}-max`);
    assert.equal(v.state, 'Done');
    assert.equal(v.result.exit, 'budget_hit', agent);
    assert.ok(v.spentMicro <= 34_000_000);
    assert.equal(v.calls.settled, 2, `${agent}: two calls fitted, the third was refused`);
    assert.equal(v.calls.refused, 1);
    assert.ok(typeof v.result.pass === 'boolean' && v.result.grades.length === 3, 'a run that hit its budget is graded');
  }
});

test('a run whose reservation would cross the cap is never started', async (t) => {
  const f = await fixture(t, { capUSD: 3, perRunBudgetUSD: 3 });
  const out = await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'opencode'), spec(f, 'fix-add-1', 'opencode')] });
  // the first reserves 3 of 3 and spends a little; the second would need 3 more: it does not fit
  assert.deepEqual(out.map((o) => o.state), ['Done', 'NotStarted']);
  assert.ok(!f.agents.opencode.log.some((l) => l[0] === 'prepare' && l[1] === 'fix-add-1-opencode-max'));
  assert.deepEqual(history(f, 'fix-add-1-opencode-max'), ['Planned', 'NotStarted']);
});

test('killed while a run works: the next start runs the interrupted run again and no other', async (t) => {
  const f = await fixture(t, { agentOver: { crashAfter: { 'fix-add-1-opencode-max': 2 } } });
  const plan = [spec(f, 'hello-1', 'opencode'), spec(f, 'fix-add-1', 'opencode'), spec(f, 'wrong-1', 'opencode')];
  await assert.rejects(runPlan({ deps: f.deps(), plan }), HarnessCrash);
  assert.deepEqual(history(f, 'hello-1-opencode-max').at(-1), 'Done');
  assert.equal(f.ledger.state('fix-add-1-opencode-max'), 'Running');
  assert.equal(f.ledger.state('wrong-1-opencode-max'), undefined, 'the third run never started');
  const callsBefore = f.model.requests.length;

  const deps = await f.reopen();
  const out = await runPlan({ deps, plan });
  assert.deepEqual(out.map((o) => [o.run, o.state, Boolean(o.skipped)]), [['hello-1-opencode-max', 'Done', true], ['fix-add-1-opencode-max', 'Done', false], ['wrong-1-opencode-max', 'Done', false]]);

  const ran = f.agents.opencode.log.filter((l) => l[0] === 'run').map((l) => `${l[1]}#${l[2]}`);
  assert.deepEqual(ran, ['fix-add-1-opencode-max#2', 'wrong-1-opencode-max#1'], 'the first run was not run again; the interrupted one on its second attempt');
  const folded = f.ledger.fold();
  assert.equal(folded.get('hello-1-opencode-max').attempt, 1);
  const killed = folded.get('fix-add-1-opencode-max');
  assert.equal(killed.attempt, 2);
  assert.equal(killed.interruptions, 1);
  assert.equal(killed.wastedMicro, 2 * PER_CALL, 'what the killed attempt spent stays in the ledger as waste');
  assert.equal(killed.spentMicro, (2 + CALLS_PER_RUN.opencode) * PER_CALL);
  assert.ok(f.model.requests.length > callsBefore);
});

test('killed while grading: the next start grades the saved patch again and never runs the agent', async (t) => {
  const f = await fixture(t);
  f.hooks.afterState = (run, state) => {
    if (state === 'Grading') throw new HarnessCrash('Grading');
  };
  await assert.rejects(runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'opencode')] }), HarnessCrash);
  assert.equal(f.ledger.state('hello-1-opencode-max'), 'Grading');
  const deps = await f.reopen();
  delete deps.afterState;
  const out = await runPlan({ deps, plan: [spec(f, 'hello-1', 'opencode')] });
  assert.deepEqual(out.map((o) => o.state), ['Done']);
  assert.equal(f.agents.opencode.log.filter((l) => l[0] === 'run').length, 0, 'the agent of the second start was never run');
  const v = f.ledger.fold().get('hello-1-opencode-max');
  assert.equal(v.result.pass, true);
  assert.equal(v.attempt, 1);
});

test('an environment that does not start is a Failed_infra with nothing spent, and the next start tries again', async (t) => {
  let failing = true;
  const f = await fixture(t, { agentOver: { fail: { prepare: () => failing } } });
  const plan = [spec(f, 'hello-1', 'opencode')];
  const out = await runPlan({ deps: f.deps(), plan });
  assert.equal(out[0].state, 'Failed_infra');
  assert.equal(f.ledger.fold().get('hello-1-opencode-max').spentMicro, 0);
  assert.ok(f.agents.opencode.log.some((l) => l[0] === 'cleanup'));
  assert.equal(f.ledger.fold().get('hello-1-opencode-max').reservationActive, false);
  failing = false;
  const deps = await f.reopen();
  const again = await runPlan({ deps, plan });
  assert.deepEqual(again.map((o) => o.state), ['Done']);
  assert.equal(f.ledger.fold().get('hello-1-opencode-max').attempt, 2);
});

test('an agent that reports an infrastructure exit is Interrupted and then Failed_infra, not graded', async (t) => {
  const f = await fixture(t, { agentOver: { exit: 'infra' } });
  const out = await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'adam')] });
  assert.equal(out[0].state, 'Failed_infra');
  assert.deepEqual(history(f, 'hello-1-adam-max').slice(-3), ['Running', 'Interrupted', 'Failed_infra']);
});

test('a run the global cap stops is StoppedByCap, and is not graded', async (t) => {
  const f = await fixture(t, { agentOver: { exit: 'stopped_by_cap' } });
  const out = await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'adam')] });
  assert.equal(out[0].state, 'StoppedByCap');
  assert.deepEqual(history(f, 'hello-1-adam-max').slice(-2), ['Running', 'StoppedByCap']);
});

test('a verifier that needs the network gets it for its grading only', async (t) => {
  const none = await fixture(t);
  await runPlan({ deps: none.deps(), plan: [spec(none, 'hello-1', 'opencode')] });
  assert.deepEqual(none.windows, []);
  const open = await fixture(t, { verifier: 'public' });
  await runPlan({ deps: open.deps(), plan: [spec(open, 'hello-1', 'opencode')] });
  assert.deepEqual(open.windows, ['grader']);
});

test('executeRun refuses an agent it has no driver for', async (t) => {
  const f = await fixture(t);
  f.deps().ledger.append({ kind: 'state', run: 'x', state: 'Planned' });
  await assert.rejects(executeRun({ deps: f.deps(), spec: { run: 'x', task: f.tasks['hello-1'], agent: 'nobody', arm: 'max', slot: 1 }, attempt: 1 }), /no driver/);
});

test('the key the model was given is in nothing the runner kept', async (t) => {
  const f = await fixture(t);
  await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'opencode')] });
  const walk = (d) => fs.readdirSync(d, { withFileTypes: true }).flatMap((e) => (e.isDirectory() ? walk(path.join(d, e.name)) : [path.join(d, e.name)]));
  for (const file of walk(f.home)) assert.ok(!fs.readFileSync(file, 'utf8').includes(TEST_KEY), file);
  assert.ok(!fs.readFileSync(path.join(f.home, 'ledger.jsonl'), 'utf8').includes(MODEL_ID));
});

test('finding 11: the agent is stopped, and that is confirmed, before its tree is read: run, stop, collect, in that order', async (t) => {
  const f = await fixture(t);
  await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'opencode')] });
  const order = f.agents.opencode.log.map((l) => l[0]).filter((n) => ['run', 'stop', 'collect', 'cleanup'].includes(n));
  assert.deepEqual(order, ['run', 'stop', 'collect', 'cleanup']);
});

test('finding 11: a coder that cannot be confirmed stopped makes the run infrastructure, and its tree is never read', async (t) => {
  const f = await fixture(t, { agentOver: { stopFails: () => true } });
  const out = await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'adam')] });
  assert.equal(out[0].state, 'Failed_infra');
  assert.equal(out[0].reason, 'not-stopped');
  assert.ok(!f.agents.adam.log.some((l) => l[0] === 'collect'));
  assert.deepEqual(history(f, 'hello-1-adam-max').slice(-3), ['Running', 'Interrupted', 'Failed_infra']);
});

test('finding 9: the slot\'s service is reaped and its volume emptied before every run, and again at the start of a plan', async (t) => {
  const f = await fixture(t);
  f.slotContainers.listing = [{ image: 'leftover', imageId: 'x', labels: {}, names: ['old'] }];
  await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'opencode'), spec(f, 'fix-add-1', 'opencode')] });
  assert.ok(f.slotContainers.reaped >= 3, `reaped ${f.slotContainers.reaped} times: at the plan, and before each of the two runs`);
  assert.deepEqual(f.resets, [1, 1]);
  const log = f.agents.opencode.log.map((l) => `${l[0]}:${l[1]}`);
  assert.ok(log.indexOf('prepare:hello-1-opencode-max') >= 0);
});

test('finding 7: grading runs on the grader service and never on the agents\' service', async (t) => {
  const f = await fixture(t);
  await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'opencode')] });
  assert.equal(f.slotContainers.runs.length, 0, 'no grading container on the agents\' service');
  assert.ok(f.grading.runs.length >= 3, 'three grading attempts on the grader');
});

test('finding 10: a run in which the agent made no model call is infrastructure, not a fail, and is not graded', async (t) => {
  const f = await fixture(t, { calls: { adam: 0, opencode: 0 } });
  const out = await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'opencode')] });
  assert.equal(out[0].state, 'Failed_infra');
  assert.equal(out[0].reason, 'infra:no-model-calls');
  assert.equal(f.grading.runs.length, 0);
  const v = f.ledger.fold().get('hello-1-opencode-max');
  assert.equal(v.state, 'Failed_infra');
  assert.equal(v.result, null);
});

test('finding 10: more than two upstream errors make a run infrastructure, whichever agent it is', async (t) => {
  const f = await fixture(t, { calls: { adam: 6, opencode: 6 } });
  f.model.behavior.status = 502; // the endpoint is down for every call
  const out = await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'adam')] });
  assert.equal(out[0].state, 'Failed_infra');
  assert.match(out[0].reason, /^infra:(upstream-errors|last-call-failed)$/);
});

test('defect 8: a stack that has a service-backed grader IO grades through it', async (t) => {
  const f = await fixture(t);
  const deps = f.deps();
  const staged = [];
  const base = hostGradeIO(path.join(f.home, 'work'));
  deps.stack = { ...deps.stack, graderIO: () => ({ stage: async (x) => (staged.push(x.task.id), base.stage(x)) }) };
  await runPlan({ deps, plan: [spec(f, 'hello-1', 'opencode')] });
  assert.equal(staged.length, 3, 'each of the three grading attempts staged through the stack\'s IO');
});

test('defects 2, 3 and 7: what was refused and the limits the run had are in the run record', async (t) => {
  const f = await fixture(t);
  f.agents.opencode.record = { limits: 'service', outputRefused: ['events.jsonl'] };
  f.agents.opencode.collectRefused = [{ name: 'tree/pipe', reason: 'not a plain file (type "6")' }];
  await runPlan({ deps: f.deps(), plan: [spec(f, 'hello-1', 'opencode')] });
  const n = f.ledger.fold().get('hello-1-opencode-max').result.notes;
  assert.equal(n.limits, 'service');
  assert.deepEqual(n.refused, [{ name: 'tree/pipe', reason: 'not a plain file (type "6")' }, { name: 'events.jsonl', reason: 'a log that is not a plain file' }]);
  const meta = JSON.parse(fs.readFileSync(path.join(f.home, 'runs', runDirName('hello-1-opencode-max', 1), 'meta.json'), 'utf8'));
  assert.equal(meta.notes.limits, 'service');
});
