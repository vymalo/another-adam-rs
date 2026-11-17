// The benchmark stack: bench/compose.yaml driven through `docker compose`, and each Podman service reached through
// `docker compose exec` (no socket is published and no port is opened to it).
//
// What the host trusts (bench/README.md): it runs no git in, reads no file from and deletes no file in a place a
// container could write. What a container wrote comes out of a service as `tar -cf -` and is parsed with limits
// (tarstream.mjs); what a container left is removed by a container, and checked to be gone.
//
// A coder's per-run settings (its model address with the run's meter token, its A2A bearer) go in an env file of
// mode 0600 that exists only while the coder is being created and is removed at the next start if a kill left it;
// they are in no command line, no log event and no error message (`redact`). The Podman client runs inside the
// service, so a mount source is a path as the service sees it.
import { execFile, spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { PodmanRemote } from './containers.mjs';
import { extractEntries, parseTar, pickFiles } from './tarstream.mjs';

export class StackError extends Error {
  constructor(message) {
    super(message);
    this.name = 'StackError';
  }
}

export const PODMAN_URL = 'unix:///run/podman/podman.sock';
export const SLOTS = 2;
export const COMPOSE_TIMEOUT_MS = 2 * 60 * 1000;
const SAFE_VALUE = /^[^\n\r\0$"'\\]*$/;
const ENV_KEY = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** The services a start brings up, for `slots` slots. */
export const serviceNames = (slots = SLOTS) => [
  'postgres',
  'mock-github',
  'git-server',
  'pub-git',
  'grader',
  ...Array.from({ length: slots }, (_, i) => [`podman-${i + 1}`, `gitbox-${i + 1}`, `meter-${i + 1}`, `pub-coder-${i + 1}`]).flat(),
];

/** Run a program and collect its output. Never throws; a program that is missing is exit 127. */
export function runCommand(file, args, { env, input, timeoutMs = 0, cwd } = {}) {
  return new Promise((resolve) => {
    const child = execFile(file, args, { env, cwd, timeout: timeoutMs, maxBuffer: 64 * 1024 * 1024, killSignal: 'SIGKILL', encoding: 'utf8' }, (err, stdout, stderr) => {
      resolve({ exitCode: err ? (typeof err.code === 'number' ? err.code : err.code === 'ENOENT' ? 127 : 125) : 0, stdout: stdout ?? '', stderr: stderr ?? '', timedOut: Boolean(err?.killed) });
    });
    child.stdin?.on('error', () => {});
    child.stdin?.end(input ?? '');
  });
}

/** Run a program and collect its stdout as bytes, at most `maxBytes` (past it the program is killed). Never throws. */
export function runBuffer(file, args, { env, timeoutMs = COMPOSE_TIMEOUT_MS, maxBytes = 512 * 1024 * 1024 } = {}) {
  return new Promise((resolve) => {
    const child = spawn(file, args, { env, stdio: ['ignore', 'pipe', 'pipe'], timeout: timeoutMs, killSignal: 'SIGKILL' });
    const chunks = [];
    let size = 0;
    let over = false;
    let err = '';
    child.stdout.on('data', (c) => {
      size += c.length;
      if (size > maxBytes) {
        over = true;
        child.kill('SIGKILL');
      } else chunks.push(c);
    });
    child.stderr.on('data', (c) => {
      if (err.length < 4000) err += c;
    });
    child.on('error', (e) => resolve({ exitCode: e.code === 'ENOENT' ? 127 : 125, stdout: Buffer.alloc(0), stderr: '', over: false }));
    child.on('close', (code) => resolve({ exitCode: code ?? 125, stdout: Buffer.concat(chunks), stderr: err, over }));
  });
}

/** `text` with every secret removed, and the bare token of a meter address. A secret shorter than 8 characters is left. */
export function redact(text, secrets) {
  let out = String(text);
  for (const s of secrets) if (typeof s === 'string' && s.length >= 8) out = out.split(s).join('[redacted]');
  return out;
}

/** What to keep out of an error for a setting: the value, and each long part of it (the token inside an address). */
export function secretsOf(value) {
  const parts = new Set([value]);
  for (const p of String(value).split(/[/:?&=@]/)) if (p.length >= 16) parts.add(p);
  return parts;
}

export class Stack {
  /**
   * @param {object} o
   * @param {string} o.composeFile
   * @param {string} o.home       $BENCH_HOME: the harness's own, never mounted into an agent's service
   * @param {string} o.tasksDir   the repository's bench/tasks, shared read-only with the grader
   * @param {string} o.seccomp    the absolute path of dev/podman/seccomp.json
   * @param {number} o.meterPort  where the harness's meter listens on the host
   */
  constructor({ composeFile, home, tasksDir, seccomp, meterPort, agentDir = null, project = 'adam-bench', ports = {}, slots = SLOTS, exec = runCommand, execBuf = runBuffer, spawn: spawnFn = execFile, log = () => {}, baseEnv = process.env }) {
    for (const [name, p] of [['composeFile', composeFile], ['home', home], ['tasksDir', tasksDir], ['seccomp', seccomp]]) {
      if (!path.isAbsolute(p)) throw new StackError(`${name} must be an absolute path`);
    }
    if (!/^[a-z0-9][a-z0-9_-]*$/.test(project)) throw new StackError('the project name is not plain');
    if (!Number.isInteger(meterPort) || meterPort < 1 || meterPort > 65535) throw new StackError('the meter port is not a port');
    Object.assign(this, { composeFile, home, tasksDir, seccomp, meterPort, agentDir, project, slots, exec, execBuf, spawn: spawnFn, log });
    this.ports = { git: ports.git ?? 18083, coder: { 1: ports.coder1 ?? 18081, 2: ports.coder2 ?? 18082 } };
    this.stackDir = path.join(home, 'stack');
    this.baseEnv = baseEnv;
    this.secrets = new Set();
  }

  slotService(prefix, slot) {
    if (!Number.isInteger(slot) || slot < 1 || slot > this.slots) throw new StackError(`there is no slot ${String(slot)}; the stack has ${this.slots}`);
    return `${prefix}-${slot}`;
  }

  /** What the harness hands a slot's agent service (a statement, the OpenCode binary): written by the harness, mounted read-only. */
  slotDir(slot) {
    this.slotService('slot', slot);
    return path.join(this.home, 'slots', String(slot));
  }

  /** The meter's name for the containers of a slot. */
  meterHost(slot) {
    return this.slotService('meter', slot);
  }

  /** The environment of every compose call: what the file's `${...}` need, and nothing of the model's. */
  composeEnv() {
    const keep = ['PATH', 'HOME', 'DOCKER_HOST', 'DOCKER_CONTEXT', 'DOCKER_CONFIG', 'XDG_RUNTIME_DIR'];
    const env = {};
    for (const k of keep) if (this.baseEnv[k] !== undefined) env[k] = this.baseEnv[k];
    Object.assign(env, {
      BENCH_HOME: this.home,
      BENCH_TASKS_DIR: this.tasksDir,
      BENCH_STACK_DIR: this.stackDir,
      BENCH_AGENT_DIR: this.agentDir ?? path.join(this.stackDir, 'no-agent'),
      BENCH_METER_PORT: String(this.meterPort),
      BENCH_GIT_PORT: String(this.ports.git),
      BENCH_CODER_1_PORT: String(this.ports.coder[1]),
      BENCH_CODER_2_PORT: String(this.ports.coder[2]),
      BENCH_SLOT_1_DIR: this.slotDir(1),
      BENCH_SLOT_2_DIR: this.slotDir(2),
      PODMAN_SECCOMP: this.seccomp,
    });
    return env;
  }

  composeArgs(args) {
    return ['compose', '-f', this.composeFile, '-p', this.project, ...args];
  }

  async compose(args, { input, timeoutMs = COMPOSE_TIMEOUT_MS } = {}) {
    const r = await this.exec('docker', this.composeArgs(args), { env: this.composeEnv(), input, timeoutMs });
    this.log({ event: 'compose', verb: args[0], exit: r.exitCode });
    if (r.exitCode !== 0 || r.timedOut) {
      const why = redact(r.stderr.trim().split('\n').slice(-3).join(' | '), this.secrets);
      throw new StackError(`docker compose ${args[0]} ${r.timedOut ? 'timed out' : `failed (exit ${r.exitCode})`}: ${why}`);
    }
    return r.stdout;
  }

  /** An env file a kill left behind holds a bearer and a run token: remove them. Returns the names removed. */
  removeStaleEnvFiles() {
    const gone = [];
    try {
      for (const f of fs.readdirSync(this.stackDir)) {
        if (/^slot-\d+\.env$/.test(f)) {
          fs.rmSync(path.join(this.stackDir, f), { force: true });
          gone.push(f);
        }
      }
    } catch {
      // no stack directory yet
    }
    return gone;
  }

  /** Make the directories the compose file binds, then start the shared services and every Podman service. */
  async up() {
    fs.mkdirSync(this.stackDir, { recursive: true, mode: 0o700 });
    fs.mkdirSync(path.join(this.stackDir, 'no-agent'), { recursive: true });
    for (let slot = 1; slot <= this.slots; slot++) fs.mkdirSync(this.slotDir(slot), { recursive: true, mode: 0o755 });
    this.removeStaleEnvFiles();
    await this.compose(['up', '-d', '--wait', '--build', ...serviceNames(this.slots)], { timeoutMs: 15 * 60 * 1000 });
  }

  async down({ volumes = false } = {}) {
    await this.compose(['down', '--remove-orphans', ...(volumes ? ['--volumes'] : [])], { timeoutMs: 5 * 60 * 1000 });
  }

  /** Run `script` with `sh -c` inside a service. Returns stdout. */
  async execIn(service, script, { timeoutMs = 120_000 } = {}) {
    return this.compose(['exec', '-T', service, 'sh', '-c', script], { timeoutMs });
  }

  /** A program's stdout, as bytes, from inside a service. */
  async streamFrom(service, argv, { maxBytes, timeoutMs = COMPOSE_TIMEOUT_MS } = {}) {
    const r = await this.execBuf('docker', this.composeArgs(['exec', '-T', service, ...argv]), { env: this.composeEnv(), timeoutMs, maxBytes });
    if (r.over) throw new StackError('the output of a container is larger than the limit');
    return r;
  }

  /** An empty database for the slot's coder (the coder migrates it itself at start). */
  async resetDatabase(slot) {
    const db = `adam_bench_${this.slotService('db', slot).slice(3)}`;
    const sql = `DROP DATABASE IF EXISTS ${db} WITH (FORCE); CREATE DATABASE ${db};`;
    await this.compose(['exec', '-T', 'postgres', 'psql', '-U', 'postgres', '-v', 'ON_ERROR_STOP=1', '-c', sql]);
  }

  /** The slot's work volume as it is when the coder first starts: nothing of an earlier run but the sandbox's directory. */
  async resetWork(slot) {
    const service = this.slotService('podman', slot);
    await this.execIn(service, 'find /work -mindepth 1 -maxdepth 1 ! -name .gitbox -exec rm -rf {} + ; mkdir -p /work/workspaces /work/.gitbox');
    const left = (await this.listDir(slot, '/work')).filter((n) => !['workspaces', '.gitbox'].includes(n));
    if (left.length > 0) throw new StackError(`the work volume still holds ${left.join(', ')}`);
  }

  /** The names in a directory of the slot's volume, as the service sees it. */
  async listDir(slot, dir) {
    if (!/^\/work(\/[A-Za-z0-9._-]+)*$/.test(dir) || dir.split('/').some((p) => p === '.' || p === '..')) throw new StackError('the directory is not a plain path on the work volume');
    const out = await this.execIn(this.slotService('podman', slot), `ls -1A ${dir} 2>/dev/null || true`);
    return out.split('\n').filter(Boolean);
  }

  /** Make a directory of the volume (`remove: false`) or remove one and check it is gone, in the service, as its owner. */
  async workDir(slot, dir, { remove = false } = {}) {
    if (!/^\/work\/[A-Za-z0-9][A-Za-z0-9._-]*(\/[A-Za-z0-9][A-Za-z0-9._-]*)?$/.test(dir)) throw new StackError('the directory is not directly under /work');
    const service = this.slotService('podman', slot);
    if (!remove) return void (await this.execIn(service, `mkdir -p ${dir} && chmod 777 ${dir}`)); // an image's user may not be the service's
    await this.execIn(service, `chmod -R u+rwX ${dir} 2>/dev/null; rm -rf ${dir}; test ! -e ${dir}`);
  }

  /** The files of a directory of the volume that the harness asked for by name, through a tar stream. Returns { files, refused }. */
  async readFiles(slot, dir, names, { maxFileBytes = 64 * 1024 * 1024 } = {}) {
    if (!/^\/work\/[A-Za-z0-9._/-]+$/.test(dir) || dir.split('/').some((p) => p === '..')) throw new StackError('the directory is not a plain path on the work volume');
    const r = await this.streamFrom(this.slotService('podman', slot), ['tar', '--format=posix', '-cf', '-', '--ignore-failed-read', '-C', dir, ...names], { maxBytes: names.length * maxFileBytes + 1024 * 1024 });
    return pickFiles(parseTar(r.stdout, { allowSymlinks: true, maxFileBytes }), names);
  }

  /** A tree of the volume (never its top-level `.git`) into `dest`, which the harness owns, through a tar stream. */
  async extractTree(slot, dir, dest, limits = {}) {
    if (!/^\/work\/workspaces\/[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/.test(dir) && !/^\/work\/[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/.test(dir)) throw new StackError('the tree is not a worktree of the volume');
    fs.rmSync(dest, { recursive: true, force: true });
    const r = await this.streamFrom(this.slotService('podman', slot), ['tar', '--format=posix', '-cf', '-', '--exclude=./.git', '-C', dir, '.'], { maxBytes: limits.maxBytes ?? 512 * 1024 * 1024 });
    if (r.exitCode !== 0) throw new StackError(`the tree could not be read (exit ${r.exitCode})`);
    extractEntries(parseTar(r.stdout, { allowSymlinks: true, ...limits }), dest);
  }

  /** A base repository of the slot's volume on the git server, pushed by a one-shot container with no secret. */
  async pushBase(slot, script) {
    await this.compose(['run', '--rm', '-T', '--no-deps', this.slotService('pusher', slot), script], { timeoutMs: 5 * 60 * 1000 });
  }

  /** Remove a run's repository from the git server. */
  async deleteRepo(name) {
    if (!/^[a-z0-9][a-z0-9-]{0,80}$/.test(name)) throw new StackError('the repository name is not plain');
    await this.compose(['exec', '-T', 'git-server', 'sh', '-c', `rm -rf /srv/git/bench/${name}.git; test ! -e /srv/git/bench/${name}.git`]);
  }

  /** Recreate the slot's coder with the run's settings (`env`: KEY -> value). The env file is gone when this returns. */
  async startCoder(slot, env) {
    const service = this.slotService('coder', slot);
    const lines = [];
    for (const [k, v] of Object.entries(env)) {
      if (!ENV_KEY.test(k) || typeof v !== 'string' || !SAFE_VALUE.test(v)) throw new StackError(`the setting ${k} is not plain (no line break, $, quote or backslash)`);
      lines.push(`${k}=${v}`);
      if (/BEARER|KEY|TOKEN|URL/.test(k)) for (const s of secretsOf(v)) this.secrets.add(s);
    }
    fs.mkdirSync(this.stackDir, { recursive: true, mode: 0o700 });
    const file = path.join(this.stackDir, `slot-${slot}.env`);
    fs.writeFileSync(file, `${lines.join('\n')}\n`, { mode: 0o600 });
    try {
      await this.compose(['up', '-d', '--force-recreate', '--no-deps', '--wait', service], { timeoutMs: 10 * 60 * 1000 });
    } finally {
      fs.rmSync(file, { force: true });
    }
  }

  /** Stop and remove the slot's coder, and confirm it. */
  async stopCoder(slot) {
    const service = this.slotService('coder', slot);
    await this.compose(['rm', '-sf', service], { timeoutMs: 2 * 60 * 1000 });
    const still = (await this.compose(['ps', '-aq', service])).trim();
    if (still !== '') throw new StackError(`the coder of slot ${slot} is not stopped`);
  }

  coderUrl(slot) {
    this.slotService('coder', slot);
    return `http://127.0.0.1:${this.ports.coder[slot]}/`;
  }

  /** The Podman client of a slot's service (or `grader`): `podman --remote` run inside it through `docker compose exec`. */
  containersIn(service) {
    const exec = (bin, args, opts, cb) => this.spawn('docker', this.composeArgs(['exec', '-T', service, bin, ...args]), { ...opts, env: this.composeEnv() }, cb);
    return new PodmanRemote({ bin: 'podman', url: PODMAN_URL, exec });
  }

  containers(slot) {
    return this.containersIn(this.slotService('podman', slot));
  }

  /** The Podman client of the service that grades: no agent runs there. */
  graderContainers() {
    return this.containersIn('grader');
  }

  /** Give a Podman service the internet while `fn` runs (a task image is pulled, a verifier needs the network), and take it away after. */
  async withEgress(service, fn) {
    const id = (await this.compose(['ps', '-q', service])).trim();
    if (!/^[0-9a-f]{12,64}$/.test(id)) throw new StackError(`the ${service} service is not running`);
    const network = `${this.project}_egress`;
    const docker = async (verb) => {
      const r = await this.exec('docker', ['network', verb, network, id], { env: this.composeEnv(), timeoutMs: 60_000 });
      if (r.exitCode !== 0 && !/already|not connected/.test(r.stderr)) throw new StackError(`docker network ${verb} failed (exit ${r.exitCode})`);
    };
    await docker('connect');
    try {
      return await fn();
    } finally {
      await docker('disconnect');
    }
  }

  pullWindow(slot, fn) {
    return this.withEgress(this.slotService('podman', slot), fn);
  }

  graderWindow(fn) {
    return this.withEgress('grader', fn);
  }
}
