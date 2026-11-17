// `bench pilot | confirm | run | regrade`. See bench/README.md, "Pilot and run".
//
// The pilot is the first blocks of the plan, run with the configuration's per-run budget (its runs have the prefix `pilot.` and
// the phase `pilot`). Its finished runs give a budget for each arm and a projection, written to `$BENCH_HOME/pilot-budget.json`;
// `bench confirm` is the owner's yes; `bench run` refuses to start without it and runs every block again under the confirmed
// budget. The pilot's results are reported apart and are never in the rates. Every run of the ledger carries the digest of the
// configuration it was made under, and a command refuses a ledger of another one.
//
// A command takes `make`, which opens what it needs (the stack, the meter, the ledger, the tasks) and returns
// { deps, tasks, slots, close }. The real one is `realHarness`; the tests pass a fake that uses the same pieces on a scripted model.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { AdamAgent } from './agents/adam.mjs';
import { OpenCodeAgent } from './agents/opencode.mjs';
import { MIN_PER_ARM, BudgetError, assertLedgerBound, configFingerprint, confirmProposal, digestKey, pilotEstimate, proposalBody, renderEstimate, requireConfirmed } from './budget.mjs';
import { CapBook } from './cap.mjs';
import { ConfigError, assertOutsideRepo, benchHome, loadConfig } from './config.mjs';
import { PodmanRemote } from './containers.mjs';
import { formatUSD } from './cost.mjs';
import { ensureImage, once } from './images.mjs';
import { Ledger, readLedger } from './ledger.mjs';
import { createMeter } from './meter.mjs';
import { buildBlocks, specsOf } from './plan.mjs';
import { PINS, coderRef } from './pins.mjs';
import { readSharedPrompt } from './shared-prompt.mjs';
import { buildReport } from './report.mjs';
import { advance, regradeInvalid } from './runner.mjs';
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
const ledgerFile = (home) => path.join(home, 'ledger.jsonl');
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

/**
 * The real harness: the stack, the meter, the ledger and the two drivers. Needs the container services; unverified here.
 * Whatever fails after the meter listens, or after the stack starts, closes the meter and brings the stack down.
 */
