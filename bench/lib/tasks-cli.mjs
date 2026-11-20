// `bench tasks <fetch|mine|lint|build|check|select>`. See bench/README.md.
//
// State outside the repository, under $BENCH_HOME/tasks: pins.json (image reference -> digest),
// check.json (the environment check's results), own-candidates.json (the miner's output), materialized/
// (own tasks as task directories), work/ (grading scratch). In the repository: bench/tasks/own/<id>/ (the
// owner's statements and commit ids) and the two lock files.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { ConfigError, assertOutsideRepo, benchHome, loadConfig } from './config.mjs';
import { execFile } from 'node:child_process';
import { HostContainers, ContainerError, dockerEnv } from './containers.mjs';
import { checkTask } from './envcheck.mjs';
import { verifierNetwork } from './grade.mjs';
import { findCheckCommand, lintStatement } from './lint.mjs';
import { classifyPath, gitIn, mine } from './mine.mjs';
import { OwnTaskError, loadOwnSpecs, materializeOwn, ownChecksum, ownImageTag, prepareOwnContext } from './own.mjs';
import { Quota, buildLock, lockText, pickInOrder, selectTasks, selectionOrder } from './select.mjs';
import { PINS } from './pins.mjs';
import { listTasks } from './task.mjs';
import { UpstreamError, fetchUpstream, taskChecksum } from './upstream.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
class UsageError extends Error {}

/** A positive whole number from a command-line option, or a usage error. */
function positiveInt(value, name) {
  if (!/^[1-9][0-9]{0,8}$/.test(String(value))) throw new UsageError(`${name} must be a positive whole number`);
  return Number(value);
}

export const PUBLIC_TOTAL = 80;
export const PUBLIC_PER_REPO = 9;
export const OWN_TOTAL = 20;
export const OWN_PER_REPO = 6;

const USAGE = `usage: bench tasks <command>

  fetch                      clone the upstream task repository at the pinned commit, check SHA256SUMS, list the tasks
  mine [--repo NAME] [--branch main] [--limit N]
                             candidate own tasks from the clones in the configuration -> $BENCH_HOME/tasks/own-candidates.json
  lint [ID...]               warn when an own statement names what only the gold change adds
  build [ID...]              build the image of each own task (needs the host's container runtime)
  check [--source public|own] [--only ID,ID] [--all] [--seed S] [--total N] [--keep-images]
                             gold passes three times and the empty patch fails three times at /app, the real grading path (needs the
                             host's container runtime); a kept task also runs its gold once at the agents' clone path, recorded as
                             its agentPath (pass, fail, not-run), never a reason to drop it;
                             public: walks the tasks in the order select uses and stops at the target (80, at most 9 per
                             repository), removes the image of every checked public task once its digest is
                             recorded (--keep-images keeps those of the kept tasks; run pulls them again by digest), resumes
                             from check.json; --all walks every task
  select [--seed S] [--total N] [--source public|own|both]
                             write bench/tasks/public.lock.json and own.lock.json
`;

export const readJson = (file, fallback) => {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch {
    return fallback;
  }
};
const writeJson = (file, value) => {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
};

export function paths(env = process.env) {
  const home = benchHome(env);
  assertOutsideRepo(home, 'BENCH_HOME');
  const state = path.join(home, 'tasks');
  return {
    home,
    state,
    pins: path.join(state, 'pins.json'),
    check: path.join(state, 'check.json'),
    candidates: path.join(state, 'own-candidates.json'),
    materialized: path.join(state, 'materialized'),
    contexts: path.join(state, 'contexts'),
    work: path.join(state, 'work'),
    upstream: path.join(home, 'upstream', 'swe-bench-pro-os'),
    repoTasks: env.BENCH_TASKS_DIR || path.join(HERE, '..', 'tasks'),
  };
}

const need = (config, field) => {
  const v = field.split('.').reduce((o, k) => o?.[k], config.tasks);
  if (v === null || v === undefined || (typeof v === 'object' && Object.keys(v).length === 0)) throw new ConfigError(`tasks.${field}`, 'is needed by this command; set it in the local configuration');
  return v;
};

