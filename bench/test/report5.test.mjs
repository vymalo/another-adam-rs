import assert from 'node:assert/strict';
import { test } from 'node:test';
import { buildReport } from '../lib/report.mjs';

// Two cells (agent a in arms max and off), three tasks, worked by hand.
//   t1: max solved 10_000, off solved 10_000
//   t2: max failed 30_000, off solved 20_000
//   t3: max solved 10_000, off never finished (Failed_infra, 5_000 wasted): t3 is an incomplete block
// and the pilot: two runs of t1 that solved at 7_000 each, in phase `pilot`, never in a rate.
let n = 0;
const at = () => new Date(Date.UTC(2026, 10, 17, 0, 0, n++)).toISOString();
const rec = (r) => ({ v: 1, t: at(), ...r });

function run({ name, task, arm, phase = 'run', pass = true, cost, infra = false, finish = true }) {
  const planned = { kind: 'state', run: name, state: 'Planned', task, agent: 'a', arm, source: 'public', language: 'go', phase };
  const calls = (attempt) => [
    { kind: 'call', run: name, attempt, call: 1, phase: 'pending', worstMicro: 5 },
    { kind: 'call', run: name, attempt, call: 1, phase: 'settled', costMicro: cost, usage: { input: 1000, cached: 500, cacheWrite: 0, output: 100, reasoning: 0 }, usageMissing: false },
  ];
  const up = [{ kind: 'state', run: name, state: 'Reserved', reservedMicro: 1_000_000 }, { kind: 'state', run: name, state: 'Preparing' }, { kind: 'state', run: name, state: 'Running' }];
  if (!finish) return [planned, ...up, ...calls(1), { kind: 'state', run: name, state: 'Failed_infra', final: true }].map(rec);
  return [planned, ...up, ...calls(1), { kind: 'state', run: name, state: 'Collecting' }, { kind: 'state', run: name, state: 'Grading' }, { kind: 'state', run: name, state: 'Done', result: { pass, grades: [1, 1, 1], exit: 'done', wallMs: 1000 } }].map(rec);
}

const records = [
  ...run({ name: 't1.max', task: 't1', arm: 'max', cost: 10_000 }),
  ...run({ name: 't1.off', task: 't1', arm: 'off', cost: 10_000 }),
  ...run({ name: 't2.max', task: 't2', arm: 'max', pass: false, cost: 30_000 }),
  ...run({ name: 't2.off', task: 't2', arm: 'off', cost: 20_000 }),
  ...run({ name: 't3.max', task: 't3', arm: 'max', cost: 10_000 }),
  ...run({ name: 't3.off', task: 't3', arm: 'off', cost: 5_000, finish: false }),
  ...run({ name: 'pilot.t1.max', task: 't1', arm: 'max', phase: 'pilot', cost: 7_000 }),
  ...run({ name: 'pilot.t1.off', task: 't1', arm: 'off', phase: 'pilot', cost: 7_000 }),
];

