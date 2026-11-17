import assert from 'node:assert/strict';
import { test } from 'node:test';
import { bootstrapPairedDiff, mcnemarExact, median, mulberry32, percentile, seedFrom, wilson } from '../lib/stats.mjs';

const near = (actual, expected, tolerance = 5e-4) => assert.ok(Math.abs(actual - expected) <= tolerance, `${actual} is not within ${tolerance} of ${expected}`);

test('Wilson 95% interval, against values computed by hand', () => {
  // k = 8, n = 10, z = 1.959964, z^2 = 3.8416:
  //   denominator = 1 + 3.8416/10 = 1.38416
  //   centre = (0.8 + 3.8416/20) / 1.38416 = 0.99208 / 1.38416 = 0.71674
  //   half = 1.959964 * sqrt(0.8*0.2/10 + 3.8416/400) / 1.38416 = 1.959964 * 0.160012 / 1.38416 = 0.22658
  //   interval = [0.4902, 0.9433]
  const w = wilson(8, 10);
  near(w.p, 0.8, 1e-12);
  near(w.low, 0.4902);
  near(w.high, 0.9433);
  // k = 0: the lower end is 0 and the upper end is z^2 / (n + z^2) = 3.8416 / 13.8416 = 0.27753
  const none = wilson(0, 10);
  assert.equal(none.low, 0);
  near(none.high, 0.2775);
  // k = n: the mirror image, [0.7225, 1]
  const all = wilson(10, 10);
  near(all.low, 0.7225);
  assert.ok(all.high > 0.9999999 && all.high <= 1);
  // 1 of 1: centre (1 + 1.9208) / 4.8416 = 0.60328, half 1.959964 * 0.98 / 4.8416 = 0.39673 -> [0.2066, 1]
  near(wilson(1, 1).low, 0.2066);
  assert.equal(wilson(1, 1).high, 1);
  // 4 of 6: centre 0.60159, half 0.30161 -> [0.2999, 0.9032]
  near(wilson(4, 6).low, 0.3);
  near(wilson(4, 6).high, 0.9032);
});

test('Wilson: no runs is no interval, and impossible counts are refused', () => {
  assert.equal(wilson(0, 0), null);
  assert.throws(() => wilson(5, 4), RangeError);
  assert.throws(() => wilson(-1, 4), RangeError);
  assert.throws(() => wilson(1.5, 4), RangeError);
});

test('exact McNemar test, against values computed by hand', () => {
  // two-sided: p = min(1, 2 * P(X <= min(b, c))) for X ~ Binomial(b + c, 1/2)
  assert.equal(mcnemarExact(0, 0), 1);
  assert.equal(mcnemarExact(1, 5), 0.21875); // n = 6: 2 * (1 + 6) / 64 = 14 / 64
  assert.equal(mcnemarExact(5, 1), 0.21875, 'symmetric');
  assert.equal(mcnemarExact(0, 6), 0.03125); // 2 * 1 / 64
  assert.equal(mcnemarExact(3, 3), 1); // 2 * 42 / 64 > 1
  assert.equal(mcnemarExact(2, 8), 0.109375); // n = 10: 2 * (1 + 10 + 45) / 1024 = 112 / 1024
  assert.equal(mcnemarExact(0, 10), 0.001953125); // 2 / 1024
});

test('the seeded generator: mulberry32', () => {
  const a = mulberry32(42);
  const b = mulberry32(42);
  const first = Array.from({ length: 20 }, () => a());
  assert.deepEqual(first, Array.from({ length: 20 }, () => b()), 'the same seed gives the same sequence');
  assert.notDeepEqual(first, Array.from({ length: 20 }, mulberry32(43)), 'another seed does not');
  assert.ok(first.every((x) => x >= 0 && x < 1));
  const rand = mulberry32(7);
  let sum = 0;
  for (let i = 0; i < 20000; i++) sum += rand();
  near(sum / 20000, 0.5, 0.01);
  // A regression pin, not a hand value: the first output for seed 1 as this implementation gives it
  // (it is also the value the reference mulberry32 gives).
  assert.equal(mulberry32(1)(), 0.6270739405881613);
});

test('seedFrom is FNV-1a over the UTF-8 bytes', () => {
  assert.equal(seedFrom('a'), 0xe40c292c); // the published FNV-1a 32-bit hash of "a"
  assert.equal(seedFrom('abc'), 0x1a47e90b);
  assert.equal(seedFrom(''), 0x811c9dc5, 'the offset basis');
});

test('bootstrap of a paired difference, against hand-computed values', () => {
  // Every task solved by the first only: the difference is 1 whatever is resampled.
  const sure = bootstrapPairedDiff(Array.from({ length: 10 }, () => [1, 0]), { seed: 3 });
  assert.deepEqual([sure.diff, sure.low, sure.high], [1, 1, 1]);
  // One task each way: resampled means are -1, 0, 1 with probabilities 1/4, 1/2, 1/4.
  const split = bootstrapPairedDiff([[1, 0], [0, 1]], { seed: 3 });
  assert.deepEqual([split.diff, split.low, split.high], [0, -1, 1]);
  // Six of ten tasks go to the first agent only: each resample's mean is Binomial(10, 0.6) / 10.
  // P(X <= 2) = 0.0123 < 0.025 <= P(X <= 3) = 0.0548, so the 2.5th percentile is 0.3;
  // P(X <= 8) = 0.9536 < 0.975 <= P(X <= 9) = 0.9940, so the 97.5th percentile is 0.9.
  const six = bootstrapPairedDiff([...Array.from({ length: 6 }, () => [1, 0]), ...Array.from({ length: 4 }, () => [0, 0])], { seed: 11 });
  near(six.diff, 0.6, 1e-12);
  near(six.low, 0.3, 1e-12);
  near(six.high, 0.9, 1e-12);
});

test('bootstrap is deterministic for a seed and empty for no tasks', () => {
  const pairs = [[1, 0], [1, 1], [0, 1], [1, 0], [0, 0], [1, 0]];
  assert.deepEqual(bootstrapPairedDiff(pairs, { seed: 5 }), bootstrapPairedDiff(pairs, { seed: 5 }));
  assert.equal(bootstrapPairedDiff([], { seed: 5 }), null);
});

test('median and nearest-rank percentile', () => {
  assert.equal(median([]), null);
  assert.equal(median([3, 1, 2]), 2);
  assert.equal(median([4, 1, 2, 3]), 2.5);
  assert.equal(percentile([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 90), 9);
  assert.equal(percentile([5], 90), 5);
  assert.equal(percentile([], 90), null);
});
