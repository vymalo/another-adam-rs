import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { test } from 'node:test';
import { Ledger, LedgerError, foldRecords, readLedger, resumeActions, resumeRecords } from '../lib/ledger.mjs';
import { tmp } from './helpers.mjs';

const clock = () => {
  let n = 0;
  return () => new Date(Date.UTC(2026, 10, 17, 0, 0, n++));
};
const open = (t, name = 'ledger.jsonl') => {
  const file = path.join(tmp(t), name);
  const ledger = Ledger.open(file, { now: clock() });
  t.after(() => ledger.close());
  return { ledger, file };
};
const state = (run, s, extra = {}) => ({ kind: 'state', run, state: s, ...extra });
const call = (run, n, phase, extra = {}) => ({ kind: 'call', run, attempt: 1, call: n, phase, ...extra });
const plan = (run) => state(run, 'Planned', { task: 'T1', agent: 'a', arm: 'max', source: 'public', language: 'go' });

function walk(ledger, run, ...states) {
  for (const s of states) {
    const extra = s === 'Planned' ? { task: 'T1', agent: 'a', arm: 'max', source: 'public', language: 'go' } : s === 'Reserved' ? { reservedMicro: 3_000_000 } : s === 'Done' ? { result: { pass: true } } : {};
    ledger.append(state(run, s, extra));
  }
}

test('records are appended as JSON lines and read back', (t) => {
  const { ledger, file } = open(t);
  ledger.append(plan('r1'));
  ledger.append(state('r1', 'Reserved', { reservedMicro: 5 }));
  const { records, warnings } = readLedger(file);
  assert.equal(records.length, 2);
  assert.deepEqual(warnings, []);
  assert.equal(records[0].v, 1);
  assert.equal(records[0].t, '2026-11-17T00:00:00.000Z');
  assert.equal(fs.readFileSync(file, 'utf8').split('\n').length, 3, 'one line per record');
});

test('a state must follow the lifecycle, and a run starts in Planned', (t) => {
  const { ledger } = open(t);
  assert.throws(() => ledger.append(state('r1', 'Running')), LedgerError);
  ledger.append(plan('r1'));
  assert.throws(() => ledger.append(state('r1', 'Running')), /cannot become/);
  assert.throws(() => ledger.append(state('r1', 'Reserved')), /reservedMicro/);
  walk(ledger, 'r1', 'Reserved', 'Preparing', 'Running', 'Collecting', 'Grading', 'Grading', 'Done');
  assert.throws(() => ledger.append(state('r1', 'Planned')), LedgerError, 'Done is final');
  assert.throws(() => ledger.append(state('r2', 'Done')), LedgerError);
});

test('the fold gives the state, the spend and what was wasted', (t) => {
  const { ledger } = open(t);
  ledger.append(plan('r1'));
  walk(ledger, 'r1', 'Reserved', 'Preparing', 'Running');
  ledger.append(call('r1', 1, 'pending', { worstMicro: 900 }));
  ledger.append(call('r1', 1, 'settled', { costMicro: 400, usage: { input: 10, cached: 4, cacheWrite: 0, output: 5, reasoning: 2 } }));
  ledger.append(call('r1', 2, 'pending', { worstMicro: 700 })); // never settled: the harness died with it in flight
  ledger.append(call('r1', 3, 'refused', { reason: 'run-budget', worstMicro: 5000 }));
  const v = ledger.fold().get('r1');
  assert.equal(v.state, 'Running');
  assert.equal(v.spentMicro, 400 + 700, 'a pending call counts its worst case');
  assert.equal(v.attemptSpentMicro, 1100);
  assert.equal(v.reservationActive, true);
  assert.deepEqual(v.tokens, { input: 10, cached: 4, cacheWrite: 0, output: 5, reasoning: 2 });
  assert.equal(v.calls.refused, 1);

  ledger.append(state('r1', 'Interrupted'));
  const after = ledger.fold().get('r1');
  assert.equal(after.wastedMicro, 1100);
  assert.equal(after.reservationActive, false);
  ledger.append(plan('r1'));
  const again = ledger.fold().get('r1');
  assert.equal(again.attempt, 2);
  assert.equal(again.priorSpentMicro, 1100);
  assert.equal(again.attemptSpentMicro, 0);
});

