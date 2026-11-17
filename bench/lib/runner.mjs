// One run, start to end, and a plan of runs with resume. This is the part of `pilot` and `run` the dry run needs:
// one run at a time (`runPlan`). Task blocks, several slots at once and the pilot's budget are schedule.mjs and budget.mjs.
//
//   Planned -> Reserved -> Preparing -> Running -> Collecting -> Grading -> Done
//
// Every transition is a ledger record (ledger.mjs refuses any other). Before a run the slot's service is reaped and
// its volume emptied; after the agent ends it is stopped, and that is confirmed, before anything is read from its tree
// (an agent that cannot be stopped is infrastructure). The meter's record decides whether the run is the agent's or the
// infrastructure's (classify.mjs). The patch is saved before grading, so a grading that is stopped is graded again from
// it and the agent is never run again. Grading runs on the grader service, which no agent can reach. A harness that is
// killed leaves the run in the state it was in; the next start reads the ledger (`resumeActions`): a Done run is not
// repeated, an interrupted one starts again from scratch (its spend stays in the ledger as waste), a Grading one is
// graded again.
import fs from 'node:fs';
import path from 'node:path';
import { prepareBase } from './base.mjs';
import { CapBook } from './cap.mjs';
import { callStats, classifyRun } from './classify.mjs';
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
 * @property {object} stack    the Stack (or a stand-in)
 * @property {object} agents   { adam, opencode } drivers
 * @property {object} config   the normalised configuration (its `limits` are the limits of a run)
 * @property {string} home     where this phase keeps its results
 * @property {(spec) => number} [reserveMicro]  what a run reserves; default the configuration's per-run budget
 * @property {(slot: number, task) => Promise<void>} [pullImage]  makes the task's image present on the slot's service
 * @property {(task) => Promise<void>} [pullGraderImage]  makes the task's image present on the grader (also before a grading again)
 * @property {string} [digest]  the fingerprint of the configuration, written on every Planned record
 * @property {(run: string, state: string) => void} [afterState]  called after each state record is written
 */

function stateRecord(deps, run, state, extra = {}) {
  deps.ledger.append({ kind: 'state', run, state, ...extra });
  deps.afterState?.(run, state);
}

async function gradeSaved({ deps, task, patchPath }) {
  const containers = deps.stack.graderContainers();
  const hasPatch = fs.existsSync(patchPath) && fs.statSync(patchPath).size > 0;
  const work = path.join(deps.home, 'work');
  fs.mkdirSync(work, { recursive: true });
  const io = deps.stack.graderIO?.();
  const grade = () => gradePatch({ containers, task, patchFile: hasPatch ? patchPath : null, workRoot: work, ...(io ? { io } : {}) });
  const net = task.verifierNetwork;
  await deps.pullGraderImage?.(task);
  // The grader has no route out; a verifier that needs the network gets it for the window of its grading, alone on the grader.
  if (net === 'public' || net === 'default') return deps.stack.graderWindow(grade);
  return deps.stack.graderShared ? deps.stack.graderShared(grade) : grade();
}

/** The size of the saved patch, so a later regrade can tell that it is the one that was graded. */
const patchBytes = (file) => {
  try {
    return fs.statSync(file).size;
  } catch {
    return null;
  }
};

