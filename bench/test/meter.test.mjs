import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { test } from 'node:test';
import { CapBook } from '../lib/cap.mjs';
import { normalizeConfig } from '../lib/config.mjs';
import { Ledger, readLedger } from '../lib/ledger.mjs';
import { LIMITS, MAX_OUTPUT_TOKENS, createMeter, deepMerge, prepareBody, toolName } from '../lib/meter.mjs';
import { createScriptedModel } from '../lib/scripted-model.mjs';
import { MODEL_ID, TEST_KEY, rawConfig, tmp } from './helpers.mjs';

// The scripted model reports: prompt 1000 (400 cached), completion 200 (50 of them reasoning).
// With prices 3 / 0.3 / 3.75 / 15 USD per million tokens a call costs
//   600 * 3e6 + 400 * 3e5 + 200 * 15e6 = 1_800_000_000 + 120_000_000 + 3_000_000_000 = 4_920_000_000
// and 4_920_000_000 / 1_000_000 = 4920 micro-dollars.
const SCRIPTED_COST = 4920;

async function setup(t, { config = {}, behavior = {}, reserve = 3_000_000, cap = 10_000_000, key = TEST_KEY, limits } = {}) {
  const model = createScriptedModel({ expectKey: key, ...behavior });
  const { url } = await model.listen();
  const cfg = normalizeConfig(rawConfig({ endpoint: url, ...config }));
  const dir = tmp(t);
  const ledger = Ledger.open(path.join(dir, 'ledger.jsonl'));
  const book = new CapBook(cap);
  const events = [];
  const meter = createMeter({ config: cfg, ledger, book, env: { BENCH_MODEL_API_KEY: key }, log: (e) => events.push(e), limits });
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

test('an endpoint that is down is not charged; an HTTP error is answered with a fixed body and not charged', async (t) => {
  const s = await setup(t, { behavior: { status: 500 } });
  const { baseUrl } = s.open();
  const res = await chat(baseUrl, { messages: [] });
  assert.equal(res.status, 502, 'an upstream 5xx is a 502 for the agent');
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

test('the endpoint gets a bad key: the agent sees a 400 with a fixed body, nothing is charged', async (t) => {
  const s = await setup(t);
  s.model.behavior.expectKey = 'another-key';
  const { baseUrl } = s.open();
  assert.equal((await chat(baseUrl, { messages: [] })).status, 400, 'an upstream 4xx other than 429 is a 400');
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
  const bodies = [];
  const secretWords = 'a-sentence-the-agent-sent-that-must-not-be-logged';
  bodies.push(await (await chat(baseUrl, { messages: [{ role: 'user', content: secretWords }] })).text());
  bodies.push(await (await chat(baseUrl, { messages: [{ role: 'user', content: secretWords }], stream: true })).text());
  s.model.behavior.status = 500;
  bodies.push(await (await chat(baseUrl, { messages: [] })).text());
  s.model.behavior.status = 200;
  await s.model.close();
  bodies.push(await (await chat(baseUrl, { messages: [{ role: 'user', content: secretWords }] })).text());
  process.stderr.write = realWrite;

  const ledgerText = fs.readFileSync(s.ledger.file, 'utf8');
  const everything = [ledgerText, JSON.stringify(s.events), stderr.join(''), bodies.join('\n')].join('\n');
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
  assert.equal(clamp({ max_tokens: 10, n: 1 }).maxOutputTokens, 10);
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

// ---- fix round: rows 1, 2, 3, 10, 13 of the review ----

const FIXED = (status) => ({ error: { message: 'upstream error', type: 'upstream_error', status } });

test('row 1: an upstream error reaches the agent as a fixed body, whatever the endpoint said', async (t) => {
  const leak = 'Incorrect API key provided: example-key-for-tests-0001 at https://models.example.test/v1/chat/completions (tail ...0001)';
  const s = await setup(t, { behavior: { errorBody: { error: { message: leak } }, errorHeaders: { 'x-upstream-secret': 'host-internal-7', 'www-authenticate': 'Bearer realm="example"' } } });
  const { baseUrl } = s.open();
  const table = [[400, 400], [401, 400], [403, 400], [404, 400], [418, 400], [429, 429], [500, 502], [502, 502], [503, 502], [302, 502]];
  for (const [upstream, expected] of table) {
    s.model.behavior.status = upstream;
    const res = await chat(baseUrl, { messages: [] });
    const text = await res.text();
    assert.equal(res.status, expected, `upstream ${upstream}`);
    assert.deepEqual(JSON.parse(text), FIXED(upstream), `body for ${upstream}`);
    assert.equal(res.headers.get('x-upstream-secret'), null, 'no upstream header is relayed');
    assert.equal(res.headers.get('www-authenticate'), null);
    assert.match(res.headers.get('content-type'), /application\/json/);
    assert.ok(!text.includes('example-key') && !text.includes('models.example.test'));
  }
  const failed = callRecords(s.ledger, 'failed');
  assert.deepEqual(failed.map((r) => r.status), table.map(([u]) => u), 'the ledger has the upstream status and nothing else of the answer');
  assert.ok(failed.every((r) => r.costMicro === 0));
  assert.ok(!JSON.stringify(s.events).includes('example-key'));
});

test('row 10: a non-2xx answer with a huge body costs nothing and is not buffered', async (t) => {
  const s = await setup(t, { behavior: { status: 500, errorBody: 'e'.repeat(3 * 1024 * 1024) } });
  const { baseUrl } = s.open();
  const res = await chat(baseUrl, { messages: [] });
  assert.equal(res.status, 502);
  assert.deepEqual(await res.json(), FIXED(500));
  assert.equal(s.book.runView('r1').spentMicro, 0);
});

test('row 2: n absent or exactly 1 is forwarded as 1; anything else is a 400 and nothing leaves', async (t) => {
  const s = await setup(t);
  const { baseUrl } = s.open();
  assert.equal((await chat(baseUrl, { messages: [] })).status, 200);
  assert.equal((await chat(baseUrl, { messages: [], n: 1 })).status, 200);
  assert.deepEqual(s.model.requests.map((r) => r.body.n), [1, 1]);
  for (const n of [2, 0, -1, 1.5, '1', null, true, [1], 1e9]) {
    const res = await chat(baseUrl, { messages: [], n });
    assert.equal(res.status, 400, `n = ${JSON.stringify(n)}`);
    assert.match((await res.json()).error.message, /\bn\b/);
  }
  assert.equal(s.model.requests.length, 2, 'nothing was forwarded for a bad n');
  assert.equal(callRecords(s.ledger, 'pending').length, 2);
  assert.throws(() => prepareBody({ n: 3 }, {}, 'm'), /\bn\b/);
  assert.equal(prepareBody({}, {}, 'm').body.n, 1);
  assert.equal(prepareBody({ max_tokens: 10, n: 1 }, {}, 'm').maxOutputTokens, 10);
});

test('row 3: a failure that throws before the request leaves frees the reservation and settles the call', async (t) => {
  // a character above 0xFF passes the key check but cannot go in a header: http.request throws at once
  const key = 'k€y-example-0001';
  const s = await setup(t, { key });
  const { baseUrl } = s.open();
  const res = await chat(baseUrl, { messages: [] });
  assert.equal(res.status, 500);
  assert.equal(s.model.requests.length, 0);
  assert.equal(callRecords(s.ledger, 'pending').length, 1);
  const failed = callRecords(s.ledger, 'failed');
  assert.equal(failed.length, 1, 'the pending record is settled, not left forever');
  assert.equal(failed[0].costMicro, 0, 'nothing was sent: free');
  assert.equal(s.book.runView('r1').pendingMicro, 0, 'the reservation is not leaked');
  assert.equal(s.ledger.fold().get('r1').spentMicro, 0, 'the fold does not count an unsettled worst case');
  assert.ok(!JSON.stringify(s.events).includes('€'));
});

test('row 13: a request body over the limit is answered 413 and the answer arrives', async (t) => {
  const s = await setup(t, { limits: { maxRequestBytes: 64 * 1024 } });
  const { baseUrl } = s.open();
  const res = await chat(baseUrl, { messages: [{ role: 'user', content: 'x'.repeat(200 * 1024) }] });
  assert.equal(res.status, 413);
  assert.match((await res.json()).error.message, /too large/);
  assert.equal(s.model.requests.length, 0);
  const small = await chat(baseUrl, { messages: [{ role: 'user', content: 'x'.repeat(20 * 1024) }] });
  assert.equal(small.status, 200, 'a body under the limit still goes');
});

test('row 13: the default limit is 8 MiB, the 413 is delivered even for a body several times over it, and a declared length is refused unread', async (t) => {
  assert.equal(LIMITS.maxRequestBytes, 8 * 1024 * 1024);
  const s = await setup(t);
  const { baseUrl } = s.open();
  const res = await chat(baseUrl, { messages: [{ role: 'user', content: 'x'.repeat(40 * 1024 * 1024) }] });
  assert.equal(res.status, 413);
  await res.text();
  // a raw request that declares a huge length and sends nothing is refused at once
  const status = await new Promise((resolve, reject) => {
    const u = new URL(`${baseUrl}/chat/completions`);
    const r = http.request({ host: u.hostname, port: u.port, path: u.pathname, method: 'POST', headers: { 'content-type': 'application/json', 'content-length': String(100 * 1024 * 1024) } }, (rs) => {
      resolve(rs.statusCode);
      rs.resume();
    });
    r.on('error', reject);
    r.write('{');
    setTimeout(() => r.destroy(), 3000).unref();
  });
  assert.equal(status, 413);
});

test('row 10: a stream that never ends is cut at the byte limit, the upstream is destroyed and the worst case is charged', async (t) => {
  const s = await setup(t, { behavior: { endless: true, endlessChunkBytes: 1000, endlessEveryMs: 1 }, limits: { maxResponseBytes: 50_000 } });
  const { baseUrl } = s.open();
  const res = await fetch(`${baseUrl}/chat/completions`, { method: 'POST', body: JSON.stringify({ messages: [], stream: true }), signal: AbortSignal.timeout(5000) });
  const got = await res.text().catch(() => '');
  assert.ok(got.length < 200_000, `the agent got ${got.length} bytes`);
  await until(() => s.model.stats.aborted === 1, 'the upstream stream was cut');
  const settled = callRecords(s.ledger, 'settled');
  assert.equal(settled.length, 1);
  assert.equal(settled[0].reason, 'response_too_large');
  assert.equal(settled[0].usageMissing, true);
  assert.equal(settled[0].costMicro, callRecords(s.ledger, 'pending')[0].worstMicro);
  assert.equal(s.book.runView('r1').pendingMicro, 0);
});

test('row 10: a stream that trickles forever is cut at the time limit, however lively it is', async (t) => {
  const s = await setup(t, { behavior: { endless: true, endlessChunkBytes: 10, endlessEveryMs: 20 }, limits: { maxCallMs: 600 } });
  const { baseUrl } = s.open();
  const started = Date.now();
  const res = await fetch(`${baseUrl}/chat/completions`, { method: 'POST', body: JSON.stringify({ messages: [], stream: true }), signal: AbortSignal.timeout(5000) });
  await res.text().catch(() => '');
  assert.ok(Date.now() - started < 3500, `ended after ${Date.now() - started} ms`);
  await until(() => s.model.stats.aborted === 1, 'the upstream stream was cut');
  const settled = callRecords(s.ledger, 'settled');
  assert.equal(settled.length, 1);
  assert.equal(settled[0].reason, 'timeout');
  assert.equal(settled[0].costMicro, callRecords(s.ledger, 'pending')[0].worstMicro);
});

test('row 10: a client that does not read slows the upstream down instead of filling the meter', async (t) => {
  const s = await setup(t, {
    behavior: { endless: true, endlessChunkBytes: 256 * 1024, endlessEveryMs: 1 },
    limits: { maxResponseBytes: 2 ** 31, maxCallMs: 60_000 },
  });
  const { baseUrl } = s.open();
  const u = new URL(`${baseUrl}/chat/completions`);
  const req = http.request({ host: u.hostname, port: u.port, path: u.pathname, method: 'POST', headers: { 'content-type': 'application/json' } }, (rs) => rs.pause());
  req.on('error', () => {});
  t.after(() => req.destroy());
  req.end(JSON.stringify({ messages: [], stream: true }));
  await new Promise((r) => setTimeout(r, 1200));
  const sent = s.model.stats.sentBytes;
  assert.ok(sent < 48 * 1024 * 1024, `the upstream got ${(sent / 1e6).toFixed(1)} MB into the meter while nobody read`);
});

async function until(fn, what, ms = 3000) {
  const end = Date.now() + ms;
  while (!fn()) {
    if (Date.now() > end) assert.fail(`timed out: ${what}`);
    await new Promise((r) => setTimeout(r, 20));
  }
}

test('row 17: the cap is soft by one call: a call that reports more than its worst case passes its reservation, and the next call is refused', async (t) => {
  // the scripted endpoint reports 100_000 output tokens: 1_500_000 micro-dollars, far above the worst case of 122_990
  const s = await setup(t, { reserve: 200_000, behavior: { usage: { prompt: 10, completion: 100_000, cached: 0, reasoning: 0, cacheWrite: 0 } } });
  const { baseUrl } = s.open();
  assert.equal((await chat(baseUrl, { messages: [] })).status, 200);
  const spent = s.book.runView('r1').spentMicro;
  assert.ok(spent > 200_000, `the run spent ${spent} against a reservation of 200000`);
  assert.equal((await chat(baseUrl, { messages: [] })).status, 429, 'the very next call is refused');
  assert.equal(s.model.requests.length, 1);
  assert.equal(s.book.runView('r1').spentMicro, spent, 'the excess is that one call, no more');
  assert.equal(s.book.committedMicro(), spent, 'a run over its reservation is committed at what it spent');
});

// ---- mutation round ----

test('the client headers never reach the endpoint: only the meter\'s own set does', async (t) => {
  const s = await setup(t);
  const { baseUrl } = s.open();
  await (await chat(baseUrl, { messages: [] }, { 'x-api-key': 'client-secret', cookie: 'session=abc', 'x-custom': 'one', authorization: 'Bearer client' })).text();
  const names = Object.keys(s.model.requests[0].headers).sort();
  assert.deepEqual(names, ['accept', 'accept-encoding', 'authorization', 'connection', 'content-length', 'content-type', 'host', 'user-agent']);
  assert.equal(s.model.requests[0].headers.authorization, `Bearer ${TEST_KEY}`);
});

test('a 2xx answer relays no upstream header, plain or stream', async (t) => {
  const s = await setup(t, { behavior: { okHeaders: { 'x-upstream-internal': 'host-7', 'set-cookie': 'a=b', 'x-request-id': 'zzz' } } });
  const { baseUrl } = s.open();
  for (const stream of [false, true]) {
    const res = await chat(baseUrl, { messages: [], stream });
    await res.text();
    assert.equal(res.status, 200);
    for (const h of ['x-upstream-internal', 'set-cookie', 'x-request-id']) assert.equal(res.headers.get(h), null, `${h} (stream ${stream})`);
  }
});

test('an endpoint that is down or times out gives the agent a fixed body with no host', async (t) => {
  const s = await setup(t, { behavior: { delayMs: 3000 }, config: { timeouts: { firstByteSeconds: 0.3 } } });
  const { baseUrl } = s.open();
  const slow = await chat(baseUrl, { messages: [] });
  assert.equal(slow.status, 504);
  assert.deepEqual(await slow.json(), { error: { message: 'upstream unavailable' } });
  await s.model.close();
  const down = await chat(baseUrl, { messages: [] });
  assert.equal(down.status, 502);
  assert.deepEqual(await down.json(), { error: { message: 'upstream unavailable' } });
});

test('an arm fragment cannot lift the output limit: it is clamped and the worst case uses the clamp', async (t) => {
  const s = await setup(t, { config: { arms: { big: { max_completion_tokens: 99_999 }, off: { reasoning_effort: 'none' } } }, reserve: 130_000 });
  const { baseUrl } = s.open('r1', 'big');
  assert.equal((await chat(baseUrl, { messages: [] })).status, 200);
  assert.equal(s.model.requests[0].body.max_completion_tokens, MAX_OUTPUT_TOKENS);
  // the worst case is that of 8192 tokens (about 122_990), so a second call does not fit 130_000 after 4_920 spent... and the first did
  assert.ok(callRecords(s.ledger, 'pending')[0].worstMicro < 130_000);
});

test('the pending record is on disk before the request leaves', async (t) => {
  const seen = [];
  const s = await setup(t);
  s.model.behavior.onRequest = () => {
    seen.push(fs.readFileSync(s.ledger.file, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l)).filter((r) => r.kind === 'call'));
  };
  const { baseUrl } = s.open();
  await (await chat(baseUrl, { messages: [] })).text();
  assert.equal(seen.length, 1);
  assert.deepEqual(seen[0].map((r) => r.phase), ['pending'], 'at arrival: pending written, nothing settled');
});

test('a reset after the request was sent is charged the worst case and flagged', async (t) => {
  const s = await setup(t, { behavior: { reset: true } });
  const { baseUrl } = s.open();
  const res = await chat(baseUrl, { messages: [] });
  assert.equal(res.status, 502);
  const settled = callRecords(s.ledger, 'settled');
  assert.equal(settled.length, 1, 'settled, not failed-free');
  assert.equal(settled[0].usageMissing, true);
  assert.equal(settled[0].costMicro, callRecords(s.ledger, 'pending')[0].worstMicro);
  assert.equal(callRecords(s.ledger, 'failed').length, 0);
});

test('a plain answer over the response limit is cut: 502, worst case, nothing relayed', async (t) => {
  const s = await setup(t, { behavior: { padBytes: 300_000 }, limits: { maxResponseBytes: 5_000 } });
  const { baseUrl } = s.open();
  const res = await chat(baseUrl, { messages: [] });
  assert.equal(res.status, 502);
  assert.deepEqual(await res.json(), { error: { message: 'upstream unavailable' } });
  const settled = callRecords(s.ledger, 'settled');
  assert.equal(settled[0].reason, 'response_too_large');
  assert.equal(settled[0].costMicro, callRecords(s.ledger, 'pending')[0].worstMicro);
});

test('a chunked upload with no content-length is counted in total and refused with 413', async (t) => {
  const s = await setup(t, { limits: { maxRequestBytes: 64 * 1024 } });
  const { baseUrl } = s.open();
  const u = new URL(`${baseUrl}/chat/completions`);
  const status = await new Promise((resolve, reject) => {
    const r = http.request({ host: u.hostname, port: u.port, path: u.pathname, method: 'POST', headers: { 'content-type': 'application/json', 'transfer-encoding': 'chunked' } }, (rs) => {
      resolve(rs.statusCode);
      rs.resume();
    });
    r.on('error', reject);
    const piece = `${'x'.repeat(40 * 1024)}`;
    r.write('{"messages":[{"role":"user","content":"');
    r.write(piece);
    r.write(piece);
    r.end(piece + '"}]}');
    setTimeout(() => r.destroy(), 4000).unref();
  });
  assert.equal(status, 413);
  assert.equal(s.model.requests.length, 0);
});

test('the settled record counts the tool calls of the answer, plain and stream (calls per model turn, ADR 0049)', async (t) => {
  const s = await setup(t, { behavior: { toolCalls: 3 } });
  const { baseUrl } = s.open();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }] })).text();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }], stream: true })).text();
  s.model.behavior.toolCalls = 0;
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }] })).text();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }], stream: true })).text();
  assert.deepEqual(
    callRecords(s.ledger, 'settled').map((r) => r.toolCalls),
    [3, 3, 0, 0],
  );
});

