// report.md from the ledger.
//
// What a run's `Done` record carries (`result`): pass (true/false), grades (the three grading
// outcomes), flaky, exit ('done', 'timeout', 'budget_hit', ...), wallMs and, optionally,
// selfReported ({input, output, calls}: the agent's own count, shown, never used for the cap).
// The `Planned` record carries task, agent, arm, block, source ('public' or 'own') and language.
// Cost, tokens and calls are the meter's, folded from the call records. The model is shown by its
// label; its id appears only with `showModelId`.
import { formatUSD } from './cost.mjs';
import { bootstrapPairedDiff, mcnemarExact, median, percentile, seedFrom, wilson } from './stats.mjs';
import { foldRecords } from './ledger.mjs';

const MIN_TASKS_PER_LANGUAGE = 15;

export class ReportError extends Error {
  constructor(message) {
    super(message);
    this.name = 'ReportError';
  }
}

export const NOT_CLAIMED = [
  'One agent is better in general, or with another model or another endpoint: this compares two agents on one model in two arms.',
  'Comparability with any public leaderboard: the environment is adapted, each task has one attempt, and the limits are ours.',
  'A difference where the intervals overlap or the interval of a difference includes zero: with about a hundred runs, gaps under about ten points are noise.',
  'Real billing: every cost here is the meter\'s count times the price table in the local configuration.',
  `A conclusion for a language with fewer than ${MIN_TASKS_PER_LANGUAGE} tasks.`,
  'That a grade is free of influence by the agent: grading runs the agent\'s code, so it cannot be made tamper-proof. The harness strips what may not change and lists the patches that touch the grading\'s machinery as suspect, for a person to read.',
];

const pct = (x) => `${(x * 100).toFixed(1)}%`;
const interval = (w) => (w ? `${pct(w.low)} to ${pct(w.high)}` : 'n/a');
const num = (n) => (n === null || n === undefined ? 'n/a' : Number.isInteger(n) ? String(n) : n.toFixed(1));
const sum = (xs) => xs.reduce((s, x) => s + x, 0);

/** A table cell on one line, its pipes escaped: nothing a run carries can break a table. A backslash is escaped first, so a name that is `a\|b` cannot end the cell. */
const cell = (c) => String(c).replace(/\\/g, '\\\\').replace(/\r\n|\r|\n/g, ' ').replace(/\|/g, '\\|');

function table(header, rows) {
  if (rows.length === 0) return '_none_\n';
  const line = (cells) => `| ${cells.map(cell).join(' | ')} |`;
  return `${[line(header), line(header.map(() => '---')), ...rows.map(line)].join('\n')}\n`;
}

const cellKey = (v) => `${v.agent} / ${v.arm}`;
const group = (items, keyFn) => {
  const m = new Map();
  for (const it of items) {
    const k = keyFn(it);
    if (!m.has(k)) m.set(k, []);
    m.get(k).push(it);
  }
  return new Map([...m].sort(([a], [b]) => String(a).localeCompare(String(b))));
};

const passed = (v) => v.result?.pass === true;

/** Cached input over input, both the meter's; input already holds the cached part. */
const cacheRate = (runs) => {
  const input = sum(runs.map((v) => v.tokens.input));
  return input > 0 ? pct(sum(runs.map((v) => v.tokens.cached)) / input) : 'n/a';
};

/** One row of pass rates for a list of runs. */
function rateRow(label, runs) {
  const k = runs.filter(passed).length;
  return [...label, runs.length, k, runs.length ? pct(k / runs.length) : 'n/a', interval(wilson(k, runs.length))];
}

