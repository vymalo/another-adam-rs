// One run, start to end, and a plan of runs with resume. This is the part of `pilot` and `run` the dry run needs:
// sequential, one slot. Task blocks, several slots at once and the pilot's budget estimate are not here.
//
//   Planned -> Reserved -> Preparing -> Running -> Collecting -> Grading -> Done
//
// Every transition is a ledger record (ledger.mjs refuses any other). The patch is saved before grading, so a
// grading that is stopped is graded again from it and the agent is never run again. A harness that is killed leaves
// the run in the state it was in; the next start reads the ledger (`resumeActions`): a Done run is not repeated,
// an interrupted one starts again from scratch (its spend stays in the ledger as waste), a Grading one is graded again.
import fs from 'node:fs';
import path from 'node:path';
import { prepareBase } from './base.mjs';
import { CapBook } from './cap.mjs';
import { gradePatch } from './grade.mjs';
import { resumeActions, resumeRecords } from './ledger.mjs';

/** Thrown by a test hook to stand for a killed harness: nothing is written and nothing is cleaned up. */
export class HarnessCrash extends Error {
  constructor(where) {
    super(`the harness stopped at ${where}`);
    this.name = 'HarnessCrash';
  }
}

const SAFE_ID = /[^A-Za-z0-9._-]/g;
export const runDirName = (run, attempt) => `${run.replace(SAFE_ID, '-')}-a${attempt}`;

/**
 * @typedef {object} Deps
 * @property {object} ledger   an open Ledger
 * @property {object} book     a CapBook
 * @property {object} meter    createMeter's
 * @property {object} stack    the Stack (or a stand-in): containers(slot), ports, agentDir
 * @property {object} agents   { adam, opencode } drivers
 * @property {object} config   the normalised configuration
 * @property {string} home     $BENCH_HOME
 * @property {number} limitMs  the time limit of a run
 * @property {(spec) => number} [reserveMicro]  what a run reserves; default the configuration's per-run budget
 * @property {(run: string, state: string) => void} [afterState]  called after each state record is written
 */

function stateRecord(deps, run, state, extra = {}) {
  deps.ledger.append({ kind: 'state', run, state, ...extra });
  deps.afterState?.(run, state);
}

async function gradeSaved({ deps, spec, task, patchPath, containers }) {
  const hasPatch = fs.existsSync(patchPath) && fs.statSync(patchPath).size > 0;
  const work = path.join(deps.home, 'work');
  fs.mkdirSync(work, { recursive: true });
  const grade = () => gradePatch({ containers, task, patchFile: hasPatch ? patchPath : null, workRoot: work });
  const net = task.verifierNetwork;
  // The service has no route out; a verifier that needs the network gets one for the window of its grading.
  return net === 'public' || net === 'default' ? deps.stack.pullWindow(spec.slot, grade) : grade();
}

function resultOf({ grade, exit, wallMs, selfReported, patchPath, notes }) {
  return {
    pass: grade.pass,
    grades: grade.attempts.map((a) => (a.pass ? 1 : 0)),
    flaky: grade.flaky,
    invalid: grade.invalid,
    exit,
    wallMs,
    ...(selfReported ? { selfReported } : {}),
    suspect: grade.suspect,
    removed: grade.removed,
    patch: patchPath,
    ...(notes ? { notes } : {}),
  };
}

/**
 * Run one planned run to its end. The run is `Planned` in the ledger already. Returns the final state.
 * An infrastructure failure before or during the run is a `Failed_infra` (retried by the next start), not a fail.
 */