test('a torn last line is ignored with a warning and cut off, so the next record starts clean', (t) => {
  const { ledger, file } = open(t);
  ledger.append(plan('r1'));
  ledger.close();
  fs.appendFileSync(file, '{"v":1,"kind":"state","run":"r1","sta');
  const warnings = [];
  const reopened = Ledger.open(file, { onWarning: (w) => warnings.push(w) });
  t.after(() => reopened.close());
  assert.equal(warnings.length, 1);
  assert.match(warnings[0], /torn/);
  assert.equal(reopened.records.length, 1);
  reopened.append(state('r1', 'Reserved', { reservedMicro: 1 }));
  const { records, warnings: again } = readLedger(file);
  assert.equal(records.length, 2);
  assert.deepEqual(again, []);
});

test('a bad line before the last is an error, not a torn tail', (t) => {
  const file = path.join(tmp(t), 'ledger.jsonl');
  fs.writeFileSync(file, '{"kind":"setup"}\nnot json\n{"kind":"setup"}\n');
  assert.throws(() => readLedger(file), (e) => e instanceof LedgerError && /line 2/.test(e.message));
  assert.throws(() => Ledger.open(file), LedgerError);
  assert.ok(!fs.existsSync(`${file}.lock`), 'a failed open leaves no lock');
});

test('a last line that is complete but lacks its newline is kept, and the next append starts on a new line', (t) => {
  const file = path.join(tmp(t), 'ledger.jsonl');
  fs.writeFileSync(file, JSON.stringify({ v: 1, kind: 'state', run: 'r1', state: 'Planned' }));
  const ledger = Ledger.open(file);
  t.after(() => ledger.close());
  ledger.append(state('r1', 'NotStarted'));
  assert.equal(readLedger(file).records.length, 2);
});

test('one writer at a time: a live lock refuses, a stale one is taken over', (t) => {
  const { ledger, file } = open(t);
  fs.writeFileSync(`${file}.lock`, `${process.ppid}\n`); // another live process (our parent)
  ledger.close();
  fs.writeFileSync(`${file}.lock`, `${process.ppid}\n`);
  assert.throws(() => Ledger.open(file), /in use/);
  fs.writeFileSync(`${file}.lock`, '2147483646\n'); // no such process
  const taken = Ledger.open(file);
  taken.close();
  assert.ok(!fs.existsSync(`${file}.lock`));
});

// The resume rules: what to do with each run when the harness starts again.
function ledgerWithRuns(t) {
  const { ledger } = open(t);
  const runs = {
    planned: ['Planned'],
    reserved: ['Planned', 'Reserved'],
    preparing: ['Planned', 'Reserved', 'Preparing'],
    running: ['Planned', 'Reserved', 'Preparing', 'Running'],
    collecting: ['Planned', 'Reserved', 'Preparing', 'Running', 'Collecting'],
    grading: ['Planned', 'Reserved', 'Preparing', 'Running', 'Collecting', 'Grading'],
    done: ['Planned', 'Reserved', 'Preparing', 'Running', 'Collecting', 'Grading', 'Done'],
    notstarted: ['Planned', 'NotStarted'],
    stopped: ['Planned', 'Reserved', 'Preparing', 'Running', 'StoppedByCap'],
    interrupted: ['Planned', 'Reserved', 'Preparing', 'Running', 'Interrupted'],
    infra1: ['Planned', 'Reserved', 'Preparing', 'Failed_infra'],
    infra3: ['Planned', 'Reserved', 'Preparing', 'Failed_infra', 'Planned', 'Reserved', 'Preparing', 'Failed_infra', 'Planned', 'Reserved', 'Preparing', 'Failed_infra'],
  };
  for (const [run, states] of Object.entries(runs)) walk(ledger, run, ...states);
  return ledger;
}

test('resume: every rule', (t) => {
  const ledger = ledgerWithRuns(t);
  const actions = Object.fromEntries(resumeActions(ledger.fold()).map((a) => [a.run, a.action]));
  assert.deepEqual(actions, {
    planned: 'plan',
    reserved: 'rerun',
    preparing: 'rerun',
    running: 'rerun',
    collecting: 'rerun',
    grading: 'regrade',
    done: 'skip',
    notstarted: 'skip',
    stopped: 'skip',
    interrupted: 'rerun',
    infra1: 'retry',
    infra3: 'give-up',
  });
  assert.equal(resumeActions(ledger.fold(), { maxInfraRetries: 3 }).find((a) => a.run === 'infra3').action, 'retry');
});

