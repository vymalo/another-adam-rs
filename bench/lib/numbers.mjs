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
// And the figures ADR 0090's decision 5 compares before and after hidden outputs, over the same
// settled calls of the same finished runs:
//
//   read_output / task  the `read_output` entries of the settled calls' `toolNames`
//                    (`adam_llm_agent::kept::READ_OUTPUT`), over the runs. A call, not what it read:
//                    the ledger keeps no tool arguments, so whose output was paged is not in it.
//   input / task     `usage.input` (which includes the cached tokens) of every settled call, a child
//                    session's too, over the runs. A settled call without usage adds nothing and is
//                    counted apart, so the reader sees the sum is then a lower bound.
//
// `compareNumbers` puts two ledgers side by side on the tasks both finished, matched per agent on the
// `task` of the run's `Planned` record, and states which case of decision 5 the movement is.
//
// Nothing here reads a request body, a key or an endpoint: it is a pure fold over records.
import { readLedger } from './ledger.mjs';

/** The tools that run the project's checks: their share is the epic's "checks step". */
export const CHECK_TOOLS = new Set(['run_checks']);

/** The tool that pages a kept output (ADR 0090): its calls are what hidden outputs may add. */
export const READ_OUTPUT_TOOL = 'read_output';

export class NumbersError extends Error {
  constructor(message) {
    super(message);
    this.name = 'NumbersError';
  }
}

const agentOf = (v) => (typeof v.agent === 'string' && v.agent !== '' ? v.agent : 'unknown');

/**
 * One view per run: its task and agent (from `Planned`), whether it reached `Done`, and its calls,
 * keyed by attempt and call number: the meter numbers the calls of each attempt from 1, so a run
 * resumed after an interruption has two call 1s, and both are the run's.
 */
function foldRuns(records) {
  /** @type {Map<string, {task?: string, agent?: string, done: boolean, calls: Map<string, any>}>} */
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
    const attempt = Number.isInteger(rec.attempt) ? rec.attempt : 1;
    const key = `${attempt}:${rec.call}`;
    const prior = v.calls.get(key) ?? { attempt, call: rec.call };
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
  return runs;
}

const emptyAgent = () => ({
  tasks: 0,
  calls: 0,
  readOutput: 0,
  input: { tokens: 0, missing: 0 },
  cache: { input: 0, cached: 0, share: null },
  tools: { named: 0, checks: 0, share: null },
});

/** The figures per agent over the finished runs that `include(agent, task)` keeps. */
function aggregate(runs, include) {
  const agents = new Map();
  const of = (name) => {
    let a = agents.get(name);
    if (!a) {
      a = emptyAgent();
      agents.set(name, a);
    }
    return a;
  };

  let finished = 0;
  for (const v of runs.values()) {
    if (!v.done) continue;
    const name = agentOf(v);
    if (!include(name, v.task)) continue;
    finished += 1;
    const a = of(name);
    a.tasks += 1;
    const calls = [...v.calls.values()].sort((x, y) => x.attempt - y.attempt || x.call - y.call);
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
        if (name === READ_OUTPUT_TOOL) a.readOutput += 1;
      }
      // Every settled call's prompt, a child session's included: what the task cost in input.
      if (c.usage) a.input.tokens += c.usage.input ?? 0;
      else a.input.missing += 1;
      if (main === null || c.prompt === undefined || c.prompt === main) {
        a.cache.input += c.usage?.input ?? 0;
        a.cache.cached += c.usage?.cached ?? 0;
      }
    }
  }

  for (const a of agents.values()) {
    a.callsPerTask = a.tasks > 0 ? a.calls / a.tasks : null;
    a.readOutputPerTask = a.tasks > 0 ? a.readOutput / a.tasks : null;
    a.inputPerTask = a.tasks > 0 ? a.input.tokens / a.tasks : null;
    a.cache.share = a.cache.input > 0 ? a.cache.cached / a.cache.input : null;
    a.tools.share = a.tools.named > 0 ? a.tools.checks / a.tools.named : null;
  }
  return { finished, agents };
}

