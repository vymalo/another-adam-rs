import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { normalizeConfig } from '../lib/config.mjs';
import { BudgetError, assertLedgerBound, configFingerprint, confirmProposal, digestKey, pilotEstimate, proposalBody, renderEstimate, requireConfirmed } from '../lib/budget.mjs';
import { PINS } from '../lib/pins.mjs';
import { rawConfig, tmp } from './helpers.mjs';

const KEY = 'key-for-tests-0001';
const mk = (over = {}) => normalizeConfig(rawConfig({ capUSD: 10, ...over }));
const config = mk();
const LOCKS = ['{"public":1}', ''];
const fp = (c = config, { pins = PINS, lockTexts = LOCKS, key = KEY } = {}) => configFingerprint({ config: c, pins, lockTexts, key });

/** A folded ledger of finished pilot runs: [arm, cost in micro-dollars, largest worst case of a call, exit]. */
function folded(rows) {
  const m = new Map();
  rows.forEach(([arm, cost, worst = 0, exit = 'done'], i) =>
    m.set(`r${i}`, { run: `r${i}`, arm, state: 'Done', result: { pass: true, exit }, attemptSpentMicro: cost, spentMicro: cost, attempts: [{ calls: new Map([[1, { worstMicro: worst, costMicro: cost }]]) }] }),
  );
  return m;
}
const ids = (rows) => new Set(rows.map((_, i) => `r${i}`));
const estimate = (rows, o = {}) => pilotEstimate({ folded: folded(rows), pilotRuns: ids(rows), arms: ['max', 'off'], totalTasks: 3, agents: 1, capMicro: 10_000_000, ...o });

test('the proposed budget is 1.5 times the p95 of the arm, by hand (no worst case seen)', () => {
  const rows = [...Array(10).keys()].map((i) => ['max', (i + 1) * 100_000]);
  rows.push(...[40_000, 80_000, 120_000, 160_000, 200_000].map((c) => ['off', c]));
  const est = estimate(rows, { totalTasks: 10, agents: 2 });
  assert.deepEqual(est.perArmMicro, { max: 1_500_000, off: 300_000 });
  assert.equal(est.perArm.max.p95Micro, 1_000_000);
  assert.equal(est.perArm.max.meanMicro, 550_000);
  assert.equal(est.perArm.off.meanMicro, 120_000);
});

test('R2: the budget is 1.5 p95 plus the largest worst case of a call, and never under two worst cases', () => {
  // p95 100_000, worst 400_000: 150_000 + 400_000 = 550_000 beats the floor 800_000? no: the floor wins
  const floorRows = [...Array(3).keys()].map(() => ['max', 100_000, 400_000]).concat([...Array(3).keys()].map(() => ['off', 100_000, 400_000]));
  const f = estimate(floorRows);
  assert.equal(f.perArm.max.floorMicro, 800_000);
  assert.equal(f.perArmMicro.max, 800_000);
  assert.equal(f.perArm.max.wonBy, 'floor');
  // p95 1_000_000, worst 100_000: 1_500_000 + 100_000 = 1_600_000 beats the floor 200_000
  const padRows = [...Array(3).keys()].map(() => ['max', 1_000_000, 100_000]).concat([...Array(3).keys()].map(() => ['off', 1_000_000, 100_000]));
  const p = estimate(padRows);
  assert.equal(p.perArmMicro.max, 1_600_000);
  assert.equal(p.perArm.max.wonBy, 'p95');
  assert.match(renderEstimate(p), /95th percentile \$1\.0000, largest worst case of one call \$0\.1000, floor \$0\.2000; proposed budget per run \$1\.6000/);
});

test('R2: a pilot that ended budget_hit is censored, and the owner has to accept that on purpose', () => {
  const rows = [['max', 100_000, 0, 'budget_hit'], ['max', 100_000], ['max', 100_000], ['off', 100_000], ['off', 100_000], ['off', 100_000]];
  const est = estimate(rows);
  assert.deepEqual(est.censoredArms, ['max']);
  assert.match(renderEstimate(est), /ended budget_hit.*--censored/);
  const p = proposalBody({ estimate: est, fingerprint: fp(), pilotBlocks: 3 });
  assert.throws(() => confirmProposal(p), /--censored/);
  const ok = confirmProposal(p, { censored: true });
  assert.doesNotThrow(() => requireConfirmed(ok, { config, fingerprint: fp() }));
  assert.throws(() => requireConfirmed({ ...ok, censoredAccepted: false }, { config, fingerprint: fp() }), /censored/);
});

