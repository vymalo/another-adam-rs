import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { Ledger, foldRecords } from '../lib/ledger.mjs';
import { NOT_CLAIMED, ReportError, buildReport } from '../lib/report.mjs';
import { rawConfig, tmp } from './helpers.mjs';

const BENCH = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'bin', 'bench.mjs');
const LABEL = 'Example Model';
const ID = 'example-model-identifier';

// Six tasks, two agents (a, b), two arms (max, off). 1 is a pass.
const PASSES = {
  'a/max': [1, 1, 1, 1, 0, 0],
  'a/off': [1, 1, 0, 0, 0, 0],
  'b/max': [1, 0, 0, 1, 0, 1],
  'b/off': [0, 0, 0, 0, 0, 0],
};
const LANGS = ['go', 'go', 'python', 'python', 'ts', 'ts'];

/** Records for one finished run: the meter's calls (one of 5000 micro-dollars) and the result. */
function finished(run, task, agent, arm, pass, i) {
  const base = { run, attempt: 1 };
  return [
    { kind: 'state', run, state: 'Planned', task, agent, arm, block: i, source: i === 5 ? 'own' : 'public', language: LANGS[i] },
    { kind: 'state', run, state: 'Reserved', reservedMicro: 3_000_000 },
    { kind: 'state', run, state: 'Preparing' },
    { kind: 'state', run, state: 'Running' },
    { ...base, kind: 'call', call: 1, phase: 'pending', worstMicro: 100_000 },
    { ...base, kind: 'call', call: 1, phase: 'settled', costMicro: 5_000, usage: { input: 1000, cached: 400, cacheWrite: 0, output: 200, reasoning: 50 }, usageMissing: false },
    { kind: 'state', run, state: 'Collecting' },
    { kind: 'state', run, state: 'Grading' },
    {
      kind: 'state',
      run,
      state: 'Done',
      result: { pass: pass === 1, grades: [pass, pass, pass], flaky: run === 'T3-a-max', exit: pass ? 'done' : 'timeout', wallMs: 60_000 + i * 1000, selfReported: { input: 900, output: 150, calls: 1 } },
    },
  ];
}

function fixture() {
  const records = [{ kind: 'setup', modelLabel: LABEL, seed: 7, taskLock: 'lock-1', adamRevision: 'abc1234', opencodeVersion: '1.19.x', limits: { minutes: 50 } }];
  for (const [cell, passes] of Object.entries(PASSES)) {
    const [agent, arm] = cell.split('/');
    passes.forEach((p, i) => records.push(...finished(`T${i + 1}-${agent}-${arm}`, `T${i + 1}`, agent, arm, p, i)));
  }
  // one run stopped by the cap after spending 8000, one never started
  records.push(
    { kind: 'state', run: 'T7-a-max', state: 'Planned', task: 'T7', agent: 'a', arm: 'max', source: 'public', language: 'go' },
    { kind: 'state', run: 'T7-a-max', state: 'Reserved', reservedMicro: 3_000_000 },
    { kind: 'state', run: 'T7-a-max', state: 'Preparing' },
    { kind: 'state', run: 'T7-a-max', state: 'Running' },
    { kind: 'call', run: 'T7-a-max', attempt: 1, call: 1, phase: 'settled', costMicro: 8_000, usage: null, usageMissing: true },
    { kind: 'state', run: 'T7-a-max', state: 'StoppedByCap' },
    { kind: 'state', run: 'T8-a-max', state: 'Planned', task: 'T8', agent: 'a', arm: 'max' },
    { kind: 'state', run: 'T8-a-max', state: 'NotStarted' },
  );
  return records.map((r, i) => ({ v: 1, t: new Date(Date.UTC(2026, 10, 17, 0, 0, i)).toISOString(), ...r }));
}

test('the report shows the model by its label and never by its id, unless asked', () => {
  const plain = buildReport(fixture(), { modelId: ID });
  assert.match(plain, new RegExp(LABEL));
  assert.ok(!plain.includes(ID));
  const shown = buildReport(fixture(), { modelId: ID, showModelId: true });
  assert.ok(shown.includes(ID));
  assert.ok(!buildReport(fixture(), { showModelId: true }).includes(ID), 'no id to show, none shown');
});

