import assert from 'node:assert/strict';
import { test } from 'node:test';
import { A2AClient, A2AError, NUDGE, USAGE_URI, driveTask } from '../lib/a2a.mjs';
import { TASK_ID, USAGE, fakeClock, fakeCoder } from './a2afix.mjs';

const MIN = 60_000;

async function drive(t, script, opts = {}, serverOpts = {}) {
  const server = await fakeCoder({ script, ...serverOpts });
  t.after(() => server.close());
  const clock = fakeClock();
  const logs = [];
  const client = new A2AClient({ url: server.url, bearer: server.bearer });
  const result = await driveTask({ client, text: 'do it', messageId: 'bench-r1-1', limitMs: 50 * MIN, now: clock.now, sleep: clock.sleep, log: (e) => logs.push(e), ...opts });
  return { server, clock, logs, result };
}

test('the usage extension URI is the one the coder lists', () => {
  assert.equal(USAGE_URI, USAGE);
});

test('a task that completes: polling reads the state, the extension and the bearer go with every call', async (t) => {
  const { server, result } = await drive(t, [{ state: 'working' }, { state: 'working' }, { state: 'completed', totals: [{ provider: 'openai', model: 'm', inputTokens: 100, outputTokens: 20, cachedInputTokens: 40 }, { provider: 'openai', model: 'n', inputTokens: 5, outputTokens: 1 }] }]);
  assert.equal(result.exit, 'done');
  assert.equal(result.taskId, TASK_ID);
  assert.equal(result.nudges, 0);
  assert.equal(server.of('GetTask').length, 3);
  assert.deepEqual(result.selfReported, { input: 105, output: 21, cached: 40, calls: 1 });
  for (const c of server.calls) {
    assert.equal(c.auth, `Bearer ${server.bearer}`);
    assert.equal(c.extensions, USAGE);
  }
  const send = server.of('SendStreamingMessage')[0].params.message;
  assert.deepEqual([send.messageId, send.role, send.parts], ['bench-r1-1', 'ROLE_USER', [{ text: 'do it' }]]);
});

test('polling decides: a stream that never ends does not hold the harness', async (t) => {
  const { result } = await drive(t, [{ state: 'working' }, { state: 'completed' }], {}, { holdStream: true });
  assert.equal(result.exit, 'done');
});

test('the first input-required gets the one fixed reply, in the same task', async (t) => {
  const { server, result } = await drive(t, [{ state: 'working' }, { state: 'input-required', ts: 'T1' }, { state: 'working' }, { state: 'completed' }]);
  assert.equal(result.exit, 'done');
  assert.equal(result.nudges, 1);
  const sends = server.of('SendStreamingMessage');
  assert.equal(sends.length, 2);
  assert.equal(sends[1].params.message.taskId, TASK_ID);
  assert.deepEqual(sends[1].params.message.parts, [{ text: 'Nobody can answer questions in this run; decide yourself and finish.' }]);
  assert.equal(NUDGE, sends[1].params.message.parts[0].text, 'the nudge is a constant, never built from the run');
  assert.equal(server.of('CancelTask').length, 0);
});

test('the second input-required is cancelled, after exactly one nudge', async (t) => {
  const { server, result } = await drive(t, [{ state: 'working' }, { state: 'input-required', ts: 'T1' }, { state: 'working' }, { state: 'input-required', ts: 'T2' }]);
  assert.equal(result.exit, 'input_required');
  assert.equal(result.nudges, 1);
  assert.equal(server.of('SendStreamingMessage').length, 2, 'one task message and one nudge, never a second nudge');
  assert.equal(server.of('CancelTask').length, 1);
  assert.deepEqual(server.of('CancelTask')[0].params, { id: TASK_ID });
});

test('the same input-required seen again is the same park, not a second one', async (t) => {
  const { server, result } = await drive(t, [{ state: 'input-required', ts: 'T1' }, { state: 'input-required', ts: 'T1' }, { state: 'input-required', ts: 'T1' }, { state: 'working' }, { state: 'completed' }]);
  assert.equal(result.exit, 'done');
  assert.equal(result.nudges, 1);
  assert.equal(server.of('CancelTask').length, 0);
});

