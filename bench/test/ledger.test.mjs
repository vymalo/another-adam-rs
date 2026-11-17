import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
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
