// The per-run budget after the pilot, the binding of a ledger to one configuration, and the owner's confirmation.
//
// Before the pilot every pilot run reserves `perRunBudgetUSD`. The pilot's finished runs give, for each arm, the 95th percentile
// of what a run cost (the meter's count, both agents together: the budget is the same for both). The budget of an arm is that
// times `factor` (1.5) plus the largest worst case of one call seen, and never under twice that worst case. The projection says
// what the full run (which runs the pilot's tasks again) would reserve and spend against the cap. The proposal is a file;
// `bench run` starts only when the owner has confirmed it (`bench confirm`) and neither the configuration, nor the tasks, nor
// the pins have changed since. Every run of the ledger carries the digest of the configuration it was made under.
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { formatUSD } from './cost.mjs';
import { percentile } from './stats.mjs';

export const FACTOR = 1.5;
export const MIN_PER_ARM = 3;
const FLOOR_WORST_CASES = 2;
const PLANNING_MARGIN = 1.2;

export class BudgetError extends Error {
  constructor(message) {
    super(message);
    this.name = 'BudgetError';
  }
}

/**
 * What the ledger and a pilot are bound to: everything that changes a treatment or the task set. The model id, prices, arms,
 * limits, context window, pins, the two lock files, `tasks.seed` and `tasks.selfRepo`. Not `concurrency` (it changes the wall
 * time of a run, not what it is given), not the endpoint (the same model elsewhere), not the cap (a bound on spend, checked
 * live). Each field is an HMAC under a key kept in `$BENCH_HOME` (`digestKey`), so no file that holds a fingerprint holds a hash
 * of the model id that a guess could be tested against.
 */
export function configFingerprint({ config, pins, lockTexts, key }) {
  const mac = (v) => crypto.createHmac('sha256', key).update(JSON.stringify(v ?? null)).digest('hex').slice(0, 16);
  const fields = {
    model: mac(config.modelId),
    prices: mac(config.prices),
    arms: mac(config.arms),
    limits: mac(config.limits),
    contextWindow: mac(config.contextWindow),
    pins: mac(pins),
    locks: mac(lockTexts),
    seed: mac(config.tasks?.seed),
    selfRepo: mac(config.tasks?.selfRepo),
  };
  return { digest: mac(fields), fields };
}

/** The per-home key of the fingerprint: made once, kept at 0600, never in the repository. */
export function digestKey(home) {
  const file = path.join(home, 'digest.key');
  try {
    return fs.readFileSync(file, 'utf8').trim();
  } catch {
    const key = crypto.randomBytes(32).toString('hex');
    fs.mkdirSync(home, { recursive: true });
    fs.writeFileSync(file, `${key}\n`, { mode: 0o600 });
    return key;
  }
}

export const changedFields = (was, now) => Object.keys(now).filter((k) => was?.[k] !== now[k]);

/** Every run of the ledger carries this digest, or a BudgetError that names what changed. */
export function assertLedgerBound(records, { digest, fields }) {
  const stamps = new Set();
  let unstamped = false;
  for (const r of records) {
    if (r.kind === 'state' && r.state === 'Planned') {
      if (r.digest) stamps.add(r.digest);
      else unstamped = true;
    }
  }
  const other = [...stamps].filter((d) => d !== digest);
  if (other.length === 0 && !unstamped) return;
  const setup = [...records].reverse().find((r) => r.kind === 'setup' && r.fields && r.digest !== digest);
  const names = setup ? changedFields(setup.fields, fields) : [];
  throw new BudgetError(`the ledger holds runs made under another configuration${names.length ? ` (changed: ${names.join(', ')})` : ''}: runs of two configurations are never pooled; use a new BENCH_HOME`);
}

