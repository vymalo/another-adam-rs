import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { CapBook } from '../lib/cap.mjs';
import { normalizeConfig } from '../lib/config.mjs';
import { Ledger, readLedger } from '../lib/ledger.mjs';
import { MAX_OUTPUT_TOKENS, createMeter, deepMerge, prepareBody } from '../lib/meter.mjs';
import { createScriptedModel } from '../lib/scripted-model.mjs';
import { MODEL_ID, TEST_KEY, rawConfig, tmp } from './helpers.mjs';

// The scripted model reports: prompt 1000 (400 cached), completion 200 (50 of them reasoning).
// With prices 3 / 0.3 / 3.75 / 15 USD per million tokens a call costs
//   600 * 3e6 + 400 * 3e5 + 200 * 15e6 = 1_800_000_000 + 120_000_000 + 3_000_000_000 = 4_920_000_000
// and 4_920_000_000 / 1_000_000 = 4920 micro-dollars.
const SCRIPTED_COST = 4920;

async function setup(t, { config = {}, behavior = {}, reserve = 3_000_000, cap = 10_000_000, key = TEST_KEY } = {}) {
  const model = createScriptedModel({ expectKey: key, ...behavior });
  const { url } = await model.listen();
  const cfg = normalizeConfig(rawConfig({ endpoint: url, ...config }));
  const dir = tmp(t);
  const ledger = Ledger.open(path.join(dir, 'ledger.jsonl'));
  const book = new CapBook(cap);
  const events = [];
  const meter = createMeter({ config: cfg, ledger, book, env: { BENCH_MODEL_API_KEY: key }, log: (e) => events.push(e) });
  await meter.listen();
  t.after(async () => {
    await meter.close();
    await model.close();
    ledger.close();
  });
  const open = (run = 'r1', arm = 'max') => {
    assert.equal(book.reserve(run, reserve), true);
    ledger.append({ kind: 'state', run, state: 'Planned', task: 'T1', agent: 'a', arm });
    ledger.append({ kind: 'state', run, state: 'Reserved', reservedMicro: reserve });
    ledger.append({ kind: 'state', run, state: 'Preparing' });
    ledger.append({ kind: 'state', run, state: 'Running' });
    return meter.openRun({ run, arm });
  };
  return { model, cfg, ledger, book, meter, events, open, url, dir };
}

const chat = (baseUrl, body, headers = {}) =>
  fetch(`${baseUrl}/chat/completions`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', ...headers },
    body: JSON.stringify(body),
  });

const callRecords = (ledger, phase) => ledger.records.filter((r) => r.kind === 'call' && r.phase === phase);

test('a plain call: the key is injected, the model rewritten, the fragment merged, the clamp applied, the cost written', async (t) => {
  const s = await setup(t);
  const { baseUrl } = s.open();
  const res = await chat(
    baseUrl,
    { model: 'whatever-the-agent-says', messages: [{ role: 'user', content: 'hi' }], max_tokens: 99_999, reasoning_effort: 'low' },
    { authorization: 'Bearer the-clients-own-key' },
  );
  assert.equal(res.status, 200);
  assert.equal((await res.json()).choices[0].message.content, 'done');

  assert.equal(s.model.requests.length, 1);
  const seen = s.model.requests[0];
  assert.equal(seen.headers.authorization, `Bearer ${TEST_KEY}`, 'the meter injected the key');
  assert.ok(!JSON.stringify(seen).includes('the-clients-own-key'), 'the client key never reached the endpoint');
  assert.equal(seen.body.model, MODEL_ID, 'model rewritten');
  assert.equal(seen.body.reasoning_effort, 'high', 'the arm fragment wins over the client');
  assert.equal(seen.body.max_tokens, MAX_OUTPUT_TOKENS, 'clamped');
  assert.deepEqual(seen.body.messages, [{ role: 'user', content: 'hi' }]);

  const settled = callRecords(s.ledger, 'settled');
  assert.equal(callRecords(s.ledger, 'pending').length, 1, 'a pending record came first');
  assert.equal(settled.length, 1);
  assert.equal(settled[0].costMicro, SCRIPTED_COST);
  assert.equal(settled[0].usageMissing, false);
  assert.deepEqual(settled[0].usage, { input: 1000, output: 200, reasoning: 50, cached: 400, cacheWrite: 0 });
  assert.equal(s.book.runView('r1').spentMicro, SCRIPTED_COST);
  assert.equal(s.book.runView('r1').pendingMicro, 0);
});

