import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ContainerError, FakeContainers, HostContainers, buildRunArgs } from '../lib/containers.mjs';

const IMAGE = `registry.example.test/x@sha256:${'a'.repeat(64)}`;

/** An execFile stand-in: records the argv and answers from `answers` (the first key the joined argv contains). */
const exec = (answers, calls) => (bin, args, opts, cb) => {
  calls.push({ bin, args });
  const key = args.join(' ');
  const out = Object.entries(answers).find(([k]) => key.includes(k))?.[1] ?? '';
  cb(null, out, '');
  return { stdin: { end() {}, on() {} } };
};

test('HostContainers runs docker with an argv array: a script, a token or a path is one argument, never interpolated', () => {
  const args = buildRunArgs({
    image: IMAGE,
    script: 'echo "$TOKEN"; cat /etc/hostname',
    network: 'none',
    user: '10001:10001',
    cpus: 4,
    memoryMb: 12288,
    env: { TOKEN: 'x$y' },
    mounts: [{ source: '/h/work/1', target: '/work' }, { source: '/h/agent', target: '/etc/adam/agent', readOnly: true }],
    labels: { 'bench.slot': '1' },
    name: 'adam-bench-oc-r1-a1',
  });
  assert.deepEqual(args.slice(0, 3), ['run', '--rm', '--pull=never']);
  assert.ok(args.includes('--label') && args.includes('bench.slot=1'));
  assert.ok(args.includes('--network=none'));
  assert.ok(args.includes('--user') && args.includes('10001:10001'));
  assert.deepEqual(args.slice(args.indexOf('--cpus'), args.indexOf('--cpus') + 2), ['--cpus', '4']);
  assert.deepEqual(args.slice(args.indexOf('--memory'), args.indexOf('--memory') + 2), ['--memory', '12288m']);
  assert.deepEqual(args.slice(args.indexOf('-v'), args.indexOf('-v') + 2), ['-v', '/h/work/1:/work']);
  assert.ok(args.includes('/h/agent:/etc/adam/agent:ro'));
  assert.equal(args[args.length - 1], 'echo "$TOKEN"; cat /etc/hostname');
  assert.equal(args[args.length - 2], '-c');
  assert.equal(args[args.length - 3], IMAGE);
  assert.equal(args[args.length - 4], 'sh');
  // the script is one argument: no shell was asked to split it
  assert.equal(args.filter((a) => a.includes('$TOKEN')).length, 1);
});

test('a network is named; the host network and Docker\'s default bridge are refused', () => {
  assert.throws(() => buildRunArgs({ image: IMAGE, script: 'true', network: 'host' }), ContainerError);
  assert.throws(() => buildRunArgs({ image: IMAGE, script: 'true', network: 'default' }), ContainerError);
  assert.throws(() => buildRunArgs({ image: IMAGE, script: 'true' }), /names its network/);
  assert.throws(() => buildRunArgs({ image: IMAGE, script: 'true', network: 'bad net' }), ContainerError);
  assert.throws(() => buildRunArgs({ image: 'x; rm', script: 'true', network: 'none' }), ContainerError);
});

test('a detached run keeps the container and takes no --rm; the entrypoint and its arguments are separate argv entries', () => {
  const args = buildRunArgs({ image: IMAGE, detach: true, name: 'coder-1', network: 'adam-bench_slot-1', entrypoint: '/opt/adam/bin/adam-coder' });
  assert.deepEqual(args.slice(0, 2), ['run', '-d']);
  assert.ok(!args.includes('--rm'));
  assert.equal(args[args.length - 1], IMAGE);
  assert.equal(args[args.length - 2], '/opt/adam/bin/adam-coder');
  const probe = buildRunArgs({ image: IMAGE, network: 'none', entrypoint: '/bin/cat', command: ['/etc/os-release'] });
  assert.deepEqual(probe.slice(-2), [IMAGE, '/etc/os-release']);
  assert.throws(() => buildRunArgs({ image: IMAGE, network: 'none', entrypoint: '/bin/cat', command: ['a\nb'] }), ContainerError);
});