/**
 * @param {Map} folded   ledger fold
 * @param {Set<string>} pilotRuns  run ids of the pilot's runs
 * @param {string[]} arms
 * @param {{ totalTasks: number, agents: number, capMicro: number, factor?: number, minPerArm?: number }} o
 *
 * An arm's budget is the larger of 1.5 times the 95th percentile of what its pilot runs cost plus the largest worst case of a
 * single call it saw, and twice that worst case: a run is refused a call that would not fit, so a budget under one worst case
 * cannot make a single call. A pilot run that ended `budget_hit` cost only what its budget allowed, so the p95 is censored.
 */
export function pilotEstimate({ folded, pilotRuns, arms, totalTasks, agents, capMicro, factor = FACTOR, minPerArm = MIN_PER_ARM }) {
  const perArm = {};
  for (const arm of arms) {
    const done = [...folded.values()].filter((v) => pilotRuns.has(v.run) && v.arm === arm && v.state === 'Done' && v.result?.invalid !== true);
    const costs = done.map((v) => v.attemptSpentMicro);
    if (costs.length < minPerArm) throw new BudgetError(`arm ${arm}: ${costs.length} finished pilot run(s), at least ${minPerArm} are needed for an estimate`);
    let maxWorstMicro = 0;
    for (const v of done) for (const a of v.attempts) for (const c of a?.calls?.values() ?? []) maxWorstMicro = Math.max(maxWorstMicro, c.worstMicro ?? 0);
    const p95Micro = percentile(costs, 95);
    const padMicro = Math.ceil(p95Micro * factor) + maxWorstMicro;
    const floorMicro = FLOOR_WORST_CASES * maxWorstMicro;
    perArm[arm] = {
      done: costs.length,
      p95Micro,
      meanMicro: Math.round(costs.reduce((s, c) => s + c, 0) / costs.length),
      maxWorstMicro,
      floorMicro,
      budgetMicro: Math.max(padMicro, floorMicro),
      wonBy: floorMicro > padMicro ? 'floor' : 'p95',
      censored: done.filter((v) => v.result?.exit === 'budget_hit').length,
    };
  }
  const perArmMicro = Object.fromEntries(arms.map((a) => [a, perArm[a].budgetMicro]));
  const spentMicro = [...folded.values()].reduce((s, v) => s + v.spentMicro, 0);
  // The pilot's blocks are run again in the full run under the confirmed budget, so every run of the plan is still to come.
  const runsPerArm = totalTasks * agents;
  let reserve = 0;
  let expect = 0;
  for (const arm of arms) {
    reserve += runsPerArm * perArm[arm].budgetMicro;
    expect += runsPerArm * perArm[arm].meanMicro;
  }
  const planningMicro = spentMicro + Math.ceil(expect * PLANNING_MARGIN);
  const projection = { spentMicro, runsPerArm, reservedIfAllMicro: spentMicro + reserve, expectedMicro: spentMicro + expect, planningMicro, capMicro, fitsCap: planningMicro <= capMicro };
  return { factor, perArm, perArmMicro, censoredArms: arms.filter((a) => perArm[a].censored > 0), projection };
}

export function renderEstimate(est) {
  const lines = ["pilot estimate (the meter's cost per finished run, both agents together):"];
  for (const [arm, a] of Object.entries(est.perArm)) {
    lines.push(
      `  ${arm}: ${a.done} runs, mean ${formatUSD(a.meanMicro)}, 95th percentile ${formatUSD(a.p95Micro)}, largest worst case of one call ${formatUSD(a.maxWorstMicro)}, floor ${formatUSD(a.floorMicro)}; proposed budget per run ${formatUSD(a.budgetMicro)} (${a.wonBy === 'floor' ? 'the floor' : `x${est.factor} of the p95 plus one worst case`})`,
    );
    if (a.censored > 0) lines.push(`  ${arm}: ${a.censored} pilot run(s) ended budget_hit: their cost is a ceiling, the p95 is censored. Confirm with --censored to accept it.`);
  }
  const p = est.projection;
  lines.push(`projection for the full run (the pilot's tasks run again; ${p.runsPerArm} runs per arm; spent so far ${formatUSD(p.spentMicro)}):`);
  lines.push(`  expected total ${formatUSD(p.expectedMicro)}, planned with a margin ${formatUSD(p.planningMicro)}; worst case if every run used its whole budget ${formatUSD(p.reservedIfAllMicro)}; cap ${formatUSD(p.capMicro)}`);
  lines.push(p.fitsCap ? '  the planned total fits under the cap.' : '  the planned total does NOT fit under the cap: the run would stop at the cap, in whole blocks. Confirm with --over-cap to accept that.');
  return lines.join('\n');
}