function pairedSection(counted, setupSeed, by, within, nameBy, nameWithin) {
  // Compare the two values of `by` (agents, or arms) inside each value of `within`.
  const rows = [];
  for (const [inner, runs] of group(counted, within)) {
    const values = [...group(runs, by).keys()];
    if (values.length !== 2) continue;
    const [first, second] = values;
    const byTask = group(runs, (v) => v.task);
    const pairs = [];
    let onlyFirst = 0;
    let onlySecond = 0;
    for (const taskRuns of byTask.values()) {
      const a = taskRuns.find((v) => by(v) === first);
      const b = taskRuns.find((v) => by(v) === second);
      if (!a || !b) continue;
      pairs.push([passed(a) ? 1 : 0, passed(b) ? 1 : 0]);
      if (passed(a) && !passed(b)) onlyFirst += 1;
      if (!passed(a) && passed(b)) onlySecond += 1;
    }
    if (pairs.length === 0) continue;
    const boot = bootstrapPairedDiff(pairs, { seed: seedFrom(`${setupSeed}:${inner}:${first}:${second}`) });
    const p = mcnemarExact(onlyFirst, onlySecond);
    const includesZero = boot.low <= 0 && boot.high >= 0;
    rows.push([
      inner,
      `${first} vs ${second}`,
      pairs.length,
      onlyFirst,
      onlySecond,
      `${(boot.diff * 100).toFixed(1)} points`,
      `${(boot.low * 100).toFixed(1)} to ${(boot.high * 100).toFixed(1)}`,
      p.toFixed(4),
      includesZero ? 'interval includes zero' : 'interval excludes zero',
    ]);
  }
  return table([nameWithin, `${nameBy} compared`, 'tasks in both', 'first only', 'second only', 'difference', '95% bootstrap interval', 'McNemar exact p', 'reading'], rows);
}

/**
 * @param {Map|Array} foldedOrRecords  the fold (ledger.mjs) or the ledger's records
 * @param {object} [opts]
 * @param {object} [opts.setup]  fields of the last `setup` record (modelLabel, seed, taskLock, ...)
 * @param {boolean} [opts.showModelId]
 * @param {string} [opts.modelId]  shown only with showModelId
 * @param {string} [opts.modelLabel]  used when the setup has no label
 * @param {number} [opts.capMicro]
 */
