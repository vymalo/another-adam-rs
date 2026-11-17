import assert from 'node:assert/strict';
import { test } from 'node:test';
import { callStats, classifyRun, externalFailure } from '../lib/classify.mjs';

const call = (n, phase, extra = {}) => ({ kind: 'call', run: 'r', attempt: 1, call: n, phase, ...extra });
const ok = (n) => [call(n, 'pending'), call(n, 'settled', { costMicro: 5 })];
const bad = (n) => [call(n, 'pending'), call(n, 'failed', { status: 502 })];
const cut = (n) => [call(n, 'pending'), call(n, 'settled', { costMicro: 9, usageMissing: true, reason: 'timeout' })];
const refused = (n) => [call(n, 'refused')];

const stats = (records) => callStats(records, 'r', 1);

test('the meter\'s records of one attempt are counted: settled, failed, cut, refused, and how the last call ended', () => {
  const s = stats([...ok(1), ...bad(2), ...cut(3), ...refused(4), { ...call(1, 'settled'), run: 'other' }, { ...call(9, 'failed'), attempt: 2 }]);
  assert.deepEqual(s, { attempted: 4, settled: 1, upstreamErrors: 2, refused: 1, lastFailed: false });
  assert.equal(stats([...ok(1), ...bad(2)]).lastFailed, true);
  assert.equal(stats([...ok(1), ...cut(2)]).lastFailed, true, 'a call cut at a limit is an upstream error');
  assert.deepEqual(stats([]), { attempted: 0, settled: 0, upstreamErrors: 0, refused: 0, lastFailed: false });
});

const run = (records, over = {}) => classifyRun({ exit: 'done', stats: stats(records), ...over });

test('an agent that made no model call is infrastructure', () => {
  assert.equal(run([])?.reason, 'no-model-calls');
  assert.equal(run([], { exit: 'timeout' })?.reason, 'no-model-calls');
});

test('more than two upstream errors, or an error on the last call, is infrastructure; two errors in the middle are not', () => {
  assert.equal(run([...ok(1), ...bad(2), ...bad(3), ...bad(4), ...ok(5)])?.reason, 'upstream-errors');
  assert.equal(run([...ok(1), ...bad(2), ...bad(3), ...ok(4)]), null);
  assert.equal(run([...ok(1), ...ok(2), ...bad(3)])?.reason, 'last-call-failed');
  assert.equal(run([...ok(1), ...ok(2), ...cut(3)], { exit: 'timeout' })?.reason, 'last-call-failed');
});

test('a refused call is a limit, not an error: budget_hit and stopped_by_cap stand', () => {
  assert.equal(run([...ok(1), ...ok(2), ...refused(3)], { exit: 'budget_hit' }), null);
  assert.equal(run([...ok(1), ...refused(2)], { exit: 'stopped_by_cap' }), null);
  assert.equal(run([...refused(1)], { exit: 'budget_hit' }), null, 'refused at the first call: the budget was too small, not the harness');
});

test('the container runtime failing, or the coder reporting an external failure, is infrastructure', () => {
  assert.equal(run([...ok(1)], { runtimeFailed: true })?.reason, 'runtime');
  assert.equal(run([...ok(1)], { exit: 'failed', externalFailure: true })?.reason, 'agent-reported-external');
  assert.equal(run([...ok(1)], { exit: 'failed' }), null, 'a failure the agent owns stands');
  assert.equal(run([...ok(1), ...ok(2)]), null);
});

test('the coder\'s own words about a transient or external failure are recognised, a code failure is not', () => {
  const task = (text) => ({ status: { state: 'TASK_STATE_FAILED', message: { parts: [{ text }] } } });
  for (const t of ['the model endpoint is unavailable', 'upstream error: 502', 'connection reset by peer', 'the git server returned HTTP 503', 'transient failure, retried 3 times', 'database error: connection refused', 'the runtime is not reachable']) assert.equal(externalFailure(task(t)), true, t);
  for (const t of ['the checks failed three times', 'the turn limit was reached', '']) assert.equal(externalFailure(task(t)), false, t);
  assert.equal(externalFailure(null), false);
});
