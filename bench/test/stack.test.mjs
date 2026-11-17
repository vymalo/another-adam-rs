import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { PODMAN_URL, Stack, StackError, redact } from '../lib/stack.mjs';
import { PINS, coderRef } from '../lib/pins.mjs';
import { tmp } from './helpers.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const COMPOSE = path.join(HERE, '..', 'compose.yaml');

function makeStack(t, { exec, spawn, baseEnv } = {}) {
  const home = tmp(t);
  const calls = [];
  const logs = [];
  const stack = new Stack({
    composeFile: COMPOSE,
    home,
    tasksDir: '/repo/bench/tasks',
    seccomp: '/repo/dev/podman/seccomp.json',
    meterPort: 18090,
    exec: exec ?? (async (file, args, opts) => {
      calls.push({ file, args, opts, envFile: snapshotEnvFiles(home) });
      return { exitCode: 0, stdout: '', stderr: '' };
    }),
    spawn,
    log: (e) => logs.push(e),
    baseEnv: baseEnv ?? { PATH: '/usr/bin', HOME: '/home/x', BENCH_MODEL_API_KEY: 'the-real-model-key-123456', ANTHROPIC_API_KEY: 'another-secret-value' },
  });
  return { stack, calls, logs, home };
}

function snapshotEnvFiles(home) {
  const dir = path.join(home, 'stack');
  const out = {};
  try {
    for (const f of fs.readdirSync(dir)) {
      if (f.endsWith('.env')) out[f] = { text: fs.readFileSync(path.join(dir, f), 'utf8'), mode: fs.statSync(path.join(dir, f)).mode & 0o777 };
    }
  } catch {
    // no stack directory yet
  }
  return out;
}

test('up starts the shared services and one Podman service for each slot, with the file, the project and the variables it needs', async (t) => {
  const { stack, calls } = makeStack(t);
  await stack.up();
  const [c] = calls;
  assert.equal(c.file, 'docker');
  assert.deepEqual(c.args, ['compose', '-f', COMPOSE, '-p', 'adam-bench', 'up', '-d', '--wait', '--build', 'postgres', 'mock-github', 'git-server', 'meter', 'podman-1', 'podman-2']);
  assert.equal(c.opts.env.BENCH_METER_PORT, '18090');
  assert.equal(c.opts.env.PODMAN_SECCOMP, '/repo/dev/podman/seccomp.json');
  assert.ok(path.isAbsolute(c.opts.env.BENCH_HOME));
});

test('a compose call gets the variables of the file and none of the model\'s: the key is not in its environment', async (t) => {
  const { stack, calls } = makeStack(t);
  await stack.up();
  const env = calls[0].opts.env;
  assert.ok(!Object.values(env).some((v) => v.includes('the-real-model-key') || v.includes('another-secret-value')));
  assert.equal(env.BENCH_MODEL_API_KEY, undefined);
});

test('the coder\'s settings are in a file of mode 0600 while it is created, never in a command line, and the file is gone after', async (t) => {
  const { stack, calls, logs, home } = makeStack(t);
  const bearer = 'bearer-value-aaaaaaaaaaaaaaaa';
  const url = 'http://meter:8090/r/tokentokentokentokentoken/v1';
  await stack.startCoder(1, { A2A_BEARER_TOKENS: bearer, MODEL_BASE_URL: url, DEVCONTAINER_DEFAULT_IMAGE: 'registry.example.test/img@sha256:' + 'a'.repeat(64) });
  const [c] = calls;
  assert.deepEqual(c.args.slice(0, 4), ['compose', '-f', COMPOSE, '-p']);
  assert.deepEqual(c.args.slice(-6), ['up', '-d', '--force-recreate', '--no-deps', '--wait', 'coder-1']);
  const file = c.envFile['slot-1.env'];
  assert.equal(file.mode, 0o600);
  assert.match(file.text, /^A2A_BEARER_TOKENS=bearer-value-aaaaaaaaaaaaaaaa$/m);
  assert.ok(!JSON.stringify(c.args).includes(bearer) && !JSON.stringify(c.args).includes('tokentoken'));
  assert.ok(!JSON.stringify(logs).includes(bearer) && !JSON.stringify(logs).includes('tokentoken'));
  assert.deepEqual(fs.readdirSync(path.join(home, 'stack')).filter((f) => f.endsWith('.env')), []);
});

