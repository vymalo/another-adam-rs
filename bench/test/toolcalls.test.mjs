import assert from 'node:assert/strict';
import { test } from 'node:test';
import { foldRecords } from '../lib/ledger.mjs';
import { buildReport } from '../lib/report.mjs';

// Calls per model turn: what the meter counted in each answer (`toolCalls` of a settled call), and the tools those
// calls named (`toolNames`), folded per run and reported per cell, so a pilot shows whether the model asks for
// several calls in one reply and which tools the calls go to.
let n = 0;
const rec = (r) => ({ v: 1, t: new Date(Date.UTC(2026, 10, 18, 0, 0, n++)).toISOString(), ...r });
const usage = { input: 1000, cached: 500, cacheWrite: 0, output: 100, reasoning: 0 };

/** A turn's count, or its count and the names its calls gave. */
const turn = (count) => (Array.isArray(count) ? { toolCalls: count[0], toolNames: count[1] } : { toolCalls: count });

function run(name, task, arm, counts) {
  const calls = counts.flatMap((count, i) => {
    const t = count === undefined ? {} : turn(count);
    return [
      { kind: 'call', run: name, attempt: 1, call: i + 1, phase: 'pending', worstMicro: 5 },
      { kind: 'call', run: name, attempt: 1, call: i + 1, phase: 'settled', costMicro: 100, usage, usageMissing: false, ...t },
    ];
  });
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

const noTools = { named: 0, byName: new Map() };

test('the fold counts the turns, the calls, the turns with several calls and the most calls in one turn', () => {
  const v = foldRecords(run('r1', 't1', 'max', [2, 0, 4, 1, 0])).get('r1');
  assert.deepEqual(v.toolCalls, { turns: 5, calls: 7, multiTurns: 2, maxInTurn: 4, ...noTools });
});

test('a turn whose count is not known is left out of the turns, and a run with none has no figures', () => {
  const v = foldRecords(run('r1', 't1', 'max', [3, undefined, 1])).get('r1');
  assert.deepEqual(v.toolCalls, { turns: 2, calls: 4, multiTurns: 1, maxInTurn: 3, ...noTools });
  assert.deepEqual(foldRecords(run('r2', 't1', 'max', [undefined])).get('r2').toolCalls, { turns: 0, calls: 0, multiTurns: 0, maxInTurn: 0, ...noTools });
});

test('the fold counts the calls of each tool by the name the answer gave', () => {
  const v = foldRecords(
    run('r1', 't1', 'max', [
      [2, ['read_file', 'search_files']],
      [1, ['read_file']],
      [2, ['run_checks', 'run_checks']],
    ]),
  ).get('r1');
  assert.equal(v.toolCalls.calls, 5);
  assert.equal(v.toolCalls.named, 5);
  assert.deepEqual(v.toolCalls.byName, new Map([['read_file', 2], ['search_files', 1], ['run_checks', 2]]));
});

test('a call whose name the answer did not give is counted in the calls and in no tool', () => {
  const v = foldRecords(run('r1', 't1', 'max', [[3, ['read_file']]])).get('r1');
  assert.equal(v.toolCalls.calls, 3);
  assert.equal(v.toolCalls.named, 1);
  assert.deepEqual(v.toolCalls.byName, new Map([['read_file', 1]]));
});

test('a name that is not a string, and an empty one, are not counted', () => {
  const v = foldRecords(run('r1', 't1', 'max', [[3, ['read_file', '', 7]]])).get('r1');
  assert.equal(v.toolCalls.named, 1);
  assert.deepEqual(v.toolCalls.byName, new Map([['read_file', 1]]));
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

test('a cell of several runs pools their turns: calls per turn is all calls over all turns, the most is the most of any run', () => {
  // t1: 4 calls in 2 turns, at most 3. t2: 4 calls in 4 turns, at most 1. Pooled: 8 / 6 = 1.33 (a mean of the runs' own
  // rates would say 1.50), 1 turn with several, at most 3 (a sum of the runs' most would say 4).
  const text = buildReport([...run('t1.max', 't1', 'max', [3, 1]), ...run('t2.max', 't2', 'max', [1, 1, 1, 1])]);
  assert.match(text, /\| a \| max \| 6 \| 8 \| 1\.33 \| 1 \| 3 \|/);
});

test('the report gives the calls of each tool per cell, the tools of the cells in one table', () => {
  const text = buildReport([
    ...run('t1.max', 't1', 'max', [[2, ['read_file', 'run_checks']], [1, ['read_file']]]),
    ...run('t1.off', 't1', 'off', [[1, ['read_file']], [1, ['commit_and_push']]]),
  ]);
  assert.match(text, /Tool calls by tool/);
  // The cells are the header, in the order the report groups them: `a / max`, `a / off`.
  assert.match(text, /\| Tool \| a \/ max \| a \/ off \|/);
  assert.match(text, /\| read_file \| 2 \| 1 \|/);
  assert.match(text, /\| run_checks \| 1 \| 0 \|/);
  assert.match(text, /\| commit_and_push \| 0 \| 1 \|/);
  // Ordered by the total across the cells, most first, and by name on a tie.
  assert.ok(text.indexOf('| read_file |') < text.indexOf('| commit_and_push |'), 'the most called tool comes first');
  assert.ok(text.indexOf('| commit_and_push |') < text.indexOf('| run_checks |'), 'a tie is broken by the name');
});

test('a tool name that is an object key of JavaScript is a name like any other', () => {
  const v = foldRecords(run('r1', 't1', 'max', [[1, ['__proto__']]])).get('r1');
  assert.equal(v.toolCalls.byName.get('__proto__'), 1);
  assert.equal(v.toolCalls.named, 1);
  const text = buildReport(run('r1', 't1', 'max', [[1, ['__proto__']]]));
  assert.match(text, /\| __proto__ \| 1 \|/);
});

test('the "(no name)" row is there only when some calls of a cell were not named', () => {
  const named = buildReport(run('t1.max', 't1', 'max', [[2, ['read_file', 'read_file']]]));
  assert.doesNotMatch(named, /\| \(no name\) \|/);
  const partly = buildReport(run('t1.max', 't1', 'max', [[3, ['read_file']]]));
  assert.match(partly, /\| \(no name\) \| 2 \|/);
});

test('a ledger with no tool name says so instead of an empty table', () => {
  const text = buildReport(run('t1.max', 't1', 'max', [2, 1]));
  assert.match(text, /Tool calls by tool: n\/a/);
});

test('the names of a cell are pooled across its runs', () => {
  const text = buildReport([
    ...run('t1.max', 't1', 'max', [[1, ['read_file']]]),
    ...run('t2.max', 't2', 'max', [[2, ['read_file', 'edit_file']]]),
  ]);
  assert.match(text, /\| read_file \| 2 \|/);
  assert.match(text, /\| edit_file \| 1 \|/);
});