test('a failed call has no tool call count', async (t) => {
  const s = await setup(t, { behavior: { status: 500 } });
  const { baseUrl } = s.open();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }] })).text();
  const done = s.ledger.records.filter((r) => r.kind === 'call' && r.phase !== 'pending');
  assert.ok(done.length >= 1);
  for (const r of done) {
    assert.equal(r.toolCalls, undefined);
    assert.equal(r.toolNames, undefined);
  }
});

test('a stream that sends its tool calls one by one, each in fragments, is counted by call and not by chunk', async (t) => {
  const s = await setup(t, { behavior: { toolCalls: 3, splitToolCalls: true } });
  const { baseUrl } = s.open();
  const text = await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }], stream: true })).text();
  assert.equal((text.match(/"tool_calls":\[/g) ?? []).length, 6, 'each call is a chunk of its own and a fragment of its arguments');
  assert.deepEqual(callRecords(s.ledger, 'settled').map((r) => r.toolCalls), [3]);
});

test('a stream whose tool calls all say index 0 is counted by the ids that tell them apart', async (t) => {
  for (const splitToolCalls of [false, true]) {
    const s = await setup(t, { behavior: { toolCalls: 3, splitToolCalls, sameIndexToolCalls: true } });
    const { baseUrl } = s.open(`r-${splitToolCalls}`);
    const text = await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }], stream: true })).text();
    assert.ok(!/"index":1/.test(text) && !/"index":2/.test(text), 'every call has the index 0');
    assert.deepEqual(callRecords(s.ledger, 'settled').map((r) => r.toolCalls), [3], `split: ${splitToolCalls}`);
  }
});