export function fetchPublic(config, p) {
  const up = fetchUpstream({ url: config.tasks.upstreamUrl, commit: need(config, 'upstreamCommit'), dir: p.upstream, sumsDir: config.tasks.upstreamDir });
  return up;
}

async function cmdFetch(config, p, out) {
  const up = fetchPublic(config, p);
  const pins = readJson(p.pins, {});
  const { tasks, errors } = listTasks(up.tasksRoot, { pins, requireDigest: false });
  const unpinned = tasks.filter((t) => !t.image.digest).length;
  out(`upstream at ${up.head}: SHA256SUMS ok, ${tasks.length} task(s), ${unpinned} without a pinned image digest (bench tasks check pins them), ${errors.length} unreadable`);
  for (const e of errors.slice(0, 20)) out(`  ${e.id}: ${e.problems.join('; ')}`);
  for (const t of tasks) out(`${t.id}\t${t.repo}\t${t.language}`);
  return 0;
}

async function cmdMine(config, p, argv, out) {
  const { values } = parseArgs({ args: argv, options: { repo: { type: 'string' }, branch: { type: 'string' }, limit: { type: 'string' } } });
  const limit = values.limit ? positiveInt(values.limit, '--limit') : Infinity;
  const clones = need(config, 'clones');
  const names = values.repo ? [values.repo] : Object.keys(clones).sort();
  const all = [];
  for (const name of names) {
    if (!clones[name]) throw new ConfigError(`tasks.clones.${name}`, 'is not in the local configuration');
    const { candidates, refused } = mine({ dir: clones[name], repoName: name, branch: values.branch ?? 'main', limit });
    out(`${name}: ${candidates.length} candidate(s); refused: ${Object.entries(refused).map(([k, n]) => `${k} ${n}`).join(', ') || 'none'}`);
    all.push(...candidates);
  }
  writeJson(p.candidates, { schema: 1, candidates: all });
  out(`${p.candidates}`);
  out('Write bench/tasks/own/<id>/instruction.md and own.json for the ones you keep (bench/tasks/own/README.md).');
  return 0;
}

function goldSourceDiff(spec, config) {
  const clone = config.tasks.clones[spec.clone];
  if (!clone) throw new ConfigError(`tasks.clones.${spec.clone}`, 'is not in the local configuration');
  const git = gitIn(clone);
  const files = git(['diff', '--name-only', '-z', '--no-renames', spec.base, spec.commit]).split('\0').filter(Boolean).filter((f) => classifyPath(f) !== 'test');
  return git(['diff', '--no-renames', '--no-color', spec.base, spec.commit, '--', ...files.map((f) => `:(literal)${f}`)]);
}

async function cmdLint(config, p, argv, out) {
  const { specs, errors } = loadOwnSpecs(path.join(p.repoTasks, 'own'));
  for (const e of errors) out(`${e.id}: ${e.problems.join('; ')}`);
  const wanted = argv.length ? specs.filter((s) => argv.includes(s.id)) : specs;
  let warned = 0;
  let refused = 0;
  for (const spec of wanted) {
    const clone = config.tasks.clones[spec.clone];
    if (clone) {
      const git = gitIn(clone);
      const found = findCheckCommand((file) => {
        try {
          return git(['show', `${spec.base}:${file}`]);
        } catch {
          return null;
        }
      });
      if (!found) {
        out(`${spec.id}: refused: its repository at the base has no test command a coder would find (a test script, a Cargo.toml, a go.mod, a test target, a test configuration, or one named in the README). Without one the agent has nothing to run and ends "completed" whatever it wrote; add one to the repository or drop the task`);
        refused++;
      }
    }
    const statement = fs.readFileSync(path.join(spec.dir, 'instruction.md'), 'utf8');
    for (const w of lintStatement(statement, goldSourceDiff(spec, config))) {
      out(`${spec.id}: warning: the statement names the ${w.kind} ${JSON.stringify(w.token)}, which only the gold change adds (an "Interface" section may name what the tests call)`);
      warned++;
    }
  }
  out(`${wanted.length} statement(s) read, ${warned} warning(s), ${refused} refused`);
  return refused > 0 ? 1 : 0;
}

