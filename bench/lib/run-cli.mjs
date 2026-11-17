// `bench pilot | confirm | run | regrade`. See bench/README.md, "Pilot and run".
//
// The pilot is the first blocks of the full plan, run with the configuration's per-run budget. Its finished runs are in
// the same ledger as the full run's, so the full run does not repeat them. `bench pilot` ends with a projection and a
// file, `$BENCH_HOME/pilot-budget.json`; `bench confirm` is the owner's yes; `bench run` refuses to start without it.
//
// A command takes `make`, which opens what it needs (the stack, the meter, the ledger, the tasks) and returns
// { deps, tasks, slots, close }. The real one is `realHarness`; the tests pass a fake that
// uses the same pieces on a scripted model.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { AdamAgent } from './agents/adam.mjs';
import { OpenCodeAgent } from './agents/opencode.mjs';
import { BudgetError, confirmProposal, lockDigest, pilotEstimate, proposalBody, renderEstimate, requireConfirmed } from './budget.mjs';
import { CapBook } from './cap.mjs';
import { ConfigError, assertOutsideRepo, benchHome, loadConfig } from './config.mjs';
import { formatUSD } from './cost.mjs';
import { Ledger } from './ledger.mjs';
import { createMeter } from './meter.mjs';
import { buildBlocks, specsOf } from './plan.mjs';
import { PINS, coderRef } from './pins.mjs';
import { buildReport } from './report.mjs';
import { regradeInvalid } from './runner.mjs';
import { runSchedule } from './schedule.mjs';
import { SLOTS, Stack } from './stack.mjs';
import { fetchPublic, paths, readJson } from './tasks-cli.mjs';
import { listTasks } from './task.mjs';
import { loadOwnSpecs, ownChecksum, ownImageTag } from './own.mjs';
import { taskChecksum } from './upstream.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const BENCH_DIR = path.join(HERE, '..');
const REPO_DIR = path.join(BENCH_DIR, '..');
export const AGENTS = ['adam', 'opencode'];
export const PILOT_BLOCKS = 5;

export class SelectionError extends Error {
  constructor(message) {
    super(message);
    this.name = 'SelectionError';
  }
}

const proposalFile = (home) => path.join(home, 'pilot-budget.json');
const readText = (file) => {
  try {
    return fs.readFileSync(file, 'utf8');
  } catch {
    return '';
  }
};

/** The text of the two lock files, public then own; '' for one that does not exist. */
export const readLocks = (p) => [readText(path.join(p.repoTasks, 'public.lock.json')), readText(path.join(p.repoTasks, 'own.lock.json'))];

/**
 * The tasks the two lock files name, in lock order, each held to its lock: the checksum and the image digest the
 * selection saw. A task that changed since is an error, never a silent substitute.
 */
export function loadSelected(config, p) {
  const [publicLock, ownLock] = readLocks(p);
  const pins = readJson(p.pins, {});
  const out = [];
  const take = (lockText, label, loaded, checksumOf, digestOf) => {
    if (lockText === '') return;
    const lock = JSON.parse(lockText);
    const byId = new Map(loaded.map((t) => [t.id, t]));
    for (const entry of lock.tasks ?? []) {
      const t = byId.get(entry.id);
      if (!t) throw new SelectionError(`${label} task ${entry.id} is in the lock but cannot be loaded (bench tasks fetch, build, check)`);
      if (checksumOf(t) !== entry.checksum) throw new SelectionError(`${label} task ${entry.id} changed since it was selected: select again`);
      if (digestOf(t) !== entry.image) throw new SelectionError(`${label} task ${entry.id}: its image is not the one that was selected: select again`);
      out.push(t);
    }
  };
  if (publicLock !== '') {
    const up = fetchPublic(config, p);
    const { tasks } = listTasks(up.tasksRoot, { pins, requireDigest: true, allowedNetworks: config.tasks.verifierNetworks });
    take(publicLock, 'public', tasks, (t) => taskChecksum(path.relative(up.tasksRoot, t.dir), up.sums), (t) => t.image.digest);
  }
  if (ownLock !== '') {
    const { specs } = loadOwnSpecs(path.join(p.repoTasks, 'own'));
    const byId = new Map(specs.map((s) => [s.id, s]));
    const { tasks } = listTasks(p.materialized, { pins, source: 'own', allowedNetworks: config.tasks.verifierNetworks });
    take(ownLock, 'own', tasks, (t) => (byId.has(t.id) ? ownChecksum(byId.get(t.id)) : null), (t) => (byId.has(t.id) ? pins[ownImageTag(byId.get(t.id))] : null));
  }
  if (out.length === 0) throw new SelectionError('no tasks are selected: run bench tasks select');
  return { tasks: out };
}