test('the report says what it does not claim', () => {
  const text = buildReport(fixture());
  assert.match(text, /## What this report does not claim/);
  assert.equal(NOT_CLAIMED.length, 6);
  for (const sentence of NOT_CLAIMED) assert.ok(text.includes(sentence), sentence);
});

test('pass rates carry Wilson intervals', () => {
  const text = buildReport(fixture());
  // a/max solved 4 of 6: Wilson 95% interval 30.0% to 90.3% (stats.test.mjs, by hand)
  assert.match(text, /\| a \| max \| 6 \| 4 \| 66\.7% \| 30\.0% to 90\.3% \|/);
  assert.match(text, /\| b \| off \| 6 \| 0 \| 0\.0% \| 0\.0% to 39\.0% \|/);
});

test('paired differences carry the exact McNemar test and a bootstrap interval', () => {
  const text = buildReport(fixture());
  // arm max, a vs b: a only T2, T3 (2); b only T6 (1); n = 3, p = 2 * (1 + 3) / 8 = 1
  assert.match(text, /\| max \| a vs b \| 6 \| 2 \| 1 \| 16\.7 points \|[^|]+\| 1\.0000 \|/);
  // arm off, a vs b: a only T1, T2 (2), b only none; p = 2 * 1 / 4 = 0.5
  assert.match(text, /\| off \| a vs b \| 6 \| 2 \| 0 \| 33\.3 points \|[^|]+\| 0\.5000 \|/);
  assert.match(text, /interval includes zero|interval excludes zero/);
  assert.equal(buildReport(fixture()), buildReport(fixture()), 'the same ledger gives the same report');
});

test('runs that did not finish are listed and never counted', () => {
  const text = buildReport(fixture());
  assert.match(text, /\| T7-a-max \| StoppedByCap \| T7 \| a \/ max \|/);
  assert.match(text, /\| T8-a-max \| NotStarted \| T8 \| a \/ max \|/);
  assert.match(text, /\| a \| max \| 6 \| 4 /, 'T7 is not among the six');
  assert.match(text, /Wasted \(interrupted, infrastructure failures, stopped by the cap\) \| \$0\.0080 \|/);
});

test('cost, tokens, time and the meter against what the agents reported', () => {
  const text = buildReport(fixture());
  // a/max: 6 counted runs of 5000 micro-dollars = $0.0300; 4 solved -> $0.0075 each
  assert.match(text, /\| a \| max \| \$0\.0300 \| \$0\.0075 \| 6000 \| 2400 \| 0 \| 1200 \| 300 \|/);
  assert.match(text, /none solved/, 'b/off solved nothing: no cost per solved task');
  assert.match(text, /\| Agent \| Runs \| Meter input \| Reported input \|/);
  assert.match(text, /\| a \| 12 \| 12000 \| 10800 \| 2400 \| 1800 \|/);
});

test('languages with few tasks carry a note, and flaky grades are named', () => {
  const text = buildReport(fixture());
  assert.match(text, /go: 2 tasks, fewer than 15, no conclusion/);
  assert.match(text, /Flaky grades[^\n]*T3-a-max/);
  assert.match(text, /\| own \|/);
});

test('the per-task table lists the four cells and where the agents differ', () => {
  const text = buildReport(fixture());
  assert.match(text, /\| T1 \| pass \| pass \| pass \| fail \|/);
  assert.match(text, /\| T6 \| fail \| fail \| pass \| fail \|/);
  assert.match(text, /Tasks where the agents differ inside an arm: [^\n]*T2 \(max\)/);
});

test('the command line builds report.md from a ledger and shows the label from the configuration', (t) => {
  const home = tmp(t);
  const ledger = Ledger.open(path.join(home, 'ledger.jsonl'));
  for (const r of fixture().filter((x) => x.kind !== 'setup')) {
    const { v, t: _t, ...rest } = r;
    ledger.append(rest);
  }
  ledger.close();
  const config = path.join(home, 'config.json');
  fs.writeFileSync(config, JSON.stringify(rawConfig({ modelId: ID, modelLabel: LABEL })));
  const env = { ...process.env, BENCH_HOME: home, BENCH_CONFIG: config, HOME: home };
  const r = spawnSync(process.execPath, [BENCH, 'report'], { env, encoding: 'utf8' });
  assert.equal(r.status, 0, r.stderr);
  const out = fs.readFileSync(path.join(home, 'report.md'), 'utf8');
  assert.match(out, new RegExp(LABEL));
  assert.ok(!out.includes(ID));
  assert.match(out, /\| Cap \| \$10\.0000 \|/);
  const shown = spawnSync(process.execPath, [BENCH, 'report', '--show-model-id', '--out', path.join(home, 'shown.md')], { env, encoding: 'utf8' });
  assert.equal(shown.status, 0, shown.stderr);
  assert.ok(fs.readFileSync(path.join(home, 'shown.md'), 'utf8').includes(ID));
});

test('an empty ledger is reported as such, not as an empty report', (t) => {
  const home = tmp(t);
  const r = spawnSync(process.execPath, [BENCH, 'report'], { env: { ...process.env, BENCH_HOME: home, HOME: home, BENCH_CONFIG: '' }, encoding: 'utf8' });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /no records/);
});