test('a stream: relayed, include_usage forced, usage read from the last chunk', async (t) => {
  const s = await setup(t);
  const { baseUrl } = s.open();
  const res = await chat(baseUrl, { messages: [], stream: true, stream_options: { include_usage: false, other: 1 } });
  assert.equal(res.status, 200);
  assert.match(res.headers.get('content-type'), /text\/event-stream/);
  const text = await res.text();
  assert.match(text, /data: \[DONE\]/);
  assert.match(text, /"content":"do"/);
  assert.deepEqual(s.model.requests[0].body.stream_options, { include_usage: true, other: 1 });
  assert.equal(callRecords(s.ledger, 'settled')[0].costMicro, SCRIPTED_COST);
  assert.equal(s.book.runView('r1').spentMicro, SCRIPTED_COST);
});

test('a stream is relayed as it arrives, not buffered whole', async (t) => {
  const s = await setup(t, { behavior: { chunkDelayMs: 250 } });
  const { baseUrl } = s.open();
  const started = Date.now();
  const res = await chat(baseUrl, { messages: [], stream: true });
  const reader = res.body.getReader();
  await reader.read();
  const firstAt = Date.now() - started;
  while (!(await reader.read()).done) {
    // drain
  }
  const total = Date.now() - started;
  assert.ok(total >= 700, `the scripted stream takes about 4 * 250 ms (took ${total})`);
  assert.ok(firstAt < total / 2, `the first chunk came at ${firstAt} ms of ${total} ms`);
});

test('no usage: the worst case is charged and the call is flagged, plain and stream', async (t) => {
  const s = await setup(t, { behavior: { omitUsage: true } });
  const { baseUrl } = s.open();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }] })).text();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }], stream: true })).text();
  const settled = callRecords(s.ledger, 'settled');
  assert.equal(settled.length, 2);
  for (const r of settled) {
    assert.equal(r.usageMissing, true);
    assert.equal(r.usage, null);
    assert.ok(r.costMicro > SCRIPTED_COST * 10, 'the worst case is far above a real call');
    assert.equal(r.costMicro, callRecords(s.ledger, 'pending').find((p) => p.call === r.call).worstMicro);
  }
  assert.equal(s.ledger.fold().get('r1').calls.usageMissing, 2);
});

test('a call whose worst case does not fit the reservation is refused with 429 and never forwarded', async (t) => {
  // the worst case of a small request is about 8192 * 15 USD / 1e6 = 122_880 micro-dollars
  const s = await setup(t, { reserve: 100_000 });
  const { baseUrl } = s.open();
  const res = await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }] });
  assert.equal(res.status, 429);
  assert.match(await res.text(), /bench budget reached/);
  assert.equal(s.model.requests.length, 0, 'nothing was forwarded');
  assert.equal(callRecords(s.ledger, 'refused').length, 1);
  assert.equal(callRecords(s.ledger, 'pending').length, 0);
  assert.deepEqual(s.meter.status('r1'), { calls: 1, refusal: 'budget', closed: false });
  assert.equal(s.book.runView('r1').spentMicro, 0);
});

test('the reservation holds across calls: the calls that fit go, the next one does not', async (t) => {
  // The worst case of this small request is 8192 * 15 USD / 1e6 = 122_880 plus about 110 for the
  // input: 122_990. With 130_000 reserved the first call fits (0 + 122_990) and so does the second
  // (4_920 spent + 122_990 = 127_910); the third (9_840 + 122_990 = 132_830) does not.
  const s = await setup(t, { reserve: 130_000 });
  const { baseUrl } = s.open();
  assert.equal((await chat(baseUrl, { messages: [] })).status, 200);
  assert.equal((await chat(baseUrl, { messages: [] })).status, 200);
  assert.equal((await chat(baseUrl, { messages: [] })).status, 429);
  assert.equal(s.model.requests.length, 2);
  assert.equal(s.book.runView('r1').spentMicro, 2 * SCRIPTED_COST);
});

