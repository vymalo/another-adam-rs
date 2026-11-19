import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { coderProbe, doctor, osRelease, renderDoctor } from '../lib/doctor.mjs';
import { FakeContainers, HostContainers } from '../lib/containers.mjs';
import { PINS, coderRef } from '../lib/pins.mjs';
import { Stack } from '../lib/stack.mjs';
import { tmp } from './helpers.mjs';

const GB = 1024 ** 3;
const INFO = (over = {}) => JSON.stringify({ ServerVersion: '29.9.2', SecurityOptions: ['name=seccomp,profile=builtin'], CgroupVersion: '2', DockerRootDir: '/var/lib/docker', ...over });
const DIGEST = `sha256:${'a'.repeat(64)}`;
const TASK = { id: 'hello', image: { ref: `registry.example.test/task@${DIGEST}` } };

/** A machine: what each command answers. */
function machine({ info = INFO(), compose = '2.40.0', digests = [`${PINS.coder.image}@${PINS.coder.digest}`], freeGb = 500, sizeGb = 0 } = {}) {
  const exec = async (file, args) => {
    const key = `${file} ${args.join(' ')}`;
    if (key.startsWith('docker compose version')) return compose ? { exitCode: 0, stdout: `${compose}\n`, stderr: '' } : { exitCode: 127, stdout: '', stderr: '' };
    if (key.startsWith('docker info')) return info ? { exitCode: 0, stdout: info, stderr: '' } : { exitCode: 1, stdout: '', stderr: 'Cannot connect' };
    if (key.includes('{{.Size}}')) return digests && sizeGb > 0 ? { exitCode: 0, stdout: `${Math.floor(sizeGb * GB)}\n`, stderr: '' } : { exitCode: 1, stdout: '', stderr: 'No such image' };
    if (key.startsWith('docker image inspect')) return digests ? { exitCode: 0, stdout: JSON.stringify(digests), stderr: '' } : { exitCode: 1, stdout: '', stderr: 'No such image' };
    return { exitCode: 127, stdout: '', stderr: '' };
  };
  const statfs = () => ({ bavail: Math.floor((freeGb * GB) / 4096), bsize: 4096 });
  return { exec, statfs };
}

const run = (m, extra = {}) => doctor({ home: '/home/x/bench', pins: PINS, nodeVersion: '24.1.0', arch: 'x64', ...m, ...extra });
const by = (r, name) => r.checks.find((c) => c.name.startsWith(name));

test('a machine that has everything is ok, and exits 0', async () => {
  const r = await run(machine());
  assert.equal(r.exitCode, 0);
  assert.deepEqual(r.checks.filter((c) => c.status === 'fail'), []);
  assert.equal(by(r, 'docker daemon').detail, '29.9.2, rootful, cgroup v2');
  assert.equal(by(r, 'coder image').status, 'ok');
});

test('rootless Docker is reported, and says what the work directory needs', async () => {
  const r = await run(machine({ info: INFO({ SecurityOptions: ['name=seccomp', 'name=rootless'] }) }));
  const c = by(r, 'rootless docker');
  assert.equal(c.status, 'warn');
  assert.match(c.detail, /uid 10001/);
  assert.equal(r.exitCode, 0);
});

test('disk: below 20 GB fails, below 100 GB warns, and both the results and Docker\'s root are looked at', async () => {
  const low = await run(machine({ freeGb: 10 }));
  assert.equal(low.exitCode, 1);
  assert.deepEqual(low.checks.filter((c) => c.name.startsWith('free disk')).map((c) => c.status), ['fail', 'fail']);
  const mid = await run(machine({ freeGb: 60 }));
  assert.equal(mid.exitCode, 0);
  assert.deepEqual(mid.checks.filter((c) => c.name.startsWith('free disk')).map((c) => c.status), ['warn', 'warn']);
  assert.match(by(mid, 'free disk, Docker root').detail, /60 GB free in \/var\/lib\/docker/);
  assert.equal(by(await run(machine({ freeGb: 19 })), 'free disk, results ($BENCH_HOME)').status, 'fail');
});

