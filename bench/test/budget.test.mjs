import assert from 'node:assert/strict';
import { test } from 'node:test';
import { normalizeConfig } from '../lib/config.mjs';
import { BudgetError, configDigest, confirmProposal, lockDigest, pilotEstimate, proposalBody, renderEstimate, requireConfirmed } from '../lib/budget.mjs';
import { PINS } from '../lib/pins.mjs';
import { rawConfig } from './helpers.mjs';

const config = normalizeConfig(rawConfig({ capUSD: 10 }));
const LOCKS = ['{"public":1}', ''];

/** A folded ledger of finished pilot runs: [arm, cost in micro-dollars]. */
function folded(rows, { extra = [] } = {}) {
  const m = new Map();
  rows.forEach(([arm, cost], i) => m.set(`r${i}`, { run: `r${i}`, arm, state: 'Done', result: { pass: true }, attemptSpentMicro: cost, spentMicro: cost }));
  for (const v of extra) m.set(v.run, v);
  return m;
}

test('the proposed budget is the 95th percentile of the arm times 1.5, by hand', () => {
  // ten runs of "max": 1..10 units of 100_000; nearest rank p95 of 10 values is the 10th: 1_000_000; x1.5 = 1_500_000
  const rows = [...Array(10).keys()].map((i) => ['max', (i + 1) * 100_000]);
  // "off": five runs 40_000 .. 200_000, p95 is the 5th = 200_000; x1.5 = 300_000
  rows.push(...[40_000, 80_000, 120_000, 160_000, 200_000].map((c) => ['off', c]));
  const est = pilotEstimate({ folded: folded(rows), pilotRuns: new Set(rows.map((_, i) => `r${i}`)), arms: ['max', 'off'], totalTasks: 10, agents: 2, capMicro: 10_000_000 });
  assert.deepEqual(est.perArmMicro, { max: 1_500_000, off: 300_000 });
  assert.equal(est.perArm.max.p95Micro, 1_000_000);
  assert.equal(est.perArm.max.meanMicro, 550_000);
  assert.equal(est.perArm.off.meanMicro, 120_000);
  assert.equal(est.perArm.max.done, 10);
});

test('the projection: spent so far, what is left reserved at the budget, and the expected spend', () => {
  const rows = [...Array(4).keys()].map(() => ['max', 100_000]).concat([...Array(4).keys()].map(() => ['off', 50_000]));
  const est = pilotEstimate({ folded: folded(rows), pilotRuns: new Set(rows.map((_, i) => `r${i}`)), arms: ['max', 'off'], totalTasks: 5, agents: 2, capMicro: 2_000_000 });
  // budgets: max 150_000, off 75_000. runs per arm 10, 4 done in each: 6 left in each
  assert.deepEqual(est.perArmMicro, { max: 150_000, off: 75_000 });
  assert.equal(est.projection.spentMicro, 600_000);
  assert.equal(est.projection.reservedIfAllMicro, 600_000 + 6 * 150_000 + 6 * 75_000); // 1_950_000
  assert.equal(est.projection.expectedMicro, 600_000 + 6 * 100_000 + 6 * 50_000); // 1_500_000
  assert.equal(est.projection.fitsCap, true);
  const tight = pilotEstimate({ folded: folded(rows), pilotRuns: new Set(rows.map((_, i) => `r${i}`)), arms: ['max', 'off'], totalTasks: 5, agents: 2, capMicro: 1_949_999 });
  assert.equal(tight.projection.fitsCap, false, 'one micro-dollar short');
  assert.match(renderEstimate(tight), /do NOT all fit/);
  assert.match(renderEstimate(est), /fit under the cap/);
});

test('too few finished runs in an arm is an error, not a guess; invalid grades and runs outside the pilot do not count', () => {
  const rows = [['max', 1], ['max', 1], ['max', 1], ['off', 1], ['off', 1]];
  assert.throws(() => pilotEstimate({ folded: folded(rows), pilotRuns: new Set(['r0', 'r1', 'r2', 'r3', 'r4']), arms: ['max', 'off'], totalTasks: 3, agents: 2, capMicro: 1e9 }), BudgetError);
  const f = folded([['max', 1], ['max', 1], ['max', 1]]);
  f.get('r2').result.invalid = true;
  assert.throws(() => pilotEstimate({ folded: f, pilotRuns: new Set(['r0', 'r1', 'r2']), arms: ['max'], totalTasks: 3, agents: 1, capMicro: 1e9 }), /2 finished pilot run/);
  assert.throws(() => pilotEstimate({ folded: folded([['max', 1], ['max', 1], ['max', 1]]), pilotRuns: new Set(['r0']), arms: ['max'], totalTasks: 3, agents: 1, capMicro: 1e9 }), /1 finished pilot run/);
});

