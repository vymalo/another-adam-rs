// The host's container runtime, behind a small interface, so grading and the drivers are tested with no runtime.
//
//   run(spec)            -> { exitCode, stdout, stderr, timedOut } (a foreground container, removed when it ends)
//   start(spec)          -> { exitCode, stdout, stderr, timedOut } (a detached container, kept; the caller names it)
//   pull(ref)            -> void
//   digestOf(ref)        -> 'sha256:...' (the registry digest of a pulled image)
//   hasImage(ref)        -> boolean
//   imageId(tag)         -> 'sha256:...' (the id of a local image)
//   imageEnv(ref)        -> { PATH: '...' } (the image's own environment)
//   extract(ref, paths, dir) -> void (files of an image, copied into a directory of the harness's)
//   build(context, tag, dockerfile, buildArgs?) -> void   (buildArgs: { NAME: value }, plain)
//   stop(name), kill(name, signal), reapAll() (every container of this instance, removed and checked),
//   listContainers() -> [{ image, id, labels, names }]
//
// RunSpec: { image, script?, command?, entrypoint?, workdir?, env?, envFile?, mounts?: [{source, target, readOnly?}],
//            network: 'none' | <a logical network name>, cpus?, memoryMb?, pidsLimit?, timeoutMs?, name?, user?,
//            readOnly?, tmpfs?: [string], capDrop?: [string], labels?: {k: v}, publish?: [{hostPort, containerPort}],
//            input?: string (given on stdin: for what must not be on a command line) }.
// A mount source is a path on the host. The `docker` CLI is run with an argv array (execFile), never through a
// shell: a script, a statement or a token is one argument and is never interpolated. A network is named, never
// `host` or Docker's default bridge (which has a route out); an unknown name is prefixed with the project's prefix
// (`slot` -> `adam-bench_slot-1`). Every container of an instance carries its labels, and `reapAll` removes only
// the containers with them, never another harness's or another project's.
import { execFile } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { parseImageRef } from './task.mjs';

export class ContainerError extends Error {
  constructor(message) {
    super(message);
    this.name = 'ContainerError';
  }
}

const SAFE_PATH = /^\/[A-Za-z0-9._/@+=-]*$/;
const SAFE_NAME = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/;
const SAFE_ARG = /^[^\n\0]*$/;

/**
 * What a `docker` child needs to find the daemon and the material that authenticates it; nothing else of the harness's
 * environment reaches it. Every entry is a path or a flag, never a secret: the credentials themselves stay in the files
 * `DOCKER_CONFIG` and `DOCKER_CERT_PATH` name, which the CLI is meant to read. `HTTPS_PROXY` and the other proxy
 * variables are deliberately absent (they can carry a password), as is anything the model's configuration sets.
 */
export const DOCKER_ENV_KEEP = [
  'PATH',
  'HOME',
  'DOCKER_HOST',
  'DOCKER_CONTEXT',
  'DOCKER_CONFIG',
  'DOCKER_TLS_VERIFY',
  'DOCKER_CERT_PATH',
  'XDG_RUNTIME_DIR',
  'TMPDIR',
  'SSL_CERT_FILE',
  'SSL_CERT_DIR',
];

/** The environment of a `docker` child: the allow-list above, taken from `baseEnv`. Pure. */
export function dockerEnv(baseEnv = process.env) {
  const env = {};
  for (const k of DOCKER_ENV_KEEP) if (baseEnv[k] !== undefined) env[k] = baseEnv[k];
  return env;
}

const labelArgs = (labels) => {
  const out = [];
  for (const [k, v] of Object.entries(labels ?? {})) {
    if (!/^[a-z][a-z0-9._/-]{0,62}$/.test(k) || !/^[A-Za-z0-9._-]{1,63}$/.test(String(v))) throw new ContainerError('a label is not plain');
    out.push('--label', `${k}=${v}`);
  }
  return out;
};

