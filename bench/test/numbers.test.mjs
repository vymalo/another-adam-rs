import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { CHECK_TOOLS, computeNumbers, formatNumbers, ratioOf } from '../lib/numbers.mjs';
import { tmp } from './helpers.mjs';

const BENCH = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'bin', 'bench.mjs');

/**
 * A ledger with two agents over two tasks, made so each figure is a round number:
 *
 *   adam      6 settled calls over 2 tasks (3.00 per task); cache 3920 + 1800 of 4000 + 2000;
 *             3 named tool calls, one of them `run_checks`
 *   opencode  3 settled calls over 2 tasks (1.50 per task); cache 500 + 250 of 1000 + 500;
 *             3 named tool calls, one of them `run_checks`
 */
function records() {
  const out = [{ kind: 'setup', modelLabel: 'Example Model', seed: 1 }];
  const planned = (run, task, agent) => out.push({ kind: 'state', run, state: 'Planned', task, agent, arm: 'max', source: 'own', language: 'go' });
  const done = (run) => out.push({ kind: 'state', run, state: 'Done', result: { pass: true, grades: [1, 1, 1], flaky: false, exit: 'done', wallMs: 1000 } });
  const call = (run, n, { prompt = 'main', input = 1000, cached = 0, toolNames = [] } = {}) => {
    out.push({ kind: 'call', run, attempt: 1, call: n, phase: 'pending', worstMicro: 100, prompt });
    out.push({ kind: 'call', run, attempt: 1, call: n, phase: 'settled', costMicro: 1, usage: { input, cached, cacheWrite: 0, output: 1, reasoning: 0 }, usageMissing: false, ...(toolNames.length > 0 ? { toolNames } : {}) });
  };

  planned('T1-adam', 'T1', 'adam');
  call('T1-adam', 1, { cached: 980, toolNames: ['read_file'] });
  call('T1-adam', 2, { cached: 980, toolNames: ['run_checks'] });
  call('T1-adam', 3, { cached: 980, toolNames: ['write_file'] });
  call('T1-adam', 4, { cached: 980 });
  done('T1-adam');

  planned('T2-adam', 'T2', 'adam');
  call('T2-adam', 1, { cached: 900 });
  call('T2-adam', 2, { cached: 900 });
  done('T2-adam');

  planned('T1-opencode', 'T1', 'opencode');
  call('T1-opencode', 1, { input: 500, cached: 250, toolNames: ['read_file'] });
  call('T1-opencode', 2, { input: 500, cached: 250, toolNames: ['run_checks'] });
  done('T1-opencode');

  planned('T2-opencode', 'T2', 'opencode');
  call('T2-opencode', 1, { input: 500, cached: 250, toolNames: ['read_file'] });
  done('T2-opencode');

  return out;
}

test('the three numbers come out of the call records, per agent', () => {
  const n = computeNumbers(records());

  assert.deepEqual(n.runs, { finished: 4, total: 4 });
  const adam = n.agents.get('adam');
  assert.equal(adam.tasks, 2);
  assert.equal(adam.calls, 6);
  assert.equal(adam.callsPerTask, 3);
  assert.deepEqual({ input: adam.cache.input, cached: adam.cache.cached }, { input: 6000, cached: 5720 });
  assert.equal(adam.tools.named, 3);
  assert.equal(adam.tools.checks, 1);
  assert.equal(adam.tools.share, 1 / 3);

  const opencode = n.agents.get('opencode');
  assert.equal(opencode.callsPerTask, 1.5);
  assert.equal(opencode.cache.share, 750 / 1500);
  assert.equal(opencode.tools.share, 1 / 3);

  assert.equal(ratioOf(n, 'adam', 'opencode'), 2);
});