test('R3: the pilot is reported apart, and none of its runs is in a rate, a cost per run or a paired table', () => {
  const text = buildReport(records);
  assert.match(text, /## Pilot/);
  assert.match(text, /\| a \| max \| 1 \| 1 \| \$0\.0070 \|/);
  // the rates are of the full run only: a/max has t1, t2, t3 (3 runs) and a/off has t1, t2 (t3 is incomplete)
  assert.doesNotMatch(text, /\| a \| max \| 4 \|/);
  assert.doesNotMatch(text, /pilot\.t1/);
});

test('R4: only blocks that are complete in every cell are in the pass rates, and the incomplete ones are listed', () => {
  const text = buildReport(records);
  // t3 is complete in max but not in off: left out of the rates of both. a/max: t1 solved, t2 failed: 1 of 2; a/off: 2 of 2
  assert.match(text, /\| a \| max \| 2 \| 1 \| 50\.0% \|/);
  assert.match(text, /\| a \| off \| 2 \| 2 \| 100\.0% \|/);
  assert.match(text, /## Incomplete blocks/);
  assert.match(text, /\| t3 \| a \/ off \|/);
});

test('R13: infrastructure failures are counted beside each rate, and the spend on failures is not hidden', () => {
  const text = buildReport(records);
  // the rate rows say how many runs of the cell are left out: a/off lost t3 to the infrastructure, a/max lost t3 to an incomplete block
  assert.match(text, /\| a \| max \| 2 \| 1 \| 50\.0% \| [^|]+ \| 1 \|/);
  assert.match(text, /\| a \| off \| 2 \| 2 \| 100\.0% \| [^|]+ \| 1 \|/);
  // cost per solved task: a/off counted spend 10_000 + 20_000 + 10_000(t3.max is another cell) ... a/off: 30_000 over 2 solved is $0.0150;
  // with the failed run's 5_000 included: 35_000 over 2 solved is $0.0175
  assert.match(text, /\| a \| off \| \$0\.0300 \| \$0\.0150 \|[^\n]*\| \$0\.0000 \| \$0\.0175 \|/);
  // a/max: counted 50_000 (t1 10_000 + t2 30_000 + t3 10_000), solved 2, unsolved run spend 30_000: all-runs spend per solved is $0.0250
  assert.match(text, /\| a \| max \| \$0\.0500 \| \$0\.0250 \|[^\n]*\| \$0\.0300 \| \$0\.0250 \|/);
  assert.match(text, /## Infrastructure failures[\s\S]*\| a \| off \| 1 \| 0 \| 1 \| 0 \| 0 \| \$0\.0050 \|/);
});

test('R13: the cache-hit label says all attempts', () => {
  assert.match(buildReport(records), /Cache-hit rate \(cached \/ input, all attempts\)/);
});

// Records with a chosen agent and end state, for the tests of what is rated.
function run2({ name, task, agent = 'a', arm, pass = true, cost, end = 'Done' }) {
  const planned = { kind: 'state', run: name, state: 'Planned', task, agent, arm, source: 'public', language: 'go', phase: 'run' };
  const calls = [
    { kind: 'call', run: name, attempt: 1, call: 1, phase: 'pending', worstMicro: 5 },
    { kind: 'call', run: name, attempt: 1, call: 1, phase: 'settled', costMicro: cost, usage: { input: 1000, cached: 500, cacheWrite: 0, output: 100, reasoning: 0 }, usageMissing: false },
  ];
  const up = [{ kind: 'state', run: name, state: 'Reserved', reservedMicro: 1_000_000 }, { kind: 'state', run: name, state: 'Preparing' }, { kind: 'state', run: name, state: 'Running' }];
  if (end === 'Failed_infra') return [planned, ...up, ...calls, { kind: 'state', run: name, state: 'Failed_infra', final: true }].map(rec);
  if (end === 'StoppedByCap') return [planned, ...up, ...calls, { kind: 'state', run: name, state: 'StoppedByCap' }].map(rec);
  return [planned, ...up, ...calls, { kind: 'state', run: name, state: 'Collecting' }, { kind: 'state', run: name, state: 'Grading' }, { kind: 'state', run: name, state: 'Done', result: { pass, grades: [1, 1, 1], exit: 'done', wallMs: 1000 } }].map(rec);
}
const base = records.filter((r) => !String(r.run).startsWith('pilot.'));

test('S-cells-from-counted: a cell with no finished run at all makes every block incomplete; nothing is rated', () => {
  const text = buildReport([...run2({ name: 't1.max', task: 't1', arm: 'max', cost: 10_000 }), ...run2({ name: 't1.off', task: 't1', arm: 'off', cost: 5_000, end: 'Failed_infra' })]);
  assert.doesNotMatch(text, /\| a \| max \| 1 \| 1 \| 100\.0% \|/);
  assert.match(text, /\| t1 \| a \/ off \|/);
});

test('S-language-unrated: the by-language rates are of complete blocks only', () => {
  const text = buildReport(base);
  assert.match(text, /\| go \| a \| max \| 2 \| 1 \| 50\.0% \|/);
  assert.doesNotMatch(text, /\| go \| a \| max \| 3 \|/);
});

test('S-source-unrated: the by-source rates are of complete blocks only', () => {
  const text = buildReport(base);
  assert.match(text, /\| public \| a \| max \| 2 \| 1 \| 50\.0% \|/);
  assert.doesNotMatch(text, /\| public \| a \| max \| 3 \|/);
});

test('S-paired-unrated: with two agents and two arms, a task missing one cell is in no pairing', () => {
  const text = buildReport([
    ...run2({ name: 't1.a.max', task: 't1', agent: 'a', arm: 'max', cost: 1 }),
    ...run2({ name: 't1.b.max', task: 't1', agent: 'b', arm: 'max', cost: 1 }),
    ...run2({ name: 't1.a.off', task: 't1', agent: 'a', arm: 'off', cost: 1 }),
    ...run2({ name: 't1.b.off', task: 't1', agent: 'b', arm: 'off', cost: 1 }),
    ...run2({ name: 't2.a.max', task: 't2', agent: 'a', arm: 'max', cost: 1 }),
    ...run2({ name: 't2.b.max', task: 't2', agent: 'b', arm: 'max', pass: false, cost: 1 }),
    ...run2({ name: 't2.a.off', task: 't2', agent: 'a', arm: 'off', cost: 1 }),
    ...run2({ name: 't2.b.off', task: 't2', agent: 'b', arm: 'off', cost: 1, end: 'Failed_infra' }),
  ]);
  assert.match(text, /\| max \| a vs b \| 1 \| 0 \| 0 \|/);
  assert.doesNotMatch(text, /\| max \| a vs b \| 2 \|/);
});

test('S-infra-wasted-whole-cell: the infrastructure table\'s Wasted is the affected runs\' waste, not a cap stop\'s', () => {
  const text = buildReport([...run2({ name: 't1.max', task: 't1', arm: 'max', cost: 5_000, end: 'Failed_infra' }), ...run2({ name: 't2.max', task: 't2', arm: 'max', cost: 2_000, end: 'StoppedByCap' })]);
  assert.match(text, /## Infrastructure failures[\s\S]*\| a \| max \| 1 \| 0 \| 1 \| 0 \| 0 \| \$0\.0050 \|/);
});
