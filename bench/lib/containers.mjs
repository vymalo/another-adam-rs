// The container runtime the tasks use, behind a small interface, so grading is tested with no runtime.
//
//   run(spec)            -> { exitCode, stdout, stderr, timedOut }
//   pull(ref)            -> void
//   digestOf(ref)        -> 'sha256:...' (the registry digest of a pulled image)
//   build(context, tag, dockerfile, buildArgs?) -> void   (buildArgs: { NAME: value }, plain)
//   imageId(tag)         -> 'sha256:...' (the id of a built local image)
//
// RunSpec: { image, script, workdir?, env?, mounts?: [{source, target, readOnly?}], network: 'none' | 'default' | <name>,
//            cpus?, memoryMb?, timeoutMs?, name?, user?, labels?: {k: v}, input?: string (given on stdin: for what must not be on a command line) }.
//   stop(name), reapAll() (every container of the service, removed and checked), listContainers() -> [{ image, imageId, labels, names }]
// A mount source is a path as the container service sees it.
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

/** The `podman --remote` arguments (after the binary) for one run. Pure. */
export function buildRunArgs(spec, { connection, url, name = spec.name } = {}) {
  if (!parseImageRef(spec.image)) throw new ContainerError('the image is not a plain reference');
  if (typeof spec.script !== 'string' || spec.script === '') throw new ContainerError('a run needs a script');
  if (spec.network === undefined) throw new ContainerError('a run names its network: "none", "default" or a network name');
  const args = ['--remote'];
  if (connection) args.push('--connection', connection);
  if (url) args.push('--url', url);
  args.push('run', '--rm', '--pull=never');
  if (spec.input !== undefined) args.push('-i');
  for (const [k, v] of Object.entries(spec.labels ?? {})) {
    if (!/^[a-z][a-z0-9._/-]{0,62}$/.test(k) || !/^[A-Za-z0-9._-]{1,63}$/.test(String(v))) throw new ContainerError('a label is not plain');
    args.push('--label', `${k}=${v}`);
  }
  if (name) {
    if (!SAFE_NAME.test(name)) throw new ContainerError('the container name is not safe');
    args.push('--name', name);
  }
  if (spec.network === 'none') args.push('--network=none');
  else if (spec.network !== 'default') {
    if (spec.network === 'host') throw new ContainerError('the host network is never given to a container');
    if (!SAFE_NAME.test(spec.network)) throw new ContainerError('the network name is not safe');
    args.push(`--network=${spec.network}`);
  }
  if (spec.cpus !== undefined) args.push('--cpus', String(spec.cpus));
  if (spec.memoryMb !== undefined) args.push('--memory', `${Math.floor(spec.memoryMb)}m`);
  if (spec.user) args.push('--user', spec.user);
  if (spec.workdir) {
    if (!SAFE_PATH.test(spec.workdir)) throw new ContainerError('the working directory is not a plain absolute path');
    args.push('--workdir', spec.workdir);
  }
  for (const [k, v] of Object.entries(spec.env ?? {})) {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(k) || /[\n\0]/.test(String(v))) throw new ContainerError('an environment variable is not plain');
    args.push('-e', `${k}=${v}`);
  }
  for (const m of spec.mounts ?? []) {
    if (!SAFE_PATH.test(m.source) || !SAFE_PATH.test(m.target)) throw new ContainerError('a mount path is not a plain absolute path');
    args.push('-v', `${m.source}:${m.target}${m.readOnly ? ':ro' : ''}`);
  }
  args.push('--entrypoint', 'sh', spec.image, '-c', spec.script);
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

export class PodmanRemote {
  constructor({ bin = 'podman', connection, url, exec = execFile } = {}) {
    this.bin = bin;
    this.opts = { connection, url };
    this.exec = exec;
  }