test('disk: the images already present count toward the floor, at exactly the boundary', async () => {
  const states = (r) => r.checks.filter((c) => c.name.startsWith('free disk')).map((c) => c.status);
  // the coder image (12 GB here) is present: free + present must reach MIN_FREE_GB
  assert.deepEqual(states(await run(machine({ freeGb: 8, sizeGb: 12 }))), ['warn', 'warn'], 'free 8 + present 12 = 20: passes');
  assert.equal((await run(machine({ freeGb: 8, sizeGb: 12 }))).exitCode, 0);
  assert.deepEqual(states(await run(machine({ freeGb: 8, sizeGb: 0 }))), ['fail', 'fail'], 'free 8 + present 0: fails');
  assert.equal((await run(machine({ freeGb: 8, sizeGb: 0 }))).exitCode, 1);
  assert.equal((await run(machine({ freeGb: 8, sizeGb: 11.9 }))).exitCode, 1, 'just under the boundary');
  const task = await run(machine({ freeGb: 8, sizeGb: 6 }), { tasks: [TASK] });
  assert.equal(task.exitCode, 0, 'the coder image and one task image, 6 GB each as the fake answers');
  assert.match(by(await run(machine({ freeGb: 8, sizeGb: 0 })), 'free disk, Docker root').detail, /8\.0 GB free .* 0\.0 GB of the images already present, less than 20/);
});

test('no daemon, no compose and an old Node are failures with a fix', async () => {
  const r = await run(machine({ info: null, compose: null }), { nodeVersion: '20.1.0' });
  assert.equal(r.exitCode, 1);
  for (const n of ['node', 'docker compose', 'docker daemon']) assert.equal(by(r, n).status, 'fail', n);
  for (const n of ['node', 'docker compose', 'docker daemon']) assert.ok(by(r, n).fix, n);
});

test('the coder image: absent is a warning with the pull command, another image under the tag is a failure', async () => {
  const absent = await run(machine({ digests: null }));
  assert.equal(by(absent, 'coder image').status, 'warn');
  assert.equal(by(absent, 'coder image').fix, `docker pull ${coderRef(PINS)}`);
  const other = await run(machine({ digests: [`${PINS.coder.image}@sha256:${'0'.repeat(64)}`] }));
  assert.equal(by(other, 'coder image').status, 'fail');
});

test('finding 15: doctor removes the env files a killed harness left and says so', async () => {
  const stack = { removeStaleEnvFiles: () => ['slot-1.env'] };
  const r = await run(machine(), { stack });
  assert.match(by(r, 'stale env files').detail, /removed slot-1\.env/);
  assert.equal(by(await run(machine(), { stack: { removeStaleEnvFiles: () => [] } }), 'stale env files').status, 'ok');
});

test('finding 20: a compose plugin older than 2.24 is a failure, because env_file needs required: false', async () => {
  const old = await run(machine({ compose: '2.20.3' }));
  assert.equal(by(old, 'docker compose').status, 'fail');
  assert.match(by(old, 'docker compose').fix, /2\.24/);
  assert.equal(by(await run(machine({ compose: 'v2.24.0' })), 'docker compose').status, 'ok');
  assert.equal(by(await run(machine({ compose: '2.40.0' })), 'docker compose').status, 'ok');
  assert.equal(by(await run(machine({ compose: '1.29.2' })), 'docker compose').status, 'fail');
});

test('finding 20: the OpenCode binary is linux-x64, so another machine is a failure that says so', async () => {
  const r = await run(machine(), { arch: 'arm64' });
  assert.equal(by(r, 'machine').status, 'fail');
  assert.match(by(r, 'machine').detail, /linux-x64/);
  assert.equal(by(await run(machine()), 'machine').status, 'ok');
});

// ---- the per-image probe: does the coder start there? ----

const probeStack = (containers) => ({ ensureCoderFiles: async () => '/h/coder-bin', hostContainers: () => containers });

test('the probe: exit 78 means the coder starts; a loader error means it does not; anything else is unclear', async () => {
  const starts = new FakeContainers({ onRun: () => ({ exitCode: 78 }) });
  assert.deepEqual((await coderProbe({ containers: starts, image: TASK.image.ref, filesDir: '/h/bin' })).starts, true);
  const loader = new FakeContainers({ onRun: () => ({ exitCode: 127, stderr: 'error while loading shared libraries: libgcc_s.so.1: cannot open shared object file' }) });
  const no = await coderProbe({ containers: loader, image: TASK.image.ref, filesDir: '/h/bin' });
  assert.equal(no.starts, false);
  assert.match(no.detail, /libgcc_s/);
  const glibc = new FakeContainers({ onRun: () => ({ exitCode: 1, stderr: "adam-coder: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.38' not found" }) });
  assert.equal((await coderProbe({ containers: glibc, image: TASK.image.ref, filesDir: '/h/bin' })).starts, false);
  const odd = new FakeContainers({ onRun: () => ({ exitCode: 139, stderr: 'segfault' }) });
  assert.equal((await coderProbe({ containers: odd, image: TASK.image.ref, filesDir: '/h/bin' })).starts, null);
  // the probe runs the binary as uid 10001, with no network and its files mounted read-only
  const spec = starts.runs[0];
  assert.equal(spec.user, '10001:10001');
  assert.equal(spec.network, 'none');
  assert.deepEqual(spec.mounts, [{ source: '/h/bin', target: '/opt/adam/bin', readOnly: true }]);
  assert.equal(spec.entrypoint, '/opt/adam/bin/adam-coder');
});

