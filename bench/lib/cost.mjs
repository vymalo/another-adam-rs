// Money and tokens. Money is an integer number of micro-dollars (1 USD = 1_000_000), never a
// floating-point sum. A price is micro-dollars per million tokens.
//
// A call costs
//   (input - cached - cacheWrite) * price.input + cached * price.cachedInput
//   + cacheWrite * price.cacheWrite + output * price.output
// rounded up to a whole micro-dollar. Reasoning tokens are inside `output`.

export const MICRO_PER_USD = 1_000_000;
const MILLION = 1_000_000n;

const isCount = (v) => Number.isSafeInteger(v) && v >= 0;

/** USD (a number a person wrote) to micro-dollars, or null when it is not a usable amount. */
export function usdToMicro(usd, max = 1e9) {
  if (typeof usd !== 'number' || !Number.isFinite(usd) || usd < 0 || usd > max) return null;
  return Math.round(usd * MICRO_PER_USD);
}

/** Micro-dollars as `$1.2346` (rounded half up to `digits` places, 0 to 6). */
export function formatUSD(micro, digits = 4) {
  const negative = micro < 0;
  const abs = BigInt(Math.abs(micro));
  const drop = 10n ** BigInt(6 - digits);
  const scaled = (abs + drop / 2n) / drop;
  const unit = 10n ** BigInt(digits);
  const whole = scaled / unit;
  const frac = digits === 0 ? '' : `.${(scaled % unit).toString().padStart(digits, '0')}`;
  return `${negative ? '-' : ''}$${whole}${frac}`;
}

/**
 * The usage object of an OpenAI-compatible response, normalised as `Usage::accounted` does
 * (docs/decisions/0032-usage-per-model-call.md, crates/adam-model/src/types.rs): reasoning above
 * the output is added to the output, cache reads and writes above the input are added to the
 * input, so every part fits in its total. A detail that is absent or not a whole number is null.
 * Returns null when the totals themselves are missing: the call has no usable usage.
 */
export function normalizeUsage(raw) {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return null;
  if (!isCount(raw.prompt_tokens) || !isCount(raw.completion_tokens)) return null;
  const detail = (d, name) => (d && typeof d === 'object' && isCount(d[name]) ? d[name] : null);
  const reasoning = detail(raw.completion_tokens_details, 'reasoning_tokens');
  const cached = detail(raw.prompt_tokens_details, 'cached_tokens');
  const cacheWrite = detail(raw.prompt_tokens_details, 'cache_write_tokens');
  let input = raw.prompt_tokens;
  let output = raw.completion_tokens;
  if (reasoning !== null && reasoning > output) output += reasoning;
  if ((cached ?? 0) + (cacheWrite ?? 0) > input) input += (cached ?? 0) + (cacheWrite ?? 0);
  return { input, output, reasoning, cached, cacheWrite };
}

const ceilDiv = (a, b) => (a + b - 1n) / b;

/** Micro-dollars for normalised usage, rounded up. */
export function priceCall(prices, usage) {
  const cached = usage.cached ?? 0;
  const cacheWrite = usage.cacheWrite ?? 0;
  const plain = Math.max(0, usage.input - cached - cacheWrite);
  const total =
    BigInt(plain) * BigInt(prices.input) +
    BigInt(cached) * BigInt(prices.cachedInput) +
    BigInt(cacheWrite) * BigInt(prices.cacheWrite) +
    BigInt(usage.output) * BigInt(prices.output);
  return Number(ceilDiv(total, MILLION));
}

/** A cheap upper estimate of the input tokens of a request body: one token per three bytes. */
export function estimateInputTokens(bytes) {
  return Math.ceil(bytes / 3);
}

/** The most one call can cost: every input token at the dearest input price, every output token. */
export function worstCase(prices, { inputTokens, maxOutputTokens }) {
  const dearestInput = Math.max(prices.input, prices.cachedInput, prices.cacheWrite);
  const total = BigInt(inputTokens) * BigInt(dearestInput) + BigInt(maxOutputTokens) * BigInt(prices.output);
  return Number(ceilDiv(total, MILLION));
}

/**
 * What a call is charged. With usable usage: its price. Without: the call's worst case, flagged,
 * because an unreported call may still have been billed.
 */
export function settleCall({ prices, rawUsage, worstMicro }) {
  const usage = normalizeUsage(rawUsage);
  if (usage === null) return { costMicro: worstMicro, usage: null, usageMissing: true };
  return { costMicro: priceCall(prices, usage), usage, usageMissing: false };
}