const BASIS = {
  callsPerTask: 'settled model calls of the runs that reached Done (all their attempts), over those runs, per agent',
  readOutputPerTask: 'read_output calls the answers of those settled calls asked for (toolNames), over the same runs — a call, not what it read: the ledger keeps no tool arguments, so it cannot say whose output was read',
  inputPerTask: 'usage.input (cached tokens included) summed over the same settled calls, a child session\'s too, over the same runs; a settled call without usage adds nothing and is counted apart',
  cacheShare: 'usage.cached / usage.input over the settled calls that carried the fixed (first) prompt of their run',
  checksShare: 'run_checks calls over the named tool calls — NOT a share of tool time: the ledger keeps no tool duration',
};

/**
 * The numbers over the runs that finished.
 *
 * Returns `{ runs, agents, basis }`: `agents` is a `Map` keyed by the agent's name in the ledger
 * (`adam`, `opencode`, …) with `{ tasks, calls, callsPerTask, readOutput, readOutputPerTask,
 * input: {tokens, missing}, inputPerTask, cache: {input, cached, share}, tools: {named, checks,
 * share} }`. `basis` is what the figures are over and what the ledger cannot say, so a page or a
 * report states it beside the numbers rather than on its own.
 */
export function computeNumbers(records) {
  const runs = foldRuns(records);
  const { agents } = aggregate(runs, () => true);
  return {
    runs: { finished: [...runs.values()].filter((v) => v.done).length, total: runs.size },
    agents,
    basis: BASIS,
  };
}

/** The tasks each agent finished in a ledger's runs: agent -> Set of task ids, and the finished runs with no task id. */
function finishedTasks(runs) {
  const tasks = new Map();
  let untasked = 0;
  for (const v of runs.values()) {
    if (!v.done) continue;
    if (typeof v.task !== 'string' || v.task === '') {
      untasked += 1;
      continue;
    }
    const name = agentOf(v);
    if (!tasks.has(name)) tasks.set(name, new Set());
    tasks.get(name).add(v.task);
  }
  return { tasks, untasked };
}

const byText = (x, y) => (x < y ? -1 : x > y ? 1 : 0);

/** Which way a figure moved from `before` to `after`: `up`, `down`, `same`, or `null` when either is missing. */
export function directionOf(before, after) {
  if (before === null || before === undefined || after === null || after === undefined) return null;
  return after > before ? 'up' : after < before ? 'down' : 'same';
}

/**
 * Decision 5 of ADR 0090, stated, not tuned: hidden outputs are kept when the input tokens they save
 * outweigh the `read_output` calls they add; otherwise the default goes back to `full`. This says
 * which way each moved and which case of the rule that is; the weighing, when both moved the way
 * that needs it, is the owner's.
 */
export function verdictOf(before, after) {
  const readOutput = directionOf(before.readOutputPerTask, after.readOutputPerTask);
  const input = directionOf(before.inputPerTask, after.inputPerTask);
  let rule;
  if (readOutput === null || input === null) rule = 'not decidable: a figure is missing on one side';
  else if (readOutput === 'up' && input === 'down') rule = 'weigh: read_output calls went up and input tokens went down; decision 5 keeps on_error only if the tokens saved outweigh the added read_output calls';
  else if (readOutput === 'up') rule = 'against on_error: read_output calls went up and input tokens did not go down; decision 5 says the default goes back to full';
  else if (input === 'down') rule = 'for on_error: input tokens went down and read_output calls did not go up';
  else rule = 'no saving: input tokens did not go down (read_output calls did not go up)';
  return { readOutput, input, rule };
}

/**
 * Two ledgers on the same tasks: `after` (the one measured, `--ledger`) against `before` (the
 * baseline, `--against`). Per agent, only the tasks both ledgers finished count; a task finished in
 * one ledger only, a finished run with no task id and an agent in one ledger only are left out and
 * listed in `leftOut`.
 *
 * Returns `{ agents, leftOut, basis }`: `agents` is a `Map` agent -> `{ tasks: string[], before,
 * after, verdict }` (the figures as `computeNumbers` gives them, over the common tasks), and
 * `leftOut` is `{ agents: Map agent -> {onlyBefore: string[], onlyAfter: string[]}, untasked:
 * {before, after} }`.
 */