test('a row dropped changes the figure — this is the case that goes red when the fold stops counting a record', () => {
  const before = computeNumbers(records());
  const without = records();
  // Drop one settled call of a finished run: `T1-adam` call 2.
  const at = without.findIndex((r) => r.kind === 'call' && r.run === 'T1-adam' && r.call === 2 && r.phase === 'settled');
  assert.notEqual(at, -1);
  without.splice(at, 1);

  const after = computeNumbers(without);
  const adam = after.agents.get('adam');
  assert.equal(adam.calls, 5, 'the settled call of a finished run no longer counts');
  assert.equal(adam.callsPerTask, 2.5);
  assert.equal(adam.cache.cached, 5720 - 980);
  assert.equal(adam.tools.checks, 0);
  assert.notEqual(formatNumbers(after), formatNumbers(before));
});

test('a run that did not finish is not in the numbers, and a child session is out of the cache share', () => {
  const rs = records();
  // An unfinished run: its calls must not move any figure.
  rs.push({ kind: 'state', run: 'T9-adam', state: 'Planned', task: 'T9', agent: 'adam', arm: 'max' });
  rs.push({ kind: 'call', run: 'T9-adam', attempt: 1, call: 1, phase: 'pending', worstMicro: 100, prompt: 'main' });
  rs.push({ kind: 'call', run: 'T9-adam', attempt: 1, call: 1, phase: 'settled', costMicro: 1, usage: { input: 1000, cached: 1000, cacheWrite: 0, output: 1, reasoning: 0 }, usageMissing: false });
  const n = computeNumbers(rs);
  assert.equal(n.agents.get('adam').calls, 6);
  assert.equal(n.agents.get('adam').cache.share, 5720 / 6000);
  assert.equal(n.runs.total, 5);

  // A subagent: another prompt fingerprint. It counts as a call, never as the fixed prompt's cache.
  const withChild = records();
  withChild.push({ kind: 'call', run: 'T2-adam', attempt: 1, call: 3, phase: 'pending', worstMicro: 100, prompt: 'child' });
  withChild.push({ kind: 'call', run: 'T2-adam', attempt: 1, call: 3, phase: 'settled', costMicro: 1, usage: { input: 1000, cached: 0, cacheWrite: 0, output: 1, reasoning: 0 }, usageMissing: false });
  const c = computeNumbers(withChild).agents.get('adam');
  assert.equal(c.calls, 7);
  assert.deepEqual({ input: c.cache.input, cached: c.cache.cached }, { input: 6000, cached: 5720 });
});

test('the check tool is the coder\'s run_checks, and the basis says time is not in the ledger', () => {
  assert.deepEqual([...CHECK_TOOLS], ['run_checks']);
  const n = computeNumbers(records());
  assert.match(n.basis.checksShare, /NOT a share of tool time/);
  const text = formatNumbers(n, { ledger: 'example.jsonl' });
  assert.match(text, /adam \| 2 \| 6 \| 3 \| 95\.3% \(5720 \/ 6000\) \| 33\.3% \(1 \/ 3\)/);
  assert.match(text, /Calls per task, adam over opencode: 2\.00x/);
});

test('bench numbers prints the table for a ledger, and refuses an empty one', (t) => {
  const dir = tmp(t);
  const file = path.join(dir, 'ledger.jsonl');
  fs.writeFileSync(file, `${records().map((r) => JSON.stringify(r)).join('\n')}\n`);

  const ok = spawnSync(process.execPath, [BENCH, 'numbers', '--ledger', file], { encoding: 'utf8' });
  assert.equal(ok.status, 0, ok.stderr);
  assert.match(ok.stdout, /adam \| 2 \| 6 \| 3 /);
  assert.match(ok.stdout, /opencode \| 2 \| 3 \| 1\.50 /);

  const empty = path.join(dir, 'empty.jsonl');
  fs.writeFileSync(empty, '');
  const none = spawnSync(process.execPath, [BENCH, 'numbers', '--ledger', empty], { encoding: 'utf8' });
  assert.equal(none.status, 1);
  assert.match(none.stderr, /no records/);
});