test('the projection: every run of the plan is still to come, the planned figure has a margin, by hand', () => {
  const rows = [...Array(4).keys()].map(() => ['max', 100_000]).concat([...Array(4).keys()].map(() => ['off', 50_000]));
  const est = estimate(rows, { totalTasks: 5, agents: 2, capMicro: 3_000_000 });
  // budgets: max 150_000, off 75_000. runs per arm 10, none of them done in the full run
  assert.deepEqual(est.perArmMicro, { max: 150_000, off: 75_000 });
  assert.equal(est.projection.spentMicro, 600_000);
  assert.equal(est.projection.reservedIfAllMicro, 600_000 + 10 * 150_000 + 10 * 75_000); // 2_850_000
  assert.equal(est.projection.expectedMicro, 600_000 + 10 * 100_000 + 10 * 50_000); // 2_100_000
  assert.equal(est.projection.planningMicro, 600_000 + Math.ceil(1_500_000 * 1.2)); // 2_400_000
  assert.equal(est.projection.fitsCap, true);
  assert.equal(estimate(rows, { totalTasks: 5, agents: 2, capMicro: 2_399_999 }).projection.fitsCap, false, 'one micro-dollar short');
  assert.match(renderEstimate(estimate(rows, { totalTasks: 5, agents: 2, capMicro: 1_000_000 })), /does NOT fit/);
});

test('too few finished runs in an arm is an error, not a guess; invalid grades and runs outside the pilot do not count', () => {
  assert.throws(() => estimate([['max', 1], ['max', 1], ['max', 1], ['off', 1], ['off', 1]]), BudgetError);
  const f = folded([['max', 1], ['max', 1], ['max', 1]]);
  f.get('r2').result.invalid = true;
  assert.throws(() => pilotEstimate({ folded: f, pilotRuns: ids([1, 1, 1]), arms: ['max'], totalTasks: 3, agents: 1, capMicro: 1e9 }), /2 finished pilot run/);
  assert.throws(() => pilotEstimate({ folded: folded([['max', 1], ['max', 1], ['max', 1]]), pilotRuns: new Set(['r0']), arms: ['max'], totalTasks: 3, agents: 1, capMicro: 1e9 }), /1 finished pilot run/);
});

const six = () => [...Array(3).keys()].map(() => ['max', 100_000]).concat([...Array(3).keys()].map(() => ['off', 100_000]));
const proposal = (over = {}) => ({ ...proposalBody({ estimate: estimate(six()), fingerprint: fp(), pilotBlocks: 3 }), ...over });

test('a proposal is not confirmed; run needs a confirmed one for this configuration, these locks and these pins', () => {
  const p = proposal();
  assert.equal(p.confirmed, false);
  assert.throws(() => requireConfirmed(null, { config, fingerprint: fp() }), /no pilot budget/);
  assert.throws(() => requireConfirmed(p, { config, fingerprint: fp() }), /not confirmed/);
  const ok = confirmProposal(p);
  assert.deepEqual(requireConfirmed(ok, { config, fingerprint: fp() }), { max: 150_000, off: 150_000 });
});

test('R1: the fingerprint covers model, prices, arms, limits, context window, pins, locks, seed and selfRepo, and names what changed', () => {
  const base = fp();
  const cases = {
    model: fp(mk({ modelId: 'another-example-model' })),
    prices: fp(mk({ prices: { inputPerMTok: 4, cachedInputPerMTok: 0.3, cacheWritePerMTok: 3.75, outputPerMTok: 15 } })),
    arms: fp(mk({ arms: { max: { reasoning_effort: 'high' }, off: { reasoning_effort: 'low' } } })),
    limits: fp(mk({ limits: { minutes: 20, steps: 100, cancelGraceSeconds: 20 } })),
    contextWindow: fp(mk({ contextWindow: 65536 })),
    pins: fp(config, { pins: { ...PINS, coder: { ...PINS.coder, tag: 'sha-0000000' } } }),
    locks: fp(config, { lockTexts: ['{"public":2}', ''] }),
    seed: fp(mk({ tasks: { seed: '99' } })),
    selfRepo: fp(mk({ tasks: { selfRepo: 'another-repo' } })),
  };
  const ok = confirmProposal(proposal());
  for (const [name, changed] of Object.entries(cases)) {
    assert.notEqual(changed.digest, base.digest, name);
    assert.throws(() => requireConfirmed(ok, { config, fingerprint: changed }), new RegExp(`changed since the pilot \\(${name}\\)`), name);
  }
});