// ---- fix round: rows 14 and 15 of the review ----

const usage = { input: 1000, cached: 400, cacheWrite: 0, output: 200, reasoning: 50 };

/** One run with two attempts: the first interrupted after a call, the second done. */
function twoAttempts(run = 'T1-a-max', extra = {}) {
  const planned = { kind: 'state', run, state: 'Planned', task: 'T1', agent: 'a', arm: 'max', source: 'public', language: 'go', ...extra };
  const up = (attempt, call) => [
    { kind: 'state', run, state: 'Reserved', reservedMicro: 3_000_000 },
    { kind: 'state', run, state: 'Preparing' },
    { kind: 'state', run, state: 'Running' },
    { kind: 'call', run, attempt, call, phase: 'pending', worstMicro: 100_000 },
    { kind: 'call', run, attempt, call, phase: 'settled', costMicro: 5_000, usage, usageMissing: false },
  ];
  return [
    planned,
    ...up(1, 1),
    { kind: 'state', run, state: 'Interrupted' },
    { ...planned, retry: true },
    ...up(2, 1),
    { kind: 'state', run, state: 'Collecting' },
    { kind: 'state', run, state: 'Grading' },
    { kind: 'state', run, state: 'Done', result: { pass: true, grades: [1, 1, 1], exit: 'done', wallMs: 1000 } },
  ];
}

test('row 14: tokens and calls sum all attempts of a run, as the cost does, and the report says so', () => {
  const folded = foldRecords(twoAttempts());
  const v = folded.get('T1-a-max');
  assert.equal(v.spentMicro, 10_000);
  assert.deepEqual(v.tokens, { input: 2000, cached: 800, cacheWrite: 0, output: 400, reasoning: 100 });
  assert.equal(v.calls.settled, 2);
  const text = buildReport(twoAttempts());
  assert.match(text, /\| a \| max \| \$0\.0100 \| \$0\.0100 \| 2000 \| 800 \| 0 \| 400 \| 100 \|/);
  assert.match(text, /sums all attempts of a run/);
});

test('row 14: the report names the states that are left out of every rate, and lists those runs', () => {
  const text = buildReport(fixture());
  assert.match(text, /StoppedByCap, Failed_infra, NotStarted/);
  assert.match(text, /left out of every rate/i);
  assert.match(text, /\| T7-a-max \| StoppedByCap \| T7 \| a \/ max \|[^\n]*\$0\.0080/, 'the list says what each one wasted');
});

test('row 15: a cell with a pipe or a line break cannot break the table', () => {
  const text = buildReport(twoAttempts('T1-a-max', { task: 'T|1\nsecond line', agent: 'a|b' }));
  assert.ok(!/T\|1/.test(text), 'no unescaped pipe from a cell');
  assert.match(text, /^\| T\\\|1 second line \| pass \|$/m, 'the per-task row is one line with the pipe escaped');
  assert.match(text, /^\| a\\\|b \| max \| 1 \| 1 \|/m);
});

test('row 15: two Done runs for one task, agent and arm are an error that names them', () => {
  const records = [...twoAttempts('T1-a-max'), ...twoAttempts('T1-a-max-again')];
  assert.throws(
    () => buildReport(records),
    (e) => e instanceof ReportError && /T1-a-max/.test(e.message) && /T1-a-max-again/.test(e.message) && /T1/.test(e.message),
  );
  assert.doesNotThrow(() => buildReport(twoAttempts('T1-a-max')));
});

test('row 15: the command line says so and writes no report', (t) => {
  const home = tmp(t);
  const ledger = Ledger.open(path.join(home, 'ledger.jsonl'));
  for (const r of [...twoAttempts('T1-a-max'), ...twoAttempts('T1-a-max-again')]) {
    const { v, t: _t, ...rest } = r;
    ledger.append(rest);
  }
  ledger.close();
  const env = { ...process.env, BENCH_HOME: home, HOME: home, BENCH_CONFIG: '' };
  const r = spawnSync(process.execPath, [BENCH, 'report'], { env, encoding: 'utf8' });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /T1-a-max-again/);
  assert.ok(!fs.existsSync(path.join(home, 'report.md')));
});

test('a lone CR or a CRLF in a name leaves no CR in the report and rows stay on one line', () => {
  for (const task of ['T\rone', 'T\r\ntwo']) {
    const text = buildReport(twoAttempts('T1-a-max', { task, agent: 'a\rb' }));
    assert.ok(!text.includes('\r'), JSON.stringify(task));
    assert.match(text, /^\| T(one|two)? ?(one|two) \| pass \|$|^\| T one \| pass \|$|^\| T two \| pass \|$/m);
  }
});