test('osRelease reads PRETTY_NAME, and says nothing when the image has no cat', async () => {
  const bookworm = new FakeContainers({ onRun: () => ({ exitCode: 0, stdout: 'PRETTY_NAME="Debian GNU/Linux 12 (bookworm)"\n' }) });
  assert.equal(await osRelease({ containers: bookworm, image: TASK.image.ref }), 'Debian GNU/Linux 12 (bookworm)');
  assert.equal(await osRelease({ containers: new FakeContainers({ onRun: () => ({ exitCode: 127, stderr: 'no cat' }) }), image: TASK.image.ref }), null);
});

test('bench doctor reports that the static coder starts in a bookworm task image: the distribution is not the question', async () => {
  // ADR 0053 decision 11: the coder's binaries are static (musl), so a Debian bookworm image runs the coder as well as
  // a trixie one. Before the static build, this probe failed a bookworm image on its glibc.
  const containers = new FakeContainers({
    onRun: (spec) => (spec.entrypoint === '/bin/cat' ? { exitCode: 0, stdout: 'PRETTY_NAME="Debian GNU/Linux 12 (bookworm)"\n' } : { exitCode: 78 }),
  });
  const r = await run(machine(), { tasks: [TASK], containers, stack: probeStack(containers) });
  const c = by(r, 'task image hello');
  assert.equal(c.status, 'ok');
  assert.match(c.detail, /bookworm/);
  assert.match(c.detail, /exit 78/);
  assert.equal(r.exitCode, 0);
});

test('bench doctor refuses, for both arms, a task image that cannot run the coder, and says why without waiting for a later slice', async () => {
  const containers = new FakeContainers({
    onRun: (spec) => (spec.entrypoint === '/bin/cat' ? { exitCode: 0, stdout: 'PRETTY_NAME="Debian GNU/Linux 12 (bookworm)"\n' } : { exitCode: 126, stderr: 'exec /opt/adam/bin/adam-coder: exec format error' }),
  });
  const r = await run(machine(), { tasks: [TASK], containers, stack: probeStack(containers) });
  const c = by(r, 'task image hello');
  assert.equal(c.status, 'fail');
  assert.match(c.detail, /bookworm/);
  assert.match(c.detail, /refused for both arms/);
  assert.match(c.detail, /exec format error/);
  assert.doesNotMatch(c.detail, /until the static coder|slice 1/);
  assert.equal(r.exitCode, 1);

  const good = new FakeContainers({ onRun: (spec) => (spec.entrypoint === '/bin/cat' ? { exitCode: 0, stdout: 'PRETTY_NAME="Debian GNU/Linux 13 (trixie)"\n' } : { exitCode: 78 }) });
  const okRun = await run(machine(), { tasks: [TASK], containers: good, stack: probeStack(good) });
  const g = by(okRun, 'task image hello');
  assert.equal(g.status, 'ok');
  assert.match(g.detail, /trixie/);
  assert.match(g.detail, /exit 78/);
  assert.equal(okRun.exitCode, 0);
});

test('no selected tasks, a selection that cannot be read, and a missing coder image are warnings, never failures', async () => {
  const none = await run(machine());
  assert.equal(by(none, 'task images').status, 'warn');
  assert.match(by(none, 'task images').detail, /none selected/);
  const broken = await run(machine(), { tasks: [TASK], taskError: 'the lock is old', stack: probeStack(new FakeContainers()) });
  assert.match(by(broken, 'task images').detail, /lock is old/);
  assert.equal(broken.exitCode, 0);
  const absent = await run(machine({ digests: null }), { tasks: [TASK], containers: new FakeContainers(), stack: probeStack(new FakeContainers()) });
  assert.match(by(absent, 'task images').detail, /coder image is not on this machine/);
});

