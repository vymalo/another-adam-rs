// What the dry run asserts, as functions of the ledger: each returns [{ ok, what, detail }]. The dry run (dryrun.mjs)
// runs the stack; these decide. They are tested without a container service on ledgers the runner made.
import { priceCall } from './cost.mjs';
import { FIXED_USAGE } from './scripted-model.mjs';
import { CALLS_PER_RUN } from './agent-script.mjs';
import { VARIANTS } from './variants.mjs';

export const EXPECTED_PASS = Object.freeze({ hello: true, 'fix-add': true, wrong: false });

/** What one scripted call costs at the configuration's prices, in micro-dollars. */
export const scriptedCallMicro = (prices) => priceCall(prices, { input: FIXED_USAGE.prompt, cached: FIXED_USAGE.cached, cacheWrite: 0, output: FIXED_USAGE.completion });

const check = (ok, what, detail = '') => ({ ok: Boolean(ok), what, detail: ok ? '' : detail });

/** Every run `Done`, graded 1, 1, 0 (hello, fix-add, wrong) for each agent, three grades each, no run invalid. */
export function checkGrades(folded, { agents, arm }) {
  const out = [];
  for (const agent of agents) {
    for (const [task, pass] of Object.entries(EXPECTED_PASS)) {
      const run = `${task}-${agent}-${arm}`;
      const v = folded.get(run);
      const r = v?.result;
      out.push(check(v?.state === 'Done' && r && r.pass === pass && !r.invalid && r.grades?.length === 3 && r.grades.every((g) => g === (pass ? 1 : 0)), `${agent} on ${task} is graded ${pass ? 1 : 0}`, `state ${v?.state ?? 'absent'}, result ${JSON.stringify(r ?? null)}`));
      out.push(check(v?.state !== 'Done' || r?.exit === 'done', `${agent} on ${task} ended done`, `exit ${r?.exit}`));
    }
  }
  return out;
}

/**
 * The ledger's cost is the scripted usage times the price: for each run, calls x one call's cost, where the
 * calls are the agent's scripted count; in total, and the scripted model saw exactly the calls the ledger settled.
 */
export function checkCost(folded, { runs, prices, modelRequests }) {
  const per = scriptedCallMicro(prices);
  const out = [];
  let settled = 0;
  for (const { run, agent } of runs) {
    const v = folded.get(run);
    const calls = CALLS_PER_RUN[VARIANTS[agent]?.driver ?? agent];
    const want = calls * per;
    out.push(check(v && v.spentMicro === want, `${run} cost ${want} micro-dollars (${calls} calls x ${per})`, `ledger says ${v?.spentMicro}`));
    out.push(check(v?.calls.usageMissing === 0 && v?.calls.failed === 0 && v?.calls.refused === 0, `${run} has no call without usage, failed or refused`, JSON.stringify(v?.calls)));
    settled += v?.calls.settled ?? 0;
  }
  out.push(check(settled === modelRequests, `the scripted model saw ${modelRequests} requests and the ledger settled ${settled}`, 'they differ: a call went round the meter or was not recorded'));
  return out;
}

/** A run that hit its budget: graded, exit budget_hit, at least one call refused, spend inside the reservation. */
export function checkBudgetHit(folded, { runs, reservedMicro }) {
  const out = [];
  for (const run of runs) {
    const v = folded.get(run);
    out.push(check(v?.state === 'Done' && v.result?.exit === 'budget_hit', `${run} ended budget_hit and was graded`, `state ${v?.state}, exit ${v?.result?.exit}`));
    out.push(check(v && v.calls.refused >= 1 && v.spentMicro <= reservedMicro, `${run} had a call refused and spent ${v?.spentMicro} of ${reservedMicro}`, JSON.stringify(v?.calls)));
  }
  return out;
}

/**
 * After a kill and a resume: the runs that were finished are not run again (one attempt, the same Running
 * count), the killed run has a second attempt with its first attempt's spend kept as waste, and the runs not yet
 * started ran once.
 */
export function checkResume(folded, { finished, killed, later }) {
  const out = [];
  for (const run of finished) {
    const v = folded.get(run);
    out.push(check(v?.state === 'Done' && v.attempt === 1 && v.history.filter((s) => s === 'Running').length === 1, `${run} finished before the kill and was not run again`, `state ${v?.state}, attempts ${v?.attempt}`));
  }
  const k = folded.get(killed);
  out.push(check(k?.state === 'Done' && k.attempt === 2 && k.interruptions === 1, `${killed} was interrupted once and run again`, `state ${k?.state}, attempts ${k?.attempt}, interruptions ${k?.interruptions}`));
  out.push(check(k && k.wastedMicro > 0 && k.spentMicro >= k.wastedMicro, `${killed}'s first attempt is kept as waste (${k?.wastedMicro})`, `waste ${k?.wastedMicro}`));
  for (const run of later) {
    const v = folded.get(run);
    out.push(check(v?.state === 'Done' && v.attempt === 1, `${run} had not started and ran once`, `state ${v?.state}, attempts ${v?.attempt}`));
  }
  return out;
}

/** Print the results; true when all passed. */
export function report(results, write = (l) => process.stdout.write(`${l}\n`)) {
  for (const r of results) write(`${r.ok ? 'ok  ' : 'FAIL'} ${r.what}${r.ok || !r.detail ? '' : ` (${r.detail})`}`);
  return results.every((r) => r.ok);
}