test('R1: the endpoint, the cap and the concurrency are deliberately not part of it, and the key makes the fields untestable without it', () => {
  const a = fp(mk({ endpoint: 'https://models.example.test/v1', capUSD: 10, concurrency: 2 }));
  const b = fp(mk({ endpoint: 'https://other.example.test/v1', capUSD: 50, concurrency: 1 }));
  assert.equal(a.digest, b.digest);
  assert.notEqual(fp(config, { key: 'another-key' }).fields.model, fp().fields.model);
  const home = tmp({ after() {} });
  const k1 = digestKey(path.join(home, 'h'));
  assert.equal(digestKey(path.join(home, 'h')), k1, 'made once');
  assert.equal(fs.statSync(path.join(home, 'h', 'digest.key')).mode & 0o777, 0o600);
  fs.rmSync(home, { recursive: true, force: true });
});

test('R1: a ledger with runs of another configuration is refused, naming the fields, and an unstamped ledger too', () => {
  const now = fp();
  const old = fp(mk({ modelId: 'another-example-model' }));
  const planned = (digest) => ({ kind: 'state', run: 'r1', state: 'Planned', digest });
  assert.doesNotThrow(() => assertLedgerBound([], now));
  assert.doesNotThrow(() => assertLedgerBound([{ kind: 'setup', digest: now.digest, fields: now.fields }, planned(now.digest)], now));
  assert.throws(() => assertLedgerBound([{ kind: 'setup', digest: old.digest, fields: old.fields }, planned(old.digest)], now), /another configuration \(changed: model\).*new BENCH_HOME/);
  assert.throws(() => assertLedgerBound([{ kind: 'state', run: 'r1', state: 'Planned' }], now), /another configuration/, 'a run with no digest is from before the binding');
  assert.throws(() => assertLedgerBound([planned(now.digest), { ...planned(old.digest), run: 'r2' }], now), /another configuration/);
});

test('R12: the cap is not in the fingerprint, so the projection is checked against the live cap at run', () => {
  const p = confirmProposal(proposal());
  assert.equal(p.projection.fitsCap, true);
  const below = { ...config, capMicro: p.projection.planningMicro - 1 };
  assert.throws(() => requireConfirmed(p, { config: below, fingerprint: fp() }), /cap is below what the confirmed projection plans/);
  assert.doesNotThrow(() => requireConfirmed({ ...p, overCap: true }, { config: below, fingerprint: fp() }));
  assert.doesNotThrow(() => requireConfirmed(p, { config: { ...config, capMicro: p.projection.planningMicro }, fingerprint: fp() }));
});

test('a projection above the cap needs --over-cap to confirm', () => {
  const over = proposal({ projection: { ...proposal().projection, fitsCap: false } });
  assert.throws(() => confirmProposal(over), /--over-cap/);
  assert.equal(confirmProposal(over, { overCap: true }).overCap, true);
});

test('a budget that is zero, missing, above the cap or under its floor is refused', () => {
  const ok = confirmProposal(proposal());
  assert.throws(() => requireConfirmed({ ...ok, perArmMicro: { max: 150_000 } }, { config, fingerprint: fp() }), /arm off/);
  assert.throws(() => requireConfirmed({ ...ok, perArmMicro: { max: 0, off: 1 } }, { config, fingerprint: fp() }), /arm max/);
  assert.throws(() => requireConfirmed({ ...ok, perArmMicro: { max: config.capMicro + 1, off: 1 } }, { config, fingerprint: fp() }), /above the cap/);
  const floored = { ...ok, perArm: { max: { floorMicro: 200_000 }, off: { floorMicro: 0 } } };
  assert.throws(() => requireConfirmed(floored, { config, fingerprint: fp() }), /under its floor/);
});
