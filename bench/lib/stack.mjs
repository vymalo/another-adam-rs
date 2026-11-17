// The benchmark stack: bench/compose.yaml driven through `docker compose`, and each slot's Podman service
// reached through `docker compose exec` (no socket is published and no port is opened to it).
//
// A coder's per-run settings (its model address with the run's meter token, its A2A bearer) go in an env file of
// mode 0600 that exists only while the coder is being created; they are in no command line, no log event and no
// error message (`redact`). The Podman client runs inside the service container, so a mount source is a path as
// the service sees it: the harness's home and the tasks directory are bound at the same path.
import { execFile } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { PodmanRemote } from './containers.mjs';

export class StackError extends Error {
  constructor(message) {
    super(message);
    this.name = 'StackError';
  }
}

export const PODMAN_URL = 'unix:///run/podman/podman.sock';
export const SLOTS = 2;
const SHARED = ['postgres', 'mock-github', 'git-server', 'meter'];
const SAFE_VALUE = /^[^\n\r\0$"'\\]*$/;
const ENV_KEY = /^[A-Za-z_][A-Za-z0-9_]*$/;

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

/** `text` with every secret removed. A secret shorter than 8 characters is not a secret and is left. */
export function redact(text, secrets) {
  let out = String(text);
  for (const s of secrets) if (typeof s === 'string' && s.length >= 8) out = out.split(s).join('[redacted]');
  return out;
}

export class Stack {
  /**
   * @param {object} o
   * @param {string} o.composeFile
   * @param {string} o.home       $BENCH_HOME, shared with the Podman services at the same path
   * @param {string} o.tasksDir   the repository's bench/tasks, shared read-only
   * @param {string} o.seccomp    the absolute path of dev/podman/seccomp.json
   * @param {number} o.meterPort  where the harness's meter listens on the host
   */
  constructor({ composeFile, home, tasksDir, seccomp, meterPort, agentDir = null, project = 'adam-bench', ports = {}, slots = SLOTS, exec = runCommand, spawn = execFile, log = () => {}, baseEnv = process.env }) {
    for (const [name, p] of [['composeFile', composeFile], ['home', home], ['tasksDir', tasksDir], ['seccomp', seccomp]]) {
      if (!path.isAbsolute(p)) throw new StackError(`${name} must be an absolute path`);
    }
    if (!/^[a-z0-9][a-z0-9_-]*$/.test(project)) throw new StackError('the project name is not plain');
    if (!Number.isInteger(meterPort) || meterPort < 1 || meterPort > 65535) throw new StackError('the meter port is not a port');
    Object.assign(this, { composeFile, home, tasksDir, seccomp, meterPort, agentDir, project, slots, exec, spawn, log });
    this.ports = { git: ports.git ?? 18083, coder: { 1: ports.coder1 ?? 18081, 2: ports.coder2 ?? 18082 } };
    this.stackDir = path.join(home, 'stack');
    this.baseEnv = baseEnv;
    this.secrets = new Set();
  }

  slotService(prefix, slot) {
    if (!Number.isInteger(slot) || slot < 1 || slot > this.slots) throw new StackError(`there is no slot ${String(slot)}; the stack has ${this.slots}`);
    return `${prefix}-${slot}`;
  }

  /** The environment of every compose call: what the file's `${...}` need, and nothing of the model. */
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
      PODMAN_SECCOMP: this.seccomp,
    });
    return env;
  }

  async compose(args, { input, timeoutMs = 0 } = {}) {
    const r = await this.exec('docker', ['compose', '-f', this.composeFile, '-p', this.project, ...args], { env: this.composeEnv(), input, timeoutMs });
    this.log({ event: 'compose', verb: args[0], exit: r.exitCode });
    if (r.exitCode !== 0) {
      const why = redact(r.stderr.trim().split('\n').slice(-3).join(' | '), this.secrets);
      throw new StackError(`docker compose ${args[0]} failed (exit ${r.exitCode}): ${why}`);
    }
    return r.stdout;
  }

  /** Make the directories the compose file binds, then start the shared services and every Podman service. */
  async up() {
    fs.mkdirSync(this.stackDir, { recursive: true, mode: 0o700 });
    fs.mkdirSync(path.join(this.stackDir, 'no-agent'), { recursive: true });
    const podmans = Array.from({ length: this.slots }, (_, i) => `podman-${i + 1}`);
    await this.compose(['up', '-d', '--wait', '--build', ...SHARED, ...podmans], { timeoutMs: 15 * 60 * 1000 });
  }

  async down({ volumes = false } = {}) {
    await this.compose(['down', '--remove-orphans', ...(volumes ? ['--volumes'] : [])]);
  }

  /** Run `script` with `sh -c` inside a service. Returns stdout. */
  async execIn(service, script, { timeoutMs = 120_000 } = {}) {
    return this.compose(['exec', '-T', service, 'sh', '-c', script], { timeoutMs });
  }

  /** An empty database for the slot's coder (the coder migrates it itself at start). */
  async resetDatabase(slot) {
    const db = `adam_bench_${this.slotService('db', slot).slice(3)}`;
    const sql = `DROP DATABASE IF EXISTS ${db} WITH (FORCE); CREATE DATABASE ${db};`;
    await this.compose(['exec', '-T', 'postgres', 'psql', '-U', 'postgres', '-v', 'ON_ERROR_STOP=1', '-c', sql]);
  }

  /** The slot's work volume as it is when the coder first starts: nothing of an earlier run. */
  async resetWork(slot) {
    await this.execIn(this.slotService('podman', slot), 'rm -rf /work/workspaces /work/git /work/coder /work/outputs /work/images; mkdir -p /work/workspaces');
  }

  /** The names in a directory of the slot's volume, as the service sees it. */
  async listDir(slot, dir) {
    if (!/^\/work(\/[A-Za-z0-9._-]+)*$/.test(dir) || dir.split('/').some((p) => p === '.' || p === '..')) throw new StackError('the directory is not a plain path on the work volume');
    const out = await this.execIn(this.slotService('podman', slot), `ls -1A ${dir} 2>/dev/null || true`);
    return out.split('\n').filter(Boolean);
  }

  /** A directory directly under /work/workspaces, made (or, with `remove`, removed) by the slot's service. */
  async workspaceDir(slot, dir, { remove = false } = {}) {
    if (!/^\/work\/workspaces\/[A-Za-z0-9][A-Za-z0-9._-]*$/.test(dir)) throw new StackError('the directory is not directly under /work/workspaces');
    await this.execIn(this.slotService('podman', slot), remove ? `rm -rf ${dir}` : `mkdir -p ${dir}`);
  }

  /** Recreate the slot's coder with the run's settings (`env`: KEY -> value). The env file is gone when this returns. */
  async startCoder(slot, env) {
    const service = this.slotService('coder', slot);
    const lines = [];
    for (const [k, v] of Object.entries(env)) {
      if (!ENV_KEY.test(k) || typeof v !== 'string' || !SAFE_VALUE.test(v)) throw new StackError(`the setting ${k} is not plain (no line break, $, quote or backslash)`);
      lines.push(`${k}=${v}`);
      if (/BEARER|KEY|TOKEN|URL/.test(k)) this.secrets.add(v);
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

  async stopCoder(slot) {
    await this.compose(['rm', '-sf', this.slotService('coder', slot)]);
  }

  coderUrl(slot) {
    this.slotService('coder', slot);
    return `http://127.0.0.1:${this.ports.coder[slot]}/`;
  }

  /** The Podman client of a slot's service: `podman --remote` run inside it through `docker compose exec`. */
  containers(slot) {
    const service = this.slotService('podman', slot);
    const exec = (bin, args, opts, cb) => this.spawn('docker', ['compose', '-f', this.composeFile, '-p', this.project, 'exec', '-T', service, bin, ...args], { ...opts, env: this.composeEnv() }, cb);
    return new PodmanRemote({ bin: 'podman', url: PODMAN_URL, exec });
  }

  /** Give the slot's Podman service the internet while `fn` runs (a task image is pulled), and take it away after. */
  async pullWindow(slot, fn) {
    const service = this.slotService('podman', slot);
    const id = (await this.compose(['ps', '-q', service])).trim();
    if (!/^[0-9a-f]{12,64}$/.test(id)) throw new StackError(`the ${service} service is not running`);
    const network = `${this.project}_egress`;
    const docker = async (verb) => {
      const r = await this.exec('docker', ['network', verb, network, id], { env: this.composeEnv() });
      if (r.exitCode !== 0 && !/already|not connected/.test(r.stderr)) throw new StackError(`docker network ${verb} failed (exit ${r.exitCode})`);
    };
    await docker('connect');
    try {
      return await fn();
    } finally {
      await docker('disconnect');
    }
  }
}