function proposal(over = {}) {
  const rows = [...Array(3).keys()].map(() => ['max', 100_000]).concat([...Array(3).keys()].map(() => ['off', 100_000]));
  const estimate = pilotEstimate({ folded: folded(rows), pilotRuns: new Set(rows.map((_, i) => `r${i}`)), arms: ['max', 'off'], totalTasks: 3, agents: 1, capMicro: config.capMicro });
  return { ...proposalBody({ estimate, config, pins: PINS, lockTexts: LOCKS, pilotBlocks: 3 }), ...over };
}

test('a proposal is not confirmed; run needs a confirmed one for this configuration, these locks and these pins', () => {
  const p = proposal();
  assert.equal(p.confirmed, false);
  assert.throws(() => requireConfirmed(null, { config, pins: PINS, lockTexts: LOCKS }), /no pilot budget/);
  assert.throws(() => requireConfirmed(p, { config, pins: PINS, lockTexts: LOCKS }), /not confirmed/);
  const ok = confirmProposal(p);
  assert.deepEqual(requireConfirmed(ok, { config, pins: PINS, lockTexts: LOCKS }), { max: 150_000, off: 150_000 });
  // a changed model, price, arm, limit, pin or lock invalidates the pilot
  const other = (raw) => normalizeConfig(rawConfig(raw));
  for (const changed of [other({ modelId: 'another-example-model' }), other({ arms: { max: { reasoning_effort: 'high' }, off: { reasoning_effort: 'low' } } }), other({ prices: { inputPerMTok: 4, cachedInputPerMTok: 0.3, cacheWritePerMTok: 3.75, outputPerMTok: 15 } })]) {
    assert.notEqual(configDigest(changed, PINS), configDigest(config, PINS));
    assert.throws(() => requireConfirmed(ok, { config: changed, pins: PINS, lockTexts: LOCKS }), /changed since the pilot/);
  }
  assert.throws(() => requireConfirmed(ok, { config, pins: { ...PINS, coder: { ...PINS.coder, tag: 'sha-0000000' } }, lockTexts: LOCKS }), /changed since the pilot/);
  assert.throws(() => requireConfirmed(ok, { config, pins: PINS, lockTexts: ['{"public":2}', ''] }), /task locks changed/);
  assert.notEqual(lockDigest(['a', 'b']), lockDigest(['ab', '']));
});

test('the endpoint and the cap are not part of what a pilot is for', () => {
  const a = normalizeConfig(rawConfig({ endpoint: 'https://models.example.test/v1', capUSD: 10 }));
  const b = normalizeConfig(rawConfig({ endpoint: 'https://other.example.test/v1', capUSD: 50 }));
  assert.equal(configDigest(a, PINS), configDigest(b, PINS));
});

test('a projection above the cap needs --over-cap, once to confirm and again to run', () => {
  const over = proposal({ projection: { ...proposal().projection, fitsCap: false } });
  assert.throws(() => confirmProposal(over), /--over-cap/);
  const accepted = confirmProposal(over, { overCap: true });
  assert.equal(accepted.overCap, true);
  assert.doesNotThrow(() => requireConfirmed(accepted, { config, pins: PINS, lockTexts: LOCKS }));
  // the file edited by hand to confirmed, without the acceptance, is refused
  assert.throws(() => requireConfirmed({ ...over, confirmed: true }, { config, pins: PINS, lockTexts: LOCKS }), /above the cap/);
});

test('a budget that is zero, missing or above the cap is refused', () => {
  const ok = confirmProposal(proposal());
  assert.throws(() => requireConfirmed({ ...ok, perArmMicro: { max: 150_000 } }, { config, pins: PINS, lockTexts: LOCKS }), /arm off/);
  assert.throws(() => requireConfirmed({ ...ok, perArmMicro: { max: 0, off: 1 } }, { config, pins: PINS, lockTexts: LOCKS }), /arm max/);
  assert.throws(() => requireConfirmed({ ...ok, perArmMicro: { max: config.capMicro + 1, off: 1 } }, { config, pins: PINS, lockTexts: LOCKS }), /above the cap/);
});