const containersOf = (config, make) => (make ? make(config) : new HostContainers());

const STACK_PROJECT = 'adam-bench';

const realDocker = (args) =>
  new Promise((resolve) => {
    execFile('docker', args, { env: dockerEnv(), encoding: 'utf8', timeout: 60_000 }, (err, stdout, stderr) => resolve({ exitCode: err ? (typeof err.code === 'number' ? err.code : 125) : 0, stdout: stdout ?? '', stderr: stderr ?? '' }));
  });

/**
 * The networks a check's gradings join, named as the stack names them (`adam-bench_verifier`, internal; `adam-bench_egress`, the
 * internet), so a check grades exactly as a run does. One that is not there is made the way `Stack.up` makes it and removed by
 * `cleanup`; one that is there (the stack is up) is used and left. A verifier that exists without the internal flag is refused.
 * A network the owner named (`tasks.verifierNetworks`) is never made: it must exist.
 */
export async function checkNetworks({ docker, tasks }) {
  const wanted = [...new Set(tasks.map((t) => verifierNetwork(t)).filter((n) => n !== 'none'))];
  const aliases = {};
  const made = [];
  const cleanup = async () => {
    for (const name of made.splice(0)) await docker(['network', 'rm', name]).catch(() => {});
  };
  try {
    for (const logical of wanted) {
      const named = logical === 'verifier' || logical === 'egress';
      const full = named ? `${STACK_PROJECT}_${logical}` : logical;
      if (named) aliases[logical] = full;
      const inspect = await docker(['network', 'inspect', '--format', '{{.Internal}}', full]);
      if (inspect.exitCode === 0) {
        if (logical === 'verifier' && inspect.stdout.trim() !== 'true') throw new ContainerError(`the ${full} network exists and is not internal: remove it (docker network rm ${full}) and run the check again`);
        continue;
      }
      if (!named) throw new ContainerError(`the verifier network ${logical} does not exist: create it (docker network create --internal ${logical}) or start the stack`);
      const created = await docker(['network', 'create', ...(logical === 'verifier' ? ['--internal'] : []), full]);
      if (created.exitCode !== 0) throw new ContainerError(`the ${full} network could not be made (exit ${created.exitCode}): ${created.stderr.trim()}`);
      made.push(full);
    }
  } catch (e) {
    await cleanup();
    throw e;
  }
  return { aliases, cleanup };
}

async function cmdBuild(config, p, argv, out, make) {
  const containers = containersOf(config, make);
  const workspaceImage = need(config, 'workspaceImage');
  const { specs } = loadOwnSpecs(path.join(p.repoTasks, 'own'));
  const pins = readJson(p.pins, {});
  for (const spec of argv.length ? specs.filter((s) => argv.includes(s.id)) : specs) {
    const clone = config.tasks.clones[spec.clone];
    if (!clone) throw new ConfigError(`tasks.clones.${spec.clone}`, 'is not in the local configuration');
    const dir = path.join(p.materialized, spec.id);
    const { tag } = materializeOwn({ spec, cloneDir: clone, outDir: dir, workspaceImage });
    const context = path.join(p.contexts, spec.id);
    prepareOwnContext({ cloneDir: clone, base: spec.base, outDir: path.join(context, 'repo') });
    fs.copyFileSync(path.join(dir, 'environment', 'Dockerfile'), path.join(context, 'Dockerfile'));
    await containers.build(context, tag, path.join(context, 'Dockerfile'));
    pins[tag] = await containers.digestOf(tag);
    writeJson(p.pins, pins);
    out(`${spec.id}: built ${tag} ${pins[tag]}`);
  }
  return 0;
}

