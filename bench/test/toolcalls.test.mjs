import assert from 'node:assert/strict';
import { test } from 'node:test';
import { foldRecords } from '../lib/ledger.mjs';
import { buildReport } from '../lib/report.mjs';

// Calls per model turn: what the meter counted in each answer (`toolCalls` of a settled call), folded per run
// and reported per cell, so a pilot shows whether the model asks for several calls in one reply.
let n = 0;
const rec = (r) => ({ v: 1, t: new Date(Date.UTC(2026, 10, 18, 0, 0, n++)).toISOString(), ...r });
const usage = { input: 1000, cached: 500, cacheWrite: 0, output: 100, reasoning: 0 };

function run(name, task, arm, counts) {
  const calls = counts.flatMap((toolCalls, i) => [
    { kind: 'call', run: name, attempt: 1, call: i + 1, phase: 'pending', worstMicro: 5 },
    { kind: 'call', run: name, attempt: 1, call: i + 1, phase: 'settled', costMicro: 100, usage, usageMissing: false, ...(toolCalls === undefined ? {} : { toolCalls }) },
  ]);
  return [
    { kind: 'state', run: name, state: 'Planned', task, agent: 'a', arm, source: 'public', language: 'go', phase: 'run' },
    { kind: 'state', run: name, state: 'Reserved', reservedMicro: 1_000_000 },
    { kind: 'state', run: name, state: 'Preparing' },
    { kind: 'state', run: name, state: 'Running' },
    ...calls,
    { kind: 'state', run: name, state: 'Collecting' },
    { kind: 'state', run: name, state: 'Grading' },
    { kind: 'state', run: name, state: 'Done', result: { pass: true, grades: [1], exit: 'done', wallMs: 1000 } },
  ].map(rec);
}

test('the fold counts the turns, the calls, the turns with several calls and the most calls in one turn', () => {
  const v = foldRecords(run('r1', 't1', 'max', [2, 0, 4, 1, 0])).get('r1');
  assert.deepEqual(v.toolCalls, { turns: 5, calls: 7, multiTurns: 2, maxInTurn: 4 });
});

test('a turn whose count is not known is left out of the turns, and a run with none has no figures', () => {
  const v = foldRecords(run('r1', 't1', 'max', [3, undefined, 1])).get('r1');
  assert.deepEqual(v.toolCalls, { turns: 2, calls: 4, multiTurns: 1, maxInTurn: 3 });
  assert.deepEqual(foldRecords(run('r2', 't1', 'max', [undefined])).get('r2').toolCalls, { turns: 0, calls: 0, multiTurns: 0, maxInTurn: 0 });
});

test('the report gives calls per model turn, turns with several calls and the most in a turn, per cell', () => {
  const text = buildReport([...run('t1.max', 't1', 'max', [2, 0, 4, 1, 0]), ...run('t1.off', 't1', 'off', [1, 1, 0])]);
  assert.match(text, /Tool calls per model turn/);
  // a / max: 7 calls in 5 turns = 1.40, 2 turns with several, at most 4. a / off: 2 calls in 3 turns = 0.67, none with several, at most 1.
  assert.match(text, /\| a \| max \| 5 \| 7 \| 1\.40 \| 2 \| 4 \|/);
  assert.match(text, /\| a \| off \| 3 \| 2 \| 0\.67 \| 0 \| 1 \|/);
});

test('a ledger from before the count says so instead of a figure', () => {
  const text = buildReport(run('t1.max', 't1', 'max', [undefined, undefined]));
  assert.match(text, /\| a \| max \| 0 \| 0 \| n\/a \| n\/a \| n\/a \|/);
});