test('the file is removed, and the error holds no secret, when compose fails', async (t) => {
  const bearer = 'bearer-value-bbbbbbbbbbbbbbbb';
  const { stack, home } = makeStack(t, { exec: async () => ({ exitCode: 1, stdout: '', stderr: `error: variable A2A_BEARER_TOKENS=${bearer} is invalid\nsecond line` }) });
  await assert.rejects(stack.startCoder(2, { A2A_BEARER_TOKENS: bearer }), (e) => e instanceof StackError && !e.message.includes(bearer) && /exit 1/.test(e.message));
  assert.deepEqual(fs.readdirSync(path.join(home, 'stack')).filter((f) => f.endsWith('.env')), []);
});

test('a setting that is not plain, a slot that does not exist and a path outside the volume are refused', async (t) => {
  const { stack, calls } = makeStack(t);
  for (const v of ['a\nb=c', 'a$b', 'a"b', "a'b", 'a\\b']) await assert.rejects(stack.startCoder(1, { X: v }), /not plain/);
  await assert.rejects(stack.startCoder(1, { 'bad key': 'x' }), /not plain/);
  await assert.rejects(stack.startCoder(3, { X: 'y' }), /no slot 3/);
  await assert.rejects(stack.startCoder(0, { X: 'y' }), /no slot 0/);
  await assert.rejects(stack.listDir(1, '/etc'), /plain path/);
  await assert.rejects(stack.listDir(1, '/work/../etc'), /plain path/);
  assert.equal(calls.length, 0, 'nothing was run');
});

test('redact removes long secrets and leaves short words', () => {
  assert.equal(redact('a secret-value-12345 b', ['secret-value-12345']), 'a [redacted] b');
  assert.equal(redact('a abc b', ['abc']), 'a abc b');
});

test('the database and the work volume of a slot are reset by the service that owns them', async (t) => {
  const { stack, calls } = makeStack(t);
  await stack.resetDatabase(2);
  await stack.resetWork(1);
  const tail = (c) => c.args.slice(5);
  assert.deepEqual(tail(calls[0]).slice(0, 3), ['exec', '-T', 'postgres']);
  assert.match(calls[0].args.at(-1), /DROP DATABASE IF EXISTS adam_bench_2 WITH \(FORCE\); CREATE DATABASE adam_bench_2;/);
  assert.deepEqual(tail(calls[1]).slice(0, 4), ['exec', '-T', 'podman-1', 'sh']);
  assert.match(calls[1].args.at(-1), /rm -rf \/work\/workspaces/);
});

test('the Podman client of a slot runs inside its service, through compose exec, on the service\'s own socket', async (t) => {
  const spawned = [];
  const { stack } = makeStack(t, {
    spawn: (file, args, opts, cb) => {
      spawned.push({ file, args });
      cb(null, 'sha256:' + 'c'.repeat(64) + '\n', '');
      return { stdin: { end() {} } };
    },
  });
  const containers = stack.containers(2);
  assert.equal(await containers.digestOf('registry.example.test/img:1'), 'sha256:' + 'c'.repeat(64));
  const [s] = spawned;
  assert.equal(s.file, 'docker');
  assert.deepEqual(s.args.slice(0, 9), ['compose', '-f', COMPOSE, '-p', 'adam-bench', 'exec', '-T', 'podman-2', 'podman']);
  assert.deepEqual(s.args.slice(9, 12), ['--remote', '--url', PODMAN_URL]);
});

test('the pull window connects the service to the network with the internet and takes it away, whatever happens', async (t) => {
  const calls = [];
  const id = 'f'.repeat(12);
  const { stack } = makeStack(t, {
    exec: async (file, args) => {
      calls.push([file, ...args.filter((a) => !a.startsWith('/') && a !== COMPOSE)]);
      return { exitCode: 0, stdout: args.includes('ps') ? `${id}\n` : '', stderr: '' };
    },
  });
  await stack.pullWindow(1, async () => calls.push('pulling'));
  await assert.rejects(stack.pullWindow(1, async () => { throw new Error('pull failed'); }), /pull failed/);
  const nets = calls.filter((c) => c[1] === 'network').map((c) => c.slice(1).join(' '));
  assert.deepEqual(nets, [`network connect adam-bench_egress ${id}`, `network disconnect adam-bench_egress ${id}`, `network connect adam-bench_egress ${id}`, `network disconnect adam-bench_egress ${id}`]);
  assert.ok(calls.indexOf('pulling') > calls.findIndex((c) => c[1] === 'network'));
});