/**
 * A recorded verdict stands for the task it was made for, unless it was an infra failure (which says nothing about the task).
 * A verdict with no `agentPath` was made before the /app rule (the clone path then decided, and the empty patch ran only
 * there), so it is not the new rule's data and the task is checked again.
 */
const reusable = (r, checksum) => r && typeof r.ok === 'boolean' && r.reason !== 'infra' && typeof r.agentPath === 'string' && typeof checksum === 'string' && r.checksum === checksum;
const keptVerdict = (r) => r.ok === true && r.devcontainer === false;
export const digestOfRef = (ref) => /@(sha256:[0-9a-f]{64})$/.exec(String(ref ?? ''))?.[1] ?? null;

async function cmdCheck(config, p, argv, out, make, docker) {
  const { values } = parseArgs({ args: argv, options: { source: { type: 'string', default: 'public' }, only: { type: 'string' }, all: { type: 'boolean', default: false }, seed: { type: 'string' }, total: { type: 'string' }, 'keep-images': { type: 'boolean', default: false } } });
  if (values.source !== 'public' && values.source !== 'own') throw new ConfigError('--source', 'is public or own');
  const pins = readJson(p.pins, {});
  const results = readJson(p.check, {});
  let tasks;
  let checksumOf;
  if (values.source === 'public') {
    const up = fetchPublic(config, p);
    ({ tasks } = listTasks(up.tasksRoot, { pins, requireDigest: false, allowedNetworks: config.tasks.verifierNetworks }));
    checksumOf = (t) => taskChecksum(path.relative(up.tasksRoot, t.dir), up.sums);
  } else {
    const { specs } = loadOwnSpecs(path.join(p.repoTasks, 'own'));
    ({ tasks } = listTasks(p.materialized, { pins, source: 'own', allowedNetworks: config.tasks.verifierNetworks }));
    const built = new Set(tasks.map((t) => t.id));
    for (const s of specs) if (!built.has(s.id)) out(`${s.id}: not built, or its image is not pinned: run bench tasks build ${s.id}`);
    const byId = new Map(specs.map((s) => [s.id, s]));
    checksumOf = (t) => (byId.has(t.id) ? ownChecksum(byId.get(t.id)) : null);
  }
  const only = values.only ? new Set(values.only.split(',')) : null;
  // The public walk: the order of `select`, until the target is kept. --only, --all and the own source walk what they name.
  const walk = values.source === 'public' && !only && !values.all;
  const total = values.total ? positiveInt(values.total, '--total') : PUBLIC_TOTAL;
  const target = walk ? new Quota({ total, perRepo: PUBLIC_PER_REPO }) : null;
  const seed = values.seed ?? config.tasks.seed;
  fs.mkdirSync(p.work, { recursive: true });
  let kept = 0;
  let dropped = 0;
  let resumed = 0;
  let skipped = 0;
  const chosen = tasks.filter((t) => !only || only.has(t.id));
  const order = values.source === 'public' && !only ? selectionOrder(chosen, { seed, scope: 'public' }) : chosen;
  const nets = docker ? await checkNetworks({ docker, tasks: chosen }) : null;
  // Images that stay: the workspace image and the coder's, and with --keep-images those of the tasks kept so far. Otherwise a
  // public task's image goes once its verdict and digest are in check.json: `run` pulls it again by that digest, just in time.
  const keep = new Set([digestOfRef(config.tasks.workspaceImage), PINS.coder.digest].filter(Boolean));
  const keptDigests = new Set();
  try {
    const containers = make ? make(config) : new HostContainers({ aliases: nets?.aliases ?? {} });
    for (const task of order) {
      const key = `${values.source}/${task.id}`;
      if (walk && target.done) break;
      if (walk && !target.open(task.repo)) {
        skipped++;
        continue;
      }
      const recorded = !only && reusable(results[key], checksumOf(task)) ? results[key] : null;
      let verdict = recorded;
      if (recorded) {
        resumed++;
      } else {
        const r = await checkTask({ containers, task, workRoot: p.work });
        if (r.pin) pins[r.pin.ref] = r.pin.digest;
        verdict = { ok: r.ok, reason: r.reason, devcontainer: r.devcontainer, imageDigest: r.imageDigest, checksum: checksumOf(task), agentPath: r.agentPath, agentPathReason: r.agentPathReason };
        results[key] = verdict;
        writeJson(p.pins, pins);
        writeJson(p.check, results);
        out(`${task.id}: ${r.ok ? `kept (agent path: ${r.agentPath}${r.agentPath === 'pass' ? '' : `, ${r.agentPathReason}`})` : `dropped (${r.reason})`}${r.devcontainer ? ' (has a .devcontainer)' : ''}`);
      }
      const isKept = keptVerdict(verdict);
      const removable = !recorded && values.source === 'public' && verdict.reason !== 'infra' && verdict.imageDigest && !keep.has(verdict.imageDigest);
      const remove = async () => {
        for (const ref of [`${task.image.name}@${verdict.imageDigest}`, task.image.tag ? `${task.image.name}:${task.image.tag}` : null].filter(Boolean)) {
          try {
            await containers.removeImage(ref);
          } catch (e) {
            out(`${task.id}: image not removed (${e.message})`);
          }
        }
      };
      if (isKept) {
        kept++;
        target?.take(task.repo);
        if (verdict.imageDigest) keptDigests.add(verdict.imageDigest);
        if (removable && !values['keep-images']) await remove();
      } else {
        dropped++;
        if (removable && !keptDigests.has(verdict.imageDigest)) await remove();
      }
    }
  } finally {
    await nets?.cleanup();
  }
  out(`${kept} kept, ${dropped} dropped${walk ? `, ${skipped} not needed` : ''}${resumed ? `, ${resumed} from an earlier check` : ''}`);
  if (walk && !target.done) out(`shortfall: ${target.kept} of ${total} kept; every task that could count was walked (at most ${PUBLIC_PER_REPO} per repository), so bench tasks select will give ${target.kept}`);
  return 0;
}