/** The real harness: the stack, the meter, the ledger and the two drivers. Needs the container services; unverified here. */
export async function realHarness({ env, config, log }) {
  const home = benchHome(env);
  assertOutsideRepo(home, 'BENCH_HOME');
  if (config.concurrency > SLOTS) throw new ConfigError('concurrency', `the stack has ${SLOTS} slots`);
  fs.mkdirSync(home, { recursive: true, mode: 0o755 });
  const p = paths(env);
  const { tasks } = loadSelected(config, p);
  const ledger = Ledger.open(path.join(home, 'ledger.jsonl'));
  try {
    const book = CapBook.fromFold(config.capMicro, ledger.fold());
    const meter = createMeter({ config, ledger, book, env });
    await meter.listen(Number(env.BENCH_METER_PORT ?? 18090), env.BENCH_METER_BIND ?? '127.0.0.1');
    const stack = new Stack({
      composeFile: path.join(BENCH_DIR, 'compose.yaml'),
      home,
      tasksDir: path.join(BENCH_DIR, 'tasks'),
      seccomp: path.join(REPO_DIR, 'dev', 'podman', 'seccomp.json'),
      meterPort: Number(env.BENCH_METER_PORT ?? 18090),
      ports: { git: Number(env.BENCH_GIT_PORT ?? 18083), coder1: Number(env.BENCH_CODER_1_PORT ?? 18081), coder2: Number(env.BENCH_CODER_2_PORT ?? 18082) },
      baseEnv: env,
      log,
    });
    await stack.up();
    const slots = config.concurrency;
    for (let slot = 1; slot <= slots; slot++) await stack.pullWindow(slot, () => stack.containers(slot).pull(coderRef(PINS)));
    await stack.graderWindow(() => stack.graderContainers().pull(coderRef(PINS)));
    // A task image is pulled onto a slot's service once, and onto the grader once, with the internet only for that window.
    // An own task's image was built where `bench tasks build` ran; it is not pulled (see README, "Unverified").
    const pulled = new Set();
    const pullImage = async (slot, task) => {
      if (task.source === 'own') return;
      const key = `${slot}|${task.image.ref}`;
      if (!pulled.has(key)) {
        await stack.pullWindow(slot, () => stack.containers(slot).pull(task.image.ref));
        pulled.add(key);
      }
      if (!pulled.has(`grader|${task.image.ref}`)) {
        await stack.graderWindow(() => stack.graderContainers().pull(task.image.ref));
        pulled.add(`grader|${task.image.ref}`);
      }
    };
    const deps = {
      ledger,
      book,
      meter,
      stack,
      config,
      home,
      pullImage,
      log,
      agents: {
        adam: new AdamAgent({ pins: PINS, home, repoDir: REPO_DIR, contextWindow: config.contextWindow }),
        opencode: new OpenCodeAgent({ pins: PINS, home, contextWindow: config.contextWindow }),
      },
    };
    return {
      deps,
      tasks,
      slots,
      close: async () => {
        await meter.close();
        ledger.close();
      },
    };
  } catch (e) {
    ledger.close();
    throw e;
  }
}