test('a stream whose tool calls carry no id is counted by their indices', async (t) => {
  for (const splitToolCalls of [false, true]) {
    const s = await setup(t, { behavior: { toolCalls: 3, splitToolCalls, noToolCallIds: true } });
    const { baseUrl } = s.open(`r-${splitToolCalls}`);
    const text = await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }], stream: true })).text();
    assert.ok(!/"id":"call/.test(text), 'no call carries an id');
    assert.deepEqual(callRecords(s.ledger, 'settled').map((r) => r.toolCalls), [3], `split: ${splitToolCalls}`);
  }
});

test('the settled record keeps the name of each tool call, plain and stream', async (t) => {
  const s = await setup(t, { behavior: { toolCalls: 3, toolNames: ['read_file', 'search_files', 'run_checks'] } });
  const { baseUrl } = s.open();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }] })).text();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }], stream: true })).text();
  const want = ['read_file', 'search_files', 'run_checks'];
  assert.deepEqual(callRecords(s.ledger, 'settled').map((r) => r.toolNames), [want, want]);
});

test('the names survive the fragments, a shared index and a missing id', async (t) => {
  const variants = [
    { splitToolCalls: true },
    { sameIndexToolCalls: true },
    { noToolCallIds: true },
    { splitToolCalls: true, sameIndexToolCalls: true },
    { splitToolCalls: true, noToolCallIds: true },
    { nameBeforeId: true },
  ];
  for (const [i, variant] of variants.entries()) {
    const s = await setup(t, { behavior: { toolCalls: 2, toolNames: ['read_file', 'run_checks'], ...variant } });
    const { baseUrl } = s.open(`r-${i}`);
    await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }], stream: true })).text();
    assert.deepEqual(callRecords(s.ledger, 'settled').map((r) => r.toolNames), [['read_file', 'run_checks']], JSON.stringify(variant));
  }
});

