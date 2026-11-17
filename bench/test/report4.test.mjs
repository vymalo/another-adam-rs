import assert from 'node:assert/strict';
import { test } from 'node:test';
import { buildReport } from '../lib/report.mjs';

// Four runs of agent a in arm max, one task each, with the meter's calls written out so every number is worked by hand.
//   t1 public  solved   cost 10_000  tokens in 1000 cached 800 out 100
//   t2 public  failed   cost 30_000  tokens in 3000 cached 600 out 300
//   t3 own     solved   cost 20_000  tokens in 2000 cached 400 out 200
//   t4 own-self failed  cost 40_000  tokens in 4000 cached 0   out 400   (one infrastructure failure before it finished)
function run(name, task, source, pass, cost, usage, { infra = false } = {}) {
  const base = { run: name, attempt: infra ? 2 : 1 };
  const planned = { kind: 'state', run: name, state: 'Planned', task, agent: 'a', arm: 'max', source, language: 'go' };
  const up = (attempt, calls) => [
    { kind: 'state', run: name, state: 'Reserved', reservedMicro: 1_000_000 },
    { kind: 'state', run: name, state: 'Preparing' },
    ...(calls ? [{ kind: 'state', run: name, state: 'Running' }, { ...base, attempt, kind: 'call', call: 1, phase: 'pending', worstMicro: 5 }, { ...base, attempt, kind: 'call', call: 1, phase: 'settled', costMicro: cost, usage, usageMissing: false }] : []),
  ];
  const head = infra ? [planned, ...up(1, false), { kind: 'state', run: name, state: 'Failed_infra' }, { ...planned, retry: true }] : [planned];
  return [
    ...head,
    ...up(infra ? 2 : 1, true),
    { kind: 'state', run: name, state: 'Collecting' },
    { kind: 'state', run: name, state: 'Grading' },
    { kind: 'state', run: name, state: 'Done', result: { pass, grades: [pass ? 1 : 0, pass ? 1 : 0, pass ? 1 : 0], exit: 'done', wallMs: 1000 } },
  ];
}
const records = [
  ...run('r1', 't1', 'public', true, 10_000, { input: 1000, cached: 800, cacheWrite: 0, output: 100, reasoning: 0 }),
  ...run('r2', 't2', 'public', false, 30_000, { input: 3000, cached: 600, cacheWrite: 0, output: 300, reasoning: 0 }),
  ...run('r3', 't3', 'own', true, 20_000, { input: 2000, cached: 400, cacheWrite: 0, output: 200, reasoning: 0 }),
  ...run('r4', 't4', 'own-self', false, 40_000, { input: 4000, cached: 0, cacheWrite: 0, output: 400, reasoning: 0 }, { infra: true }),
].map((r, i) => ({ v: 1, t: new Date(Date.UTC(2026, 10, 17, 0, 0, i)).toISOString(), ...r }));

test('cost per run, cost per solved task, tokens and the cache-hit rate equal the hand-computed figures', () => {
  const text = buildReport(records);
  // all four: cost 100_000 = $0.1000; per solved (2) $0.0500; per run $0.0250; input 10000, cached 1800, output 1000; hit rate 1800 / 10000 = 18.0%
  assert.match(text, /\| a \| max \| \$0\.1000 \| \$0\.0500 \| 10000 \| 1800 \| 0 \| 1000 \| 0 \| [^|]+ \| \$0\.0250 \| 18\.0% \|/);
  assert.match(text, /\| a \| max \| 4 \| 2 \| 50\.0% \| 15\.0% to 85\.0% \|/);
});

test('public, own and adam-rs\'s own tasks are reported apart, each with its own rate and cost', () => {
  const text = buildReport(records);
  assert.match(text, /\| public \| a \| max \| 2 \| 1 \| 50\.0% \|/);
  assert.match(text, /\| own \| a \| max \| 1 \| 1 \| 100\.0% \|/);
  assert.match(text, /\| own-self \| a \| max \| 1 \| 0 \| 0\.0% \|/);
  // public: cost 40_000, one solved -> $0.0400 each; own: 20_000, one solved; own-self: 40_000, none solved
  assert.match(text, /\| public \| a \| max \| \$0\.0400 \| \$0\.0400 \|/);
  assert.match(text, /\| own \| a \| max \| \$0\.0200 \| \$0\.0200 \|/);
  assert.match(text, /\| own-self \| a \| max \| \$0\.0400 \| n\/a \(none solved\) \|/);
});

test('an infrastructure failure that was retried is counted, and the run still counts in the rates', () => {
  const text = buildReport(records);
  assert.match(text, /## Infrastructure failures/);
  // one run affected, finished after a retry, none lost, one infrastructure failure, no interruption, nothing wasted (it spent nothing)
  assert.match(text, /\| a \| max \| 1 \| 1 \| 0 \| 1 \| 0 \| \$0\.0000 \|/);
});

test('no infrastructure failure: the table says none', () => {
  const text = buildReport(records.filter((r) => r.run !== 'r4'));
  assert.match(text, /## Infrastructure failures[\s\S]*_none_/);
});