test('without --start the host runtime is not touched; with --start a container runs for each slot and the grader', async () => {
  const calls = [];
  const fake = (label) => ({
    run: async () => (calls.push(label), { exitCode: 0, stdout: '', stderr: '' }),
  });
  const stack = {
    slots: 2,
    up: async () => calls.push('up'),
    removeStaleEnvFiles: () => [],
    containers: (slot) => fake(`slot ${slot}`),
    graderContainers: () => fake('grader'),
  };
  const off = await run(machine(), { stack, start: false });
  assert.deepEqual(calls, []);
  assert.equal(by(off, 'host runtime').status, 'warn');
  const on = await run(machine(), { stack, start: true });
  assert.deepEqual(calls, ['up', 'slot 1', 'slot 2', 'grader', 'slot 1', 'slot 1']);
  assert.deepEqual(on.checks.filter((c) => c.name.startsWith('host runtime')).map((c) => c.status), ['ok', 'ok', 'ok']);
  assert.equal(by(on, 'runtime limits').status, 'ok');});

test('a stack that does not come up is a failure', async () => {
  const stack = { slots: 2, up: async () => { throw new Error('docker compose up failed (exit 1)'); }, removeStaleEnvFiles: () => [], containers: () => ({ run: async () => ({ exitCode: 0 }) }), graderContainers: () => ({ run: async () => ({ exitCode: 0 }) }) };
  const r = await run(machine(), { stack, start: true });
  assert.equal(by(r, 'stack').status, 'fail');
  assert.equal(r.exitCode, 1);
});

test('the stack is not started while a check has already failed', async () => {
  const calls = [];
  const stack = { slots: 2, up: async () => calls.push('up'), removeStaleEnvFiles: () => [], containers: () => ({ run: async () => ({ exitCode: 0 }) }), graderContainers: () => ({ run: async () => ({ exitCode: 0 }) }) };
  await run(machine({ info: null }), { stack, start: true });
  assert.deepEqual(calls, []);
});

test('renderDoctor prints one line per check and its fix', async () => {
  const r = await run(machine({ info: null }));
  const text = renderDoctor(r);
  assert.match(text, /^FAIL docker daemon:/m);
  assert.match(text, /^     fix: /m);
});

// ---- the environment of every docker child the doctor starts: an allow-list, never the key ----

const SENTINEL = 'sk-sentinel-do-not-leak';
const OTHER_SECRET = 'another-sentinel-value-0123456789';

/** Set variables for one test and put them back, so a leak is visible in the process's own environment. */
function withEnv(t, vars) {
  const before = {};
  for (const [k, v] of Object.entries(vars)) {
    before[k] = process.env[k];
    process.env[k] = v;
  }
  t.after(() => {
    for (const [k, v] of Object.entries(before)) (v === undefined ? delete process.env[k] : (process.env[k] = v));
  });
}

/** What the fake docker answers for one argv: the doctor's own commands, and the two probes of a task image. */
function dockerAnswer(args) {
  const key = args.join(' ');
  if (key.startsWith('compose version')) return { exitCode: 0, stdout: '2.40.0\n', stderr: '' };
  if (key.startsWith('info')) return { exitCode: 0, stdout: INFO(), stderr: '' };
  if (key.startsWith('image inspect')) return { exitCode: 0, stdout: JSON.stringify([`${PINS.coder.image}@${PINS.coder.digest}`]), stderr: '' };
  if (args.includes('/bin/cat')) return { exitCode: 0, stdout: 'PRETTY_NAME="Debian GNU/Linux 13 (trixie)"\n', stderr: '' };
  return { exitCode: 78, stdout: '', stderr: '' };
}