test('a nudge that is never answered ends at the grace, cancelled', async (t) => {
  const { server, result, clock } = await drive(t, [{ state: 'input-required', ts: 'T1' }], { nudgeGraceMs: 2 * MIN });
  assert.equal(result.exit, 'input_required');
  assert.equal(server.of('CancelTask').length, 1);
  assert.ok(clock.elapsed() < 10 * MIN);
});

test('a nudge the server refuses is not sent again: one nudge ever, and the park ends at the grace, cancelled', async (t) => {
  const { server, result } = await drive(t, [{ state: 'input-required', ts: 'T1' }], { nudgeGraceMs: 60_000 }, { failSend: true });
  assert.equal(result.exit, 'input_required');
  assert.equal(server.of('SendStreamingMessage').length, 2, 'the task, and one nudge');
  assert.equal(server.of('CancelTask').length, 1);
});

test('a nudge that is delivered but answered slowly is not sent a second time (finding 18)', async (t) => {
  const { server, result } = await drive(t, [{ state: 'input-required', ts: 'T1' }, { state: 'working' }, { state: 'completed' }], {}, { followUpHang: true });
  assert.equal(result.exit, 'done');
  assert.equal(result.nudges, 1);
  assert.equal(server.of('SendStreamingMessage').length, 2, 'the task, and exactly one nudge');
});

test('every request has a timeout: a coder that never answers is an error, not a hang (finding 17)', async (t) => {
  const server = await fakeCoder({ script: [{ state: 'working' }], hangGetTask: true });
  t.after(() => server.close());
  const client = new A2AClient({ url: server.url, bearer: server.bearer, requestTimeoutMs: 80 });
  const started = Date.now();
  await assert.rejects(client.getTask('x'), (e) => e instanceof A2AError && !e.message.includes(server.bearer));
  assert.ok(Date.now() - started < 2000);
});

test('six polls that time out in a row end the run as infrastructure, quickly', async (t) => {
  const server = await fakeCoder({ script: [{ state: 'working' }], hangGetTask: true });
  t.after(() => server.close());
  const client = new A2AClient({ url: server.url, bearer: server.bearer, requestTimeoutMs: 40 });
  const clock = fakeClock();
  const result = await driveTask({ client, text: 'x', messageId: 'm', limitMs: 50 * MIN, now: clock.now, sleep: clock.sleep });
  assert.equal(result.exit, 'infra');
});

test('auth-required is a terminal state: the task is cancelled and the run is infrastructure (finding 26)', async (t) => {
  const { server, result } = await drive(t, [{ state: 'working' }, { state: 'auth-required' }]);
  assert.equal(result.exit, 'infra');
  assert.equal(server.of('CancelTask').length, 1);
});

test('the elapsed time is the time to the decision, without the grace of a cancel (finding 19)', async (t) => {
  const { result, clock } = await drive(t, [{ state: 'working' }], { limitMs: 3 * MIN, graceMs: 60_000 });
  assert.equal(result.exit, 'timeout');
  assert.ok(result.elapsedMs >= 3 * MIN && result.elapsedMs < 3 * MIN + 15_000, `elapsed ${result.elapsedMs}`);
  assert.ok(clock.elapsed() >= result.elapsedMs);
});

test('at the time limit the task is cancelled, and the exit is timeout', async (t) => {
  const { server, result, clock } = await drive(t, [{ state: 'working' }], { limitMs: 3 * MIN, pollMs: 10_000 });
  assert.equal(result.exit, 'timeout');
  assert.equal(server.of('CancelTask').length, 1);
  assert.ok(clock.elapsed() >= 3 * MIN && clock.elapsed() < 3 * MIN + 2 * MIN, `elapsed ${clock.elapsed()}`);
  assert.equal(result.finalState, 'canceled');
});

test('the poll never sleeps past the limit', async (t) => {
  const sleeps = [];
  const clock = fakeClock();
  const server = await fakeCoder({ script: [{ state: 'working' }] });
  t.after(() => server.close());
  const client = new A2AClient({ url: server.url, bearer: server.bearer });
  await driveTask({ client, text: 'x', messageId: 'm', limitMs: 25_000, pollMs: 10_000, now: clock.now, sleep: async (ms) => { sleeps.push(ms); await clock.sleep(ms); } });
  assert.deepEqual(sleeps.slice(0, 3), [10_000, 10_000, 5_000]);
});