test('an answer with no tool call carries no names', async (t) => {
  const s = await setup(t, { behavior: { toolCalls: 0 } });
  const { baseUrl } = s.open();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }] })).text();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }], stream: true })).text();
  for (const r of callRecords(s.ledger, 'settled')) assert.equal(r.toolNames, undefined);
});

test('a tool name is text: control characters dropped, cut to 64, a non-string not kept', async (t) => {
  assert.equal(toolName(null), null);
  assert.equal(toolName(7), null);
  assert.equal(toolName('   '), null);
  assert.equal(toolName('read\u0000_file\n'), 'read_file');
  assert.equal(toolName('x'.repeat(200)), 'x'.repeat(64));
  const s = await setup(t, { behavior: { toolCalls: 2, toolNames: ['x'.repeat(200), 'read_file'] } });
  const { baseUrl } = s.open();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }] })).text();
  const [record] = callRecords(s.ledger, 'settled');
  assert.deepEqual(record.toolNames, ['x'.repeat(64), 'read_file']);
});

test('a client that closes the stream right after [DONE] keeps the tool call count of its answer', async (t) => {
  const s = await setup(t, { behavior: { toolCalls: 2, toolNames: ['read_file', 'run_checks'], holdAfterDoneMs: 400 } });
  const { baseUrl } = s.open();
  const res = await fetch(`${baseUrl}/chat/completions`, { method: 'POST', body: JSON.stringify({ messages: [{ role: 'user', content: 'hi' }], stream: true, stream_options: { include_usage: true } }) });
  const reader = res.body.getReader();
  let text = '';
  const decoder = new TextDecoder();
  while (!text.includes('[DONE]')) {
    const { value, done } = await reader.read();
    if (done) break;
    text += decoder.decode(value);
  }
  await reader.cancel(); // adam closes at [DONE]
  await until(() => callRecords(s.ledger, 'settled').length === 1, 'the call was settled');
  const settled = callRecords(s.ledger, 'settled')[0];
  assert.equal(settled.costMicro, SCRIPTED_COST, 'settled on the usage it saw');
  assert.equal(settled.toolCalls, 2);
  assert.deepEqual(settled.toolNames, ['read_file', 'run_checks'], 'the names of a call cut at [DONE] are kept too');
});

