// Statistics for the report, with no dependencies.
//
// * wilson: the Wilson score interval for a proportion (z = 1.959964 for 95 %).
// * mcnemarExact: the exact two-sided McNemar test on the discordant pairs, a two-sided binomial
//   test of b against b + c at 1/2.
// * bootstrapPairedDiff: a percentile bootstrap interval of the paired difference of two columns
//   of numbers over tasks, resampling tasks with replacement, with a seeded generator.
// * mulberry32: the seeded generator (Tommy Ettinger's public-domain 32-bit generator).
//   A seed is an unsigned 32-bit integer; `seedFrom` makes one from a string (FNV-1a, 32 bit).

const Z95 = 1.959964;

export function wilson(successes, n, z = Z95) {
  if (!Number.isInteger(n) || n < 0 || !Number.isInteger(successes) || successes < 0 || successes > n) {
    throw new RangeError('successes must be between 0 and n');
  }
  if (n === 0) return null;
  const p = successes / n;
  const z2 = z * z;
  const denom = 1 + z2 / n;
  const center = (p + z2 / (2 * n)) / denom;
  const half = (z * Math.sqrt((p * (1 - p)) / n + z2 / (4 * n * n))) / denom;
  return { p, low: Math.max(0, center - half), high: Math.min(1, center + half) };
}

/**
 * P(X <= k) for X ~ Binomial(n, 1/2). Up to n = 1000 the terms are summed from the smallest as
 * they are (exact for small n). Above it 2^-n would underflow, so the terms are built in log
 * space; a term below the smallest double is dropped (it is that small).
 */
function binomialCdfHalf(k, n) {
  let sum = 0;
  if (n <= 1000) {
    let term = 0.5 ** n;
    for (let i = 0; i <= k; i++) {
      sum += term;
      term = (term * (n - i)) / (i + 1);
    }
    return sum;
  }
  let logTerm = -n * Math.LN2;
  for (let i = 0; i <= k; i++) {
    sum += Math.exp(logTerm);
    logTerm += Math.log(n - i) - Math.log(i + 1);
  }
  return sum;
}

/** Exact two-sided McNemar p-value from the two discordant counts. */
export function mcnemarExact(b, c) {
  const n = b + c;
  if (n === 0) return 1;
  return Math.min(1, 2 * binomialCdfHalf(Math.min(b, c), n));
}

export function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export function seedFrom(text) {
  let h = 0x811c9dc5;
  for (const byte of Buffer.from(String(text), 'utf8')) {
    h ^= byte;
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h >>> 0;
}

/** pairs: [[a, b], ...], one per task. Returns the mean difference a - b and a percentile interval. */
export function bootstrapPairedDiff(pairs, { seed = 1, iterations = 10000, level = 0.95 } = {}) {
  const n = pairs.length;
  if (n === 0) return null;
  const diffs = pairs.map(([a, b]) => a - b);
  const mean = diffs.reduce((s, d) => s + d, 0) / n;
  const rand = mulberry32(seed);
  const means = new Float64Array(iterations);
  for (let it = 0; it < iterations; it++) {
    let sum = 0;
    for (let i = 0; i < n; i++) sum += diffs[Math.floor(rand() * n)];
    means[it] = sum / n;
  }
  means.sort();
  const tail = (1 - level) / 2;
  const low = means[Math.floor(tail * iterations)];
  const high = means[Math.ceil((1 - tail) * iterations) - 1];
  return { n, diff: mean, low, high };
}

export function median(values) {
  if (values.length === 0) return null;
  const s = [...values].sort((x, y) => x - y);
  const mid = s.length >> 1;
  return s.length % 2 ? s[mid] : (s[mid - 1] + s[mid]) / 2;
}

/** Nearest-rank percentile, p in (0, 100]. */
export function percentile(values, p) {
  if (values.length === 0) return null;
  const s = [...values].sort((x, y) => x - y);
  return s[Math.max(0, Math.ceil((p / 100) * s.length) - 1)];
}
