import assert from 'node:assert/strict';
import { test } from 'node:test';
import { estimateInputTokens, formatUSD, normalizeUsage, priceCall, settleCall, usdToMicro, worstCase } from '../lib/cost.mjs';

// Prices in micro-dollars per million tokens: input 3 USD, cached 0.30, cache write 3.75, output 15.
const prices = { input: 3_000_000, cachedInput: 300_000, cacheWrite: 3_750_000, output: 15_000_000 };

test('usage: totals only', () => {
  assert.deepEqual(normalizeUsage({ prompt_tokens: 1000, completion_tokens: 200 }), { input: 1000, output: 200, reasoning: null, cached: null, cacheWrite: null });
});

test('usage: parts inside their totals are left as they are', () => {
  const u = normalizeUsage({
    prompt_tokens: 1000,
    completion_tokens: 200,
    prompt_tokens_details: { cached_tokens: 400, cache_write_tokens: 100 },
    completion_tokens_details: { reasoning_tokens: 150 },
  });
  assert.deepEqual(u, { input: 1000, output: 200, reasoning: 150, cached: 400, cacheWrite: 100 });
});

test('usage: a part beside a smaller total is added in (ADR 0032, Usage::accounted)', () => {
  const reasoning = normalizeUsage({ prompt_tokens: 10, completion_tokens: 5, completion_tokens_details: { reasoning_tokens: 300 } });
  assert.equal(reasoning.output, 305);
  const cache = normalizeUsage({ prompt_tokens: 50, completion_tokens: 2, prompt_tokens_details: { cached_tokens: 400, cache_write_tokens: 10 } });
  assert.equal(cache.input, 460);
  // applying the rule to its own result changes nothing
  const again = normalizeUsage({ prompt_tokens: cache.input, completion_tokens: cache.output, prompt_tokens_details: { cached_tokens: 400, cache_write_tokens: 10 } });
  assert.equal(again.input, 460);
});

test('usage: an absent, null or non-integer detail is null; missing totals are no usage', () => {
  const u = normalizeUsage({ prompt_tokens: 10, completion_tokens: 5, prompt_tokens_details: { cached_tokens: 1.5, cache_write_tokens: null }, completion_tokens_details: null });
  assert.deepEqual([u.cached, u.cacheWrite, u.reasoning], [null, null, null]);
  for (const bad of [undefined, null, 'x', [], {}, { prompt_tokens: 1 }, { prompt_tokens: 1, completion_tokens: -1 }, { prompt_tokens: '1', completion_tokens: 1 }]) {
    assert.equal(normalizeUsage(bad), null);
  }
});

test('price of a call, by hand', () => {
  // input 1000 = 500 plain + 400 cached + 100 cache write; output 200
  // 500*3_000_000 + 400*300_000 + 100*3_750_000 + 200*15_000_000
  // = 1_500_000_000 + 120_000_000 + 375_000_000 + 3_000_000_000 = 4_995_000_000; / 1_000_000 = 4995
  assert.equal(priceCall(prices, { input: 1000, output: 200, reasoning: 50, cached: 400, cacheWrite: 100 }), 4995);
});

test('price: reasoning is inside the output and is not charged twice', () => {
  const withReasoning = priceCall(prices, { input: 100, output: 200, reasoning: 150, cached: null, cacheWrite: null });
  const without = priceCall(prices, { input: 100, output: 200, reasoning: null, cached: null, cacheWrite: null });
  assert.equal(withReasoning, without);
  assert.equal(without, 3300); // 100*3e6 + 200*15e6 = 3.3e9 / 1e6
});

test('price rounds a fraction of a micro-dollar up', () => {
  assert.equal(priceCall({ ...prices, input: 1 }, { input: 1, output: 0, reasoning: null, cached: null, cacheWrite: null }), 1);
  assert.equal(priceCall(prices, { input: 0, output: 0, reasoning: null, cached: null, cacheWrite: null }), 0);
});

test('price: parts that exceed the input never make the plain part negative', () => {
  assert.equal(priceCall(prices, { input: 10, output: 0, reasoning: null, cached: 10, cacheWrite: 10 }), Math.ceil((10 * 300_000 + 10 * 3_750_000) / 1e6));
});

test('worst case, by hand', () => {
  // 3000 input tokens at the dearest input price (3_750_000) + 8192 output at 15_000_000
  // = 11_250_000_000 + 122_880_000_000 = 134_130_000_000; / 1e6 = 134_130
  assert.equal(worstCase(prices, { inputTokens: 3000, maxOutputTokens: 8192 }), 134_130);
  assert.equal(estimateInputTokens(9000), 3000);
  assert.equal(estimateInputTokens(9001), 3001);
});

test('a call with usage is charged its price; a call without is charged its worst case and flagged', () => {
  const ok = settleCall({ prices, rawUsage: { prompt_tokens: 100, completion_tokens: 200 }, worstMicro: 134_130 });
  assert.deepEqual([ok.costMicro, ok.usageMissing], [3300, false]);
  for (const rawUsage of [undefined, null, {}, { prompt_tokens: 5 }]) {
    const missing = settleCall({ prices, rawUsage, worstMicro: 134_130 });
    assert.deepEqual([missing.costMicro, missing.usageMissing, missing.usage], [134_130, true, null]);
  }
});

test('money in and out', () => {
  assert.equal(usdToMicro(0.3), 300_000);
  assert.equal(usdToMicro(3.75), 3_750_000);
  assert.equal(usdToMicro(-1), null);
  assert.equal(usdToMicro('3'), null);
  assert.equal(usdToMicro(Number.NaN), null);
  assert.equal(formatUSD(0), '$0.0000');
  assert.equal(formatUSD(4995), '$0.0050');
  assert.equal(formatUSD(1_234_567), '$1.2346');
  assert.equal(formatUSD(1_234_567, 6), '$1.234567');
  assert.equal(formatUSD(1_234_567, 0), '$1');
  assert.equal(formatUSD(10_000_000), '$10.0000');
});

test('sums stay exact where floating point would drift', () => {
  let micro = 0;
  for (let i = 0; i < 10; i++) micro += usdToMicro(0.1);
  assert.equal(micro, 1_000_000);
  assert.equal(formatUSD(micro), '$1.0000');
});
