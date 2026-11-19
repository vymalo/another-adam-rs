// The dry run (`bench dry-run`, bench/dry-run.sh): the whole path with no model and no money. A scripted model
// stands in for the endpoint and drives both agents through the three self-test tasks (bench/tasks/self) on the
// stack of bench/compose.yaml. It proves, from the ledger (dryrun-checks.mjs):
//   1. the grades are 1, 1, 0, for both agents;
//   2. the ledger's cost is the scripted usage times the price, and the model saw exactly the settled calls;
//   3. a run over its budget ends budget_hit and is graded;
//   4. a harness killed in the middle of a run starts again and re-runs only the interrupted run.
// It needs a container service; nothing in it has run without one (bench/README.md, "Unverified").
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { AdamAgent } from './agents/adam.mjs';
import { OpenCodeAgent } from './agents/opencode.mjs';
import { CapBook } from './cap.mjs';
import { assertOutsideRepo, benchHome, normalizeConfig } from './config.mjs';
import { checkBudgetHit, checkCost, checkGrades, checkResume, report } from './dryrun-checks.mjs';
import { checkTask } from './envcheck.mjs';
import { hostGradeIO, localGit } from './grade.mjs';
import { Ledger } from './ledger.mjs';
import { createMeter } from './meter.mjs';
import { PINS, coderRef } from './pins.mjs';
import { runPlan } from './runner.mjs';
import { readSharedPrompt } from './shared-prompt.mjs';
import { createScriptedModel } from './scripted-model.mjs';
import { Stack } from './stack.mjs';
import { loadTask } from './task.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const BENCH_DIR = path.join(HERE, '..');
export const REPO_DIR = path.join(BENCH_DIR, '..');
export const SELF_IDS = ['hello', 'fix-add', 'wrong'];
const KEY_ENV = 'BENCH_MODEL_API_KEY';
const DRYRUN_KEY = 'dry-run-key-0001-not-a-secret';
const PRICES = { inputPerMTok: 3, cachedInputPerMTok: 0.3, cacheWritePerMTok: 3.75, outputPerMTok: 15 };
/** Output so dear that a call's worst case is about $32.8 and its actual cost about $0.8: a budget of $34 fits two calls. */
const DEAR = { ...PRICES, outputPerMTok: 4000 };

/** The made-up configuration of a phase: the scripted model's address, no real value anywhere. */
export function dryRunConfig({ endpoint, prices = PRICES, capUSD = 100, perRunBudgetUSD = 3 }) {
  return {
    endpoint,
    modelId: 'example-model',
    modelLabel: 'Scripted model',
    prices,
    capUSD,
    perRunBudgetUSD,
    arms: { max: { reasoning_effort: 'high' }, off: { reasoning_effort: 'none' } },
    keyEnv: KEY_ENV,
    contextWindow: 131072,
    limits: { minutes: 10, steps: 200, cancelGraceSeconds: 20 },
    tasks: { workspaceImage: `${PINS.coder.image}@${PINS.coder.digest}` },
  };
}

/** "<run>:<n>": the harness kills itself after the n-th settled call of the run. Null when unset or not that shape. */
export function parseKill(text) {
  const m = /^([A-Za-z0-9._-]{1,128}):(\d{1,3})$/.exec(text ?? '');
  return m ? { run: m[1], calls: Number(m[2]) } : null;
}

/**
 * The task directory and the image build context of a self-test task, under `home` (outside the repository).
 * The repository of the task is made here: its files and one commit, because a git directory cannot be checked in.
 */
export async function materializeSelf({ srcDir, home }) {
  const id = path.basename(srcDir);
  const taskDir = path.join(home, 'self', id);
  const context = path.join(home, 'self-context', id);
  fs.rmSync(taskDir, { recursive: true, force: true });
  fs.rmSync(context, { recursive: true, force: true });
  fs.cpSync(srcDir, taskDir, { recursive: true, filter: (src) => path.relative(srcDir, src).split(path.sep)[0] !== 'repo' });
  fs.mkdirSync(context, { recursive: true });
  const repo = path.join(context, 'repo');
  fs.cpSync(path.join(srcDir, 'repo'), repo, { recursive: true });
  const git = localGit(repo);
  await git(['init', '-q', '-b', 'main']);
  await git(['add', '-A']);
  await git(['-c', 'user.name=bench', '-c', 'user.email=bench@example.test', '-c', 'commit.gpgsign=false', 'commit', '-q', '-m', 'self test base']);
  fs.copyFileSync(path.join(srcDir, 'environment', 'Dockerfile'), path.join(context, 'Dockerfile'));
  for (const d of [taskDir, context]) fs.chmodSync(d, 0o755);
  return { id, taskDir, context, dockerfile: path.join(context, 'Dockerfile'), tag: `adam-bench-self/${id}:1` };
}