test('the labels an instance carries are on every run and select what it reaps and lists', async () => {
  const calls = [];
  const c = new HostContainers({ labels: { 'bench.project': 'adam-bench', 'bench.role': 'run' }, exec: exec({}, calls) });
  await c.run({ image: IMAGE, script: 'true', network: 'none' });
  assert.ok(calls[0].args.includes('bench.project=adam-bench'));
  assert.ok(calls[0].args.includes('bench.role=run'));
  assert.ok(calls[0].args.includes('--label'));
});

test('reapAll removes only the containers of its labels, then lists, and fails when one is left', async () => {
  const calls = [];
  const left = new HostContainers({ labels: { 'bench.project': 'p' }, exec: exec({ 'ps -aq': 'deadbeef\n' }, calls) });
  await assert.rejects(left.reapAll(), /left after the reap/);
  const ps = calls.filter((c) => c.args.includes('ps'));
  assert.deepEqual(ps[0].args, ['ps', '-aq', '--filter', 'label=bench.project=p']);
  assert.deepEqual(calls[1].args, ['rm', '-f', 'deadbeef']);
  const empty = new HostContainers({ labels: { 'bench.project': 'p' }, exec: exec({}, []) });
  await empty.reapAll();
  await assert.rejects(new HostContainers({ exec: exec({}, []) }).reapAll(), /labels/);
});

test('listContainers reads Docker\'s JSON lines, its labels and its names', async () => {
  const line = JSON.stringify({ Image: 'x@sha256:' + 'a'.repeat(64), ID: 'sha256:' + 'b'.repeat(64), Labels: 'bench.slot=1,bench.role=run', Names: 'adam-bench-oc-1' });
  const c = new HostContainers({ labels: { 'bench.project': 'p' }, exec: exec({ 'ps -a': `${line}\n` }, []) });
  assert.deepEqual(await c.listContainers(), [{ image: 'x@sha256:' + 'a'.repeat(64), id: 'sha256:' + 'b'.repeat(64), labels: { 'bench.slot': '1', 'bench.role': 'run' }, names: ['adam-bench-oc-1'] }]);
});

test('a container is told to stop with a signal by name, and only with a plain name and a known signal', async () => {
  const calls = [];
  const c = new HostContainers({ exec: exec({}, calls) });
  await c.kill('adam-bench-oc-r1-a1', 'TERM');
  assert.deepEqual(calls[0].args, ['kill', '--signal', 'TERM', 'adam-bench-oc-r1-a1']);
  await assert.rejects(c.kill('x; rm', 'TERM'), ContainerError);
  await assert.rejects(c.kill('ok', 'STOP'), ContainerError);
  const fake = new FakeContainers();
  await fake.kill('n', 'TERM');
  assert.deepEqual(fake.killed, [['n', 'TERM']]);
});

test('the image\'s environment and its files: inspect and create/cp/rm, never a run of the image', async () => {
  const calls = [];
  const env = JSON.stringify(['PATH=/a:/b', 'HOME=/root']);
  const c = new HostContainers({ exec: exec({ 'image inspect': `${env}\n`, create: 'id\n' }, calls) });
  assert.deepEqual(await c.imageEnv(IMAGE), { PATH: '/a:/b', HOME: '/root' });
  await c.extract(IMAGE, ['/usr/local/bin/adam-coder'], '/h/coder-bin');
  const verbs = calls.map((c) => c.args[0]);
  assert.deepEqual(verbs, ['image', 'create', 'cp', 'rm']);
  assert.deepEqual(calls[1].args.slice(0, 2), ['create', '--name']);
  assert.equal(calls[2].args[2], '/h/coder-bin/adam-coder');
  assert.deepEqual(calls[3].args.slice(0, 2), ['rm', '-f']);
});

test('a run that times out is removed by name; a build argument that is not plain is refused', async () => {
  const calls = [];
  const execFile = (bin, args, opts, cb) => {
    calls.push(args);
    cb(Object.assign(new Error('killed'), { killed: true, code: null }), '', '');
    return { stdin: { end() {}, on() {} } };
  };
  const c = new HostContainers({ exec: execFile });
  await c.run({ image: IMAGE, script: 'sleep 9', network: 'none', name: 'adam-bench-x' });
  assert.deepEqual(calls.at(-1), ['rm', '-f', 'adam-bench-x']);
  const fake = new FakeContainers();
  await assert.rejects(fake.build('/c', 't', '/d', { BASE_IMAGE: 'a b' }), ContainerError);
});
