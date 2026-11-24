// The three numbers the faster-coder epic rests on, computed from a ledger, and the one it cannot
// answer.
//
// The epic (sys#345, slice 1) rests on: the model calls per task, the checks step's share of the
// coder's tool time, and the cached share of the fixed prompt. This module computes them from the
// `call` records of a run (bench/lib/meter.mjs writes them, bench/lib/ledger.mjs folds them), so a
// reader reproduces a figure from any ledger instead of trusting a sentence.
//
// What the ledger holds, and what follows from it:
//
//   calls per task   `call` records of phase `settled` of the runs that reached `Done`, divided by
//                    those runs. Counted per agent, because the epic's figure is a comparison.
//   cache share      `usage.cached / usage.input` over the settled calls whose `prompt`
//                    fingerprint is the run's own first one: the fixed main prompt (`promptId`, the
//                    meter's). A child session (a subagent) has another fingerprint and is left out,
//                    because its prompt is not the fixed one the figure is about.
//   checks share     **of the named tool calls**, not of tool time: the ledger records which tools
//                    an answer asked for (`toolNames`) and when a model call started and ended, never
//                    how long a tool ran. `run_checks` is the check tool
//                    (bin/adam-coder/src/tools/mod.rs). A figure stated as a share of *tool time*
//                    therefore does not reproduce from a ledger, which this module says instead of
//                    inventing a denominator.
//
// Nothing here reads a request body, a key or an endpoint: it is a pure fold over records.
import { readLedger } from './ledger.mjs';

/** The tools that run the project's checks: their share is the epic's "checks step". */
export const CHECK_TOOLS = new Set(['run_checks']);

export class NumbersError extends Error {
  constructor(message) {
    super(message);
    this.name = 'NumbersError';
  }
}

const agentOf = (v) => (typeof v.agent === 'string' && v.agent !== '' ? v.agent : 'unknown');

/**
 * The three numbers over the runs that finished.
 *
 * Returns `{ runs, agents, basis }`: `agents` is a `Map` keyed by the agent's name in the ledger
 * (`adam`, `opencode`, …) with `{ tasks, calls, callsPerTask, cache: {input, cached, share},
 * tools: {named, checks, share} }`. `basis` is what the figures are over and what the ledger cannot
 * say, so a page or a report states it beside the numbers rather than on its own.
 */
