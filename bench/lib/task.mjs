// The task format, the Harbor layout both task sources use, and its validator.
//
//   <id>/instruction.md        the statement the agent gets
//   <id>/task.toml             [agent] network_mode, [verifier] network_mode, [environment] cpus, memory_mb,
//                              optional docker_image and workdir, optional [metadata] repo, language
//   <id>/environment/          Dockerfile whose first FROM names the image (or task.toml docker_image)
//   <id>/tests/test.sh         writes /logs/verifier/reward.txt (1 pass, 0 fail)
//   <id>/solution/             the gold change, `*.patch` (used by the environment check only)
//
// The image must be pinned by digest. Upstream images are named by tag, so a digest comes from `pins`
// (image reference without digest -> `sha256:...`), which `bench tasks check` fills in locally.
import fs from 'node:fs';
import path from 'node:path';
import { TomlError, parseToml } from './toml.mjs';

export class TaskError extends Error {
  constructor(id, problems) {
    const list = Array.isArray(problems) ? problems : [problems];
    super(`task ${id}: ${list.join('; ')}`);
    this.name = 'TaskError';
    this.taskId = id;
    this.problems = list;
  }
}

export const TASK_ID = /^[A-Za-z0-9][A-Za-z0-9._-]{0,199}$/;
const DIGEST = /^sha256:[0-9a-f]{64}$/;

/** Split an image reference into name, tag and digest. Returns null when it is not a reference. */
export function parseImageRef(ref) {
  if (typeof ref !== 'string' || ref === '' || /[\s$"'\\]/.test(ref)) return null;
  const at = ref.indexOf('@');
  const digest = at >= 0 ? ref.slice(at + 1) : null;
  const rest = at >= 0 ? ref.slice(0, at) : ref;
  const slash = rest.lastIndexOf('/');
  const colon = rest.indexOf(':', slash + 1);
  const name = colon >= 0 ? rest.slice(0, colon) : rest;
  const tag = colon >= 0 ? rest.slice(colon + 1) : null;
  if (!/^[a-z0-9][a-z0-9._:/-]*$/i.test(name) || (tag !== null && !/^[A-Za-z0-9_][A-Za-z0-9._-]{0,127}$/.test(tag))) return null;
  if (digest !== null && !DIGEST.test(digest)) return null;
  return { name, tag, digest };
}

/** The image of the first FROM line of a Dockerfile, or null. */
export function dockerfileFrom(text) {
  for (const raw of String(text).split('\n')) {
    const line = raw.trim();
    if (line === '' || line.startsWith('#')) continue;
    const m = /^FROM\s+(?:--\S+\s+)*(\S+)/i.exec(line);
    if (m) return m[1];
  }
  return null;
}

const BUILTIN_NETWORKS = new Set(['none', 'no-network', 'default', 'public']);

/** The first symlink under `dir` (relative path), or null. Nothing is followed. */
function findSymlink(dir) {
  const walk = (d, rel) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const r = rel ? `${rel}/${e.name}` : e.name;
      if (e.isSymbolicLink()) return r;
      if (e.isDirectory()) {
        const found = walk(path.join(d, e.name), r);
        if (found) return found;
      }
    }
    return null;
  };
  try {
    return walk(dir, '');
  } catch {
    return null;
  }
}

const isTable = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);

/** Language by the repository name's conventions is not guessed: it comes from [metadata], or is "unknown". */
export function repoFromId(id) {
  const m = /^instance_([A-Za-z0-9-]+)__([A-Za-z0-9._-]+?)-[0-9a-f]{7,}/.exec(id);
  return m ? `${m[1]}/${m[2]}` : null;
}

function goldPatchFile(dir) {
  const sol = path.join(dir, 'solution');
  const names = fs.readdirSync(sol).filter((n) => /\.(patch|diff)$/.test(n)).sort();
  if (names.includes('solution.patch')) return path.join(sol, 'solution.patch');
  return names.length === 1 ? path.join(sol, names[0]) : null;
}

/**
 * Read and validate a task directory. Every problem is collected; one TaskError names them all.
 * `pins` maps an image reference without digest to its digest. `requireDigest: false` is for listing only.
 * The verifier's network is `none`, `default`, `public` or a name in `allowedNetworks` (the owner's configuration): never `host`.
 * A symlink anywhere in the directory is refused.
 */
