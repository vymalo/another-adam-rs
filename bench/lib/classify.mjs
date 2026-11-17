// Infrastructure is told apart from the agent (finding 10 of the review of slice 3): one function, a table of rules.
//
// A run is infrastructure (`Failed_infra`: tried again as the resume rules say, then `invalid`, never a fail) when
//   * the container runtime failed;
//   * the agent made no model call at all;
//   * the meter recorded upstream errors (a non-2xx answer, a refused connection, a call cut at a limit) on more than
//     two calls, or on the last call before the end;
//   * the coder ended its task `failed` with words that name an external or transient cause.
// A refused call is a limit (`budget_hit`, `stopped_by_cap`), not an error, and stands. Otherwise the outcome stands.
// The input is the meter's own record, never an agent's.

const UPSTREAM_ERRORS_ALLOWED = 2;

/** Counts of one attempt's calls from the ledger's records. */
export function callStats(records, run, attempt) {
  const calls = new Map();
  for (const r of records) {
    if (r.kind !== 'call' || r.run !== run || r.attempt !== attempt) continue;
    const prior = calls.get(r.call) ?? {};
    if (r.phase === 'pending') calls.set(r.call, { ...prior, pending: true });
    else if (r.phase === 'refused') calls.set(r.call, { ...prior, refused: true });
    else if (r.phase === 'failed') calls.set(r.call, { ...prior, bad: true });
    else if (r.phase === 'settled') calls.set(r.call, { ...prior, settled: true, bad: Boolean(r.reason) || prior.bad });
  }
  let settled = 0;
  let upstreamErrors = 0;
  let refused = 0;
  for (const c of calls.values()) {
    if (c.refused) refused += 1;
    else if (c.bad) upstreamErrors += 1;
    else if (c.settled) settled += 1;
  }
  const last = [...calls.keys()].sort((a, b) => a - b).at(-1);
  return { attempted: calls.size, settled, upstreamErrors, refused, lastFailed: last !== undefined && Boolean(calls.get(last).bad) };
}

const EXTERNAL = /unavailab|upstream|connection (reset|refused|closed)|econn|timed? ?out|http 5\d\d|\b5\d\d\b.*(error|status)|transient|not reachable|rate.?limit|database error|service error|temporar/i;

/** Do the coder's words about a failed task name a cause outside the agent? A heuristic, shown in the report. */
export function externalFailure(task) {
  const parts = task?.status?.message?.parts;
  const text = Array.isArray(parts) ? parts.map((p) => p?.text ?? '').join(' ') : '';
  return text !== '' && EXTERNAL.test(text);
}

/** null when the outcome stands, or { infra: true, reason }. */
export function classifyRun({ exit, stats, runtimeFailed = false, externalFailure: external = false }) {
  const infra = (reason) => ({ infra: true, reason });
  if (runtimeFailed) return infra('runtime');
  if (stats.attempted === 0) return infra('no-model-calls');
  if (stats.upstreamErrors > UPSTREAM_ERRORS_ALLOWED) return infra('upstream-errors');
  if (stats.lastFailed) return infra('last-call-failed');
  if (exit === 'failed' && external) return infra('agent-reported-external');
  return null;
}