export function buildReport(foldedOrRecords, opts = {}) {
  const records = Array.isArray(foldedOrRecords) ? foldedOrRecords : null;
  const folded = records ? foldRecords(records) : foldedOrRecords;
  const setup = opts.setup ?? (records ? [...records].reverse().find((r) => r.kind === 'setup') : null) ?? {};
  const everything = [...folded.values()].sort((a, b) => a.run.localeCompare(b.run));
  // The pilot ran under another budget and is reported apart: none of its runs is in a rate, a cost or a pairing.
  const pilot = everything.filter((v) => v.phase === 'pilot');
  const all = everything.filter((v) => v.phase !== 'pilot');
  // A Done run whose grade is invalid (an infrastructure error that did not go away) is not a fail and not a pass: it is left out.
  const counted = all.filter((v) => v.state === 'Done' && v.result?.invalid !== true);
  const seen = new Map();
  for (const v of counted) {
    const key = JSON.stringify([v.task, v.agent, v.arm]);
    if (seen.has(key)) {
      throw new ReportError(`two Done runs for task ${v.task}, agent ${v.agent}, arm ${v.arm}: ${seen.get(key)} and ${v.run}; a task has one attempt per cell`);
    }
    seen.set(key, v.run);
  }
  const excluded = all.filter((v) => !counted.includes(v));
  // A block is complete when every cell of the plan has a finished run of it; only complete blocks are compared and rated.
  const cellNames = [...new Set(all.filter((v) => v.agent).map(cellKey))].sort();
  const cellsOfTask = new Map();
  for (const v of counted) {
    if (!cellsOfTask.has(v.task)) cellsOfTask.set(v.task, new Set());
    cellsOfTask.get(v.task).add(cellKey(v));
  }
  const complete = new Set([...cellsOfTask].filter(([, cs]) => cellNames.every((c) => cs.has(c))).map(([task]) => task));
  const rated = counted.filter((v) => complete.has(v.task));
  const leftOut = (cell) => all.filter((v) => v.agent && cellKey(v) === cell).length - rated.filter((v) => cellKey(v) === cell).length;
  const label = setup.modelLabel ?? opts.modelLabel ?? 'unlabelled';
  const seed = setup.seed ?? 1;
  const out = [];
  const h = (text) => out.push(`\n## ${text}\n`);
  const show = (v) => (v === undefined || v === null ? 'not recorded' : typeof v === 'object' ? JSON.stringify(v) : String(v));

  const lastAt = (records ?? []).map((r) => r.t).filter(Boolean).sort().at(-1);
  out.push('# Benchmark report');
  out.push(`\nBuilt from the ledger${lastAt ? `; last record ${lastAt}` : ''}. Costs are the meter's count times the owner's price table.\n`);

  h('Setup');
  const spent = sum(everything.map((v) => v.spentMicro));
  const wasted = sum(everything.map((v) => v.wastedMicro));
  const setupRows = [
    ['Model', opts.showModelId && opts.modelId ? `${label} (id ${opts.modelId})` : label],
    ['Seed', show(setup.seed)],
    ['Task lock', show(setup.taskLock)],
    ['adam image', show(setup.adamImage)],
    ['adam revision', show(setup.adamRevision)],
    ['OpenCode version', show(setup.opencodeVersion)],
    ['OpenCode integrity', show(setup.opencodeIntegrity)],
    ['Limits', show(setup.limits)],
    ['Cap', opts.capMicro === undefined ? 'not known to this report' : formatUSD(opts.capMicro)],
    ['Spent (all runs, wasted included)', formatUSD(spent)],
    ['Wasted (interrupted, infrastructure failures, stopped by the cap)', formatUSD(wasted)],
  ];
  out.push(table(['Item', 'Value'], setupRows));

  h('Runs');
  out.push(table(['State', 'Runs'], [...group(all, (v) => v.state ?? 'unknown')].map(([s, rs]) => [s, rs.length])));
  out.push('\nExit reasons of the counted runs (state Done):\n');
  out.push(table(['Exit', 'Runs'], [...group(counted, (v) => v.result?.exit ?? 'not recorded')].map(([e, rs]) => [e, rs.length])));
  out.push(
    '\nA run counts in the rates only when it is Done. Runs in StoppedByCap, Failed_infra, NotStarted (and any other state but Done) are left out of every rate, so the selection is visible here and not in the numbers:\n',
  );
  if (excluded.length > 0) {
    out.push(table(['Run', 'State', 'Task', 'Agent / arm', 'Wasted'], excluded.map((v) => [v.run, v.state === 'Done' ? 'Done (invalid grade)' : v.state ?? 'unknown', v.task ?? '-', v.agent ? cellKey(v) : '-', formatUSD(v.wastedMicro)])));
  } else out.push('None: every run is Done.\n');

  h('Pass rate by agent and arm');
  out.push('Only tasks that have a finished run in every cell are rated, so the cells are compared on the same tasks. The last column counts the runs of the cell that are left out (an infrastructure failure, the cap, or a block that is incomplete).\n\n');
  out.push(table(['Agent', 'Arm', 'Runs', 'Solved', 'Rate', '95% interval (Wilson)', 'Left out'], [...group(rated, cellKey)].map(([cell, rs]) => [...rateRow([rs[0].agent, rs[0].arm], rs), leftOut(cell)])));
  h('Incomplete blocks');
  out.push('Tasks with no finished run in some cell: left out of the rates above, and listed with the cells that lack one.\n\n');
  const taskNames = [...new Set(all.filter((v) => v.task).map((v) => v.task))].sort();
  out.push(table(['Task', 'Cells without a finished run'], taskNames.filter((task) => !complete.has(task)).map((task) => [task, cellNames.filter((c) => !cellsOfTask.get(task)?.has(c)).join(', ')])));

  h('Paired differences');
  out.push('Agents compared inside each arm, and arms inside each agent, on the tasks both have a Done run for.\n\n');
  out.push(pairedSection(rated, seed, (v) => v.agent, (v) => v.arm, 'agent', 'arm'));
  out.push('\n');
  out.push(pairedSection(rated, seed, (v) => v.arm, (v) => v.agent, 'arm', 'agent'));

  h('By source');
  out.push(table(['Source', 'Agent', 'Arm', 'Runs', 'Solved', 'Rate', '95% interval (Wilson)'], [...group(rated, (v) => `${v.source ?? 'unknown'} / ${cellKey(v)}`)].map(([, rs]) => rateRow([rs[0].source ?? 'unknown', rs[0].agent, rs[0].arm], rs))));

  out.push('\nSources: `public` is the public task set, `own` the owner\'s tasks from other repositories, `own-self` the tasks from adam-rs\'s own repository (its agent may know them).\n\n');
  out.push(
    table(
      ['Source', 'Agent', 'Arm', 'Cost', 'Cost per solved task'],
      [...group(counted, (v) => `${v.source ?? 'unknown'} / ${cellKey(v)}`)].map(([, rs]) => {
        const cost = sum(rs.map((v) => v.spentMicro));
        const solved = rs.filter(passed).length;
        return [rs[0].source ?? 'unknown', rs[0].agent, rs[0].arm, formatUSD(cost), solved ? formatUSD(Math.round(cost / solved)) : 'n/a (none solved)'];
      }),
    ),
  );

  h('By language');
  const langRows = [];
  const langNotes = [];
  for (const [lang, rs] of group(rated, (v) => v.language ?? 'unknown')) {
    const tasks = new Set(rs.map((v) => v.task)).size;
    if (tasks < MIN_TASKS_PER_LANGUAGE) langNotes.push(`${lang}: ${tasks} tasks, fewer than ${MIN_TASKS_PER_LANGUAGE}, no conclusion`);
    for (const [, cell] of group(rs, cellKey)) langRows.push(rateRow([lang, cell[0].agent, cell[0].arm], cell));
  }
  out.push(table(['Language', 'Agent', 'Arm', 'Runs', 'Solved', 'Rate', '95% interval (Wilson)'], langRows));
  if (langNotes.length > 0) out.push(`\n${langNotes.map((n) => `* ${n}`).join('\n')}\n`);

  h('Agent path');
  out.push('Whether the gold patch also passed at the agents\' working path (the clone path) when the task was checked. Tasks are kept and graded at `/app` whatever this says; a task that fails there did not fail for lack of a fix, and both arms work at the same path, so it is reported and never a reason to leave a task out. The tasks are counted once each, whatever the arm; the rates are of the rated runs.\n\n');
  const pathOf = (v) => v.agentPath ?? 'not recorded';
  const tasksByPath = new Map();
  for (const v of rated) {
    const k = pathOf(v);
    if (!tasksByPath.has(k)) tasksByPath.set(k, new Set());
    tasksByPath.get(k).add(v.task);
  }
  out.push(table(['Agent path', 'Tasks'], [...tasksByPath].sort(([x], [y]) => (x < y ? -1 : 1)).map(([k, ts]) => [k, ts.size])));
  out.push('\n');
  out.push(table(['Agent path', 'Agent', 'Arm', 'Runs', 'Solved', 'Rate', '95% interval (Wilson)'], [...group(rated, (v) => `${pathOf(v)} / ${cellKey(v)}`)].map(([, rs]) => rateRow([pathOf(rs[0]), rs[0].agent, rs[0].arm], rs))));

  h('Cost, tokens, time and calls');
  out.push('Every figure of this section sums all attempts of a run (an interrupted attempt that was run again is in it), and counts only Done runs.\n\n');
  const costRows = [];
  const timeRows = [];
  const turnRows = [];
  const byTool = [];
  for (const [cell, rs] of group(counted, cellKey)) {
    const solved = rs.filter(passed).length;
    const cost = sum(rs.map((v) => v.spentMicro));
    // every run of the cell, the failed and interrupted and left-out ones too: what a solved task really cost
    const every = all.filter((v) => v.agent && cellKey(v) === cell);
    const everSolved = solved > 0;
    const t = (field) => median(rs.map((v) => v.tokens[field]));
    costRows.push([
      rs[0].agent,
      rs[0].arm,
      formatUSD(cost),
      solved ? formatUSD(Math.round(cost / solved)) : 'n/a (none solved)',
      sum(rs.map((v) => v.tokens.input)),
      sum(rs.map((v) => v.tokens.cached)),
      sum(rs.map((v) => v.tokens.cacheWrite)),
      sum(rs.map((v) => v.tokens.output)),
      sum(rs.map((v) => v.tokens.reasoning)),
      `${num(t('input'))} / ${num(t('output'))} / ${num(t('reasoning'))}`,
      formatUSD(Math.round(cost / rs.length)),
      cacheRate(rs),
      formatUSD(sum(rs.filter((v) => !passed(v)).map((v) => v.spentMicro))),
      everSolved ? formatUSD(Math.round(sum(every.map((v) => v.spentMicro)) / solved)) : 'n/a (none solved)',
    ]);
    const wall = rs.map((v) => v.result?.wallMs).filter((x) => Number.isFinite(x));
    const calls = rs.map((v) => v.calls.settled + v.calls.failed);
    timeRows.push([
      rs[0].agent,
      rs[0].arm,
      wall.length ? `${num(median(wall) / 1000)} s` : 'n/a',
      wall.length ? `${num(percentile(wall, 90) / 1000)} s` : 'n/a',
      num(median(calls)),
      sum(rs.map((v) => v.calls.refused)),
      sum(rs.map((v) => v.calls.usageMissing)),
    ]);
    // What the meter read in the answers: how many tool calls each model turn asked for.
    const turns = sum(rs.map((v) => v.toolCalls.turns));
    const asked = sum(rs.map((v) => v.toolCalls.calls));
    turnRows.push([
      rs[0].agent,
      rs[0].arm,
      turns,
      asked,
      turns > 0 ? (asked / turns).toFixed(2) : 'n/a',
      turns > 0 ? sum(rs.map((v) => v.toolCalls.multiTurns)) : 'n/a',
      turns > 0 ? Math.max(...rs.map((v) => v.toolCalls.maxInTurn)) : 'n/a',
    ]);
    // Which tools those calls asked for: the meter keeps the name each call gave. A Map, for the
    // same reason the fold's is one: a name is the model's text.
    const names = new Map();
    for (const v of rs) for (const [name, n] of v.toolCalls.byName) names.set(name, (names.get(name) ?? 0) + n);
    byTool.push({ cell, names, named: sum(rs.map((v) => v.toolCalls.named)), calls: asked });
  }
  out.push(table(['Agent', 'Arm', 'Cost', 'Cost per solved task', 'Input', 'Cached', 'Cache write', 'Output', 'Reasoning (in output)', 'Median per run: input / output / reasoning', 'Cost per run', 'Cache-hit rate (cached / input, all attempts)', 'Spend on unsolved runs', 'Spend per solved task, every run of the cell'], costRows));
  out.push('\n');
  out.push(table(['Agent', 'Arm', 'Wall median', 'Wall p90', 'Model calls (median)', 'Refused calls', 'Calls with no usage (charged worst case)'], timeRows));
  out.push('\n\nTool calls per model turn, read from the model\'s answers (a turn with a count of its own; a run recorded before the count has none):\n\n');
  out.push(table(['Agent', 'Arm', 'Model turns counted', 'Tool calls asked', 'Tool calls per turn', 'Turns with several calls', 'Most calls in one turn'], turnRows));

  const named = byTool.filter((t) => t.named > 0);
  if (named.length === 0) {
    out.push('\n\nTool calls by tool: n/a (no record of this ledger carries the names; a run made before the meter read them has none).\n');
  } else {
    const totals = new Map();
    for (const t of named) {
      for (const [name, n] of t.names) totals.set(name, (totals.get(name) ?? 0) + n);
    }
    const sorted = [...totals].sort(([a, na], [b, nb]) => nb - na || a.localeCompare(b));
    const shown = sorted.slice(0, 30);
    const rows = shown.map(([name]) => [name, ...byTool.map((t) => t.names.get(name) ?? 0)]);
    const rest = sorted.slice(30);
    if (rest.length > 0) rows.push([`other (${rest.length} tools)`, ...byTool.map((t) => sum(rest.map(([name]) => t.names.get(name) ?? 0)))]);
    const unnamed = byTool.map((t) => t.calls - t.named);
    if (unnamed.some((n) => n > 0)) rows.push(['(no name)', ...unnamed]);
    out.push('\n\nTool calls by tool, read from the model\'s answers (the tool each call named; a cell whose calls are not all named says how many in the row "(no name)"):\n\n');
    out.push(table(['Tool', ...byTool.map((t) => t.cell)], rows));
  }

  h('Infrastructure failures');
  out.push('A run that failed in the infrastructure (the environment, the model endpoint, a container that would not stop) is tried again at most twice and is never a fail. Left out of the rates when it never finished.\n\n');
  const infraRows = [...group(all, cellKey)]
    .map(([, rs]) => {
      const hit = rs.filter((v) => v.infraFailures > 0 || v.interruptions > 0 || v.state === 'Failed_infra');
      return [rs[0].agent, rs[0].arm, hit.length, hit.filter((v) => v.state === 'Done').length, hit.filter((v) => v.state === 'Failed_infra').length, sum(rs.map((v) => v.infraFailures)), sum(rs.map((v) => v.interruptions)), formatUSD(sum(hit.map((v) => v.wastedMicro)))];
    })
    .filter((r) => r[2] > 0);
  out.push(table(['Agent', 'Arm', 'Runs affected', 'Finished after a retry', 'Never finished', 'Infrastructure failures', 'Interruptions', 'Wasted'], infraRows));

  h('Pilot');
  out.push('The pilot ran the first blocks under the configuration\'s per-run budget to set the budgets of the full run; the full run ran those tasks again. None of these runs is in a rate, a cost per run or a pairing above.\n\n');
  out.push(
    table(
      ['Agent', 'Arm', 'Runs', 'Solved', 'Cost'],
      [...group(pilot.filter((v) => v.state === 'Done' && v.result?.invalid !== true), cellKey)].map(([, rs]) => [rs[0].agent, rs[0].arm, rs.length, rs.filter(passed).length, formatUSD(sum(rs.map((v) => v.spentMicro)))]),
    ),
  );

  h('Per task');
  const cells = [...group(counted, cellKey).keys()];
  const taskRows = [];
  const differing = [];
  for (const [task, rs] of group(counted, (v) => v.task ?? 'unknown')) {
    const marks = cells.map((c) => {
      const r = rs.find((v) => cellKey(v) === c);
      return r ? (passed(r) ? 'pass' : 'fail') : '-';
    });
    taskRows.push([task, ...marks]);
    for (const [arm, armRuns] of group(rs, (v) => v.arm)) {
      const results = new Set(armRuns.map((v) => (passed(v) ? 'pass' : 'fail')));
      if (armRuns.length > 1 && results.size > 1) differing.push(`${task} (${arm})`);
    }
  }
  out.push(table(['Task', ...cells], taskRows));
  out.push(`\nTasks where the agents differ inside an arm: ${differing.length ? differing.join(', ') : 'none'}.\n`);

  h('Flaky grades, waste and the meter against the agents');
  const flaky = counted.filter((v) => v.result?.flaky);
  out.push(`Flaky grades (the three grading runs disagreed): ${flaky.length ? flaky.map((v) => v.run).join(', ') : 'none'}.\n`);
  out.push(`\nSpend wasted on runs that were interrupted, failed in the infrastructure or were stopped by the cap: ${formatUSD(wasted)}.\n`);
  const reported = counted.filter((v) => v.result?.selfReported);
  if (reported.length > 0) {
    const rows = [...group(reported, (v) => v.agent)].map(([agent, rs]) => [
      agent,
      rs.length,
      sum(rs.map((v) => v.tokens.input)),
      sum(rs.map((v) => v.result.selfReported.input ?? 0)),
      sum(rs.map((v) => v.tokens.output)),
      sum(rs.map((v) => v.result.selfReported.output ?? 0)),
    ]);
    out.push('\nThe meter counts every call; an agent may count fewer (subagents, retries). The meter decides the cap.\n\n');
    out.push(table(['Agent', 'Runs', 'Meter input', 'Reported input', 'Meter output', 'Reported output'], rows));
  } else {
    out.push('\nNo run recorded its own token count.\n');
  }

  h('Suspect patches');
  const suspects = all.filter((v) => Array.isArray(v.result?.suspect) && v.result.suspect.length > 0);
  out.push('Grading runs the agent\'s code, so it cannot be made tamper-proof; a patch that touches the grading\'s own machinery is graded as it is and listed here for a person to read. What was stripped from a patch is listed with it.\n\n');
  if (suspects.length > 0) {
    out.push(table(['Run', 'Task', 'Agent / arm', 'Why', 'Stripped', 'Patch'], suspects.map((v) => [v.run, v.task ?? '-', v.agent ? cellKey(v) : '-', v.result.suspect.join(', '), (v.result.removed ?? []).join(', ') || '-', v.result.patch ?? '-'])));
  } else out.push('None.\n');

  h('Refused entries and limits');
  out.push('What an agent left in its tree or its output that the harness would not read (a FIFO, a device, a link, a file over its cap) is skipped, the run goes on, and the entry is listed here. A run whose container refused the task\'s CPU and memory limits ran on the service\'s own, which are the same for both agents.\n\n');
  const refusedRuns = all.filter((v) => (v.result?.notes?.refused?.length ?? 0) > 0 || v.result?.notes?.limits === 'service');
  if (refusedRuns.length > 0) out.push(table(['Run', 'Agent / arm', 'Refused', 'Limits'], refusedRuns.map((v) => [v.run, v.agent ? cellKey(v) : '-', (v.result.notes.refused ?? []).map((r) => `${r.name}: ${r.reason}`).join('; ') || '-', v.result.notes.limits ?? '-'])));
  else out.push('None.\n');

  h('What this report does not claim');
  out.push(NOT_CLAIMED.map((s) => `* ${s}`).join('\n'));
  out.push('');
  return `${out.join('\n').replace(/\n{3,}/g, '\n\n').trim()}\n`;
}