function resultOf({ grade, exit, wallMs, selfReported, patchPath, notes, meter }) {
  return {
    pass: grade.pass,
    grades: grade.attempts.map((a) => (a.pass ? 1 : 0)),
    flaky: grade.flaky,
    invalid: grade.invalid,
    exit,
    wallMs,
    ...(selfReported ? { selfReported } : {}),
    ...(meter ? { meter } : {}),
    suspect: grade.suspect,
    removed: grade.removed,
    patch: patchPath,
    patchBytes: patchBytes(patchPath),
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
  const paths = { run: path.join(deps.home, 'runs', dirName), scratch: path.join(deps.home, 'scratch', dirName) };
  fs.mkdirSync(paths.run, { recursive: true });
  fs.mkdirSync(paths.scratch, { recursive: true });
  const containers = stack.containers(spec.slot);
  const opened = { meter: false };
  let ctx = null;
  const infra = async (reason, message, { interrupted }) => {
    await agent.cleanup(ctx ?? { run, attempt, slot: spec.slot, stack, containers, paths }).catch(() => {});
    if (opened.meter) meter.closeRun(run);
    else book.close(run);
    if (interrupted) stateRecord(deps, run, 'Interrupted', { reason });
    stateRecord(deps, run, 'Failed_infra', { reason });
    return { state: 'Failed_infra', reason, error: message };
  };

  try {
    stateRecord(deps, run, 'Preparing');
    // What an earlier run or a killed harness left in the slot's service is removed first, and checked.
    await containers.reapAll();
    await deps.pullImage?.(spec.slot, task);
    await stack.resetWork(spec.slot);
    const base = await (deps.prepareBase ?? prepareBase)({ containers, stack, slot: spec.slot, task, hostRoot: path.join(paths.scratch, 'base') });
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
      meter: { token, baseUrl: `http://${stack.meterHost(spec.slot)}:8090/r/${token}/v1`, refusal: () => meter.status(run)?.refusal ?? null },
      paths,
      limits: config.limits,
      contextWindow: config.contextWindow,
      log: deps.log ?? (() => {}),
    };
    await agent.prepare(ctx);
  } catch (e) {
    if (e.name === 'HarnessCrash') throw e;
    return infra(e.reason ?? 'prepare', e.message, { interrupted: false });
  }

  let ran;
  try {
    stateRecord(deps, run, 'Running');
    ran = await agent.run(ctx);
  } catch (e) {
    if (e.name === 'HarnessCrash') throw e;
    return infra(e.reason ?? 'agent_error', e.message, { interrupted: true });
  }
  if (ran.exit === 'infra') return infra('infra', 'the agent could not be driven', { interrupted: true });

  // The agent has ended or been cancelled. It and everything it started are stopped, and that is confirmed, before
  // any file of its tree is read: a tree that is still changing is not a result.
  try {
    await agent.stop?.(ctx);
  } catch (e) {
    if (e.name === 'HarnessCrash') throw e;
    return infra(e.reason ?? 'not-stopped', e.message, { interrupted: true });
  }
  if (ran.exit === 'stopped_by_cap') {
    await agent.cleanup(ctx).catch(() => {});
    meter.closeRun(run);
    stateRecord(deps, run, 'StoppedByCap');
    return { state: 'StoppedByCap' };
  }
  // Infrastructure is told apart from the agent by the meter's own record.
  const stats = callStats(ledger.records, run, attempt);
  const verdict = classifyRun({ exit: ran.exit, stats, externalFailure: ran.externalFailure });
  if (verdict) return infra(`infra:${verdict.reason}`, `classified as infrastructure: ${verdict.reason}`, { interrupted: true });

  meter.closeRun(run);
  stateRecord(deps, run, 'Collecting');
  let collected;
  try {
    collected = await agent.collect(ctx);
  } catch (e) {
    if (e.name === 'HarnessCrash') throw e;
    await agent.cleanup(ctx).catch(() => {});
    stateRecord(deps, run, 'Interrupted', { reason: 'collect' });
    stateRecord(deps, run, 'Failed_infra', { reason: 'collect' });
    return { state: 'Failed_infra', error: e.message };
  }
  const patchPath = path.join(paths.run, 'model.patch');
  fs.writeFileSync(patchPath, collected.patch);
  // What was refused (entries of the tree, files of the output) and the limits the run had are part of its record.
  const notes = { ...(collected.notes ?? {}) };
  const refused = [...(notes.refused ?? []), ...(ran.record?.outputRefused ?? []).map((name) => ({ name, reason: 'a log that is not a plain file' }))];
  if (refused.length > 0) notes.refused = refused;
  else delete notes.refused;
  if (ran.record?.limits) notes.limits = ran.record.limits;
  const meterNote = { settled: stats.settled, upstreamErrors: stats.upstreamErrors, refused: stats.refused };
  fs.writeFileSync(path.join(paths.run, 'meta.json'), `${JSON.stringify({ exit: ran.exit, wallMs: ran.wallMs, selfReported: ran.selfReported ?? null, notes, meter: meterNote }, null, 2)}\n`);
  await agent.cleanup(ctx).catch(() => {});

  stateRecord(deps, run, 'Grading');
  const grade = await gradeSaved({ deps, task, patchPath });
  stateRecord(deps, run, 'Done', { result: resultOf({ grade, exit: ran.exit, wallMs: ran.wallMs, selfReported: ran.selfReported, patchPath, notes, meter: meterNote }) });
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
  const grade = await gradeSaved({ deps, task: spec.task, patchPath });
  stateRecord(deps, spec.run, 'Done', { result: resultOf({ grade, exit: meta.exit ?? 'unknown', wallMs: meta.wallMs, selfReported: meta.selfReported, patchPath, notes: meta.notes, meter: meta.meter }) });
  return { state: 'Done', pass: grade.pass };
}