/** The `docker` arguments (after the binary) for one run. Pure. */
export function buildRunArgs(spec, { name = spec.name, labels = {} } = {}) {
  if (!parseImageRef(spec.image)) throw new ContainerError('the image is not a plain reference');
  if (spec.script !== undefined && (typeof spec.script !== 'string' || spec.script === '')) throw new ContainerError('a run script is a non-empty string');
  if (spec.network === undefined) throw new ContainerError('a run names its network: "none" or a network name');
  const args = ['run'];
  args.push(spec.detach ? '-d' : '--rm');
  args.push('--pull=never');
  if (spec.input !== undefined) args.push('-i');
  args.push(...labelArgs({ ...labels, ...(spec.labels ?? {}) }));
  if (name) {
    if (!SAFE_NAME.test(name)) throw new ContainerError('the container name is not safe');
    args.push('--name', name);
  }
  if (spec.network === 'none') args.push('--network=none');
  else {
    if (spec.network === 'host' || spec.network === 'default') throw new ContainerError('the host network and Docker\'s default bridge are never given to a container');
    if (!SAFE_NAME.test(spec.network)) throw new ContainerError('the network name is not safe');
    args.push(`--network=${spec.network}`);
  }
  if (spec.cpus !== undefined) args.push('--cpus', String(spec.cpus));
  if (spec.memoryMb !== undefined) args.push('--memory', `${Math.floor(spec.memoryMb)}m`);
  if (spec.pidsLimit !== undefined) args.push('--pids-limit', String(Math.floor(spec.pidsLimit)));
  if (spec.user) args.push('--user', spec.user);
  if (spec.readOnly) args.push('--read-only');
  for (const t of spec.tmpfs ?? []) {
    if (typeof t !== 'string' || !SAFE_ARG.test(t)) throw new ContainerError('a tmpfs is not plain');
    args.push('--tmpfs', t);
  }
  for (const c of spec.capDrop ?? []) {
    if (!/^[A-Z_]+$/.test(c)) throw new ContainerError('a capability name is not plain');
    args.push('--cap-drop', c);
  }
  if (spec.workdir) {
    if (!SAFE_PATH.test(spec.workdir)) throw new ContainerError('the working directory is not a plain absolute path');
    args.push('--workdir', spec.workdir);
  }
  if (spec.envFile !== undefined) {
    if (!SAFE_PATH.test(spec.envFile)) throw new ContainerError('the env file is not a plain absolute path');
    args.push('--env-file', spec.envFile);
  }
  for (const [k, v] of Object.entries(spec.env ?? {})) {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(k) || /[\n\0]/.test(String(v))) throw new ContainerError('an environment variable is not plain');
    args.push('-e', `${k}=${v}`);
  }
  for (const m of spec.mounts ?? []) {
    if (!SAFE_PATH.test(m.source) || !SAFE_PATH.test(m.target)) throw new ContainerError('a mount path is not a plain absolute path');
    args.push('-v', `${m.source}:${m.target}${m.readOnly ? ':ro' : ''}`);
  }
  for (const p of spec.publish ?? []) {
    const ok = (n) => Number.isInteger(n) && n >= 1 && n <= 65535;
    if (!ok(p.hostPort) || !ok(p.containerPort)) throw new ContainerError('a published port is not a port');
    args.push('-p', `127.0.0.1:${p.hostPort}:${p.containerPort}`);
  }
  if (spec.script !== undefined) {
    args.push('--entrypoint', 'sh', spec.image, '-c', spec.script);
  } else {
    if (spec.entrypoint !== undefined) {
      if (!SAFE_PATH.test(spec.entrypoint)) throw new ContainerError('the entrypoint is not a plain absolute path');
      args.push('--entrypoint', spec.entrypoint);
    }
    args.push(spec.image);
    for (const a of spec.command ?? []) {
      if (typeof a !== 'string' || !SAFE_ARG.test(a)) throw new ContainerError('a command argument is not plain');
      args.push(a);
    }
  }
  return args;
}

