// The benchmark stack: bench/compose.yaml driven through `docker compose`, and the containers the harness starts on
// the host's Docker (lib/containers.mjs). A slot's work directory is a directory of the harness's on the host, mounted
// at `/work` into the coder, the git sandbox, the OpenCode container and the helper that reads a tree.
//
// What the host trusts (bench/README.md): it runs no git in, reads no file from and deletes no file in a place a
// container could write. A tree or a log comes out as `tar -cf -` from a helper container and is parsed with limits
// (tarstream.mjs); what a container left is removed by a helper container, and checked to be gone. Directory names
// under the work directory are read on the host (a name is not a file).
//
// A coder's per-run settings (its model address with the run's meter token, its A2A bearer) go in an env file of
// mode 0600 that exists only while the coder is being created and is removed at the next start if a kill left it;
// they are in no command line, no log event and no error message (`redact`).
import { execFile, execFileSync, spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { HostContainers, buildRunArgs, dockerEnv } from './containers.mjs';
import { PINS, coderRef } from './pins.mjs';
import { RUNTIME, runtimeLimits, runtimeUser } from './runtime.mjs';
import { extractEntries, parseTar, pickFiles } from './tarstream.mjs';

export class StackError extends Error {
  constructor(message) {
    super(message);
    this.name = 'StackError';
  }
}

export const SLOTS = 2;
export const COMPOSE_TIMEOUT_MS = 2 * 60 * 1000;
export const DEFAULT_PATH = '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin';
const SAFE_VALUE = /^[^\n\r\0$"'\\]*$/;
const ENV_KEY = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** The services of compose.yaml that have a `build:` (a test holds this equal to the file). */
const BUILT_SERVICES = ['git-server'];

/** The services a start brings up, for `slots` slots. The arms and the grading are host containers, not services. */
export const serviceNames = (slots = SLOTS) => [
  'postgres',
  'mock-github',
  'git-server',
  'pub-git',
  ...Array.from({ length: slots }, (_, i) => [`gitbox-${i + 1}`, `meter-${i + 1}`, `pub-coder-${i + 1}`]).flat(),
];

/** The settings every coder of a slot gets; the run's own (model address, bearer, window) come from `startCoder`. Pure. */
export function coderEnvironment({ slot, publicUrl, databaseUrl, imagePath = DEFAULT_PATH }) {
  return {
    MODEL_API_KEY: 'bench-dummy-key', // the meter holds the real key
    MODEL: 'bench-model',
    WORKSPACE_ROOT: '/work',
    WORKSPACE_SWEEP_SECS: '0',
    ALLOWED_REPO_HOSTS: 'git-server',
    ALLOW_LOCAL_REPOS: 'true',
    GITHUB_API_URL: 'http://mock-github:8080',
    GITHUB_TOKEN: 'bench-dummy-token',
    ADAM_AGENT_DIR: '/etc/adam/agent',
    RUN_ENVIRONMENT: 'local',
    DEVCONTAINER_RUNTIME: 'off',
    GIT_SANDBOX_SOCKET: '/work/.gitbox/git.sock',
    LISTEN_ADDR: '0.0.0.0:8080',
    DATABASE_URL: databaseUrl,
    PUBLIC_URL: publicUrl,
    DEVCONTAINER_DEPLOYMENT_ID: `bench-slot-${slot}`,
    PATH: `/opt/adam/bin:${imagePath}`,
  };
}

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

/**
 * Gradings share the verifier network; a window with the internet is alone. A writer that is waiting holds back new
 * readers, so a pull is not starved by a stream of gradings. Both release in a `finally`: a failure never leaves the
 * lock held.
 */
export class RWLock {
  #readers = 0;
  #writing = false;
  #queue = [];

  async shared(fn) {
    await new Promise((resolve) => {
      this.#queue.push({ write: false, resolve });
      this.#pump();
    });
    try {
      return await fn();
    } finally {
      this.#readers--;
      this.#pump();
    }
  }

  async exclusive(fn) {
    await new Promise((resolve) => {
      this.#queue.push({ write: true, resolve });
      this.#pump();
    });
    try {
      return await fn();
    } finally {
      this.#writing = false;
      this.#pump();
    }
  }

  #pump() {
    while (this.#queue.length > 0) {
      const next = this.#queue[0];
      if (this.#writing) return;
      if (next.write) {
        if (this.#readers > 0) return;
        this.#writing = true;
      } else this.#readers++;
      this.#queue.shift();
      next.resolve();
    }
  }
}

export class Stack {
  /**
   * @param {object} o
   * @param {string} o.composeFile
   * @param {string} o.home       $BENCH_HOME: the harness's own, never mounted into a container that runs an agent
   * @param {number} o.meterPort  where the harness's meter listens on the host
   * @param {string} [o.coderImage] the pinned coder image, the source of the coder's binaries and the helpers' tools
   */
  constructor({ composeFile, home, meterPort, coderImage = coderRef(PINS), agentDir = null, project = 'adam-bench', ports = {}, slots = SLOTS, exec = runCommand, execBuf = runBuffer, spawn: spawnFn = execFile, log = () => {}, baseEnv = process.env, meterHost = null }) {
    for (const [name, p] of [['composeFile', composeFile], ['home', home]]) {
      if (!path.isAbsolute(p)) throw new StackError(`${name} must be an absolute path`);
    }
    if (!/^[a-z0-9][a-z0-9_-]*$/.test(project)) throw new StackError('the project name is not plain');
    if (!Number.isInteger(meterPort) || meterPort < 1 || meterPort > 65535) throw new StackError('the meter port is not a port');
    Object.assign(this, { composeFile, home, meterPort, coderImage, agentDir, project, slots, exec, execBuf, spawn: spawnFn, log });
    this.ports = { git: ports.git ?? 18083, coder: { 1: ports.coder1 ?? 18081, 2: ports.coder2 ?? 18082 } };
    this.stackDir = path.join(home, 'stack');
    this.baseEnv = baseEnv;
    const build = baseEnv.BENCH_COMPOSE_BUILD;
    if (build !== undefined && build !== '0' && build !== '1') throw new StackError('BENCH_COMPOSE_BUILD is 0 or 1');
    /** `BENCH_COMPOSE_BUILD=0`: start without `--build`; the images compose would build must already exist (the doctor checks). */
    this.composeBuild = build !== '0';
    this.meterAddress = meterHost;
    this.secrets = new Set();
    this.gates = new Map();
    this.imageEnvCache = null;
  }

  /** The images compose builds from this repository (a service with `build:` in compose.yaml), as compose names them. */
  builtImages() {
    return BUILT_SERVICES.map((service) => `${this.project}-${service}`);
  }

  /** One lock for each service whose network is opened for a window: its windows and its gradings are told apart by it. */
  gate(service) {
    if (!this.gates.has(service)) this.gates.set(service, new RWLock());
    return this.gates.get(service);
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

  /** The slot's work directory on the host; the containers mount it at `/work`. */
  workRoot(slot) {
    this.slotService('work', slot);
    return path.join(this.home, 'work', String(slot));
  }

  /** The meter's name for the containers of a slot. */
  meterHost(slot) {
    return this.slotService('meter', slot);
  }

  /** The host's own address, as a container on the egress network reaches it: the source of the default route. */
  meterHostAddress() {
    if (this.meterAddress) return this.meterAddress;
    try {
      // `ip` is a host program the harness starts, so it gets the same allow-list as a `docker` child: it must not
      // carry the model's key while it runs (`ps e`).
      const r = execFileSync('ip', ['-4', 'route', 'get', '1.1.1.1'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], env: this.dockerEnv() });
      const m = /\bsrc\s+(\d{1,3}(?:\.\d{1,3}){3})\b/.exec(r);
      if (m && !m[1].startsWith('127.')) return (this.meterAddress = m[1]);
    } catch {
      // no `ip`: fall back to the interfaces
    }
    for (const list of Object.values(os.networkInterfaces())) for (const a of list ?? []) if (a.family === 'IPv4' && !a.internal && !a.address.startsWith('127.')) return (this.meterAddress = a.address);
    return (this.meterAddress = '127.0.0.1');
  }

  /** The environment of every compose call: what the file's `${...}` need, and nothing of the model's. */
  composeEnv() {
    const env = this.dockerEnv();
    Object.assign(env, {
      BENCH_WORK_1_DIR: this.workRoot(1),
      BENCH_WORK_2_DIR: this.workRoot(2),
      BENCH_METER_HOST: this.meterHostAddress(),
      BENCH_METER_PORT: String(this.meterPort),
      BENCH_GIT_PORT: String(this.ports.git),
      BENCH_CODER_1_PORT: String(this.ports.coder[1]),
      BENCH_CODER_2_PORT: String(this.ports.coder[2]),
    });
    return env;
  }

  /** The environment the docker CLI gets: what finds the daemon, and none of the model's variables. */
  dockerEnv() {
    return dockerEnv(this.baseEnv);
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

  /** A docker call outside compose: the coder, a probe, a network. */
  async docker(args, { timeoutMs = 120_000, input } = {}) {
    return this.exec('docker', args, { env: this.dockerEnv(), input, timeoutMs });
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

  /** Make the directories the compose file binds and the harness's work directories, then start the shared services. */
  async up() {
    fs.mkdirSync(this.stackDir, { recursive: true, mode: 0o700 });
    for (let slot = 1; slot <= this.slots; slot++) {
      fs.mkdirSync(this.workRoot(slot), { recursive: true, mode: 0o777 });
      fs.chmodSync(this.workRoot(slot), 0o777);
      fs.mkdirSync(this.slotDir(slot), { recursive: true, mode: 0o755 });
    }
    this.removeStaleEnvFiles();
    await this.compose(['up', '-d', '--wait', ...(this.composeBuild ? ['--build'] : []), ...serviceNames(this.slots)], { timeoutMs: 15 * 60 * 1000 });
    // Compose does not make a network that has no service; the gradings need theirs.
    await this.ensureNetwork('verifier');
  }

  async down({ volumes = false } = {}) {
    // The arms and the coder are host containers, not compose services: a killed harness leaves them.
    for (let slot = 1; slot <= this.slots; slot++) {
      await this.containers(slot).reapAll().catch(() => {});
      await this.coderContainers(slot).reapAll().catch(() => {});
    }
    await this.graderContainers().reapAll().catch(() => {});
    await this.compose(['down', '--remove-orphans', ...(volumes ? ['--volumes'] : [])], { timeoutMs: 5 * 60 * 1000 });
    await this.docker(['network', 'rm', `${this.project}_verifier`]).catch(() => {});
  }

  /** `adam-bench_verifier` (internal): made here, because no compose service is on it. Returns its name. */
  async ensureNetwork(name) {
    const full = `${this.project}_${name}`;
    const inspect = await this.docker(['network', 'inspect', '--format', '{{.Internal}}', full], { timeoutMs: 60_000 });
    if (inspect.exitCode === 0) {
      // The flag is what keeps a grading off the internet: a network that exists without it would silently open every
      // grading, so it is refused, never used.
      if (inspect.stdout.trim() !== 'true') throw new StackError(`the ${full} network exists and is not internal: remove it (docker network rm ${full}) and start again`);
      return full;
    }
    const created = await this.docker(['network', 'create', '--internal', full], { timeoutMs: 60_000 });
    if (created.exitCode !== 0 && !/already exists/.test(created.stderr)) throw new StackError(`the ${full} network could not be made (exit ${created.exitCode}): ${created.stderr.trim()}`);
    return full;
  }

  /** Run `script` with `sh -c` inside a compose service. Returns stdout. */
  async execIn(service, script, { timeoutMs = 120_000 } = {}) {
    return this.compose(['exec', '-T', service, 'sh', '-c', script], { timeoutMs });
  }

  /** An empty database for the slot's coder (the coder migrates it itself at start). */
  async resetDatabase(slot) {
    const db = `adam_bench_${this.slotService('db', slot).slice(3)}`;
    // Two statements: psql sends each -c in its own transaction, and DROP DATABASE cannot run inside one.
    await this.compose(['exec', '-T', 'postgres', 'psql', '-U', 'postgres', '-v', 'ON_ERROR_STOP=1', '-c', `DROP DATABASE IF EXISTS ${db} WITH (FORCE)`, '-c', `CREATE DATABASE ${db}`]);
  }

  /** A helper container of the coder image, as uid 10001, with the slot's work directory at `/work`. */
  async helper(slot, script, { readOnly = false, timeoutMs = 120_000, user = runtimeUser() } = {}) {
    return this.containers(slot).run({
      image: this.coderImage,
      script,
      network: 'none',
      user,
      mounts: [{ source: this.workRoot(slot), target: '/work', readOnly }],
      timeoutMs,
    });
  }

  /**
   * The slot's work directory as it is when the coder first starts: nothing of an earlier run but the sandbox's directory
   * and the names `keep` lists. The runner keeps nothing: `/work/bench-base` is shared by every task, so a base kept
   * from another task would be the wrong repository (the host's copy is still cached per task, image and workdir).
   */
  async resetWork(slot, { keep = [] } = {}) {
    if (!keep.every((n) => /^[A-Za-z0-9._-]+$/.test(n))) throw new StackError('a kept name is not plain');
    const kept = keep.map((n) => `! -name ${n} `).join('');
    // root inside the helper is the host user under rootless Docker: it can remove the harness's own directories,
    // whatever user wrote in them; the helper mounts the work directory only.
    const r = await this.helper(slot, `find /work -mindepth 1 -maxdepth 1 ! -name .gitbox ${kept}-exec rm -rf {} + ; mkdir -p /work/workspaces /work/.gitbox && chmod 777 /work/workspaces /work/.gitbox`, { user: '0:0' });
    if (r.exitCode !== 0) throw new StackError(`the work directory of slot ${slot} could not be emptied (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-2).join(' | ')}`);
    const left = (await this.listDir(slot, '/work')).filter((n) => !['workspaces', '.gitbox', ...keep].includes(n));
    if (left.length > 0) throw new StackError(`the work volume still holds ${left.join(', ')}`);
  }

  /** The names in a directory of the slot's work directory. A name is not a file: nothing is opened here. */
  async listDir(slot, dir, { dirsOnly = false } = {}) {
    if (!/^\/work(\/[A-Za-z0-9._-]+)*$/.test(dir) || dir.split('/').some((p) => p === '.' || p === '..')) throw new StackError('the directory is not a plain path on the work volume');
    const rel = dir === '/work' ? '' : dir.slice('/work/'.length);
    try {
      return fs
        .readdirSync(path.join(this.workRoot(slot), rel), { withFileTypes: true })
        .filter((e) => !dirsOnly || e.isDirectory())
        .map((e) => e.name)
        .sort();
    } catch (e) {
      if (e.code === 'ENOENT') return [];
      throw e;
    }
  }

  /** Make a directory of the work directory (`remove: false`) or remove one and check it is gone, as uid 10001. */
  async workDir(slot, dir, { remove = false } = {}) {
    if (!/^\/work\/[A-Za-z0-9][A-Za-z0-9._-]*(\/[A-Za-z0-9][A-Za-z0-9._-]*)?$/.test(dir)) throw new StackError('the directory is not directly under /work');
    if (!remove) {
      fs.mkdirSync(this.workRoot(slot), { recursive: true, mode: 0o777 });
      const rel = dir.slice('/work/'.length).split('/');
      let host = this.workRoot(slot);
      for (const part of rel) {
        host = path.join(host, part);
        if (!fs.existsSync(host)) {
          fs.mkdirSync(host, { mode: 0o777 });
          fs.chmodSync(host, 0o777);
        }
      }
      return;
    }
    const r = await this.helper(slot, `chmod -R u+rwX ${dir} 2>/dev/null; rm -rf ${dir}; test ! -e ${dir}`, { user: '0:0' });
    if (r.exitCode !== 0) throw new StackError(`the directory ${dir} could not be removed (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-2).join(' | ')}`);
  }

  /** A program's stdout, as bytes, from a helper container that mounts the work directory read-only. */
  async streamFrom(slot, argv, { maxBytes, timeoutMs = COMPOSE_TIMEOUT_MS } = {}) {
    const r = await this.execBuf('docker', ['run', '--rm', '--pull=never', '--network=none', '--user', runtimeUser(), '--entrypoint', '/bin/tar', '-v', `${this.workRoot(slot)}:/work:ro`, this.coderImage, ...argv], { env: this.dockerEnv(), timeoutMs, maxBytes });
    if (r.over) throw new StackError('the output of a container is larger than the limit');
    return r;
  }

  /** The files of a directory of the work directory that the harness asked for by name, through a tar stream. Returns { files, refused }. */
  async readFiles(slot, dir, names, { maxFileBytes = 64 * 1024 * 1024 } = {}) {
    if (!/^\/work\/[A-Za-z0-9._/-]+$/.test(dir) || dir.split('/').some((p) => p === '..')) throw new StackError('the directory is not a plain path on the work volume');
    const r = await this.streamFrom(slot, ['--format=posix', '-cf', '-', '--ignore-failed-read', '-C', dir, ...names], { maxBytes: names.length * maxFileBytes + 1024 * 1024 });
    return pickFiles(parseTar(r.stdout, { allowSymlinks: true, maxFileBytes }), names);
  }

  /** A tree of the work directory (never its top-level `.git`) into `dest`, which the harness owns, through a tar stream. */
  async extractTree(slot, dir, dest, limits = {}) {
    if (!/^\/work\/workspaces\/[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/.test(dir) && !/^\/work\/[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/.test(dir)) throw new StackError('the tree is not a worktree of the volume');
    fs.rmSync(dest, { recursive: true, force: true });
    const r = await this.streamFrom(slot, ['--format=posix', '-cf', '-', '--exclude=./.git', '-C', dir, '.'], { maxBytes: limits.maxBytes ?? 512 * 1024 * 1024 });
    if (r.exitCode !== 0) throw new StackError(`the tree could not be read (exit ${r.exitCode})`);
    const entries = parseTar(r.stdout, { allowSymlinks: true, ...limits });
    extractEntries(entries, dest);
    return { refused: entries.refused }; // a FIFO, a device or a file over its cap: skipped, and recorded
  }

  /** A base repository of the slot's work directory on the git server, pushed by a one-shot container with no secret. */
  async pushBase(slot, script) {
    await this.compose(['run', '--rm', '-T', '--no-deps', this.slotService('pusher', slot), script], { timeoutMs: 5 * 60 * 1000 });
  }

  /** Remove a run's repository from the git server. */
  async deleteRepo(name) {
    if (!/^[a-z0-9][a-z0-9-]{0,80}$/.test(name)) throw new StackError('the repository name is not plain');
    await this.compose(['exec', '-T', 'git-server', 'sh', '-c', `rm -rf /srv/git/bench/${name}.git; test ! -e /srv/git/bench/${name}.git`]);
  }

  /** The coder's binaries, copied out of the pinned coder image once (never run there). */
  async ensureCoderFiles() {
    const dir = path.join(this.stackDir, 'coder-bin');
    const marker = path.join(dir, '.image');
    try {
      if (fs.readFileSync(marker, 'utf8').trim() === this.coderImage) return dir;
    } catch {
      // not made yet
    }
    fs.rmSync(dir, { recursive: true, force: true });
    fs.mkdirSync(dir, { recursive: true, mode: 0o755 });
    await this.host().extract(this.coderImage, ['/usr/local/bin/adam-coder', '/usr/local/bin/adam-git'], dir);
    for (const f of ['adam-coder', 'adam-git']) {
      try {
        fs.chmodSync(path.join(dir, f), 0o755);
      } catch {
        throw new StackError(`the coder's ${f} could not be copied out of ${this.coderImage}: pull the image and retry`);
      }
    }
    fs.writeFileSync(marker, `${this.coderImage}\n`);
    return dir;
  }

  /** The image's own environment, read once for each image (`docker image inspect`, never a run). */
  async imageEnvOf(image) {
    this.imageEnvCache ??= new Map();
    if (!this.imageEnvCache.has(image)) this.imageEnvCache.set(image, await this.host().imageEnv(image));
    return this.imageEnvCache.get(image);
  }

  /** The coder image's own environment (its PATH), the fallback when a task image sets none. */
  async coderImageEnv() {
    return this.imageEnvOf(this.coderImage);
  }

  /**
   * Start the slot's coder: a container of the task image, as uid 10001, with the coder's binaries and agent folder
   * mounted read-only and `RUN_ENVIRONMENT=local`, so its commands run in that container. The run's settings go in
   * an env file of mode 0600 that is gone when this returns. The coder is up when its A2A port answers.
   */
  async startCoder(slot, env, { image } = {}) {
    const name = this.slotService('coder', slot);
    if (typeof image !== 'string' || !/@sha256:[0-9a-f]{64}$/.test(image)) throw new StackError('the coder starts from the task image, named by digest');
    if (!this.agentDir) throw new StackError('the coder needs its agent folder: set the stack\'s agentDir before startCoder');
    // The task image's own PATH, so the coder finds the tools that image installs (a conda or a /opt toolchain); the
    // coder image's is the fallback, and /opt/adam/bin (its binaries) comes first.
    const taskEnv = await this.imageEnvOf(image);
    const pathEnv = taskEnv.PATH ? taskEnv : await this.coderImageEnv();
    const settings = {
      ...coderEnvironment({ slot, publicUrl: this.coderUrl(slot), databaseUrl: `postgres://postgres:postgres@postgres:5432/adam_bench_${slot}`, imagePath: pathEnv.PATH ?? DEFAULT_PATH }),
      ...env,
    };
    const lines = [];
    for (const [k, v] of Object.entries(settings)) {
      if (!ENV_KEY.test(k) || typeof v !== 'string' || !SAFE_VALUE.test(v)) throw new StackError(`the setting ${k} is not plain (no line break, $, quote or backslash)`);
      lines.push(`${k}=${v}`);
      if (/BEARER|KEY|TOKEN|URL/.test(k)) for (const s of secretsOf(v)) this.secrets.add(s);
    }
    const files = await this.ensureCoderFiles();
    fs.mkdirSync(this.stackDir, { recursive: true, mode: 0o700 });
    const file = path.join(this.stackDir, `slot-${slot}.env`);
    fs.writeFileSync(file, `${lines.join('\n')}\n`, { mode: 0o600 });
    try {
      // A killed harness may have left a coder with this name.
      await this.docker(['rm', '-f', name]).catch(() => {});
      const args = buildRunArgs(
        {
          image,
          detach: true,
          name,
          network: `${this.project}_slot-${slot}`,
          user: runtimeUser(),
          ...runtimeLimits(),
          labels: { 'bench.project': this.project, 'bench.slot': String(slot), 'bench.role': 'coder' },
          envFile: file,
          mounts: [
            { source: this.workRoot(slot), target: '/work' },
            { source: this.agentDir, target: '/etc/adam/agent', readOnly: true },
            { source: files, target: '/opt/adam/bin', readOnly: true },
          ],
          entrypoint: '/opt/adam/bin/adam-coder',
        },
        { name },
      );
      const r = await this.docker(args, { timeoutMs: 120_000 });
      if (r.exitCode !== 0) throw new StackError(`the coder of slot ${slot} did not start (exit ${r.exitCode}): ${redact(r.stderr.trim().split('\n').slice(-3).join(' | '), this.secrets)}`);
      await this.waitForCoder(slot, name);
    } finally {
      fs.rmSync(file, { force: true });
    }
  }

  /** Wait until the coder's A2A port answers, or say why it stopped; a container that exits is the reason. */
  async waitForCoder(slot, name, { timeoutMs = 120_000 } = {}) {
    const port = this.ports.coder[slot];
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const state = await this.docker(['inspect', '--format', '{{.State.Status}}', name], { timeoutMs: 30_000 });
      if (state.exitCode !== 0) throw new StackError(`the coder of slot ${slot} is gone: ${redact(state.stderr.trim(), this.secrets)}`);
      if (!['running', 'created'].includes(state.stdout.trim())) {
        const logs = await this.docker(['logs', '--tail', '20', name], { timeoutMs: 30_000 });
        throw new StackError(`the coder of slot ${slot} ended (${state.stdout.trim()}): ${redact(`${logs.stdout}${logs.stderr}`.trim().split('\n').slice(-4).join(' | '), this.secrets)}`);
      }
      if (await this.portAnswers(port)) return;
      if (Date.now() > deadline) throw new StackError(`the coder of slot ${slot} did not answer on 127.0.0.1:${port} in ${Math.round(timeoutMs / 1000)} s`);
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
  }

  /** Any HTTP answer on the loopback port means the coder listens; the status does not matter. */
  portAnswers(port) {
    return new Promise((resolve) => {
      const req = http.get({ host: '127.0.0.1', port, path: '/', timeout: 1000 }, (res) => {
        res.resume();
        resolve(true);
      });
      req.on('timeout', () => {
        req.destroy();
        resolve(false);
      });
      req.on('error', () => resolve(false));
    });
  }

  /** Stop and remove the slot's coder, and confirm it. */
  async stopCoder(slot) {
    const name = this.slotService('coder', slot);
    await this.docker(['rm', '-f', name], { timeoutMs: 2 * 60 * 1000 });
    const still = await this.docker(['ps', '-aq', '--filter', `name=^${name}$`], { timeoutMs: 60_000 });
    if (still.stdout.trim() !== '') throw new StackError(`the coder of slot ${slot} is not stopped`);
  }

  coderUrl(slot) {
    this.slotService('coder', slot);
    return `http://127.0.0.1:${this.ports.coder[slot]}/`;
  }

  /** A HostContainers over this stack's promise-returning exec (the tests' fake is async too). */
  host(labels = {}, aliases = {}, prefix = `${this.project}_`) {
    return new HostContainers({
      labels,
      prefix,
      aliases,
      invoke: (args, { timeoutMs, input }) => this.exec('docker', args, { env: this.dockerEnv(), timeoutMs, input }),
    });
  }

  /** The host containers of a slot's runs: only theirs are listed and reaped (`bench.slot`, `bench.role=run`). */
  containers(slot) {
    this.slotService('work', slot);
    return this.host({ 'bench.project': this.project, 'bench.slot': String(slot), 'bench.role': 'run' }, { slot: `${this.project}_slot-${slot}` });
  }

  /** The coder container of a slot, for the check that the run's image was the task's. */
  coderContainers(slot) {
    this.slotService('coder', slot);
    return this.host({ 'bench.project': this.project, 'bench.slot': String(slot), 'bench.role': 'coder' });
  }

  /** The containers that grade: no agent runs under these labels. A network the owner named is used as it is. */
  graderContainers() {
    return this.host({ 'bench.project': this.project, 'bench.role': 'grader' }, { verifier: `${this.project}_verifier`, egress: `${this.project}_egress` }, '');
  }

  /** Containers that neither arm owns (a probe): their labels keep them out of a reap of a slot or the grader. */
  hostContainers() {
    return this.host({ 'bench.project': this.project, 'bench.role': 'probe' });
  }

  /** A pull from the host: the internet is the host's, so only the slot's lock is kept (one pull at a time). */
  pullWindow(slot, fn) {
    return this.gate(this.slotService('slot', slot)).exclusive(fn);
  }

  /** A grading whose verifier needs the internet: alone, on the egress network (its container is started with it). */
  graderWindow(fn) {
    return this.gate('grader').exclusive(fn);
  }

  /** A grading with no network: it runs with others, never while a window is open. */
  graderShared(fn) {
    return this.gate('grader').shared(fn);
  }
}