test('an answer that asks for more than 64 calls keeps the first 64 names and the whole count', async (t) => {
  const s = await setup(t, { behavior: { toolCalls: 70, toolNames: ['read_file', 'run_checks'] } });
  const { baseUrl } = s.open();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }], stream: true })).text();
  const [record] = callRecords(s.ledger, 'settled');
  assert.equal(record.toolCalls, 70);
  assert.equal(record.toolNames.length, 64);
  assert.equal(record.toolNames[0], 'read_file');
  assert.equal(record.toolNames[63], 'run_checks');
});

test('a 200 answer that is not a completion has no tool call count', async (t) => {
  const s = await setup(t, { behavior: { okBody: 'not json' } });
  const { baseUrl } = s.open();
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }] })).text();
  s.model.behavior.okBody = JSON.stringify({ object: 'chat.completion' });
  await (await chat(baseUrl, { messages: [{ role: 'user', content: 'x' }] })).text();
  const settled = callRecords(s.ledger, 'settled');
  assert.equal(settled.length, 2);
  for (const r of settled) assert.equal(r.toolCalls, undefined);
});

test('S1: a client that closes the stream right after [DONE] is charged the usage the meter already saw, not the worst case', async (t) => {
  const s = await setup(t, { behavior: { holdAfterDoneMs: 400 } });
  const { baseUrl } = s.open();
  const res = await fetch(`${baseUrl}/chat/completions`, { method: 'POST', body: JSON.stringify({ messages: [{ role: 'user', content: 'hi' }], stream: true, stream_options: { include_usage: true } }) });
  const reader = res.body.getReader();
  let text = '';
  const decoder = new TextDecoder();
  while (!text.includes('[DONE]')) {
    const { value, done } = await reader.read();
    if (done) break;
    text += decoder.decode(value);
  }
  await reader.cancel(); // adam closes at [DONE]
  await until(() => callRecords(s.ledger, 'settled').length === 1, 'the call was settled');
  const settled = callRecords(s.ledger, 'settled')[0];
  assert.equal(settled.costMicro, SCRIPTED_COST, 'the scripted usage times the price, not the worst case');
  assert.notEqual(settled.usageMissing, true);
  assert.equal(settled.usage.input, 1000);
});