const lines = (text) => process.stdout.write(`${text}\n`);

/** Everything a phase needs: scripted model, meter, ledger, the stack up, the images built, the drivers. */
async function startHarness({ env, home, name, raw, killSpec = null }) {
  const model = createScriptedModel({ script: 'agents', expectKey: DRYRUN_KEY });
  const modelAt = await model.listen();
  const config = normalizeConfig({ ...raw, endpoint: modelAt.url });
  const dir = path.join(home, 'dryrun', name);
  fs.mkdirSync(dir, { recursive: true });
  const ledger = Ledger.open(path.join(dir, 'ledger.jsonl'));
  const book = CapBook.fromFold(config.capMicro, ledger.fold());
  let killed = 0;
  const meter = createMeter({
    config,
    ledger,
    book,
    env: { [KEY_ENV]: DRYRUN_KEY },
    log: (e) => {
      if (killSpec && e.event === 'call' && e.run === killSpec.run && ++killed >= killSpec.calls) process.kill(process.pid, 'SIGKILL');
    },
  });
  const meterPort = Number(env.BENCH_METER_PORT ?? 18090);
  await meter.listen(meterPort, env.BENCH_METER_BIND ?? '0.0.0.0');
  const stack = new Stack({
    composeFile: path.join(BENCH_DIR, 'compose.yaml'),
    home,
    meterPort,
    ports: { git: Number(env.BENCH_GIT_PORT ?? 18083), coder1: Number(env.BENCH_CODER_1_PORT ?? 18081), coder2: Number(env.BENCH_CODER_2_PORT ?? 18082) },
    baseEnv: env,
  });
  // The coder image is the base of the self-test images and the source of the coder's binaries: pull it once, before
  // compose starts the services that run it.
  await stack.containers(1).pull(coderRef(PINS));
  await stack.up();
  const containers = stack.containers(1);
  const grader = stack.graderContainers();
  const base = config.tasks.workspaceImage;
  const pins = {};
  const tasks = {};
  for (const id of SELF_IDS) {
    const m = await materializeSelf({ srcDir: path.join(BENCH_DIR, 'tasks', 'self', id), home });
    await containers.build(m.context, m.tag, m.dockerfile, { BASE_IMAGE: base });
    try {
      pins[m.tag] = await containers.digestOf(m.tag);
    } catch {
      pins[m.tag] = await containers.imageId(m.tag); // a local image may have no manifest digest yet
    }
    tasks[id] = loadTask(m.taskDir, { pins, source: 'self' });
  }
  const deps = {
    ledger,
    book,
    meter,
    stack,
    config,
    home: dir, // results of this phase; the shared home above is what the stack mounts
    agents: {
      adam: new AdamAgent({ pins: PINS, home, repoDir: REPO_DIR, contextWindow: config.contextWindow }),
      opencode: new OpenCodeAgent({ pins: PINS, home, contextWindow: config.contextWindow, sharedPrompt: await readSharedPrompt({ repoDir: REPO_DIR, revision: PINS.coder.revision }) }),
    },
  };
  const close = async () => {
    await meter.close();
    ledger.close();
    await model.close();
  };
  return { deps, close, model, config, tasks, stack, containers: grader, ledger };
}

const specs = (tasks, agents, arm = 'max') => SELF_IDS.flatMap((id) => agents.map((agent) => ({ run: `${id}-${agent}-${arm}`, task: tasks[id], agent, arm, slot: 1, source: 'self', language: 'javascript' })));

async function phaseEnvCheck(h) {
  lines('phase 0: the environment check of each self-test task (gold passes three times, the empty patch fails three times)');
  const results = [];
  const workRoot = path.join(h.deps.home, 'work');
  fs.mkdirSync(workRoot, { recursive: true });
  for (const id of SELF_IDS) {
    const r = await checkTask({ containers: h.containers, task: h.tasks[id], workRoot, io: hostGradeIO(workRoot) });
    results.push({ ok: r.ok, what: `the environment check of ${id} passes`, detail: r.reason ?? '' });
  }
  return report(results, lines);
}

