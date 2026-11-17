import assert from 'node:assert/strict';
import { test } from 'node:test';
import { FakeContainers, PodmanRemote } from '../lib/containers.mjs';
import { ensureImage, once } from '../lib/images.mjs';
import { Stack } from '../lib/stack.mjs';

test('R7: once runs one call for each key however many ask at the same time, and forgets a failure', async () => {
  const once1 = once();
  let calls = 0;
  const slow = async () => {
    calls++;
    await new Promise((r) => setTimeout(r, 15));
    return 'pulled';
  };
  const results = await Promise.all([once1('a', slow), once1('a', slow), once1('a', slow), once1('b', slow)]);
  assert.deepEqual(results, ['pulled', 'pulled', 'pulled', 'pulled']);
  assert.equal(calls, 2, 'one call for a, one for b');
  let tries = 0;
  const flaky = async () => {
    tries++;
    if (tries === 1) throw new Error('network');
    return 'ok';
  };
  await assert.rejects(once1('c', flaky), /network/);
  assert.equal(await once1('c', flaky), 'ok', 'a failed pull is tried again');
});

test('R14: an own image that is not on the slot is saved from where it was built and loaded; one that is there is left alone', async () => {
  const source = new FakeContainers();
  source.images = new Set(['adam-bench-own/x:1']);
  const target = new FakeContainers();
  const loads = [];
  const load = async (file) => loads.push(file);
  assert.equal(await ensureImage({ ref: 'adam-bench-own/x:1', source, target, archive: '/h/archives/x.tar', load }), 'loaded');
  assert.deepEqual(source.saved, [['adam-bench-own/x:1', '/h/archives/x.tar']]);
  assert.deepEqual(loads, ['/h/archives/x.tar']);
  target.images = new Set(['adam-bench-own/x:1']);
  assert.equal(await ensureImage({ ref: 'adam-bench-own/x:1', source, target, archive: '/h/archives/x.tar', load }), 'present');
  assert.equal(loads.length, 1);
  await assert.rejects(ensureImage({ ref: 'adam-bench-own/missing:1', source, target, archive: '/a', load }), /on no service/);
});

test('R14: PodmanRemote asks for an image with `image exists`, saves and loads archives, and tells a missing image from an error', async () => {
  const seen = [];
  const answers = [0, 1, 125];
  const exec = (bin, args, opts, cb) => {
    seen.push(args);
    const code = answers.length > 0 ? answers.shift() : 0;
    cb(code === 0 ? null : Object.assign(new Error('x'), { code }), '', '');
    return { stdin: { on() {}, end() {} } };
  };
  const p = new PodmanRemote({ url: 'unix:///run/p.sock', exec });
  assert.equal(await p.hasImage('adam-bench-own/x:1'), true);
  assert.equal(await p.hasImage('adam-bench-own/x:1'), false);
  await assert.rejects(p.hasImage('adam-bench-own/x:1'), /failed \(exit 125\)/);
  await p.save('adam-bench-own/x:1', '/h/x.tar');
  await p.load('/h/x.tar');
  assert.deepEqual(seen[0].slice(-3), ['image', 'exists', 'adam-bench-own/x:1']);
  assert.deepEqual(seen[3].slice(-5), ['--format', 'oci-archive', '-o', '/h/x.tar', 'adam-bench-own/x:1']);
  assert.deepEqual(seen[4].slice(-4), ['load', '-q', '-i', '/h/x.tar']);
});

test('R14: the stack copies an archive into the slot\'s Podman service, loads it there and removes the copy', async () => {
  const calls = [];
  const stack = new Stack({
    composeFile: '/repo/bench/compose.yaml',
    home: '/h',
    tasksDir: '/repo/bench/tasks',
    seccomp: '/repo/dev/podman/seccomp.json',
    meterPort: 18090,
    exec: async (file, args) => (calls.push(args.slice(5)), { exitCode: 0, stdout: '', stderr: '' }),
    spawn: (file, args, opts, cb) => {
      calls.push(['spawn', ...args.slice(5)]);
      cb(null, '', '');
      return { stdin: { on() {}, end() {} } };
    },
    baseEnv: { PATH: '/usr/bin' },
  });
  await stack.loadImage('podman-2', '/h/archives/x.tar');
  assert.deepEqual(calls[0], ['cp', '/h/archives/x.tar', 'podman-2:/tmp/bench-own-image.tar']);
  assert.ok(calls.some((c) => c[0] === 'spawn' && c.includes('load') && c.includes('/tmp/bench-own-image.tar')));
  assert.ok(calls.some((c) => c.includes('rm -f /tmp/bench-own-image.tar')));
});