test('the model key is not in the environment of any docker child the doctor starts, and the allow-list still reaches it', async (t) => {
  const seenFile = `${tmp(t)}/children.jsonl`;
  withEnv(t, { BENCH_MODEL_API_KEY: SENTINEL, ANTHROPIC_API_KEY: OTHER_SECRET, HTTPS_PROXY: 'http://user:pw@proxy.example.test:3128', HOME: '/home/bench-test', DOCKER_HOST: 'unix:///run/user/1000/docker.sock', XDG_RUNTIME_DIR: '/run/user/1000' });
  // A child with no `env` inherits the process's; one with an `env` sees only that. Write what each child would see.
  const record = (args, opts) => fs.appendFileSync(seenFile, `${JSON.stringify({ args, env: opts?.env ?? process.env })}\n`);
  const exec = async (file, args, opts) => {
    record(args, opts);
    return dockerAnswer(args);
  };
  const dockerExec = (bin, args, opts, cb) => {
    record(args, opts);
    const a = dockerAnswer(args);
    cb(a.exitCode === 0 ? null : Object.assign(new Error(`exit ${a.exitCode}`), { code: a.exitCode }), a.stdout, a.stderr);
    return { stdin: { end() {}, on() {} } };
  };
  const containers = new HostContainers({ exec: dockerExec });
  const r = await run({ exec, containers }, { tasks: [TASK], stack: probeStack(containers) });
  const rows = fs.readFileSync(seenFile, 'utf8').trim().split('\n').map((l) => JSON.parse(l));
  const verbs = rows.map((row) => row.args.slice(0, 2).join(' '));
  assert.ok(verbs.includes('compose version'), 'the doctor asked the compose plugin');
  assert.ok(verbs.includes('image inspect'), 'the doctor looked for the coder image');
  assert.ok(rows.some((row) => row.args[0] === 'info'), 'the doctor asked the daemon');
  assert.ok(rows.some((row) => row.args[0] === 'run'), 'a probe ran');
  for (const row of rows) {
    assert.equal(row.env.BENCH_MODEL_API_KEY, undefined, `the key reaches: docker ${row.args[0]}`);
    assert.equal(row.env.ANTHROPIC_API_KEY, undefined, `another secret reaches: docker ${row.args[0]}`);
    assert.equal(row.env.HTTPS_PROXY, undefined, `the proxy reaches: docker ${row.args[0]}`);
    assert.equal(row.env.PATH, process.env.PATH, `PATH is missing: docker ${row.args[0]}`);
    assert.equal(row.env.HOME, '/home/bench-test', `HOME is missing: docker ${row.args[0]}`);
    assert.equal(row.env.DOCKER_HOST, 'unix:///run/user/1000/docker.sock', `DOCKER_HOST is missing: docker ${row.args[0]}`);
    assert.equal(row.env.XDG_RUNTIME_DIR, '/run/user/1000', `XDG_RUNTIME_DIR is missing: docker ${row.args[0]}`);
  }
  assert.equal(by(r, 'task image hello').status, 'ok');
  assert.match(by(r, 'task image hello').detail, /trixie/);
  assert.doesNotMatch(renderDoctor(r), new RegExp(`${SENTINEL}|${OTHER_SECRET}`));
});

test('a probe that echoes its environment cannot put the key in the doctor\'s report', async (t) => {
  withEnv(t, { BENCH_MODEL_API_KEY: SENTINEL, ANTHROPIC_API_KEY: OTHER_SECRET });
  const exec = async (file, args) => dockerAnswer(args);
  const dockerExec = (bin, args, opts, cb) => {
    const env = opts?.env ?? process.env;
    if (args[0] === 'run' && !args.includes('/bin/cat')) {
      const leak = Object.keys(env).filter((k) => /KEY|TOKEN|SECRET|PROXY/i.test(k)).map((k) => `${k}=${env[k]}`).join('\n');
      cb(Object.assign(new Error('exec format error'), { code: 126 }), '', leak || 'exec format error');
      return { stdin: { end() {}, on() {} } };
    }
    const a = dockerAnswer(args);
    cb(a.exitCode === 0 ? null : Object.assign(new Error(`exit ${a.exitCode}`), { code: a.exitCode }), a.stdout, a.stderr);
    return { stdin: { end() {}, on() {} } };
  };
  const containers = new HostContainers({ exec: dockerExec });
  const r = await run({ exec, containers }, { tasks: [TASK], stack: probeStack(containers) });
  assert.doesNotMatch(renderDoctor(r), new RegExp(`${SENTINEL}|${OTHER_SECRET}`));
  assert.doesNotMatch(JSON.stringify(r), new RegExp(`${SENTINEL}|${OTHER_SECRET}`));
  assert.equal(by(r, 'task image hello').status, 'fail');
  assert.match(by(r, 'task image hello').detail, /exec format error/);
});