/** `--build-arg NAME=value` for each argument; a name or a value that is not plain is refused. Pure. */
export function buildArgsFlags(buildArgs) {
  const out = [];
  for (const [k, v] of Object.entries(buildArgs ?? {})) {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(k) || typeof v !== 'string' || /[\s\0$"'\\]/.test(v)) throw new ContainerError('a build argument is not plain');
    out.push('--build-arg', `${k}=${v}`);
  }
  return out;
}

export class HostContainers {
  /**
   * @param {object} o
   * @param {string} [o.bin]        `docker`
   * @param {Function} [o.exec]     execFile, for tests
   * @param {Function} [o.invoke]   a promise-returning `(args, { timeoutMs, input })` in place of execFile
   * @param {object} [o.labels]     every container of this instance carries them
   * @param {string} [o.prefix]     a logical network name becomes `<prefix><name>`
   * @param {object} [o.aliases]    logical network name -> final name
   * @param {string[]} [o.filters]  the `ps` filters that select this instance's containers (default: its labels)
   * @param {object} [o.env]        the environment of the `docker` child (default: the allow-list of `dockerEnv`)
   */
  constructor({ bin = 'docker', exec = execFile, invoke = null, labels = {}, prefix = '', aliases = {}, filters = null, env = dockerEnv() } = {}) {
    this.bin = bin;
    this.exec = exec;
    this.invoke = invoke;
    this.labels = labels;
    this.prefix = prefix;
    this.aliases = aliases;
    this.filters = filters ?? Object.entries(labels).map(([k, v]) => `label=${k}=${v}`);
    this.env = env;
  }

  #network(name) {
    if (name === 'none') return 'none';
    if (name === 'host' || name === 'default') throw new ContainerError('the host network and Docker\'s default bridge are never given to a container');
    return this.aliases[name] ?? `${this.prefix}${name}`;
  }

  #args(spec, name) {
    return buildRunArgs({ ...spec, network: this.#network(spec.network) }, { labels: this.labels, name });
  }

  #call(args, { timeoutMs = 0, maxBuffer = 64 * 1024 * 1024, input = '' } = {}) {
    if (this.invoke) return Promise.resolve().then(() => this.invoke(args, { timeoutMs, input }));
    return new Promise((resolve) => {
      const child = this.exec(this.bin, args, { env: this.env, timeout: timeoutMs, maxBuffer, killSignal: 'SIGKILL', encoding: 'utf8' }, (err, stdout, stderr) => {
        const timedOut = Boolean(err && err.killed);
        resolve({ exitCode: err ? (typeof err.code === 'number' ? err.code : 125) : 0, stdout: stdout ?? '', stderr: stderr ?? '', timedOut });
      });
      child?.stdin?.on?.('error', () => {});
      child?.stdin?.end?.(input);
    });
  }

  async run(spec) {
    const name = spec.name ?? `adam-bench-${randomBytes(6).toString('hex')}`;
    const r = await this.#call(this.#args(spec, name), { timeoutMs: spec.timeoutMs ?? 0, input: spec.input ?? '' });
    if (r.timedOut) await this.#call(['rm', '-f', name]);
    return r;
  }

  /** Start a detached container and return at once; its name is the caller's (the coder, a probe that is stopped). */
  async start(spec) {
    if (!spec.name || !SAFE_NAME.test(spec.name)) throw new ContainerError('a detached container needs a plain name');
    return this.#call(this.#args({ ...spec, detach: true }, spec.name), { timeoutMs: spec.timeoutMs ?? 0 });
  }

  async #must(args, what) {
    const r = await this.#call(args);
    if (r.exitCode !== 0) throw new ContainerError(`${what} failed (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-3).join(' | ')}`);
    return r.stdout.trim();
  }

  async pull(ref) {
    await this.#must(['pull', '-q', ref], `pulling ${ref}`);
  }

  /** The ids of this instance's containers, running or not. */
  async #ids() {
    if (this.filters.length === 0) throw new ContainerError('a reap needs the labels that select this harness\'s containers');
    const args = ['ps', '-aq'];
    for (const f of this.filters) args.push('--filter', f);
    const out = await this.#must(args, 'listing the containers');
    return out.split('\n').map((s) => s.trim()).filter(Boolean);
  }

  /** Every container of this instance, removed by force; then the list is read and must be empty. */
  async reapAll() {
    const ids = await this.#ids();
    if (ids.length > 0) await this.#call(['rm', '-f', ...ids], { timeoutMs: 120_000 });
    const left = await this.#ids();
    if (left.length > 0) throw new ContainerError('containers are left after the reap');
  }

  /** What this instance holds, running or not: [{ image, id, labels, names }]. */
  async listContainers() {
    const args = ['ps', '-a', '--no-trunc', '--format', '{{json .}}'];
    for (const f of this.filters) args.push('--filter', f);
    const text = await this.#must(args, 'listing the containers');
    const rows = [];
    for (const line of text.split('\n').filter(Boolean)) {
      try {
        rows.push(JSON.parse(line));
      } catch {
        throw new ContainerError('the container list is not JSON');
      }
    }
    const labelsOf = (text) => {
      const out = {};
      for (const part of String(text ?? '').split(',')) {
        const eq = part.indexOf('=');
        if (eq > 0) out[part.slice(0, eq)] = part.slice(eq + 1);
      }
      return out;
    };
    return rows.map((r) => ({ image: String(r.Image ?? ''), id: String(r.ID ?? ''), labels: labelsOf(r.Labels), names: String(r.Names ?? '').split(',').filter(Boolean) }));
  }

  /** Send a signal to a running container by name: the polite end of a run, before the forced one (`stop`). */
  async kill(name, signal) {
    if (!SAFE_NAME.test(name)) throw new ContainerError('the container name is not safe');
    if (!['TERM', 'INT', 'KILL'].includes(signal)) throw new ContainerError('the signal is not one the harness sends');
    await this.#call(['kill', '--signal', signal, name], { timeoutMs: 60_000 });
  }

  /** Remove a running container by name (a run the harness ends early). */
  async stop(name) {
    if (!SAFE_NAME.test(name)) throw new ContainerError('the container name is not safe');
    await this.#call(['rm', '-f', name]);
  }

  async digestOf(ref) {
    const out = await this.#must(['image', 'inspect', '--format', '{{.Digest}}', ref], `reading the digest of ${ref}`);
    if (!/^sha256:[0-9a-f]{64}$/.test(out)) throw new ContainerError(`the digest of ${ref} is not readable`);
    return out;
  }

  /** Is the image in the host's store? (`docker image inspect`: exit 0 yes, 1 no, anything else is an error.) */
  async hasImage(ref) {
    if (!parseImageRef(ref) && !/^[a-z0-9][a-z0-9._/-]*:[A-Za-z0-9._-]+$/.test(ref)) throw new ContainerError('the image is not a plain reference');
    const r = await this.#call(['image', 'inspect', '--format', '{{.Id}}', ref]);
    if (r.exitCode === 0) return true;
    if (r.exitCode === 1) return false;
    throw new ContainerError(`asking for ${ref} failed (exit ${r.exitCode})`);
  }

  async build(context, tag, dockerfile, buildArgs = {}) {
    await this.#must(['build', '-t', tag, '-f', dockerfile, ...buildArgsFlags(buildArgs), context], `building ${tag}`);
  }

  async imageId(tag) {
    const out = await this.#must(['image', 'inspect', '--format', '{{.Id}}', tag], `reading the id of ${tag}`);
    const id = out.startsWith('sha256:') ? out : `sha256:${out}`;
    if (!/^sha256:[0-9a-f]{64}$/.test(id)) throw new ContainerError(`the id of ${tag} is not readable`);
    return id;
  }

  /** The image's own environment, as `{ KEY: value }`; `{}` when the image says nothing. */
  async imageEnv(ref) {
    const out = await this.#must(['image', 'inspect', '--format', '{{json .Config.Env}}', ref], `reading the environment of ${ref}`);
    let list;
    try {
      list = JSON.parse(out || '[]');
    } catch {
      throw new ContainerError(`the environment of ${ref} is not readable`);
    }
    const env = {};
    for (const entry of Array.isArray(list) ? list : []) {
      const eq = String(entry).indexOf('=');
      if (eq > 0) env[String(entry).slice(0, eq)] = String(entry).slice(eq + 1);
    }
    return env;
  }

  /** Copy files out of an image (never run it): `docker create` once, `docker cp` each path, remove the container. */
  async extract(ref, paths, dir) {
    if (!parseImageRef(ref)) throw new ContainerError('the image is not a plain reference');
    if (!SAFE_PATH.test(dir)) throw new ContainerError('the destination is not a plain absolute path');
    const name = `adam-bench-extract-${randomBytes(6).toString('hex')}`;
    await this.#must(['create', '--name', name, ref], `creating a container of ${ref}`);
    try {
      for (const p of paths) {
        if (!SAFE_PATH.test(p)) throw new ContainerError('a path to extract is not plain');
        await this.#must(['cp', `${name}:${p}`, `${dir}/${p.split('/').pop()}`], `copying ${p} out of ${ref}`);
      }
    } finally {
      await this.#call(['rm', '-f', name]);
    }
  }
}