/** Grade again a Done run whose grade is invalid (the grader failed, not the agent), from its saved patch. */
export async function regradeInvalid({ deps, spec }) {
  const view = deps.ledger.fold().get(spec.run);
  if (view?.state !== 'Done' || view.result?.invalid !== true) return { run: spec.run, state: view?.state, skipped: true };
  // A patch that is gone or changed would be graded as "no patch" and turn an invalid grade into a valid fail: left invalid.
  const file = path.join(deps.home, 'runs', runDirName(spec.run, view.attempt), 'model.patch');
  const bytes = patchBytes(file);
  if (bytes === null || (Number.isSafeInteger(view.result.patchBytes) && bytes !== view.result.patchBytes)) {
    return { run: spec.run, state: 'Done', skipped: true, reason: 'the saved patch is missing or is not the one that was graded' };
  }
  stateRecord(deps, spec.run, 'Grading', { regrade: true });
  return { run: spec.run, ...(await regradeRun({ deps, spec, attempt: view.attempt })) };
}

/**
 * Bring the ledger to where a restart leaves it: a run that was in flight is interrupted (its reservation is free, its
 * spend stays), a retry is planned again, a Grading run re-enters Grading. Then every named slot is reaped, because a
 * killed harness may have left a coder and the containers of a run in a service.
 */
export async function resumeLedger({ deps, slots }) {
  const folded = deps.ledger.fold();
  const actions = resumeActions(folded);
  for (const rec of resumeRecords(actions, folded)) {
    stateRecord(deps, rec.run, rec.state, Object.fromEntries(Object.entries(rec).filter(([k]) => !['kind', 'run', 'state'].includes(k))));
    if (rec.state === 'Interrupted') deps.book.close(rec.run);
  }
  for (const slot of slots) await deps.stack.containers(slot).reapAll();
  return actions;
}

/** Take one spec as far as the ledger says it should go: plan it if new, run it if Planned, grade it if Grading. */
export async function advance({ deps, spec }) {
  let view = deps.ledger.fold().get(spec.run);
  if (!view) {
    stateRecord(deps, spec.run, 'Planned', {
      task: spec.task.id,
      agent: spec.agent,
      arm: spec.arm,
      block: spec.block ?? null,
      source: spec.source ?? spec.task.source ?? null,
      language: spec.language ?? spec.task.language ?? null,
      ...(deps.digest ? { digest: deps.digest } : {}),
      ...(spec.phase ? { phase: spec.phase } : {}),
    });
    view = deps.ledger.fold().get(spec.run);
  }
  if (view.state === 'Grading') return { run: spec.run, ...(await regradeRun({ deps, spec, attempt: view.attempt })) };
  if (view.state === 'Planned') return { run: spec.run, ...(await executeRun({ deps, spec, attempt: view.attempt })) };
  return { run: spec.run, state: view.state, skipped: true };
}

/**
 * Bring the ledger to where a restart leaves it, then run what is left, one at a time, in the order of `plan`.
 * `plan` is a list of { run, task, agent, arm, slot, block?, source?, language? }.
 */
export async function runPlan({ deps, plan }) {
  await resumeLedger({ deps, slots: new Set(plan.map((s) => s.slot)) });
  const outcomes = [];
  for (const spec of plan) outcomes.push(await advance({ deps, spec }));
  return outcomes;
}

/** A book that knows what the ledger already holds. */
export const bookFromLedger = (ledger, capMicro) => CapBook.fromFold(capMicro, ledger.fold());