async function cmdSelect(config, p, argv, out) {
  const { values } = parseArgs({ args: argv, options: { seed: { type: 'string' }, total: { type: 'string' }, source: { type: 'string', default: 'both' } } });
  const seed = values.seed ?? config.tasks.seed;
  const totalOption = values.total ? positiveInt(values.total, '--total') : null;
  const results = readJson(p.check, {});
  const pins = readJson(p.pins, {});
  // A result belongs to the task it was made for: the same checksum and the same image digest.
  const usable = (source, id, checksum, digest) => {
    const r = results[`${source}/${id}`];
    return r?.ok === true && r.devcontainer === false && typeof r.agentPath === 'string' && typeof checksum === 'string' && r.checksum === checksum && typeof digest === 'string' && r.imageDigest === digest;
  };
  const explain = (source, items) => {
    const dropped = items.filter(([id, checksum, digest]) => !usable(source, id, checksum, digest));
    if (dropped.length) out(`${source}: ${dropped.length} task(s) not selectable (not checked, changed since its check, dropped by the check, or a .devcontainer at the base)`);
  };
  if (values.source === 'public' || values.source === 'both') {
    const up = fetchPublic(config, p);
    // The order is that of `check`: over every listed task, pinned or not, so a verdict never moves it.
    const { tasks } = listTasks(up.tasksRoot, { pins, requireDigest: false, allowedNetworks: config.tasks.verifierNetworks });
    const sumOf = (t) => taskChecksum(path.relative(up.tasksRoot, t.dir), up.sums);
    explain('public', tasks.map((t) => [t.id, sumOf(t), t.image.digest]));
    const total = totalOption ?? PUBLIC_TOTAL;
    const order = selectionOrder(tasks, { seed, scope: 'public' });
    const picked = pickInOrder(order, (t) => usable('public', t.id, sumOf(t), t.image.digest), { total, perRepo: PUBLIC_PER_REPO });
    const chosen = picked
      .map((t) => ({ id: t.id, repo: t.repo, checksum: sumOf(t), imageDigest: t.image.digest, agentPath: results[`public/${t.id}`].agentPath }))
      .sort((x, y) => (x.repo < y.repo ? -1 : x.repo > y.repo ? 1 : x.id < y.id ? -1 : x.id > y.id ? 1 : 0));
    const usableCount = tasks.filter((t) => usable('public', t.id, sumOf(t), t.image.digest)).length;
    const cands = { length: usableCount };
    fs.mkdirSync(p.repoTasks, { recursive: true });
    fs.writeFileSync(path.join(p.repoTasks, 'public.lock.json'), lockText(buildLock({ source: 'public', seed, upstreamCommit: up.head, tasks: chosen })));
    out(`public: ${chosen.length} of ${cands.length} selected`);
  }
  if (values.source === 'own' || values.source === 'both') {
    const { specs, errors } = loadOwnSpecs(path.join(p.repoTasks, 'own'));
    for (const e of errors) out(`${e.id}: ${e.problems.join('; ')}`);
    explain('own', specs.map((s) => [s.id, ownChecksum(s), pins[ownImageTag(s)]]));
    const cands = specs.filter((s) => usable('own', s.id, ownChecksum(s), pins[ownImageTag(s)])).map((s) => ({ id: s.id, repo: s.repo, checksum: ownChecksum(s), imageDigest: pins[ownImageTag(s)], agentPath: results[`own/${s.id}`].agentPath }));
    const total = totalOption ?? OWN_TOTAL;
    const repoCaps = config.tasks.selfRepo ? { [config.tasks.selfRepo]: OWN_PER_REPO } : {};
    const chosen = selectTasks(cands, { seed, scope: 'own', total, perRepoCap: OWN_PER_REPO, repoCaps });
    fs.mkdirSync(p.repoTasks, { recursive: true });
    fs.writeFileSync(path.join(p.repoTasks, 'own.lock.json'), lockText(buildLock({ source: 'own', seed, tasks: chosen })));
    out(`own: ${chosen.length} of ${cands.length} selected`);
  }
  return 0;
}

