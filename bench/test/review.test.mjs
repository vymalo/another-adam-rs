import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { buildReport } from '../lib/report.mjs';
import { CRITERIA, RUBRIC, cleanPatch, mixedOrder, parseScores, readReviews, reviewRequest, reviewRuns, scoresByCell } from '../lib/review.mjs';

const diff = (file, body = '+x\n') => `diff --git a/${file} b/${file}\nindex 1..2 100644\n--- a/${file}\n+++ b/${file}\n@@ -1 +1 @@\n${body}`;
const PRICES = { input: 1_000_000, cachedInput: 100_000, cacheWrite: 1_000_000, output: 2_000_000 };
const CONFIG = { modelId: 'm', prices: PRICES };
const ANSWER = (scores) => ({ choices: [{ message: { content: `Here: ${JSON.stringify(scores)}` } }], usage: { prompt_tokens: 1000, completion_tokens: 100 } });
const GOOD = { correctness: 5, tests: 4, design: 4, scope: 5, hygiene: 3, docs: 3, overall: 4, summary: 'fine' };

test('cleanPatch cuts the files under tool caches out and counts them', () => {
  const c = cleanPatch(diff('src/a.py') + diff('.mypy_cache/3.11/x.json') + diff('pkg/__pycache__/m.pyc') + diff('test/t.py'));
  assert.deepEqual(c.files, ['src/a.py', 'test/t.py']);
  assert.equal(c.stray, 2);
  assert.equal(c.truncated, false);
  assert.ok(!c.patch.includes('mypy_cache'));
  const big = cleanPatch(diff('a.txt', `+${'y'.repeat(200_000)}\n`));
  assert.equal(big.truncated, true);
  assert.ok(Buffer.byteLength(big.patch) < 130_000);
});

test('the request is blind: rubric, statement, files and diff, and nothing about the agent', () => {
  const body = reviewRequest({ modelId: 'm', statement: 'Fix the slug.', cleaned: cleanPatch(diff('src/slug.mjs')) });
  assert.equal(body.messages[0].content, RUBRIC);
  assert.equal(body.reasoning_effort, 'max');
  assert.match(body.messages[1].content, /Fix the slug\.[\s\S]*- src\/slug\.mjs[\s\S]*```diff/);
  assert.ok(!/adam|opencode/i.test(JSON.stringify(body)));
});

test('parseScores takes the JSON object out of the answer and refuses a missing or out-of-range score', () => {
  assert.deepEqual(parseScores(`ok ${JSON.stringify(GOOD)}`), GOOD);
  assert.throws(() => parseScores('no json'), /no JSON/);
  assert.throws(() => parseScores(JSON.stringify({ ...GOOD, tests: 6 })), /tests/);
  assert.throws(() => parseScores(JSON.stringify({ ...GOOD, overall: undefined })), /overall/);
});

test('mixedOrder is seeded and does not keep the cells together', () => {
  const runs = ['a', 'b', 'c', 'd', 'e', 'f'].flatMap((t) => [{ run: `${t}.adam.max` }, { run: `${t}.opencode.max` }]);
  assert.deepEqual(mixedOrder(runs).map((r) => r.run), mixedOrder([...runs].reverse()).map((r) => r.run));
  assert.notDeepEqual(mixedOrder(runs).map((r) => r.run), runs.map((r) => r.run));
});

test('reviewRuns reviews each Done patch once, writes the scores and the cost, and stops before the cap', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'review-'));
  const runs = [
    { run: 'r1', state: 'Done', task: 't1', agent: 'adam', arm: 'max', attempt: 1 },
    { run: 'r2', state: 'Done', task: 't1', agent: 'opencode', arm: 'max', attempt: 1 },
    { run: 'r3', state: 'Failed_infra', task: 't1', agent: 'adam', arm: 'low', attempt: 1 },
    { run: 'r4', state: 'Done', task: 't2', agent: 'adam', arm: 'max', attempt: 1 },
  ];
  const patches = { r1: diff('a.py'), r2: diff('b.py') + diff('.pytest_cache/v'), r4: null };
  const sent = [];
  const send = async (body) => {
    sent.push(body);
    return ANSWER(GOOD);
  };
  const args = { runs, statementOf: () => 'do it', patchOf: (v) => patches[v.run] ?? null, send, config: CONFIG, home, budgetMicro: 1e9 };
  const first = await reviewRuns(args);
  assert.equal(first.reviewed, 2);
  assert.equal(sent.length, 2);
  // 1000 input at $1/M + 100 output at $2/M = 1000 + 200 micro
  assert.equal(first.spentMicro, 2 * 1200);
  const rows = readReviews(home);
  assert.deepEqual(rows.map((r) => r.run).sort(), ['r1', 'r2']);
  assert.equal(rows.find((r) => r.run === 'r2').stray, 1);
  const again = await reviewRuns(args);
  assert.equal(again.reviewed, 0, 'a run reviewed with the same patch and rubric is not sent again');
  assert.equal(sent.length, 2);
  patches.r1 = diff('a.py', '+changed\n');
  assert.equal((await reviewRuns(args)).reviewed, 1, 'a new patch is reviewed again');
  const capped = await reviewRuns({ ...args, home: fs.mkdtempSync(path.join(os.tmpdir(), 'review-')), budgetMicro: 10 });
  assert.equal(capped.reviewed, 0, 'nothing is sent when the worst case does not fit under the cap');
  fs.rmSync(home, { recursive: true, force: true });
});

test('an answer without scores is recorded as an error and left out of the means', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'review-'));
  const runs = [{ run: 'r1', state: 'Done', task: 't', agent: 'adam', arm: 'max', attempt: 1 }];
  const r = await reviewRuns({ runs, statementOf: () => 's', patchOf: () => diff('a'), send: async () => ({ choices: [{ message: { content: 'cannot' } }] }), config: CONFIG, home, budgetMicro: 1e9 });
  assert.equal(r.failed.length, 1);
  assert.equal(readReviews(home)[0].error, 'the reviewer gave no JSON object');
  assert.deepEqual(scoresByCell(readReviews(home), runs), []);
  fs.rmSync(home, { recursive: true, force: true });
});

test('scoresByCell averages the latest review of each Done run per cell, and the report shows them', () => {
  const runs = [
    { run: 'r1', state: 'Done', agent: 'adam', arm: 'max' },
    { run: 'r2', state: 'Done', agent: 'adam', arm: 'max' },
    { run: 'r3', state: 'Done', agent: 'opencode', arm: 'max' },
  ];
  const s = (n) => Object.fromEntries([...CRITERIA, 'overall'].map((k) => [k, n]));
  const reviews = [
    { run: 'r1', scores: s(1), stray: 0, costMicro: 5 },
    { run: 'r1', scores: s(3), stray: 0, costMicro: 5 },
    { run: 'r2', scores: s(5), stray: 2, costMicro: 5 },
    { run: 'r3', scores: s(2), stray: 0, costMicro: 5 },
  ];
  const cells = scoresByCell(reviews, runs);
  assert.deepEqual(cells.map((c) => [c.agent, c.n, c.mean.overall, c.stray]), [
    ['adam', 2, 4, 2],
    ['opencode', 1, 2, 0],
  ]);
  const md = buildReport([{ v: 1, t: '2026-11-20T10:00:00.000Z', kind: 'state', run: 'r1', state: 'Planned', task: 't', agent: 'adam', arm: 'max' }], { reviews: [] });
  assert.match(md, /## Quality \(blind review\)\n\nNo review yet/);
});