export function computeNumbers(records) {
  /** @type {Map<string, {task?: string, agent?: string, done: boolean, calls: Map<number, any>}>} */
  const runs = new Map();
  const run = (id) => {
    let v = runs.get(id);
    if (!v) {
      v = { task: undefined, agent: undefined, done: false, calls: new Map() };
      runs.set(id, v);
    }
    return v;
  };

  for (const rec of records) {
    if (rec.kind === 'state') {
      if (rec.state === 'Planned') {
        const v = run(rec.run);
        if (typeof rec.task === 'string') v.task = rec.task;
        if (typeof rec.agent === 'string') v.agent = rec.agent;
      } else if (rec.state === 'Done') {
        run(rec.run).done = true;
      }
      continue;
    }
    if (rec.kind !== 'call') continue;
    const v = run(rec.run);
    const key = rec.call;
    const prior = v.calls.get(key) ?? {};
    if (rec.phase === 'settled' || rec.phase === 'failed') {
      v.calls.set(key, {
        ...prior,
        settled: rec.phase === 'settled',
        usage: rec.usage ?? null,
        toolNames: Array.isArray(rec.toolNames) ? rec.toolNames : [],
      });
    } else {
      v.calls.set(key, { ...prior, prompt: rec.prompt ?? prior.prompt, ...(rec.settled ? { settled: rec.settled } : {}) });
    }
  }

  const agents = new Map();
  const of = (name) => {
    let a = agents.get(name);
    if (!a) {
      a = { tasks: 0, calls: 0, cache: { input: 0, cached: 0, share: null }, tools: { named: 0, checks: 0, share: null } };
      agents.set(name, a);
    }
    return a;
  };

  for (const v of runs.values()) {
    if (!v.done) continue;
    const a = of(agentOf(v));
    a.tasks += 1;
    const calls = [...v.calls.entries()].sort(([x], [y]) => x - y).map(([, c]) => c);
    const settled = calls.filter((c) => c.settled);
    a.calls += settled.length;
    // The fixed prompt is the one the run's first settled call carried; a call with no fingerprint
    // (an older ledger) counts as the main one, as the fold in ledger.mjs treats it.
    const main = settled[0]?.prompt ?? null;
    for (const c of settled) {
      for (const name of c.toolNames) {
        if (typeof name !== 'string' || name === '') continue;
        a.tools.named += 1;
        if (CHECK_TOOLS.has(name)) a.tools.checks += 1;
      }
      if (main === null || c.prompt === undefined || c.prompt === main) {
        a.cache.input += c.usage?.input ?? 0;
        a.cache.cached += c.usage?.cached ?? 0;
      }
    }
  }

  for (const a of agents.values()) {
    a.callsPerTask = a.tasks > 0 ? a.calls / a.tasks : null;
    a.cache.share = a.cache.input > 0 ? a.cache.cached / a.cache.input : null;
    a.tools.share = a.tools.named > 0 ? a.tools.checks / a.tools.named : null;
  }

  return {
    runs: { finished: [...runs.values()].filter((v) => v.done).length, total: runs.size },
    agents,
    basis: {
      callsPerTask: 'settled model calls of the runs that reached Done, over those runs, per agent',
      cacheShare: 'usage.cached / usage.input over the settled calls that carried the fixed (first) prompt of their run',
      checksShare: 'run_checks calls over the named tool calls — NOT a share of tool time: the ledger keeps no tool duration',
    },
  };
}

/** The ratio of one agent's calls per task to another's, or `null` when either has none. */
export function ratioOf(numbers, numerator, denominator) {
  const a = numbers.agents.get(numerator)?.callsPerTask;
  const b = numbers.agents.get(denominator)?.callsPerTask;
  if (!a || !b || !(b > 0)) return null;
  return a / b;
}

const pct = (x) => (x === null || x === undefined ? 'n/a' : `${(x * 100).toFixed(1)}%`);
const num = (x) => (x === null || x === undefined ? 'n/a' : Number.isInteger(x) ? String(x) : x.toFixed(2));

/** The figures as a small table and their basis, as `bench numbers` prints them. */
export function formatNumbers(numbers, { ledger = 'ledger.jsonl' } = {}) {
  const names = [...numbers.agents.keys()].sort();
  const rows = names.map((n) => {
    const a = numbers.agents.get(n);
    return `| ${n} | ${a.tasks} | ${a.calls} | ${num(a.callsPerTask)} | ${pct(a.cache.share)} (${a.cache.cached} / ${a.cache.input}) | ${pct(a.tools.share)} (${a.tools.checks} / ${a.tools.named}) |`;
  });
  const lines = [
    `# bench numbers — ${ledger}`,
    '',
    `Runs that reached \`Done\`: ${numbers.runs.finished} of ${numbers.runs.total} in the ledger.`,
    '',
    '| agent | tasks | settled calls | calls / task | cache share of the fixed prompt | run_checks share of named tool calls |',
    '|---|---|---|---|---|---|',
    ...rows,
    '',
  ];
  const ratio = names.length === 2 ? ratioOf(numbers, names[0], names[1]) : null;
  if (ratio !== null) lines.push(`Calls per task, ${names[0]} over ${names[1]}: ${ratio.toFixed(2)}x`, '');
  lines.push(
    'The basis of each figure, which a page must state beside it:',
    `- calls / task: ${numbers.basis.callsPerTask}`,
    `- cache share: ${numbers.basis.cacheShare}`,
    `- run_checks share: ${numbers.basis.checksShare}`,
    '',
  );
  return lines.join('\n');
}
