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
import { PodmanRemote, ContainerError } from './containers.mjs';
import { checkTask } from './envcheck.mjs';
import { findCheckCommand, lintStatement } from './lint.mjs';
import { classifyPath, gitIn, mine } from './mine.mjs';
import { OwnTaskError, loadOwnSpecs, materializeOwn, ownChecksum, ownImageTag, prepareOwnContext } from './own.mjs';
import { buildLock, lockText, selectTasks } from './select.mjs';
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
  build [ID...]              build the image of each own task (needs the Podman service)
  check [--source public|own] [--only ID,ID]
                             gold passes three times and the empty patch fails three times (needs the Podman service)
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

const containersOf = (config, make) => (make ? make(config) : new PodmanRemote(config.tasks.podman));

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

async function cmdCheck(config, p, argv, out, make) {
  const { values } = parseArgs({ args: argv, options: { source: { type: 'string', default: 'public' }, only: { type: 'string' } } });
  if (values.source !== 'public' && values.source !== 'own') throw new ConfigError('--source', 'is public or own');
  const containers = containersOf(config, make);
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
  fs.mkdirSync(p.work, { recursive: true });
  let kept = 0;
  let dropped = 0;
  for (const task of tasks.filter((t) => !only || only.has(t.id))) {
    const r = await checkTask({ containers, task, workRoot: p.work });
    if (r.pin) pins[r.pin.ref] = r.pin.digest;
    results[`${values.source}/${task.id}`] = { ok: r.ok, reason: r.reason, devcontainer: r.devcontainer, imageDigest: r.imageDigest, checksum: checksumOf(task) };
    writeJson(p.pins, pins);
    writeJson(p.check, results);
    if (r.ok) kept++;
    else dropped++;
    out(`${task.id}: ${r.ok ? 'kept' : `dropped (${r.reason})`}${r.devcontainer ? ' (has a .devcontainer)' : ''}`);
  }
  out(`${kept} kept, ${dropped} dropped`);
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
    return r?.ok === true && r.devcontainer === false && typeof checksum === 'string' && r.checksum === checksum && typeof digest === 'string' && r.imageDigest === digest;
  };
  const explain = (source, items) => {
    const dropped = items.filter(([id, checksum, digest]) => !usable(source, id, checksum, digest));
    if (dropped.length) out(`${source}: ${dropped.length} task(s) not selectable (not checked, changed since its check, dropped by the check, or a .devcontainer at the base)`);
  };
  if (values.source === 'public' || values.source === 'both') {
    const up = fetchPublic(config, p);
    const { tasks } = listTasks(up.tasksRoot, { pins, requireDigest: true, allowedNetworks: config.tasks.verifierNetworks });
    const sumOf = (t) => taskChecksum(path.relative(up.tasksRoot, t.dir), up.sums);
    explain('public', tasks.map((t) => [t.id, sumOf(t), t.image.digest]));
    const cands = tasks.filter((t) => usable('public', t.id, sumOf(t), t.image.digest)).map((t) => ({ id: t.id, repo: t.repo, checksum: sumOf(t), imageDigest: t.image.digest }));
    const total = totalOption ?? PUBLIC_TOTAL;
    const chosen = selectTasks(cands, { seed, scope: 'public', total, perRepoCap: PUBLIC_PER_REPO });
    fs.mkdirSync(p.repoTasks, { recursive: true });
    fs.writeFileSync(path.join(p.repoTasks, 'public.lock.json'), lockText(buildLock({ source: 'public', seed, upstreamCommit: up.head, tasks: chosen })));
    out(`public: ${chosen.length} of ${cands.length} selected`);
  }
  if (values.source === 'own' || values.source === 'both') {
    const { specs, errors } = loadOwnSpecs(path.join(p.repoTasks, 'own'));
    for (const e of errors) out(`${e.id}: ${e.problems.join('; ')}`);
    explain('own', specs.map((s) => [s.id, ownChecksum(s), pins[ownImageTag(s)]]));
    const cands = specs.filter((s) => usable('own', s.id, ownChecksum(s), pins[ownImageTag(s)])).map((s) => ({ id: s.id, repo: s.repo, checksum: ownChecksum(s), imageDigest: pins[ownImageTag(s)] }));
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
export async function runTasks(argv, { env = process.env, out = (l) => process.stdout.write(`${l}\n`), err = (l) => process.stderr.write(`${l}\n`), make } = {}) {
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
    if (sub === 'check') return await cmdCheck(config, p, rest, out, make);
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