  #call(args, { timeoutMs = 0, maxBuffer = 64 * 1024 * 1024, input = '' } = {}) {
    return new Promise((resolve) => {
      const child = this.exec(this.bin, args, { timeout: timeoutMs, maxBuffer, killSignal: 'SIGKILL', encoding: 'utf8' }, (err, stdout, stderr) => {
        const timedOut = Boolean(err && err.killed);
        resolve({ exitCode: err ? (typeof err.code === 'number' ? err.code : 125) : 0, stdout: stdout ?? '', stderr: stderr ?? '', timedOut });
      });
      child?.stdin?.on?.('error', () => {});
      child?.stdin?.end?.(input);
    });
  }

  #base() {
    const a = ['--remote'];
    if (this.opts.connection) a.push('--connection', this.opts.connection);
    if (this.opts.url) a.push('--url', this.opts.url);
    return a;
  }

  async run(spec) {
    const name = spec.name ?? `adam-bench-${randomBytes(6).toString('hex')}`;
    const r = await this.#call(buildRunArgs(spec, { ...this.opts, name }), { timeoutMs: spec.timeoutMs ?? 0, input: spec.input ?? '' });
    if (r.timedOut) await this.#call([...this.#base(), 'rm', '-f', name]);
    return r;
  }

  async #must(args, what) {
    const r = await this.#call([...this.#base(), ...args]);
    if (r.exitCode !== 0) throw new ContainerError(`${what} failed (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-3).join(' | ')}`);
    return r.stdout.trim();
  }

  async pull(ref) {
    await this.#must(['pull', '-q', ref], `pulling ${ref}`);
  }

  /** Every container of the service, removed by force; then the list is read and must be empty. */
  async reapAll() {
    await this.#call([...this.#base(), 'rm', '-f', '-a', '-t', '0'], { timeoutMs: 120_000 });
    await this.#call([...this.#base(), 'pod', 'rm', '-f', '-a'], { timeoutMs: 120_000 });
    const left = await this.#must(['ps', '-a', '-q'], 'listing the containers');
    if (left !== '') throw new ContainerError('containers are left after the reap');
  }

  /** What the service holds, running or not: [{ image, imageId, labels, names }]. */
  async listContainers() {
    const text = await this.#must(['ps', '-a', '--no-trunc', '--format', 'json'], 'listing the containers');
    let rows;
    try {
      rows = text === '' ? [] : JSON.parse(text);
    } catch {
      throw new ContainerError('the container list is not JSON');
    }
    return (Array.isArray(rows) ? rows : []).map((r) => ({ image: String(r.Image ?? ''), imageId: String(r.ImageID ?? ''), labels: r.Labels ?? {}, names: r.Names ?? [] }));
  }

  /** Remove a running container by name (a run the harness ends early). */
  async stop(name) {
    if (!SAFE_NAME.test(name)) throw new ContainerError('the container name is not safe');
    await this.#call([...this.#base(), 'rm', '-f', name]);
  }

  async digestOf(ref) {
    const out = await this.#must(['image', 'inspect', '--format', '{{.Digest}}', ref], `reading the digest of ${ref}`);
    if (!/^sha256:[0-9a-f]{64}$/.test(out)) throw new ContainerError(`the digest of ${ref} is not readable`);
    return out;
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
}

/** For tests: records every call, and answers from `onRun(spec, index)`. */
export class FakeContainers {
  constructor({ onRun = () => ({ exitCode: 0 }), digests = {} } = {}) {
    this.onRun = onRun;
    this.digests = digests;
    this.runs = [];
    this.pulled = [];
    this.built = [];
    this.stopped = [];
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
  async pull(ref) {
    this.pulled.push(ref);
  }
  async stop(name) {
    this.stopped.push(name);
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
  async build(context, tag, dockerfile, buildArgs = {}) {
    buildArgsFlags(buildArgs); // the same refusals as the real thing
    this.built.push({ context, tag, dockerfile, buildArgs });
  }
  async imageId(tag) {
    return this.digests[tag] ?? `sha256:${'b'.repeat(64)}`;
  }
}
