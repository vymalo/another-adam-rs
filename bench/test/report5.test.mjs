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