test('the global stop refuses a call that would fit its own reservation', async (t) => {
  const s = await setup(t, { cap: 500_000, reserve: 400_000 });
  const { baseUrl } = s.open();
  s.book.endCall('r1', 0, 500_000); // the spend reached the cap by another road
  const res = await chat(baseUrl, { messages: [] });
  assert.equal(res.status, 429);
  assert.equal(s.meter.status('r1').refusal, 'cap');
  assert.equal(s.model.requests.length, 0);
});

test('an unknown token, a wrong path and a wrong method are 404 and nothing is forwarded', async (t) => {
  const s = await setup(t);
  const { token, baseUrl } = s.open();
  const root = baseUrl.slice(0, baseUrl.indexOf('/r/'));
  const attempts = [
    ['POST', `${root}/r/${'A'.repeat(32)}/v1/chat/completions`],
    ['POST', `${root}/v1/chat/completions`],
    ['POST', `${root}/r/short/v1/chat/completions`],
    ['POST', `${baseUrl}/responses`],
    ['POST', `${baseUrl}/embeddings`],
    ['POST', `${baseUrl}/chat/completions/extra`],
    ['POST', `${baseUrl}/chat/completions?x=1`],
    ['GET', `${baseUrl}/models`],
    ['GET', `${baseUrl}/chat/completions`],
    ['PUT', `${baseUrl}/chat/completions`],
    ['POST', `${root}/r/${token}/v2/chat/completions`],
  ];
  for (const [method, url] of attempts) {
    const res = await fetch(url, { method, headers: { 'content-type': 'application/json' }, body: method === 'GET' ? undefined : '{}' });
    assert.equal(res.status, 404, `${method} ${url.replace(token, '<token>')}`);
    assert.ok(!(await res.text()).includes(token));
  }
  assert.equal(s.model.requests.length, 0);
  assert.equal(s.ledger.records.filter((r) => r.kind === 'call').length, 0);
});

test('a bad body is 400, and a closed run has no token', async (t) => {
  const s = await setup(t);
  const { baseUrl } = s.open();
  const bad = await fetch(`${baseUrl}/chat/completions`, { method: 'POST', body: '{not json' });
  assert.equal(bad.status, 400);
  assert.equal((await fetch(`${baseUrl}/chat/completions`, { method: 'POST', body: '[]' })).status, 400);
  assert.equal((await chat(baseUrl, { stream: 'yes' })).status, 400);
  assert.equal(s.model.requests.length, 0);
  s.meter.closeRun('r1');
  assert.equal((await chat(baseUrl, { messages: [] })).status, 404);
  assert.equal(s.meter.status('r1').closed, true);
});

test('a hung upstream times out: 504, the worst case is charged, and nothing stays in flight', async (t) => {
  const s = await setup(t, { behavior: { delayMs: 3000 }, config: { timeouts: { firstByteSeconds: 0.3 } } });
  const { baseUrl } = s.open();
  const started = Date.now();
  const res = await chat(baseUrl, { messages: [] });
  assert.equal(res.status, 504);
  assert.ok(Date.now() - started < 2500);
  const settled = callRecords(s.ledger, 'settled')[0];
  assert.equal(settled.reason, 'timeout');
  assert.equal(settled.usageMissing, true);
  assert.equal(settled.costMicro, callRecords(s.ledger, 'pending')[0].worstMicro);
  assert.equal(s.book.runView('r1').pendingMicro, 0);
});

test('a stream that stalls is cut by the idle timeout', async (t) => {
  const s = await setup(t, { behavior: { chunkDelayMs: 3000 }, config: { timeouts: { idleSeconds: 0.3 } } });
  const { baseUrl } = s.open();
  const res = await chat(baseUrl, { messages: [], stream: true });
  await res.text().catch(() => {});
  await new Promise((r) => setTimeout(r, 100));
  const settled = callRecords(s.ledger, 'settled');
  assert.equal(settled.length, 1);
  assert.equal(settled[0].usageMissing, true);
});

test('an endpoint that is down is not charged; an HTTP error is relayed and not charged', async (t) => {
  const s = await setup(t, { behavior: { status: 500 } });
  const { baseUrl } = s.open();
  const res = await chat(baseUrl, { messages: [] });
  assert.equal(res.status, 500);
  const failed = callRecords(s.ledger, 'failed');
  assert.equal(failed.length, 1);
  assert.equal(failed[0].costMicro, 0);
  assert.equal(failed[0].status, 500);
  assert.equal(s.book.runView('r1').spentMicro, 0);

  await s.model.close();
  const down = await chat(baseUrl, { messages: [] });
  assert.equal(down.status, 502);
  assert.equal(s.book.runView('r1').spentMicro, 0, 'a refused connection cannot have been billed');
});