test('a cancel that fails still ends the run as a timeout, and says so', async (t) => {
  const clock = fakeClock();
  const server = await fakeCoder({ script: [{ state: 'working' }] });
  t.after(() => server.close());
  const client = new A2AClient({ url: server.url, bearer: server.bearer });
  client.cancelTask = async () => {
    throw new A2AError('cancel failed', 500);
  };
  const result = await driveTask({ client, text: 'x', messageId: 'm', limitMs: 30_000, now: clock.now, sleep: clock.sleep, graceMs: 20_000 });
  assert.equal(result.exit, 'timeout');
  assert.equal(result.cancelFailed, true);
});

test('a meter refusal cancels the run and ends it budget_hit; a global stop ends it stopped_by_cap', async (t) => {
  for (const [why, exit] of [['budget', 'budget_hit'], ['cap', 'stopped_by_cap']]) {
    let n = 0;
    const { server, result } = await drive(t, [{ state: 'working' }], { refusal: () => (++n > 2 ? why : null) });
    assert.equal(result.exit, exit);
    assert.equal(server.of('CancelTask').length, 1);
  }
});

test('a refusal on a run that failed is the reason; on a run that completed it is not', async (t) => {
  const failed = await drive(t, [{ state: 'working' }, { state: 'failed' }], { refusal: () => 'budget' });
  assert.equal(failed.result.exit, 'budget_hit');
  const done = await drive(t, [{ state: 'completed' }], { refusal: () => 'budget' });
  assert.equal(done.result.exit, 'done');
});

test('terminal states map to exits', async (t) => {
  for (const [state, exit] of [['completed', 'done'], ['failed', 'failed'], ['canceled', 'canceled'], ['rejected', 'rejected']]) {
    assert.equal((await drive(t, [{ state }])).result.exit, exit, state);
  }
});

test('a few failed polls are tolerated; six in a row end the run as infrastructure', async (t) => {
  const fine = await drive(t, [{ state: 'completed' }], {}, { getTaskStatus: (i) => (i < 2 ? 503 : 200) });
  assert.equal(fine.result.exit, 'done');
  const dead = await drive(t, [{ state: 'working' }], {}, { getTaskStatus: () => 503 });
  assert.equal(dead.result.exit, 'infra');
  assert.equal(dead.server.of('GetTask').length, 6);
});

test('the bearer is in no log line, no result and no error, even when a transport error carries it', async (t) => {
  const { logs, result, server } = await drive(t, [{ state: 'working' }, { state: 'input-required', ts: 'T1' }, { state: 'completed' }]);
  const text = JSON.stringify({ logs, result });
  assert.ok(!text.includes(server.bearer));

  const secret = 'Bearer super-secret-bearer-value';
  const client = new A2AClient({
    url: 'http://127.0.0.1:1/',
    bearer: 'super-secret-bearer-value',
    fetch: async () => {
      throw Object.assign(new Error(`connect failed with headers {authorization: "${secret}"}`), { cause: new Error(secret) });
    },
  });
  const seen = [];
  await assert.rejects(client.getTask('x'), (e) => {
    seen.push(String(e.message), String(e.stack), JSON.stringify(e), String(e.cause));
    return e instanceof A2AError;
  });
  assert.ok(!seen.join('\n').includes('super-secret-bearer-value'));
  assert.ok(!JSON.stringify(client).includes('super-secret-bearer-value'), 'the client does not print its bearer');
});

test('a wrong bearer is an error that names no token', async (t) => {
  const server = await fakeCoder({ script: [{ state: 'working' }] });
  t.after(() => server.close());
  const client = new A2AClient({ url: server.url, bearer: 'wrong-bearer-value' });
  await assert.rejects(client.getTask('x'), (e) => e instanceof A2AError && e.status === 401 && !e.message.includes('wrong-bearer-value'));
});

test('a bearer or a URL that is not plain is refused', () => {
  assert.throws(() => new A2AClient({ url: 'http://127.0.0.1:1/', bearer: 'has space' }), /bearer/);
  assert.throws(() => new A2AClient({ url: 'http://user:pw@127.0.0.1:1/', bearer: 'abcdefgh' }), /url/);
  assert.throws(() => new A2AClient({ url: 'http://example.test:1/', bearer: 'abcdefgh' }), /loopback/);
});