/** Run a `tasks` subcommand. `make(config)` builds the containers (tests pass a fake). Returns an exit code. */
export async function runTasks(argv, { env = process.env, out = (l) => process.stdout.write(`${l}\n`), err = (l) => process.stderr.write(`${l}\n`), make, docker = make ? null : realDocker } = {}) {
  const [sub, ...rest] = argv;
  if (!sub || !['fetch', 'mine', 'lint', 'build', 'check', 'select'].includes(sub)) {
    err(USAGE.trimEnd());
    return sub === '--help' || sub === '-h' ? 0 : 64;
  }
  try {
    const p = paths(env);
    const config = loadConfig({ env });
    if (sub === 'fetch') return await cmdFetch(config, p, out);
    if (sub === 'mine') return await cmdMine(config, p, rest, out);
    if (sub === 'lint') return await cmdLint(config, p, rest, out);
    if (sub === 'build') return await cmdBuild(config, p, rest, out, make);
    if (sub === 'check') return await cmdCheck(config, p, rest, out, make, docker);
    return await cmdSelect(config, p, rest, out);
  } catch (e) {
    if (e instanceof ConfigError) throw e;
    if (e instanceof UsageError) {
      err(`bench: ${e.message}`);
      return 64;
    }
    if (e instanceof UpstreamError || e instanceof OwnTaskError || e instanceof ContainerError || e?.code === 'ERR_PARSE_ARGS_UNKNOWN_OPTION' || e?.code === 'ERR_PARSE_ARGS_INVALID_OPTION_VALUE') {
      err(`bench: ${e.message}`);
      return e instanceof UpstreamError || e instanceof OwnTaskError || e instanceof ContainerError ? 1 : 64;
    }
    throw e;
  }
}