test('the endpoint gets a bad key: the answer is relayed, nothing is charged', async (t) => {
  const s = await setup(t);
  s.model.behavior.expectKey = 'another-key';
  const { baseUrl } = s.open();
  assert.equal((await chat(baseUrl, { messages: [] })).status, 401);
  assert.equal(s.book.runView('r1').spentMicro, 0);
});

test('nothing the meter wrote holds the key, the endpoint, the run token or a body', async (t) => {
  const s = await setup(t, { behavior: { omitUsage: false } });
  const { token, baseUrl } = s.open();
  const stderr = [];
  const realWrite = process.stderr.write.bind(process.stderr);
  process.stderr.write = (chunk, ...rest) => {
    stderr.push(String(chunk));
    return realWrite(chunk, ...rest);
  };
  t.after(() => {
    process.stderr.write = realWrite;
  });
  const secretWords = 'a-sentence-the-agent-sent-that-must-not-be-logged';
  await (await chat(baseUrl, { messages: [{ role: 'user', content: secretWords }] })).text();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: secretWords }], stream: true })).text();
  s.model.behavior.status = 500;
  await (await chat(baseUrl, { messages: [] })).text();
  s.model.behavior.status = 200;
  await s.model.close();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: secretWords }] })).text();
  process.stderr.write = realWrite;

  const ledgerText = fs.readFileSync(s.ledger.file, 'utf8');
  const everything = [ledgerText, JSON.stringify(s.events), stderr.join('')].join('\n');
  const upstreamHost = new URL(s.cfg.endpoint).host;
  for (const secret of [TEST_KEY, s.cfg.endpoint, upstreamHost, token, secretWords]) {
    assert.ok(!everything.includes(secret), `found ${secret === token ? '<token>' : secret} in what the meter wrote`);
  }
  assert.ok(s.events.length > 0 && ledgerText.length > 0);
});

test('prepareBody: the clamp, the default limit, n, and include_usage only for streams', () => {
  const clamp = (body, fragment = {}) => prepareBody(body, fragment, 'm');
  assert.equal(clamp({ max_tokens: 100 }).body.max_tokens, 100);
  assert.equal(clamp({ max_tokens: 100000 }).body.max_tokens, 8192);
  assert.equal(clamp({ max_completion_tokens: 100000 }).body.max_completion_tokens, 8192);
  assert.equal('max_tokens' in clamp({ max_completion_tokens: 5 }).body, false, 'it does not add the other field');
  assert.equal(clamp({}).body.max_tokens, 8192, 'no limit asked: the clamp is the limit');
  assert.equal(clamp({ max_tokens: 'lots' }).body.max_tokens, 8192);
  assert.equal(clamp({ max_tokens: -5 }).body.max_tokens, 8192);
  assert.equal(clamp({ max_tokens: 10, max_completion_tokens: 20 }).maxOutputTokens, 20, 'the worst case takes the larger');
  assert.equal(clamp({ max_tokens: 10, n: 3 }).maxOutputTokens, 30);
  assert.equal('stream_options' in clamp({ stream: false }).body, false);
  assert.deepEqual(clamp({ stream: true }).body.stream_options, { include_usage: true });
  assert.equal(clamp({ stream: true }, { max_tokens: 99999 }).body.max_tokens, 8192, 'the clamp applies to the fragment too');
  assert.throws(() => clamp({ stream: 'yes' }), /boolean/);
});

test('deepMerge: objects merge, anything else is replaced, the second wins, no pollution', () => {
  assert.deepEqual(deepMerge({ a: { x: 1, y: 2 }, l: [1, 2] }, { a: { y: 3, z: 4 }, l: [9] }), { a: { x: 1, y: 3, z: 4 }, l: [9] });
  const polluted = deepMerge({}, JSON.parse('{"__proto__":{"bad":true},"ok":1}'));
  assert.equal({}.bad, undefined);
  assert.equal(polluted.bad, undefined);
  assert.equal(polluted.ok, 1);
});
