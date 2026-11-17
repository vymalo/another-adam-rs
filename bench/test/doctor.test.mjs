import assert from 'node:assert/strict';
import { test } from 'node:test';
import { FALLBACKS, doctor, renderDoctor } from '../lib/doctor.mjs';
import { PINS, coderRef } from '../lib/pins.mjs';

const GB = 1024 ** 3;
const INFO = (over = {}) => JSON.stringify({ ServerVersion: '29.9.2', SecurityOptions: ['name=seccomp,profile=builtin'], CgroupVersion: '2', DockerRootDir: '/var/lib/docker', ...over });

/** A machine: what each command answers. */
function machine({ info = INFO(), compose = '2.40.0', digests = [`${PINS.coder.image}@${PINS.coder.digest}`], sysctl = {}, freeGb = 500 } = {}) {
  const exec = async (file, args) => {
    const key = `${file} ${args.join(' ')}`;
    if (key.startsWith('docker compose version')) return compose ? { exitCode: 0, stdout: `${compose}\n`, stderr: '' } : { exitCode: 127, stdout: '', stderr: '' };
    if (key.startsWith('docker info')) return info ? { exitCode: 0, stdout: info, stderr: '' } : { exitCode: 1, stdout: '', stderr: 'Cannot connect' };
    if (key.startsWith('docker image inspect')) return digests ? { exitCode: 0, stdout: JSON.stringify(digests), stderr: '' } : { exitCode: 1, stdout: '', stderr: 'No such image' };
    return { exitCode: 127, stdout: '', stderr: '' };
  };
  const readFile = (file) => {
    const k = file.replace('/proc/sys/', '');
    if (k in sysctl) return `${sysctl[k]}\n`;
    throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
  };
  const statfs = () => ({ bavail: Math.floor((freeGb * GB) / 4096), bsize: 4096 });
  return { exec, readFile, statfs };
}

const run = (m, extra = {}) => doctor({ home: '/home/x/bench', pins: PINS, nodeVersion: '24.1.0', ...m, ...extra });
const by = (r, name) => r.checks.find((c) => c.name.startsWith(name));

test('a machine that has everything is ok, and exits 0', async () => {
  const r = await run(machine({ sysctl: { 'kernel/apparmor_restrict_unprivileged_userns': 0, 'user/max_user_namespaces': 15000 } }));
  assert.equal(r.exitCode, 0);
  assert.deepEqual(r.checks.filter((c) => c.status === 'fail'), []);
  assert.equal(by(r, 'docker daemon').detail, '29.9.2, rootful, cgroup v2');
  assert.equal(by(r, 'coder image').status, 'ok');
  assert.equal(r.fallback, null);
});

test('rootless Docker is flagged, and says which fallback to use if the nested check fails', async () => {
  const r = await run(machine({ info: INFO({ SecurityOptions: ['name=seccomp', 'name=rootless'] }) }));
  const c = by(r, 'rootless docker');
  assert.equal(c.status, 'warn');
  assert.match(c.fix, /fallback 1: a rootless Podman on the host/);
  assert.equal(r.exitCode, 0);
});

test('the Ubuntu sysctl that stops a rootless Podman is a failure with the command that fixes it', async () => {
  const r = await run(machine({ sysctl: { 'kernel/apparmor_restrict_unprivileged_userns': 1 } }));
  const c = by(r, 'sysctl kernel.apparmor');
  assert.equal(c.status, 'fail');
  assert.match(c.fix, /sysctl -w kernel\.apparmor_restrict_unprivileged_userns=0/);
  assert.equal(r.exitCode, 1);
  assert.equal(r.fallback, FALLBACKS[0]);
});

test('user namespaces switched off are a failure', async () => {
  assert.equal((await run(machine({ sysctl: { 'user/max_user_namespaces': 0 } }))).exitCode, 1);
  assert.equal((await run(machine({ sysctl: { 'kernel/unprivileged_userns_clone': 0 } }))).exitCode, 1);
});

test('disk: below 30 GB fails, below 100 GB warns, and both the results and Docker\'s root are looked at', async () => {
  const low = await run(machine({ freeGb: 10 }));
  assert.equal(low.exitCode, 1);
  assert.deepEqual(low.checks.filter((c) => c.name.startsWith('free disk')).map((c) => c.status), ['fail', 'fail']);
  const mid = await run(machine({ freeGb: 60 }));
  assert.equal(mid.exitCode, 0);
  assert.deepEqual(mid.checks.filter((c) => c.name.startsWith('free disk')).map((c) => c.status), ['warn', 'warn']);
  assert.match(by(mid, 'free disk, Docker root').detail, /60 GB free in \/var\/lib\/docker/);
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

function fakeStack({ nested = true, up = true } = {}) {
  const calls = [];
  return {
    calls,
    slots: 2,
    up: async () => {
      calls.push('up');
      if (!up) throw new Error('docker compose up failed (exit 1): podman-1 is unhealthy');
    },
    pullWindow: async (slot, fn) => fn(),
    containers: (slot) => ({
      pull: async () => calls.push(`pull ${slot}`),
      run: async () => (nested ? { exitCode: 0, stdout: '', stderr: '' } : { exitCode: 125, stdout: '', stderr: 'Error: cannot clone: Operation not permitted\nexit' }),
    }),
  };
}

test('without --start the stack is not touched and the nested check is a warning', async () => {
  const stack = fakeStack();
  const r = await run(machine(), { stack, start: false });
  assert.deepEqual(stack.calls, []);
  assert.equal(by(r, 'nested Podman').status, 'warn');
  assert.equal(r.exitCode, 0);
});

test('with --start a container is run inside each Podman service', async () => {
  const stack = fakeStack();
  const r = await run(machine(), { stack, start: true });
  assert.deepEqual(stack.calls, ['up', 'pull 1', 'pull 2']);
  assert.deepEqual(r.checks.filter((c) => c.name.startsWith('nested Podman')).map((c) => c.status), ['ok', 'ok']);
});

test('nested Podman that does not work fails with the reason, exits 1 and names the fallback', async () => {
  const r = await run(machine(), { stack: fakeStack({ nested: false }), start: true });
  assert.equal(r.exitCode, 1);
  assert.match(by(r, 'nested Podman, slot 1').detail, /cannot clone: Operation not permitted/);
  assert.equal(r.fallback, FALLBACKS[0]);
  const text = renderDoctor(r);
  assert.match(text, /^FAIL nested Podman, slot 1/m);
  assert.match(text, /Use fallback 1: a rootless Podman on the host.*then fallback 2: the coder's run pods on a k3d cluster/);
});

test('a stack that does not come up is a failure that names the fallback', async () => {
  const r = await run(machine(), { stack: fakeStack({ up: false }), start: true });
  assert.equal(by(r, 'stack').status, 'fail');
  assert.equal(r.exitCode, 1);
  assert.equal(r.fallback, FALLBACKS[0]);
});

test('the stack is not started while a check has already failed', async () => {
  const stack = fakeStack();
  await run(machine({ info: null }), { stack, start: true });
  assert.deepEqual(stack.calls, []);
});