test('with --start every docker child uses the stack\'s injected baseEnv, not the ambient environment, and none sees the key', async (t) => {
  const seen = [];
  const record = (file, args, opts) => seen.push({ file, args, env: opts?.env ?? process.env });
  const exec = async (file, args, opts) => {
    record(file, args, opts);
    const key = args.join(' ');
    if (key.startsWith('compose version')) return { exitCode: 0, stdout: '2.40.0\n', stderr: '' };
    if (args[0] === 'compose') return { exitCode: 0, stdout: '', stderr: '' };
    if (args[0] === 'info') return { exitCode: 0, stdout: INFO(), stderr: '' };
    if (args[0] === 'image' && args[1] === 'inspect') return { exitCode: 0, stdout: JSON.stringify([`${PINS.coder.image}@${PINS.coder.digest}`]), stderr: '' };
    if (args[0] === 'network') return { exitCode: 0, stdout: 'true\n', stderr: '' };
    if (args[0] === 'create') return { exitCode: 0, stdout: 'id\n', stderr: '' };
    if (args[0] === 'cp') {
      fs.writeFileSync(args[2], '');
      return { exitCode: 0, stdout: '', stderr: '' };
    }
    if (args[0] === 'run') {
      if (args.includes('/bin/cat')) return { exitCode: 0, stdout: 'PRETTY_NAME="Debian GNU/Linux 13 (trixie)"\n', stderr: '' };
      if (args.includes('/opt/adam/bin/adam-coder')) return { exitCode: 78, stdout: '', stderr: '' };
      return { exitCode: 0, stdout: '', stderr: '' };
    }
    return { exitCode: 0, stdout: '', stderr: '' };
  };
  const baseEnv = { PATH: process.env.PATH, HOME: '/home/bench-test', DOCKER_HOST: 'unix:///from-base-env', XDG_RUNTIME_DIR: '/run/user/1000', BENCH_MODEL_API_KEY: SENTINEL, ANTHROPIC_API_KEY: OTHER_SECRET };
  withEnv(t, { DOCKER_HOST: 'unix:///from-process-env' });
  const home = tmp(t);
  const stack = new Stack({ composeFile: path.join(home, 'compose.yaml'), home, meterPort: 18090, exec, baseEnv, meterHost: '10.0.0.9' });
  const statfs = () => ({ bavail: Math.floor((500 * GB) / 4096), bsize: 4096 });
  const r = await doctor({ home, pins: PINS, nodeVersion: '24.1.0', arch: 'x64', stack, start: true, tasks: [TASK], exec, statfs });
  const dockerRows = seen.filter((row) => row.file === 'docker');
  const verbs = new Set(dockerRows.map((row) => row.args[0]));
  for (const v of ['compose', 'info', 'image', 'network', 'create', 'cp', 'rm', 'run']) assert.ok(verbs.has(v), `no docker ${v} child`);
  assert.ok(dockerRows.some((row) => row.args[0] === 'run' && row.args.includes('/bin/cat')), 'the distribution probe ran');
  assert.ok(dockerRows.some((row) => row.args[0] === 'run' && row.args.includes('/opt/adam/bin/adam-coder')), 'the coder probe ran');
  assert.ok(dockerRows.some((row) => row.args.includes('--cpus')), 'the limits probe ran');
  for (const row of dockerRows) {
    assert.equal(row.env.BENCH_MODEL_API_KEY, undefined, `the key reaches: docker ${row.args[0]}`);
    assert.equal(row.env.ANTHROPIC_API_KEY, undefined, `another secret reaches: docker ${row.args[0]}`);
    assert.equal(row.env.DOCKER_HOST, 'unix:///from-base-env', `the stack baseEnv is not the one used: docker ${row.args[0]}`);
    assert.equal(row.env.PATH, process.env.PATH, `PATH is missing: docker ${row.args[0]}`);
  }
  assert.equal(by(r, 'stack').status, 'ok');
  assert.equal(by(r, 'task image hello').status, 'ok');
  assert.equal(by(r, 'runtime limits').status, 'ok');
  assert.equal(by(r, 'host runtime, slot 1').status, 'ok');
  assert.equal(r.exitCode, 0);
});

test('a probe that times out is unclear, and the doctor still reports the image', async () => {
  const timed = new FakeContainers({ onRun: () => ({ exitCode: 137, timedOut: true }) });
  const p = await coderProbe({ containers: timed, image: TASK.image.ref, filesDir: '/h/bin', timeoutMs: 5 });
  assert.equal(p.starts, null);
  assert.equal(timed.runs[0].timeoutMs, 5);
  const r = await run(machine(), { tasks: [TASK], containers: timed, stack: probeStack(timed) });
  assert.equal(by(r, 'task image hello').status, 'warn');
  assert.match(by(r, 'task image hello').detail, /unclear/);
});