export const proposalBody = ({ estimate, fingerprint, pilotBlocks, now = new Date() }) => ({
  schema: 2,
  confirmed: false,
  createdAt: now.toISOString(),
  pilotBlocks,
  digest: fingerprint.digest,
  fields: fingerprint.fields,
  factor: estimate.factor,
  perArmMicro: estimate.perArmMicro,
  perArm: estimate.perArm,
  censoredArms: estimate.censoredArms,
  projection: estimate.projection,
});

/** The owner's confirmation: the same file with `confirmed` true. `overCap` accepts a projection above the cap, `censored` a censored p95. */
export function confirmProposal(proposal, { overCap = false, censored = false, now = new Date() } = {}) {
  if (!proposal || proposal.schema !== 2) throw new BudgetError('the pilot budget file is not one this harness wrote');
  if (!proposal.projection.fitsCap && !overCap) throw new BudgetError('the projection is above the cap; confirm with --over-cap to run until the cap stops it');
  if (proposal.censoredArms.length > 0 && !censored) throw new BudgetError(`pilot runs of arm ${proposal.censoredArms.join(', ')} ended budget_hit, so their cost is a ceiling and the budget is a guess; confirm with --censored to accept it`);
  return { ...proposal, confirmed: true, confirmedAt: now.toISOString(), overCap: overCap === true, censoredAccepted: censored === true };
}

/**
 * The budgets a run may start with, or a BudgetError saying why not. Checks that the file is confirmed, that it belongs to this
 * configuration and these tasks, that the projection fits the cap *now* (the cap is not in the fingerprint), and that every arm has
 * a whole budget between its floor and the cap.
 */
export function requireConfirmed(proposal, { config, fingerprint }) {
  if (!proposal) throw new BudgetError('no pilot budget: run `bench pilot`, read its projection, then `bench confirm`');
  if (proposal.schema !== 2) throw new BudgetError('the pilot budget file is not one this harness wrote');
  if (proposal.confirmed !== true) throw new BudgetError('the pilot budget is not confirmed: read it, then run `bench confirm`');
  if (proposal.digest !== fingerprint.digest) {
    throw new BudgetError(`the configuration, the pins or the task locks changed since the pilot (${changedFields(proposal.fields, fingerprint.fields).join(', ') || 'unknown'}): run \`bench pilot\` again, in a new BENCH_HOME`);
  }
  if (proposal.projection.planningMicro > config.capMicro && proposal.overCap !== true) throw new BudgetError('the cap is below what the confirmed projection plans to spend, and --over-cap was not given');
  if (proposal.censoredArms.length > 0 && proposal.censoredAccepted !== true) throw new BudgetError('the pilot was censored and --censored was not given');
  for (const arm of Object.keys(config.arms)) {
    const b = proposal.perArmMicro?.[arm];
    if (!Number.isSafeInteger(b) || b <= 0) throw new BudgetError(`the pilot budget has no whole positive budget for arm ${arm}`);
    if (b > config.capMicro) throw new BudgetError(`the budget of arm ${arm} is above the cap`);
    if (b < (proposal.perArm?.[arm]?.floorMicro ?? 0)) throw new BudgetError(`the budget of arm ${arm} is under its floor of two worst-case calls`);
  }
  return proposal.perArmMicro;
}