test('resume: the records it asks for are valid transitions, and a Done run is never touched', (t) => {
  const ledger = ledgerWithRuns(t);
  const folded = ledger.fold();
  ledger.append(call('running', 1, 'settled', { costMicro: 123 }));
  const actions = resumeActions(ledger.fold());
  const records = resumeRecords(actions, ledger.fold());
  for (const r of records) ledger.append(r); // would throw on an invalid transition
  const after = ledger.fold();
  assert.equal(after.get('running').state, 'Planned');
  assert.equal(after.get('running').attempt, 2);
  assert.equal(after.get('running').wastedMicro, 123, 'the interrupted attempt stays as waste');
  assert.equal(after.get('running').task, 'T1', 'the plan carries over');
  assert.equal(after.get('interrupted').state, 'Planned');
  assert.equal(after.get('infra1').state, 'Planned');
  assert.equal(after.get('infra3').state, 'Failed_infra', 'no retry left');
  assert.equal(after.get('grading').state, 'Grading');
  assert.equal(after.get('grading').attempt, 1, 'a Grading run is not run again');
  assert.equal(after.get('done').state, 'Done');
  assert.equal(after.get('done').history.length, folded.get('done').history.length);
  assert.equal(after.get('stopped').state, 'StoppedByCap');
  assert.equal(after.get('planned').state, 'Planned');
});

test('foldRecords ignores setup records and works on plain arrays', () => {
  const v = foldRecords([{ kind: 'setup', modelLabel: 'x' }, state('r', 'Planned')]);
  assert.equal(v.size, 1);
  assert.equal(v.get('r').state, 'Planned');
});

// ---- fix round: rows 7, 8, 9 of the review ----

const LEDGER_MODULE = new URL('../lib/ledger.mjs', import.meta.url).href;

test('row 7: a second open of the same ledger in one process fails, and the first keeps its lock', (t) => {
  const { ledger, file } = open(t);
  assert.throws(() => Ledger.open(file), (e) => e instanceof LedgerError && /in use by this process/.test(e.message));
  assert.equal(fs.readFileSync(`${file}.lock`, 'utf8').trim(), String(process.pid), 'the failed open did not touch the lock');
  ledger.append(plan('r1')); // the first is still the writer
  ledger.close();
  assert.ok(!fs.existsSync(`${file}.lock`));
  const again = Ledger.open(file);
  again.close();
});

test('row 7: the lock never exists without its pid, and no lock file is left behind', (t) => {
  const { ledger, file } = open(t);
  assert.match(fs.readFileSync(`${file}.lock`, 'utf8'), /^\d+\n$/);
  ledger.close();
  assert.deepEqual(fs.readdirSync(path.dirname(file)).filter((f) => f.includes('.lock')), [], 'no temp file of the lock stays');
});

test('row 7: processes racing for one ledger: exactly one wins', async (t) => {
  const file = path.join(tmp(t), 'ledger.jsonl');
  const startAt = Date.now() + 800;
  const script = `
    import { Ledger } from ${JSON.stringify(LEDGER_MODULE)};
    const wait = ${startAt} - Date.now();
    await new Promise((r) => setTimeout(r, Math.max(0, wait)));
    try {
      const l = Ledger.open(${JSON.stringify(file)});
      process.stdout.write('WON\\n');
      await new Promise((r) => setTimeout(r, 1500));
      l.close();
    } catch (e) {
      process.stdout.write('LOST ' + e.message + '\\n');
    }`;
  const spawnChild = () =>
    new Promise((resolve) => {
      const child = spawn(process.execPath, ['--input-type=module', '-e', script], { stdio: ['ignore', 'pipe', 'inherit'] });
      let out = '';
      child.stdout.on('data', (d) => (out += d));
      child.on('close', () => resolve(out.trim()));
    });
  const results = await Promise.all(Array.from({ length: 6 }, spawnChild));
  assert.equal(results.filter((r) => r === 'WON').length, 1, results.join(' | '));
  assert.equal(results.filter((r) => r.startsWith('LOST')).length, 5, results.join(' | '));
});