export function loadTask(dir, { pins = {}, source = 'public', requireDigest = true, allowedNetworks = [] } = {}) {
  const id = path.basename(path.resolve(dir));
  if (!TASK_ID.test(id) || id.includes('..')) throw new TaskError(id, 'the directory name is not a safe task id');
  const problems = [];
  const link = findSymlink(dir);
  if (link !== null) throw new TaskError(id, `a symlink is not allowed in a task directory: ${link}`);
  const file = (rel) => {
    const p = path.join(dir, rel);
    try {
      return fs.statSync(p).isFile() ? fs.readFileSync(p, 'utf8') : null;
    } catch {
      return null;
    }
  };
  const isDir = (rel) => {
    try {
      return fs.statSync(path.join(dir, rel)).isDirectory();
    } catch {
      return false;
    }
  };

  const instruction = file('instruction.md');
  if (instruction === null) problems.push('instruction.md is missing');
  else if (instruction.trim() === '') problems.push('instruction.md is empty');

  let toml = null;
  const tomlText = file('task.toml');
  if (tomlText === null) problems.push('task.toml is missing');
  else {
    try {
      toml = parseToml(tomlText);
    } catch (e) {
      if (!(e instanceof TomlError)) throw e;
      problems.push(e.message);
    }
  }

  const testSh = file('tests/test.sh');
  if (testSh === null) problems.push('tests/test.sh is missing');
  else if (!testSh.includes('/logs/verifier/reward.txt')) problems.push('tests/test.sh does not write /logs/verifier/reward.txt');

  if (!isDir('solution')) problems.push('solution/ is missing');
  else if (fs.readdirSync(path.join(dir, 'solution')).length === 0) problems.push('solution/ is empty');

  let imageRef = null;
  const dockerfile = file('environment/Dockerfile');
  if (!isDir('environment')) problems.push('environment/ is missing');
  else if (dockerfile === null && !(toml && isTable(toml.environment) && toml.environment.docker_image)) {
    problems.push('environment/ has no Dockerfile and task.toml names no docker_image');
  }
  if (toml && isTable(toml.environment) && typeof toml.environment.docker_image === 'string') imageRef = toml.environment.docker_image;
  else if (dockerfile !== null) imageRef = dockerfileFrom(dockerfile);

  let image = null;
  if (imageRef !== null || dockerfile !== null) {
    const parsed = imageRef === null ? null : parseImageRef(imageRef);
    if (!parsed) problems.push('the image reference is missing or not a plain reference (no variables)');
    else {
      const digest = parsed.digest ?? pins[`${parsed.name}${parsed.tag ? `:${parsed.tag}` : ''}`] ?? null;
      const shown = `${parsed.name}${parsed.tag ? `:${parsed.tag}` : ''}`;
      if (!digest || !DIGEST.test(digest)) {
        if (requireDigest) problems.push(`the image ${shown} is not pinned by digest`);
        else image = { ref: shown, name: parsed.name, tag: parsed.tag, digest: null };
      }
      else image = { ref: `${parsed.name}@${digest}`, name: parsed.name, tag: parsed.tag, digest };
    }
  }

  let agentNetwork = null;
  let verifierNetwork = 'public';
  let cpus = 1;
  let memoryMb = 4096;
  let workdir = '/app';
  let repo = null;
  let language = 'unknown';
  if (toml) {
    const agent = isTable(toml.agent) ? toml.agent : {};
    const mode = typeof agent.network_mode === 'string' ? agent.network_mode.toLowerCase() : null;
    if (mode !== 'none' && mode !== 'no-network') problems.push('[agent] network_mode must be "none" (or "no-network"): the agent has no network');
    else agentNetwork = 'none';
    const verifier = isTable(toml.verifier) ? toml.verifier : {};
    if (verifier.network_mode !== undefined) {
      const n = verifier.network_mode;
      if (typeof n !== 'string' || !(BUILTIN_NETWORKS.has(n) || allowedNetworks.includes(n))) problems.push(`[verifier] network_mode must be one of ${[...BUILTIN_NETWORKS, ...allowedNetworks].join(', ')}`);
      else verifierNetwork = n;
    }
    const env = isTable(toml.environment) ? toml.environment : {};
    for (const [key, set] of [['cpus', (v) => (cpus = v)], ['memory_mb', (v) => (memoryMb = v)]]) {
      if (env[key] === undefined) continue;
      if (typeof env[key] !== 'number' || !(env[key] > 0) || env[key] > 1e6) problems.push(`[environment] ${key} must be a positive number`);
      else set(env[key]);
    }
    if (env.workdir !== undefined) {
      if (typeof env.workdir !== 'string' || !/^\/[A-Za-z0-9._/-]*$/.test(env.workdir) || env.workdir.includes('..')) problems.push('[environment] workdir must be an absolute path');
      else workdir = env.workdir;
    }
    const meta = isTable(toml.metadata) ? toml.metadata : {};
    if (typeof meta.repo === 'string') repo = meta.repo;
    if (typeof meta.language === 'string') language = meta.language;
  }
  repo ??= repoFromId(id) ?? 'unknown';

  if (problems.length > 0) throw new TaskError(id, problems);
  return {
    id,
    dir: path.resolve(dir),
    source,
    instruction,
    image,
    agentNetwork,
    verifierNetwork,
    cpus,
    memoryMb,
    workdir,
    repo,
    language,
    goldPatch: goldPatchFile(dir),
  };
}

/** Load every task directory under `root` (those with a task.toml, one or two levels deep). Never throws on a bad task. */
export function listTasks(root, opts = {}) {
  const dirs = [];
  const walk = (d, depth) => {
    let entries;
    try {
      entries = fs.readdirSync(d, { withFileTypes: true });
    } catch {
      return;
    }
    if (entries.some((e) => e.isFile() && e.name === 'task.toml')) return void dirs.push(d);
    if (depth >= 2) return;
    for (const e of entries.sort((a, b) => (a.name < b.name ? -1 : 1))) if (e.isDirectory() && !e.name.startsWith('.')) walk(path.join(d, e.name), depth + 1);
  };
  walk(root, 0);
  const tasks = [];
  const errors = [];
  for (const d of dirs) {
    try {
      tasks.push(loadTask(d, opts));
    } catch (e) {
      if (!(e instanceof TaskError)) throw e;
      errors.push({ id: e.taskId, problems: e.problems });
    }
  }
  return { tasks, errors };
}