async function phaseGrades(env, home) {
  lines('phase 1: both agents on the three tasks; grades, cost');
  fs.rmSync(path.join(home, 'dryrun', 'grades'), { recursive: true, force: true });
  const h = await startHarness({ env, home, name: 'grades', raw: dryRunConfig({ endpoint: 'http://127.0.0.1:1/v1' }) });
  try {
    const envOk = await phaseEnvCheck(h);
    await runPlan({ deps: h.deps, plan: specs(h.tasks, ['adam', 'opencode']) });
    const folded = h.ledger.fold();
    const runs = specs(h.tasks, ['adam', 'opencode']).map((s) => ({ run: s.run, agent: s.agent }));
    return report([...checkGrades(folded, { agents: ['adam', 'opencode'], arm: 'max' }), ...checkCost(folded, { runs, prices: h.config.prices, modelRequests: h.model.requests.length })], lines) && envOk;
  } finally {
    await h.close();
  }
}

async function phaseBudget(env, home) {
  lines('phase 2: a run over its budget ends budget_hit and is graded');
  fs.rmSync(path.join(home, 'dryrun', 'budget'), { recursive: true, force: true });
  const reserved = 34;
  const h = await startHarness({ env, home, name: 'budget', raw: dryRunConfig({ endpoint: 'http://127.0.0.1:1/v1', prices: DEAR, perRunBudgetUSD: reserved }) });
  try {
    await runPlan({ deps: h.deps, plan: specs(h.tasks, ['adam', 'opencode']).filter((s) => s.task.id === 'hello') });
    return report(checkBudgetHit(h.ledger.fold(), { runs: ['hello-adam-max', 'hello-opencode-max'], reservedMicro: reserved * 1_000_000 }), lines);
  } finally {
    await h.close();
  }
}

/** One start of the harness over the resume plan: the three tasks through OpenCode. */
async function resumeStart(env, home, killSpec) {
  const h = await startHarness({ env, home, name: 'resume', raw: dryRunConfig({ endpoint: 'http://127.0.0.1:1/v1' }), killSpec });
  try {
    await runPlan({ deps: h.deps, plan: specs(h.tasks, ['opencode']) });
  } finally {
    await h.close();
  }
}

async function phaseResume(env, home, argv0) {
  lines('phase 3: the harness is killed in the middle of the second run and started again');
  const child = (extra) =>
    new Promise((resolve) => {
      const p = spawn(process.execPath, [argv0, 'dry-run', '--phase', 'resume-child'], { env: { ...env, ...extra }, stdio: 'inherit' });
      p.on('close', (code, signal) => resolve({ code, signal }));
    });
  const first = await child({ BENCH_DRYRUN_KILL: 'fix-add-opencode-max:2' });
  const killedOk = first.signal === 'SIGKILL';
  lines(`${killedOk ? 'ok  ' : 'FAIL'} the first start was killed in the middle of fix-add (${first.signal ?? `exit ${first.code}`})`);
  const second = await child({ BENCH_DRYRUN_KILL: '' });
  lines(`${second.code === 0 ? 'ok  ' : 'FAIL'} the second start ran to its end (exit ${second.code})`);
  const ledger = Ledger.open(path.join(home, 'dryrun', 'resume', 'ledger.jsonl'));
  try {
    const results = checkResume(ledger.fold(), { finished: ['hello-opencode-max'], killed: 'fix-add-opencode-max', later: ['wrong-opencode-max'] });
    return report(results, lines) && killedOk && second.code === 0;
  } finally {
    ledger.close();
  }
}

/** `bench dry-run [--phase all|grades|budget|resume|resume-child] [--down]`. Returns the exit code. */
export async function runDryRun({ phase = 'all', down = false, env = process.env, entry }) {
  const home = benchHome(env);
  assertOutsideRepo(home, 'BENCH_HOME');
  fs.mkdirSync(home, { recursive: true, mode: 0o755 });
  if (down) {
    const stack = new Stack({ composeFile: path.join(BENCH_DIR, 'compose.yaml'), home, meterPort: Number(env.BENCH_METER_PORT ?? 18090), baseEnv: env });
    await stack.down({ volumes: true });
    return 0;
  }
  if (phase === 'resume-child') {
    await resumeStart(env, home, parseKill(env.BENCH_DRYRUN_KILL));
    return 0;
  }
  const passed = [];
  if (phase === 'all' || phase === 'grades') passed.push(await phaseGrades(env, home));
  if (phase === 'all' || phase === 'budget') passed.push(await phaseBudget(env, home));
  if (phase === 'all' || phase === 'resume') {
    fs.rmSync(path.join(home, 'dryrun', 'resume'), { recursive: true, force: true });
    passed.push(await phaseResume(env, home, entry));
  }
  lines(passed.every(Boolean) ? 'the dry run passed' : 'the dry run FAILED');
  return passed.every(Boolean) ? 0 : 1;
}