test('the coder\'s address is the loopback port the compose file publishes', () => {
  const stack = new Stack({ composeFile: COMPOSE, home: '/h', tasksDir: '/t', seccomp: '/s', meterPort: 1, ports: { coder1: 20001, coder2: 20002 } });
  assert.equal(stack.coderUrl(1), 'http://127.0.0.1:20001/');
  assert.equal(stack.coderUrl(2), 'http://127.0.0.1:20002/');
  assert.throws(() => new Stack({ composeFile: 'compose.yaml', home: '/h', tasksDir: '/t', seccomp: '/s', meterPort: 1 }), /absolute/);
});

// ---- the compose file itself, rendered by `docker compose config` (no daemon needed) ----

function rendered() {
  const env = { ...process.env, BENCH_HOME: '/tmp/bh', BENCH_TASKS_DIR: '/tmp/bt', BENCH_STACK_DIR: '/tmp/bs', BENCH_AGENT_DIR: '/tmp/ba', BENCH_METER_PORT: '8090', PODMAN_SECCOMP: '/tmp/seccomp.json' };
  try {
    return JSON.parse(execFileSync('docker', ['compose', '-f', COMPOSE, 'config', '--format', 'json'], { env, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }));
  } catch {
    return null;
  }
}
const config = rendered();
const compose = { skip: config ? false : 'docker compose is not installed here' };

test('compose: the coder and the relay run the pinned image, tag and digest, the same as bench/pins.json', compose, () => {
  for (const name of ['coder-1', 'coder-2', 'meter']) assert.equal(config.services[name].image, coderRef(PINS), name);
});

test('compose: the Podman services are on the internal model network only, and the internal networks have no route out', compose, () => {
  for (const name of ['podman-1', 'podman-2']) assert.deepEqual(Object.keys(config.services[name].networks), ['model-net'], name);
  assert.equal(config.networks['coder-net'].internal, true);
  assert.equal(config.networks['model-net'].internal, true);
  assert.ok(!config.networks.egress.internal);
  assert.deepEqual(Object.keys(config.services.meter.networks).sort(), ['egress', 'model-net']);
  for (const name of ['postgres', 'mock-github']) assert.deepEqual(Object.keys(config.services[name].networks), ['coder-net'], `${name} is not reachable by a task's commands`);
});

test('compose: no privileged, no cap_add, no devices, no Docker socket, and no model key anywhere', compose, () => {
  const text = JSON.stringify(config);
  for (const [name, s] of Object.entries(config.services)) {
    assert.ok(!s.privileged && !s.cap_add && !s.devices, name);
    for (const v of s.volumes ?? []) assert.ok(!/docker\.sock/.test(v.source ?? ''), `${name} mounts a Docker socket`);
  }
  assert.doesNotMatch(text, /sk-[A-Za-z0-9]{10,}/);
  assert.equal(config.services['podman-1'].cpus, 4);
  assert.equal(Number(config.services['podman-1'].mem_limit), 12 * 1024 ** 3);
  assert.equal(config.services['coder-1'].environment.MODEL_API_KEY, 'bench-dummy-key');
});

test('compose: both coders carry the settings the plan names, each its own database, and only the stack\'s git server', compose, () => {
  for (const slot of [1, 2]) {
    const e = config.services[`coder-${slot}`].environment;
    assert.equal(e.ALLOWED_REPO_HOSTS, 'git-server');
    assert.equal(e.ALLOW_LOCAL_REPOS, 'true');
    assert.equal(e.GITHUB_API_URL, 'http://mock-github:8080');
    assert.equal(e.WORKSPACE_SWEEP_SECS, '0');
    assert.equal(e.DEVCONTAINER_RUNTIME, 'podman');
    assert.equal(e.DEVCONTAINER_PREPULL, 'false');
    assert.equal(e.DEVCONTAINER_NETWORK, 'inherit');
    assert.match(e.DATABASE_URL, new RegExp(`/adam_bench_${slot}$`));
    assert.match(fs.readFileSync(COMPOSE, "utf8"), new RegExp(`slot-${slot}\\.env\\n\\s+required: false`));
  }
  assert.equal(config.services['git-server'].environment.AUTO_CREATE_OWNERS, 'bench');
  assert.match(fs.readFileSync(path.join(HERE, '..', 'postgres-init.sql'), 'utf8'), /adam_bench_1[\s\S]*adam_bench_2/);
});

test('compose: the stack has the slots the harness knows, and a service for each', compose, () => {
  for (const kind of ['podman', 'coder']) for (const slot of [1, 2]) assert.ok(config.services[`${kind}-${slot}`]);
});