/** For tests: records every call, and answers from `onRun(spec, index)`. */
export class FakeContainers {
  constructor({ onRun = () => ({ exitCode: 0 }), digests = {}, imageEnv: env = {} } = {}) {
    this.onRun = onRun;
    this.digests = digests;
    this.imageEnvValue = env;
    this.runs = [];
    this.started = [];
    this.pulled = [];
    this.built = [];
    this.stopped = [];
    this.killed = [];
    this.extracted = [];
    this.reaped = 0;
    this.listing = [];
  }
  async run(spec) {
    buildRunArgs(spec); // the same refusals as the real thing
    const index = this.runs.length;
    this.runs.push(spec);
    const r = await this.onRun(spec, index);
    return { exitCode: 0, stdout: '', stderr: '', timedOut: false, ...r };
  }
  async start(spec) {
    buildRunArgs({ ...spec, detach: true });
    this.started.push(spec);
    return { exitCode: 0, stdout: 'container-id\n', stderr: '', timedOut: false };
  }
  async pull(ref) {
    this.pulled.push(ref);
  }
  async stop(name) {
    this.stopped.push(name);
  }
  async kill(name, signal) {
    this.killed.push([name, signal]);
  }
  async reapAll() {
    this.reaped += 1;
    this.listing = [];
  }
  async listContainers() {
    return this.listing;
  }
  async digestOf(ref) {
    return this.digests[ref] ?? `sha256:${'a'.repeat(64)}`;
  }
  async hasImage(ref) {
    return this.images?.has(ref) ?? false;
  }
  async build(context, tag, dockerfile, buildArgs = {}) {
    buildArgsFlags(buildArgs); // the same refusals as the real thing
    this.built.push({ context, tag, dockerfile, buildArgs });
  }
  async imageId(tag) {
    return this.digests[tag] ?? `sha256:${'b'.repeat(64)}`;
  }
  async imageEnv() {
    return this.imageEnvValue;
  }
  async extract(ref, paths, dir) {
    this.extracted.push({ ref, paths, dir });
  }
}