test('row 8: a complete last line that does not parse stops the harness with its line number and nothing is cut', (t) => {
  const file = path.join(tmp(t), 'ledger.jsonl');
  const good = JSON.stringify({ v: 1, kind: 'state', run: 'r1', state: 'Planned' });
  const content = `${good}\nthis is not json\n`;
  fs.writeFileSync(file, content);
  assert.throws(() => readLedger(file), (e) => e instanceof LedgerError && /line 2/.test(e.message));
  assert.throws(() => Ledger.open(file), (e) => e instanceof LedgerError && /line 2/.test(e.message));
  assert.equal(fs.readFileSync(file, 'utf8'), content, 'the file is as it was');
  assert.ok(!fs.existsSync(`${file}.lock`));
  // a complete line that is JSON but no record is corrupt too
  fs.writeFileSync(file, `${good}\n{"no":"kind"}\n`);
  assert.throws(() => readLedger(file), /line 2/);
});

test('row 8: only a last line without its newline is torn', (t) => {
  const file = path.join(tmp(t), 'ledger.jsonl');
  const good = JSON.stringify({ v: 1, kind: 'state', run: 'r1', state: 'Planned' });
  fs.writeFileSync(file, `${good}\n{"v":1,"kind":"sta`);
  const { records, warnings, goodBytes } = readLedger(file);
  assert.equal(records.length, 1);
  assert.equal(warnings.length, 1);
  assert.equal(goodBytes, Buffer.byteLength(good) + 1);
});

const planned = (ledger, run) => walk(ledger, run, 'Planned');
const interrupted = (ledger, run, ...more) => walk(ledger, run, 'Reserved', 'Preparing', 'Running', ...more);

test('row 9: the third interruption ends the run Failed_infra for good, with its spend counted once', (t) => {
  const { ledger } = open(t);
  planned(ledger, 'r');
  interrupted(ledger, 'r');
  ledger.append(call('r', 1, 'settled', { costMicro: 777 }));
  walk(ledger, 'r', 'Interrupted'); // 1
  walk(ledger, 'r', 'Planned');
  interrupted(ledger, 'r', 'Interrupted'); // 2
  walk(ledger, 'r', 'Planned');
  interrupted(ledger, 'r'); // running when the harness dies the third time
  ledger.append({ kind: 'call', run: 'r', attempt: 3, call: 1, phase: 'settled', costMicro: 400 });
  const actions = resumeActions(ledger.fold());
  assert.equal(actions[0].action, 'give-up');
  assert.equal(actions[0].reason, 'interrupted');
  const records = resumeRecords(actions, ledger.fold());
  assert.deepEqual(records.map((r) => r.state), ['Interrupted', 'Failed_infra']);
  for (const r of records) ledger.append(r);
  const v = ledger.fold().get('r');
  assert.equal(v.state, 'Failed_infra');
  assert.equal(v.interruptions, 3);
  assert.equal(v.wastedMicro, 777 + 400, 'every attempt counted once, the last not twice');
  // the next start finds it final and writes nothing
  const next = resumeActions(ledger.fold());
  assert.equal(next[0].action, 'give-up');
  assert.deepEqual(resumeRecords(next, ledger.fold()), []);
});

test('row 9: two interruptions are still restarted; a run that was already Interrupted twice is restarted once more', (t) => {
  const { ledger } = open(t);
  planned(ledger, 'two');
  interrupted(ledger, 'two', 'Interrupted');
  walk(ledger, 'two', 'Planned');
  interrupted(ledger, 'two'); // the second, in flight now
  planned(ledger, 'both');
  interrupted(ledger, 'both', 'Interrupted');
  walk(ledger, 'both', 'Planned');
  interrupted(ledger, 'both', 'Interrupted'); // two recorded, last state Interrupted
  const a = Object.fromEntries(resumeActions(ledger.fold()).map((x) => [x.run, x.action]));
  assert.deepEqual(a, { two: 'rerun', both: 'rerun' });
  assert.equal(resumeActions(ledger.fold(), { maxInterruptions: 1 }).find((x) => x.run === 'two').action, 'give-up');
});

test('row 9: a run in Interrupted can end Failed_infra directly', (t) => {
  const { ledger } = open(t);
  planned(ledger, 'r');
  interrupted(ledger, 'r', 'Interrupted');
  ledger.append(state('r', 'Failed_infra', { final: true }));
  assert.equal(ledger.fold().get('r').final, true);
});
