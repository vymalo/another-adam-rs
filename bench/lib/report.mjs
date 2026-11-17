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

export const NOT_CLAIMED = [
  'One agent is better in general, or with another model or another endpoint: this compares two agents on one model in two arms.',
  'Comparability with any public leaderboard: the environment is adapted, each task has one attempt, and the limits are ours.',
  'A difference where the intervals overlap or the interval of a difference includes zero: with about a hundred runs, gaps under about ten points are noise.',
  'Real billing: every cost here is the meter\'s count times the price table in the local configuration.',
  `A conclusion for a language with fewer than ${MIN_TASKS_PER_LANGUAGE} tasks.`,
];

const pct = (x) => `${(x * 100).toFixed(1)}%`;
const interval = (w) => (w ? `${pct(w.low)} to ${pct(w.high)}` : 'n/a');
const num = (n) => (n === null || n === undefined ? 'n/a' : Number.isInteger(n) ? String(n) : n.toFixed(1));
const sum = (xs) => xs.reduce((s, x) => s + x, 0);

function table(header, rows) {
  if (rows.length === 0) return '_none_\n';
  const line = (cells) => `| ${cells.join(' | ')} |`;
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
  const all = [...folded.values()].sort((a, b) => a.run.localeCompare(b.run));
  const counted = all.filter((v) => v.state === 'Done');
  const excluded = all.filter((v) => v.state !== 'Done');
  const label = setup.modelLabel ?? opts.modelLabel ?? 'unlabelled';
  const seed = setup.seed ?? 1;
  const out = [];
  const h = (text) => out.push(`\n## ${text}\n`);
  const show = (v) => (v === undefined || v === null ? 'not recorded' : typeof v === 'object' ? JSON.stringify(v) : String(v));

  const lastAt = (records ?? []).map((r) => r.t).filter(Boolean).sort().at(-1);
  out.push('# Benchmark report');
  out.push(`\nBuilt from the ledger${lastAt ? `; last record ${lastAt}` : ''}. Costs are the meter's count times the owner's price table.\n`);

  h('Setup');
  const spent = sum(all.map((v) => v.spentMicro));
  const wasted = sum(all.map((v) => v.wastedMicro));
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
  if (excluded.length > 0) {
    out.push('\nExcluded from every rate below (not Done):\n');
    out.push(table(['Run', 'State', 'Task', 'Agent / arm'], excluded.map((v) => [v.run, v.state ?? 'unknown', v.task ?? '-', v.agent ? cellKey(v) : '-'])));
  }

  h('Pass rate by agent and arm');
  out.push(table(['Agent', 'Arm', 'Runs', 'Solved', 'Rate', '95% interval (Wilson)'], [...group(counted, cellKey)].map(([, rs]) => rateRow([rs[0].agent, rs[0].arm], rs))));

  h('Paired differences');
  out.push('Agents compared inside each arm, and arms inside each agent, on the tasks both have a Done run for.\n\n');
  out.push(pairedSection(counted, seed, (v) => v.agent, (v) => v.arm, 'agent', 'arm'));
  out.push('\n');
  out.push(pairedSection(counted, seed, (v) => v.arm, (v) => v.agent, 'arm', 'agent'));

  h('By source');
  out.push(table(['Source', 'Agent', 'Arm', 'Runs', 'Solved', 'Rate', '95% interval (Wilson)'], [...group(counted, (v) => `${v.source ?? 'unknown'} / ${cellKey(v)}`)].map(([, rs]) => rateRow([rs[0].source ?? 'unknown', rs[0].agent, rs[0].arm], rs))));

  h('By language');
  const langRows = [];
  const langNotes = [];
  for (const [lang, rs] of group(counted, (v) => v.language ?? 'unknown')) {
    const tasks = new Set(rs.map((v) => v.task)).size;
    if (tasks < MIN_TASKS_PER_LANGUAGE) langNotes.push(`${lang}: ${tasks} tasks, fewer than ${MIN_TASKS_PER_LANGUAGE}, no conclusion`);
    for (const [, cell] of group(rs, cellKey)) langRows.push(rateRow([lang, cell[0].agent, cell[0].arm], cell));
  }
  out.push(table(['Language', 'Agent', 'Arm', 'Runs', 'Solved', 'Rate', '95% interval (Wilson)'], langRows));
  if (langNotes.length > 0) out.push(`\n${langNotes.map((n) => `* ${n}`).join('\n')}\n`);

  h('Cost, tokens, time and calls');
  const costRows = [];
  const timeRows = [];
  for (const [, rs] of group(counted, cellKey)) {
    const solved = rs.filter(passed).length;
    const cost = sum(rs.map((v) => v.spentMicro));
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
  }
  out.push(table(['Agent', 'Arm', 'Cost', 'Cost per solved task', 'Input', 'Cached', 'Cache write', 'Output', 'Reasoning (in output)', 'Median per run: input / output / reasoning'], costRows));
  out.push('\n');
  out.push(table(['Agent', 'Arm', 'Wall median', 'Wall p90', 'Model calls (median)', 'Refused calls', 'Calls with no usage (charged worst case)'], timeRows));

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

  h('What this report does not claim');
  out.push(NOT_CLAIMED.map((s) => `* ${s}`).join('\n'));
  out.push('');
  return `${out.join('\n').replace(/\n{3,}/g, '\n\n').trim()}\n`;
}