const plan = (harness, config) => {
  const blocks = buildBlocks({ tasks: harness.tasks, agents: AGENTS, arms: Object.keys(config.arms), seed: config.tasks.seed });
  return { blocks, specs: specsOf(blocks, { selfRepo: config.tasks.selfRepo }) };
};

const setupRecord = (config, ctx) => ({
  kind: 'setup',
  modelLabel: config.modelLabel,
  seed: config.tasks.seed,
  taskLock: lockDigest(ctx.lockTexts).slice(0, 12),
  adamImage: `${ctx.pins.coder.image}:${ctx.pins.coder.tag}`,
  adamRevision: ctx.pins.coder.revision,
  opencodeVersion: ctx.pins.opencode.version,
  opencodeIntegrity: ctx.pins.opencode.integrity,
  limits: config.limits,
});

const progress = (out) => (o) => out(`${o.run}: ${o.state}${o.pass === undefined ? '' : o.pass ? ' (solved)' : ' (not solved)'}`);

async function cmdPilot(argv, ctx) {
  const { values } = parseArgs({ args: argv, options: { blocks: { type: 'string' } }, allowPositionals: false });
  const count = values.blocks === undefined ? PILOT_BLOCKS : Number(values.blocks);
  if (!Number.isInteger(count) || count < 1 || count > 100) throw new SelectionError('--blocks is a whole number from 1 to 100');
  const file = proposalFile(ctx.home);
  fs.rmSync(file, { force: true }); // a new pilot invalidates any earlier confirmation
  const harness = await ctx.make({ env: ctx.env, config: ctx.config, log: () => {} });
  try {
    const { blocks, specs } = plan(harness, ctx.config);
    const pilot = blocks.slice(0, count);
    const pilotSpecs = specs.filter((s) => s.block < pilot.length);
    harness.deps.ledger.append(setupRecord(ctx.config, ctx));
    ctx.out(`pilot: ${pilot.length} block(s), ${pilotSpecs.length} runs of ${specs.length}, ${harness.slots} slot(s), ${formatUSD(ctx.config.perRunBudgetMicro)} reserved for each run`);
    delete harness.deps.reserveMicro;
    await runSchedule({ deps: harness.deps, specs: pilotSpecs, slots: harness.slots, onOutcome: progress(ctx.out) });
    const estimate = pilotEstimate({
      folded: harness.deps.ledger.fold(),
      pilotRuns: new Set(pilotSpecs.map((s) => s.run)),
      arms: Object.keys(ctx.config.arms),
      totalTasks: blocks.length,
      agents: AGENTS.length,
      capMicro: ctx.config.capMicro,
    });
    ctx.out(renderEstimate(estimate));
    fs.writeFileSync(file, `${JSON.stringify(proposalBody({ estimate, config: ctx.config, pins: ctx.pins, lockTexts: ctx.lockTexts, pilotBlocks: pilot.length }), null, 2)}\n`);
    ctx.out(`written: ${file}\nread it, then: bench confirm${estimate.projection.fitsCap ? '' : ' --over-cap'}`);
    return 0;
  } finally {
    await harness.close();
  }
}

async function cmdConfirm(argv, ctx) {
  const { values } = parseArgs({ args: argv, options: { 'over-cap': { type: 'boolean' } }, allowPositionals: false });
  const file = proposalFile(ctx.home);
  const proposal = readJson(file, null);
  if (!proposal) throw new BudgetError('no pilot budget to confirm: run `bench pilot` first');
  fs.writeFileSync(file, `${JSON.stringify(confirmProposal(proposal, { overCap: values['over-cap'] === true }), null, 2)}\n`);
  ctx.out(`confirmed: ${Object.entries(proposal.perArmMicro).map(([a, m]) => `${a} ${formatUSD(m)}`).join(', ')} per run`);
  return 0;
}