export function compareNumbers(afterRecords, beforeRecords) {
  const afterRuns = foldRuns(afterRecords);
  const beforeRuns = foldRuns(beforeRecords);
  const a = finishedTasks(afterRuns);
  const b = finishedTasks(beforeRuns);

  const common = new Map();
  const leftOutAgents = new Map();
  const names = [...new Set([...a.tasks.keys(), ...b.tasks.keys()])].sort(byText);
  for (const name of names) {
    const inAfter = a.tasks.get(name) ?? new Set();
    const inBefore = b.tasks.get(name) ?? new Set();
    const both = new Set([...inAfter].filter((t) => inBefore.has(t)));
    if (both.size > 0) common.set(name, both);
    const onlyAfter = [...inAfter].filter((t) => !both.has(t)).sort(byText);
    const onlyBefore = [...inBefore].filter((t) => !both.has(t)).sort(byText);
    if (onlyAfter.length > 0 || onlyBefore.length > 0) leftOutAgents.set(name, { onlyBefore, onlyAfter });
  }

  const keep = (name, task) => common.get(name)?.has(task) === true;
  const after = aggregate(afterRuns, keep).agents;
  const before = aggregate(beforeRuns, keep).agents;
  const agents = new Map();
  for (const [name, tasks] of common) {
    const x = before.get(name);
    const y = after.get(name);
    agents.set(name, { tasks: [...tasks].sort(byText), before: x, after: y, verdict: verdictOf(x, y) });
  }
  return { agents, leftOut: { agents: leftOutAgents, untasked: { before: b.untasked, after: a.untasked } }, basis: BASIS };
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
const tok = (x) => (x === null || x === undefined ? 'n/a' : String(Math.round(x)));
const signed = (d, fmt) => (d > 0 ? `+${fmt(d)}` : d < 0 ? `-${fmt(-d)}` : '0');

/** The change from `before` to `after`: the difference, and the relative change when `before` is not zero. */
function change(before, after, fmt) {
  if (before === null || before === undefined || after === null || after === undefined) return 'n/a';
  const d = after - before;
  const rel = before !== 0 ? ` (${signed((d / before) * 100, (x) => x.toFixed(1))}%)` : '';
  return `${signed(d, fmt)}${rel}`;
}

/** The change of a share, in percentage points. */
function changePoints(before, after) {
  if (before === null || before === undefined || after === null || after === undefined) return 'n/a';
  return `${signed((after - before) * 100, (x) => x.toFixed(1))} pt`;
}

/** The figures as a small table and their basis, as `bench numbers` prints them. */
export function formatNumbers(numbers, { ledger = 'ledger.jsonl' } = {}) {
  const names = [...numbers.agents.keys()].sort();
  const rows = names.map((n) => {
    const a = numbers.agents.get(n);
    return `| ${n} | ${a.tasks} | ${a.calls} | ${num(a.callsPerTask)} | ${num(a.readOutputPerTask)} (${a.readOutput}) | ${tok(a.inputPerTask)} | ${pct(a.cache.share)} (${a.cache.cached} / ${a.cache.input}) | ${pct(a.tools.share)} (${a.tools.checks} / ${a.tools.named}) |`;
  });
  const lines = [
    `# bench numbers — ${ledger}`,
    '',
    `Runs that reached \`Done\`: ${numbers.runs.finished} of ${numbers.runs.total} in the ledger.`,
    '',
    '| agent | tasks | settled calls | calls / task | read_output calls / task | input tokens / task | cache share of the fixed prompt | run_checks share of named tool calls |',
    '|---|---|---|---|---|---|---|---|',
    ...rows,
    '',
  ];
  const ratio = names.length === 2 ? ratioOf(numbers, names[0], names[1]) : null;
  if (ratio !== null) lines.push(`Calls per task, ${names[0]} over ${names[1]}: ${ratio.toFixed(2)}x`, '');
  for (const n of names) {
    const missing = numbers.agents.get(n).input.missing;
    if (missing > 0) lines.push(`${n}: ${missing} settled call(s) without usage add nothing to the input tokens, which are therefore a lower bound.`, '');
  }
  lines.push(
    'The basis of each figure, which a page must state beside it:',
    `- calls / task: ${numbers.basis.callsPerTask}`,
    `- read_output calls / task: ${numbers.basis.readOutputPerTask}`,
    `- input tokens / task: ${numbers.basis.inputPerTask}`,
    `- cache share: ${numbers.basis.cacheShare}`,
    `- run_checks share: ${numbers.basis.checksShare}`,
    '',
  );
  return lines.join('\n');
}

const LIST_AT_MOST = 20;
const listOf = (ids) => (ids.length <= LIST_AT_MOST ? ids.join(', ') : `${ids.slice(0, LIST_AT_MOST).join(', ')}, and ${ids.length - LIST_AT_MOST} more`);

/** The comparison as `bench numbers --against` prints it: per agent the figures before and after, the change, and decision 5's case. */
export function formatComparison(cmp, { ledger = 'after.jsonl', against = 'before.jsonl' } = {}) {
  const lines = [
    `# bench numbers — ${ledger} against ${against}`,
    '',
    `Before is \`--against\` (${against}); after is \`--ledger\` (${ledger}). Per agent, only the tasks both ledgers finished are counted.`,
    '',
  ];
  for (const [name, c] of cmp.agents) {
    const { before: x, after: y, verdict: v } = c;
    lines.push(
      `## ${name}: ${c.tasks.length} task(s) finished in both (${x.tasks} run(s) before, ${y.tasks} after)`,
      '',
      '| figure | before | after | change |',
      '|---|---|---|---|',
      `| calls / task | ${num(x.callsPerTask)} | ${num(y.callsPerTask)} | ${change(x.callsPerTask, y.callsPerTask, (d) => d.toFixed(2))} |`,
      `| read_output calls / task | ${num(x.readOutputPerTask)} (${x.readOutput}) | ${num(y.readOutputPerTask)} (${y.readOutput}) | ${change(x.readOutputPerTask, y.readOutputPerTask, (d) => d.toFixed(2))} |`,
      `| input tokens / task | ${tok(x.inputPerTask)} | ${tok(y.inputPerTask)} | ${change(x.inputPerTask, y.inputPerTask, (d) => String(Math.round(d)))} |`,
      `| cache share of the fixed prompt | ${pct(x.cache.share)} | ${pct(y.cache.share)} | ${changePoints(x.cache.share, y.cache.share)} |`,
      '',
      `Decision 5 (ADR 0090): read_output calls per task ${v.readOutput ?? 'n/a'}, input tokens per task ${v.input ?? 'n/a'} — ${v.rule}.`,
    );
    if (x.input.missing > 0 || y.input.missing > 0) {
      lines.push(`Settled calls without usage: ${x.input.missing} before, ${y.input.missing} after; the input tokens on that side are a lower bound.`);
    }
    lines.push('');
  }
  if (cmp.agents.size === 0) lines.push('No agent finished the same task in both ledgers: nothing to compare.', '');

  const out = [];
  for (const [name, l] of cmp.leftOut.agents) {
    const parts = [];
    if (l.onlyAfter.length > 0) parts.push(`${l.onlyAfter.length} only after (${listOf(l.onlyAfter)})`);
    if (l.onlyBefore.length > 0) parts.push(`${l.onlyBefore.length} only before (${listOf(l.onlyBefore)})`);
    out.push(`- ${name}: ${parts.join('; ')}`);
  }
  const { before: ub, after: ua } = cmp.leftOut.untasked;
  if (ub > 0 || ua > 0) out.push(`- finished runs with no task id, which cannot be matched: ${ub} before, ${ua} after`);
  lines.push('Left out, finished in one ledger only:', ...(out.length > 0 ? out : ['- none']), '');

  lines.push(
    'The basis of each figure:',
    `- calls / task: ${cmp.basis.callsPerTask}`,
    `- read_output calls / task: ${cmp.basis.readOutputPerTask}`,
    `- input tokens / task: ${cmp.basis.inputPerTask}`,
    `- cache share: ${cmp.basis.cacheShare}`,
    '- decision 5 is per tool, but a read_output call names no tool in the ledger: a case that points back to full does not say for which tool',
    '',
  );
  return lines.join('\n');
}