test('S1: a client that closes before any usage was seen is still charged the worst case', async (t) => {
  const s = await setup(t, { behavior: { omitUsage: true, endless: true, endlessChunkBytes: 100, endlessEveryMs: 5 } });
  const { baseUrl } = s.open();
  const ctl = new AbortController();
  const res = await fetch(`${baseUrl}/chat/completions`, { method: 'POST', body: JSON.stringify({ messages: [], stream: true }), signal: ctl.signal });
  await res.body.getReader().read();
  ctl.abort();
  await until(() => callRecords(s.ledger, 'settled').length === 1, 'settled');
  assert.equal(callRecords(s.ledger, 'settled')[0].costMicro, callRecords(s.ledger, 'pending')[0].worstMicro);
});

test('defect 1: a client that aborts after the usage chunk but before [DONE] is charged the worst case, reason client_abort', async (t) => {
  const s = await setup(t, { behavior: { holdBeforeDoneMs: 600 } });
  const { baseUrl } = s.open();
  const res = await fetch(`${baseUrl}/chat/completions`, { method: 'POST', body: JSON.stringify({ messages: [{ role: 'user', content: 'hi' }], stream: true, stream_options: { include_usage: true } }) });
  const reader = res.body.getReader();
  let text = '';
  const decoder = new TextDecoder();
  while (!text.includes('"usage":{')) {
    const { value, done } = await reader.read();
    if (done) break;
    text += decoder.decode(value);
  }
  assert.ok(!text.includes('[DONE]'), 'the usage chunk came and [DONE] has not');
  await reader.cancel();
  await until(() => callRecords(s.ledger, 'settled').length === 1, 'settled');
  const settled = callRecords(s.ledger, 'settled')[0];
  assert.equal(settled.reason, 'client_abort');
  assert.equal(settled.usageMissing, true);
  assert.equal(settled.costMicro, callRecords(s.ledger, 'pending')[0].worstMicro);
});

test('the meter opens a connection for each call: a peer that closes an idle socket cannot fail the next call', async (t) => {
  // A real endpoint or proxy closes an idle connection (Node's own server default is 5 s). The dry run hit exactly this:
  // two adam calls failed after the coder took about 5.8 s between them, and each was charged its worst case. A fresh
  // connection for every call removes the class, at the cost of one handshake per model call.
  const s = await setup(t);
  let connections = 0;
  s.model.server.on('connection', () => {
    connections += 1;
  });
  const { baseUrl } = s.open();
  assert.equal((await chat(baseUrl, { model: 'whatever', messages: [{ role: 'user', content: 'one' }] })).status, 200);
  assert.equal((await chat(baseUrl, { model: 'whatever', messages: [{ role: 'user', content: 'two' }] })).status, 200);
  assert.equal(connections, 2, 'no keep-alive socket is reused');
  assert.deepEqual(callRecords(s.ledger, 'settled').map((r) => r.usageMissing), [false, false]);
});