export async function executeRun({ deps, spec, attempt }) {
  const { ledger, book, meter, stack, config } = deps;
  const { run, task } = spec;
  const agent = deps.agents[spec.agent];
  if (!agent) throw new Error(`no driver for agent ${spec.agent}`);
  const reservedMicro = deps.reserveMicro ? deps.reserveMicro(spec) : config.perRunBudgetMicro;
  if (!book.reserve(run, reservedMicro)) {
    stateRecord(deps, run, 'NotStarted', { reason: 'cap' });
    return { state: 'NotStarted' };
  }
  stateRecord(deps, run, 'Reserved', { reservedMicro });

  const dirName = runDirName(run, attempt);
  const paths = { run: path.join(deps.home, 'runs', dirName), in: path.join(deps.home, 'scratch', dirName, 'in'), out: path.join(deps.home, 'scratch', dirName, 'out') };
  fs.mkdirSync(paths.run, { recursive: true });
  const containers = stack.containers(spec.slot);
  const opened = { meter: false };
  let ctx = null;
  const fail = (state, reason) => {
    if (opened.meter) meter.closeRun(run);
    else book.close(run);
    stateRecord(deps, run, state, { reason });
  };
  try {
    stateRecord(deps, run, 'Preparing');
    const base = await (deps.prepareBase ?? prepareBase)({ containers, task, root: path.join(deps.home, 'bases') });
    const { token } = meter.openRun({ run, attempt, arm: spec.arm });
    opened.meter = true;
    ctx = {
      run,
      attempt,
      slot: spec.slot,
      task,
      base,
      stack,
      containers,
      meter: {
        token,
        baseUrl: `http://meter:8090/r/${token}/v1`,
        refusal: () => meter.status(run)?.refusal ?? null,
      },
      paths,
      limitMs: deps.limitMs,
      log: deps.log ?? (() => {}),
    };
    await agent.prepare(ctx);
  } catch (e) {
    if (e instanceof HarnessCrash) throw e;
    await agent.cleanup(ctx ?? { run, slot: spec.slot, stack, paths }).catch(() => {});
    fail('Failed_infra', 'prepare');
    return { state: 'Failed_infra', error: e.message };
  }

  let ran;
  try {
    stateRecord(deps, run, 'Running');
    ran = await agent.run(ctx);
  } catch (e) {
    if (e instanceof HarnessCrash) throw e;
    await agent.cleanup(ctx).catch(() => {});
    meter.closeRun(run);
    stateRecord(deps, run, 'Interrupted', { reason: 'agent_error' });
    stateRecord(deps, run, 'Failed_infra', { reason: 'agent_error' });
    return { state: 'Failed_infra', error: e.message };
  }

  if (ran.exit === 'infra') {
    await agent.cleanup(ctx).catch(() => {});
    meter.closeRun(run);
    stateRecord(deps, run, 'Interrupted', { reason: 'infra' });
    stateRecord(deps, run, 'Failed_infra', { reason: 'infra' });
    return { state: 'Failed_infra', error: 'infra' };
  }
  if (ran.exit === 'stopped_by_cap') {
    await agent.cleanup(ctx).catch(() => {});
    meter.closeRun(run);
    stateRecord(deps, run, 'StoppedByCap');
    return { state: 'StoppedByCap' };
  }

  // The agent has ended. Its reservation is not needed to collect or to grade.
  meter.closeRun(run);
  stateRecord(deps, run, 'Collecting');
  let collected;
  try {
    collected = await agent.collect(ctx);
  } catch (e) {
    if (e instanceof HarnessCrash) throw e;
    await agent.cleanup(ctx).catch(() => {});
    stateRecord(deps, run, 'Interrupted', { reason: 'collect' });
    stateRecord(deps, run, 'Failed_infra', { reason: 'collect' });
    return { state: 'Failed_infra', error: e.message };
  }
  const patchPath = path.join(paths.run, 'model.patch');
  fs.writeFileSync(patchPath, collected.patch);
  fs.writeFileSync(path.join(paths.run, 'meta.json'), `${JSON.stringify({ exit: ran.exit, wallMs: ran.wallMs, selfReported: ran.selfReported ?? null, notes: collected.notes ?? null }, null, 2)}\n`);
  await agent.cleanup(ctx).catch(() => {});

  stateRecord(deps, run, 'Grading');
  const grade = await gradeSaved({ deps, spec, task, patchPath, containers });
  stateRecord(deps, run, 'Done', { result: resultOf({ grade, exit: ran.exit, wallMs: ran.wallMs, selfReported: ran.selfReported, patchPath, notes: collected.notes }) });
  return { state: 'Done', pass: grade.pass };
}

/** Grade a run again from its saved patch; the agent is not run. */
export async function regradeRun({ deps, spec, attempt }) {
  const dir = path.join(deps.home, 'runs', runDirName(spec.run, attempt));
  const patchPath = path.join(dir, 'model.patch');
  let meta = {};
  try {
    meta = JSON.parse(fs.readFileSync(path.join(dir, 'meta.json'), 'utf8'));
  } catch {
    // no meta: graded as an unknown exit
  }
  const grade = await gradeSaved({ deps, spec, task: spec.task, patchPath, containers: deps.stack.containers(spec.slot) });
  stateRecord(deps, spec.run, 'Done', { result: resultOf({ grade, exit: meta.exit ?? 'unknown', wallMs: meta.wallMs, selfReported: meta.selfReported, patchPath, notes: meta.notes }) });
  return { state: 'Done', pass: grade.pass };
}

/**
 * Bring the ledger to where a restart leaves it, then run what is left, in the order of `plan`.
 * `plan` is a list of { run, task, agent, arm, slot, block?, source?, language? }.
 */
export async function runPlan({ deps, plan }) {
  const { ledger } = deps;
  const folded = ledger.fold();
  const actions = resumeActions(folded);
  for (const rec of resumeRecords(actions, folded)) {
    stateRecord(deps, rec.run, rec.state, Object.fromEntries(Object.entries(rec).filter(([k]) => !['kind', 'run', 'state'].includes(k))));
    if (rec.state === 'Interrupted') deps.book.close(rec.run); // the killed attempt's reservation is free; its spend stays
  }
  const outcomes = [];
  for (const spec of plan) {
    let view = ledger.fold().get(spec.run);
    if (!view) {
      stateRecord(deps, spec.run, 'Planned', { task: spec.task.id, agent: spec.agent, arm: spec.arm, block: spec.block ?? null, source: spec.source ?? spec.task.source ?? null, language: spec.language ?? spec.task.language ?? null });
      view = ledger.fold().get(spec.run);
    }
    if (view.state === 'Grading') outcomes.push({ run: spec.run, ...(await regradeRun({ deps, spec, attempt: view.attempt })) });
    else if (view.state === 'Planned') outcomes.push({ run: spec.run, ...(await executeRun({ deps, spec, attempt: view.attempt })) });
    else outcomes.push({ run: spec.run, state: view.state, skipped: true });
  }
  return outcomes;
}

/** A book that knows what the ledger already holds. */
export const bookFromLedger = (ledger, capMicro) => CapBook.fromFold(capMicro, ledger.fold());