async function cmdRun(argv, ctx) {
  parseArgs({ args: argv, options: {}, allowPositionals: false });
  // Nothing is started, not even the stack, before the owner's confirmation is checked.
  const perArm = requireConfirmed(readJson(proposalFile(ctx.home), null), { config: ctx.config, pins: ctx.pins, lockTexts: ctx.lockTexts });
  const harness = await ctx.make({ env: ctx.env, config: ctx.config, log: () => {} });
  try {
    const { blocks, specs } = plan(harness, ctx.config);
    const { ledger } = harness.deps;
    ledger.append(setupRecord(ctx.config, ctx));
    harness.deps.reserveMicro = (spec) => perArm[spec.arm];
    ctx.out(`run: ${blocks.length} block(s), ${specs.length} runs, ${harness.slots} slot(s), budget per run ${Object.entries(perArm).map(([a, m]) => `${a} ${formatUSD(m)}`).join(', ')}`);
    const result = await runSchedule({ deps: harness.deps, specs, slots: harness.slots, onOutcome: progress(ctx.out) });
    const folded = ledger.fold();
    const states = {};
    for (const s of specs) {
      const st = folded.get(s.run)?.state ?? 'not started';
      states[st] = (states[st] ?? 0) + 1;
    }
    ctx.out(`ended: ${Object.entries(states).map(([k, n]) => `${k} ${n}`).join(', ')}${result.capHit ? '; stopped at the cap, nothing new was started' : ''}`);
    const reportFile = path.join(ctx.home, 'report.md');
    fs.writeFileSync(reportFile, buildReport(ledger.records, { modelLabel: ctx.config.modelLabel, capMicro: ctx.config.capMicro }));
    ctx.out(reportFile);
    return 0;
  } finally {
    await harness.close();
  }
}

async function cmdRegrade(argv, ctx) {
  parseArgs({ args: argv, options: {}, allowPositionals: false });
  const harness = await ctx.make({ env: ctx.env, config: ctx.config, log: () => {} });
  try {
    const { specs } = plan(harness, ctx.config);
    const folded = harness.deps.ledger.fold();
    const todo = specs.filter((s) => folded.get(s.run)?.state === 'Done' && folded.get(s.run).result?.invalid === true);
    ctx.out(`regrade: ${todo.length} run(s) whose grading failed in the infrastructure`);
    for (const spec of todo) progress(ctx.out)(await regradeInvalid({ deps: harness.deps, spec }));
    return 0;
  } finally {
    await harness.close();
  }
}

/** Run one of the four commands. Returns an exit code. `make` opens the harness (tests pass a fake). */
export async function runCli(command, argv, { env = process.env, out = (l) => process.stdout.write(`${l}\n`), err = (l) => process.stderr.write(`${l}\n`), make = realHarness, pins = PINS, lockTexts = null } = {}) {
  try {
    const home = benchHome(env);
    assertOutsideRepo(home, 'BENCH_HOME');
    fs.mkdirSync(home, { recursive: true, mode: 0o755 });
    const ctx = { env, home, out, make, pins, config: loadConfig({ env }) };
    ctx.lockTexts = lockTexts ?? readLocks(paths(env));
    if (command === 'pilot') return await cmdPilot(argv, ctx);
    if (command === 'confirm') return await cmdConfirm(argv, ctx);
    if (command === 'run') return await cmdRun(argv, ctx);
    return await cmdRegrade(argv, ctx);
  } catch (e) {
    if (e instanceof ConfigError) throw e;
    if (e instanceof BudgetError || e instanceof SelectionError) {
      err(`bench: ${e.message}`);
      return 1;
    }
    if (e?.code === 'ERR_PARSE_ARGS_UNKNOWN_OPTION' || e?.code === 'ERR_PARSE_ARGS_INVALID_OPTION_VALUE' || e?.code === 'ERR_PARSE_ARGS_UNEXPECTED_POSITIONAL') {
      err(`bench: ${e.message}`);
      return 64;
    }
    throw e;
  }
}