export async function realHarness({ env, config, log, sharedPrompt, inject = {} }) {
  if (typeof sharedPrompt !== 'string' || sharedPrompt === '') throw new Error('the shared system prompt is required: both agents are given the same one');
  const { load = loadSelected, makeStack = (o) => new Stack(o), makeMeter = createMeter, makeOrigin = () => new PodmanRemote(config.tasks.podman) } = inject;
  const home = benchHome(env);
  assertOutsideRepo(home, 'BENCH_HOME');
  if (config.concurrency > SLOTS) throw new ConfigError('concurrency', `the stack has ${SLOTS} slots`);
  fs.mkdirSync(home, { recursive: true, mode: 0o755 });
  const p = paths(env);
  const { tasks } = load(config, p);
  const ledger = Ledger.open(ledgerFile(home));
  let meter = null;
  let stack = null;
  try {
    const book = CapBook.fromFold(config.capMicro, ledger.fold());
    meter = makeMeter({ config, ledger, book, env });
    await meter.listen(Number(env.BENCH_METER_PORT ?? 18090), env.BENCH_METER_BIND ?? '127.0.0.1');
    stack = makeStack({
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

    // A task image is made present on a slot's service once, and on the grader once, with the internet only for the window of a
    // pull. `once` keeps the in-flight promise, so two slots never pull the same image twice. An own task's image was built where
    // `bench tasks build` ran: it is saved from there into an archive the harness makes itself and loaded into the service.
    const first = once();
    const origin = makeOrigin();
    const archives = path.join(home, 'archives');
    const archiveOf = (ref) => path.join(archives, `${ref.replace(/[^A-Za-z0-9._-]/g, '_')}.tar`);
    const putOwn = (ref, target, service, window) =>
      ensureImage({
        ref,
        source: { hasImage: (r) => origin.hasImage(r), save: (r, f) => first(`save|${r}`, () => (fs.mkdirSync(archives, { recursive: true }), origin.save(r, f))) },
        target,
        archive: archiveOf(ref),
        load: (file) => window(() => stack.loadImage(service, file)),
      });
    const pullImage = (slot, task) =>
      first(`slot${slot}|${task.image.ref}`, () =>
        task.source === 'own'
          ? putOwn(task.image.ref, stack.containers(slot), `podman-${slot}`, (fn) => fn())
          : stack.pullWindow(slot, () => stack.containers(slot).pull(task.image.ref)),
      );
    const pullGraderImage = (task) =>
      first(`grader|${task.image.ref}`, () =>
        task.source === 'own' ? putOwn(task.image.ref, stack.graderContainers(), 'grader', (fn) => fn()) : stack.graderWindow(() => stack.graderContainers().pull(task.image.ref)),
      );
    // Before any money is spent: every own image is on every slot and on the grader.
    for (const task of tasks.filter((t) => t.source === 'own')) {
      for (let slot = 1; slot <= slots; slot++) await pullImage(slot, task);
      await pullGraderImage(task);
    }

    const deps = {
      ledger,
      book,
      meter,
      stack,
      config,
      home,
      pullImage,
      pullGraderImage,
      log,
      agents: {
        adam: new AdamAgent({ pins: PINS, home, repoDir: REPO_DIR, contextWindow: config.contextWindow }),
        opencode: new OpenCodeAgent({ pins: PINS, home, contextWindow: config.contextWindow, sharedPrompt }),
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
    await meter?.close().catch(() => {});
    await stack?.down().catch(() => {});
    ledger.close();
    throw e;
  }
}

const plans = (harness, config) => {
  const blocks = buildBlocks({ tasks: harness.tasks, agents: AGENTS, arms: Object.keys(config.arms), seed: config.tasks.seed });
  const opts = { selfRepo: config.tasks.selfRepo };
  return { blocks, specs: specsOf(blocks, opts), pilotSpecs: (n) => specsOf(blocks.slice(0, n), { ...opts, pilot: true }), allPilotSpecs: specsOf(blocks, { ...opts, pilot: true }) };
};

const setupRecord = (config, ctx) => ({
  kind: 'setup',
  modelLabel: config.modelLabel,
  seed: config.tasks.seed,
  digest: ctx.fingerprint.digest,
  fields: ctx.fingerprint.fields,
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
  const least = Math.ceil(MIN_PER_ARM / AGENTS.length);
  if (!Number.isInteger(count) || count < least || count > 100) {
    throw new SelectionError(`--blocks is a whole number from ${least} to 100: a block gives ${AGENTS.length} runs for each arm and an estimate needs ${MIN_PER_ARM}`);
  }
  const cells = AGENTS.length * Object.keys(ctx.config.arms).length;
  const blockMicro = cells * ctx.config.perRunBudgetMicro;
  if (blockMicro > ctx.config.capMicro) throw new BudgetError(`the cap (${formatUSD(ctx.config.capMicro)}) cannot hold one block of ${cells} runs at ${formatUSD(ctx.config.perRunBudgetMicro)} each: raise the cap or lower perRunBudgetUSD`);
  const file = proposalFile(ctx.home);
  fs.rmSync(file, { force: true }); // a new pilot invalidates any earlier confirmation
  const harness = await ctx.make({ env: ctx.env, config: ctx.config, log: () => {}, sharedPrompt: ctx.sharedPrompt });
  try {
    const plan = plans(harness, ctx.config);
    if (count > plan.blocks.length) throw new SelectionError(`--blocks ${count} is more than the ${plan.blocks.length} selected tasks`);
    const pilotSpecs = plan.pilotSpecs(count);
    const worst = pilotSpecs.length * ctx.config.perRunBudgetMicro;
    harness.deps.digest = ctx.fingerprint.digest;
    harness.deps.ledger.append(setupRecord(ctx.config, ctx));
    ctx.out(`pilot: ${count} block(s), ${pilotSpecs.length} runs, ${harness.slots} slot(s), ${formatUSD(ctx.config.perRunBudgetMicro)} reserved for each run`);
    if (worst > ctx.config.capMicro) ctx.out(`warning: if every pilot run used its whole reservation it would cost ${formatUSD(worst)}, more than the cap ${formatUSD(ctx.config.capMicro)}; the cap holds, and blocks may stop early`);
    delete harness.deps.reserveMicro;
    await runSchedule({ deps: harness.deps, specs: pilotSpecs, slots: harness.slots, onOutcome: progress(ctx.out) });
    const estimate = pilotEstimate({
      folded: harness.deps.ledger.fold(),
      pilotRuns: new Set(pilotSpecs.map((s) => s.run)),
      arms: Object.keys(ctx.config.arms),
      totalTasks: plan.blocks.length,
      agents: AGENTS.length,
      capMicro: ctx.config.capMicro,
    });
    ctx.out(renderEstimate(estimate));
    fs.writeFileSync(file, `${JSON.stringify(proposalBody({ estimate, fingerprint: ctx.fingerprint, pilotBlocks: count }), null, 2)}\n`);
    const flags = [...(estimate.projection.fitsCap ? [] : ['--over-cap']), ...(estimate.censoredArms.length > 0 ? ['--censored'] : [])];
    ctx.out(`written: ${file}\nread it, then: bench confirm${flags.length ? ` ${flags.join(' ')}` : ''}`);
    return 0;
  } finally {
    await harness.close();
  }
}

async function cmdConfirm(argv, ctx) {
  const { values } = parseArgs({ args: argv, options: { 'over-cap': { type: 'boolean' }, censored: { type: 'boolean' } }, allowPositionals: false });
  const file = proposalFile(ctx.home);
  const proposal = readJson(file, null);
  if (!proposal) throw new BudgetError('no pilot budget to confirm: run `bench pilot` first');
  fs.writeFileSync(file, `${JSON.stringify(confirmProposal(proposal, { overCap: values['over-cap'] === true, censored: values.censored === true }), null, 2)}\n`);
  ctx.out(`confirmed: ${Object.entries(proposal.perArmMicro).map(([a, m]) => `${a} ${formatUSD(m)}`).join(', ')} per run`);
  return 0;
}

async function cmdRun(argv, ctx) {
  parseArgs({ args: argv, options: {}, allowPositionals: false });
  // Nothing is started, not even the stack, before the owner's confirmation is checked.
  const perArm = requireConfirmed(readJson(proposalFile(ctx.home), null), { config: ctx.config, fingerprint: ctx.fingerprint });
  const harness = await ctx.make({ env: ctx.env, config: ctx.config, log: () => {}, sharedPrompt: ctx.sharedPrompt });
  try {
    const { blocks, specs } = plans(harness, ctx.config);
    const { ledger } = harness.deps;
    ledger.append(setupRecord(ctx.config, ctx));
    harness.deps.digest = ctx.fingerprint.digest;
    harness.deps.reserveMicro = (spec) => perArm[spec.arm];
    ctx.out(`run: ${blocks.length} block(s), ${specs.length} runs, ${harness.slots} slot(s), budget per run ${Object.entries(perArm).map(([a, m]) => `${a} ${formatUSD(m)}`).join(', ')}`);
    const result = await runSchedule({ deps: harness.deps, specs, slots: harness.slots, onOutcome: progress(ctx.out) });
    const folded = ledger.fold();
    const states = {};
    for (const s of specs) {
      const st = folded.get(s.run)?.state ?? 'not started';
      states[st] = (states[st] ?? 0) + 1;
    }
    ctx.out(`ended: ${Object.entries(states).map(([k, n]) => `${k} ${n}`).join(', ')}${result.capHit ? '; stopped at the cap, the blocks that did not fit were not started (raise the cap and run again to continue)' : ''}`);
    const reportFile = path.join(ctx.home, 'report.md');
    fs.writeFileSync(reportFile, buildReport(ledger.records, { modelLabel: ctx.config.modelLabel, capMicro: ctx.config.capMicro }));
    ctx.out(reportFile);
    return 0;
  } finally {
    await harness.close();
  }
}

/** Grade again the runs a regrade left in `Grading` (a kill, or a grader that failed again): from the saved patch, the agent is never run. */
export async function regradeStranded({ deps, specs }) {
  const folded = deps.ledger.fold();
  const out = [];
  for (const spec of specs) if (folded.get(spec.run)?.state === 'Grading') out.push(await advance({ deps, spec: { ...spec, slot: 1 } }));
  return out;
}

async function cmdRegrade(argv, ctx) {
  parseArgs({ args: argv, options: {}, allowPositionals: false });
  const harness = await ctx.make({ env: ctx.env, config: ctx.config, log: () => {}, sharedPrompt: ctx.sharedPrompt });
  try {
    const plan = plans(harness, ctx.config);
    const specs = [...plan.allPilotSpecs, ...plan.specs];
    harness.deps.digest = ctx.fingerprint.digest;
    const stranded = await regradeStranded({ deps: harness.deps, specs });
    for (const o of stranded) progress(ctx.out)(o);
    const folded = harness.deps.ledger.fold();
    const todo = specs.filter((s) => folded.get(s.run)?.state === 'Done' && folded.get(s.run).result?.invalid === true);
    ctx.out(`regrade: ${stranded.length} run(s) left in Grading, ${todo.length} run(s) whose grading failed in the infrastructure`);
    for (const spec of todo) {
      const o = await regradeInvalid({ deps: harness.deps, spec });
      progress(ctx.out)(o);
      if (o.reason) ctx.out(`  ${o.reason}`);
    }
    return 0;
  } finally {
    await harness.close();
  }
}

/** Run one of the four commands. Returns an exit code. `make` opens the harness (tests pass a fake). */
export async function runCli(command, argv, { env = process.env, out = (l) => process.stdout.write(`${l}\n`), err = (l) => process.stderr.write(`${l}\n`), make = realHarness, pins = PINS, lockTexts = null, sharedPrompt = null } = {}) {
  try {
    const home = benchHome(env);
    assertOutsideRepo(home, 'BENCH_HOME');
    fs.mkdirSync(home, { recursive: true, mode: 0o755 });
    const ctx = { env, home, out, make, pins, config: loadConfig({ env }) };
    ctx.lockTexts = lockTexts ?? readLocks(paths(env));
    ctx.sharedPrompt = sharedPrompt ?? (await readSharedPrompt({ repoDir: REPO_DIR, revision: pins.coder.revision }));
    ctx.fingerprint = configFingerprint({ config: ctx.config, pins, lockTexts: ctx.lockTexts, key: digestKey(home), sharedPrompt: ctx.sharedPrompt });
    // The ledger holds runs of one configuration only; nothing here pools two.
    assertLedgerBound(readLedger(ledgerFile(home)).records, ctx.fingerprint);
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
