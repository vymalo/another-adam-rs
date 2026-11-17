// The per-run budget after the pilot, and the owner's confirmation of it.
//
// Before the pilot every run reserves `perRunBudgetUSD`. The pilot's finished runs give, for each arm, the 95th percentile
// of what a run cost (the meter's count, both agents together: the budget is the same for both); the proposed budget is
// that times `factor` (1.5). The projection says what the full run would reserve and spend against the cap. The proposal
// is a file; `bench run` starts only when the owner has confirmed it (`bench confirm`) and neither the configuration nor
// the tasks nor the pins have changed since.
import crypto from 'node:crypto';
import { formatUSD } from './cost.mjs';
import { percentile } from './stats.mjs';

export const FACTOR = 1.5;
export const MIN_PER_ARM = 3;

export class BudgetError extends Error {
  constructor(message) {
    super(message);
    this.name = 'BudgetError';
  }
}

const digest = (value) => crypto.createHash('sha256').update(JSON.stringify(value)).digest('hex');

/** What a pilot is only good for: the same model, prices, arms, limits, context window, agents. Not the endpoint, key or cap. */
export const configDigest = (config, pins) =>
  digest({ model: config.modelId, prices: config.prices, arms: config.arms, limits: config.limits, contextWindow: config.contextWindow, pins });

/** `texts` are the two lock files' text (an absent file is ''). */
export const lockDigest = (texts) => digest(texts);

/**
 * @param {Map} folded   ledger fold
 * @param {Set<string>} pilotRuns  run ids of the pilot's blocks
 * @param {string[]} arms
 * @param {{ totalTasks: number, agents: number, capMicro: number, factor?: number, minPerArm?: number }} o
 */
export function pilotEstimate({ folded, pilotRuns, arms, totalTasks, agents, capMicro, factor = FACTOR, minPerArm = MIN_PER_ARM }) {
  const perArm = {};
  for (const arm of arms) {
    const done = [...folded.values()].filter((v) => pilotRuns.has(v.run) && v.arm === arm && v.state === 'Done' && v.result?.invalid !== true);
    const costs = done.map((v) => v.attemptSpentMicro);
    if (costs.length < minPerArm) throw new BudgetError(`arm ${arm}: ${costs.length} finished pilot run(s), at least ${minPerArm} are needed for an estimate`);
    perArm[arm] = { done: costs.length, p95Micro: percentile(costs, 95), meanMicro: Math.round(costs.reduce((s, c) => s + c, 0) / costs.length), budgetMicro: Math.ceil(percentile(costs, 95) * factor) };
  }
  const perArmMicro = Object.fromEntries(arms.map((a) => [a, perArm[a].budgetMicro]));
  const spentMicro = [...folded.values()].reduce((s, v) => s + v.spentMicro, 0);
  const runsPerArm = totalTasks * agents;
  let reserve = 0;
  let expect = 0;
  const doneNow = (arm) => [...folded.values()].filter((v) => v.arm === arm && v.state === 'Done').length;
  for (const arm of arms) {
    const left = Math.max(0, runsPerArm - doneNow(arm));
    reserve += left * perArm[arm].budgetMicro;
    expect += left * perArm[arm].meanMicro;
  }
  const projection = {
    spentMicro,
    runsPerArm,
    reservedIfAllMicro: spentMicro + reserve,
    expectedMicro: spentMicro + expect,
    capMicro,
    fitsCap: spentMicro + reserve <= capMicro,
  };
  return { factor, perArm, perArmMicro, projection };
}

export function renderEstimate(est) {
  const lines = ["pilot estimate (the meter's cost per finished run, both agents together):"];
  for (const [arm, a] of Object.entries(est.perArm)) {
    lines.push(`  ${arm}: ${a.done} runs, mean ${formatUSD(a.meanMicro)}, 95th percentile ${formatUSD(a.p95Micro)}, proposed budget per run ${formatUSD(a.budgetMicro)} (x${est.factor})`);
  }
  const p = est.projection;
  lines.push(`projection for the full run (${p.runsPerArm} runs per arm; spent so far ${formatUSD(p.spentMicro)}):`);
  lines.push(`  expected total ${formatUSD(p.expectedMicro)}; if every run used its whole budget ${formatUSD(p.reservedIfAllMicro)}; cap ${formatUSD(p.capMicro)}`);
  lines.push(p.fitsCap ? '  the budgets fit under the cap.' : '  the budgets do NOT all fit under the cap: runs would stop at the cap, in complete blocks. Confirm with --over-cap to accept that.');
  return lines.join('\n');
}

export const proposalBody = ({ estimate, config, pins, lockTexts, pilotBlocks, now = new Date() }) => ({
  schema: 1,
  confirmed: false,
  createdAt: now.toISOString(),
  pilotBlocks,
  configDigest: configDigest(config, pins),
  lockDigest: lockDigest(lockTexts),
  factor: estimate.factor,
  perArmMicro: estimate.perArmMicro,
  perArm: estimate.perArm,
  projection: estimate.projection,
});

/** The owner's confirmation: the same file with `confirmed` true. `overCap` accepts a projection above the cap. */
export function confirmProposal(proposal, { overCap = false, now = new Date() } = {}) {
  if (!proposal || proposal.schema !== 1) throw new BudgetError('the pilot budget file is not one this harness wrote');
  if (!proposal.projection.fitsCap && !overCap) throw new BudgetError('the projection is above the cap; confirm with --over-cap to run until the cap stops it');
  return { ...proposal, confirmed: true, confirmedAt: now.toISOString(), overCap: overCap === true };
}

/**
 * The budgets a run may start with, or a BudgetError saying why not. Checks that the file is confirmed, that it belongs to this
 * configuration and these tasks, and that every arm has a whole positive budget no larger than the cap.
 */
export function requireConfirmed(proposal, { config, pins, lockTexts }) {
  if (!proposal) throw new BudgetError('no pilot budget: run `bench pilot`, read its projection, then `bench confirm`');
  if (proposal.schema !== 1) throw new BudgetError('the pilot budget file is not one this harness wrote');
  if (proposal.confirmed !== true) throw new BudgetError('the pilot budget is not confirmed: read it, then run `bench confirm`');
  if (proposal.configDigest !== configDigest(config, pins)) throw new BudgetError('the configuration or the pins changed since the pilot: run `bench pilot` again');
  if (proposal.lockDigest !== lockDigest(lockTexts)) throw new BudgetError('the task locks changed since the pilot: run `bench pilot` again');
  if (proposal.projection?.fitsCap !== true && proposal.overCap !== true) throw new BudgetError('the confirmed projection is above the cap and --over-cap was not given');
  for (const arm of Object.keys(config.arms)) {
    const b = proposal.perArmMicro?.[arm];
    if (!Number.isSafeInteger(b) || b <= 0) throw new BudgetError(`the pilot budget has no whole positive budget for arm ${arm}`);
    if (b > config.capMicro) throw new BudgetError(`the budget of arm ${arm} is above the cap`);
  }
  return proposal.perArmMicro;
}
