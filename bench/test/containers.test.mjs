import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ContainerError, FakeContainers, PodmanRemote, buildRunArgs } from '../lib/containers.mjs';

const exec = (answers, calls) => (bin, args, opts, cb) => {
  calls.push(args.slice(args.indexOf('--url') + 2));
  const key = args.join(' ');
  const out = Object.entries(answers).find(([k]) => key.includes(k))?.[1] ?? '';
  cb(null, out, '');
  return { stdin: { end() {}, on() {} } };
};

test('M-reap-unchecked: reapAll removes everything, then lists, and fails when a container is left', async () => {
  const calls = [];
  const left = new PodmanRemote({ url: 'unix:///x', exec: exec({ 'ps -a -q': 'deadbeef\n' }, calls) });
  await assert.rejects(left.reapAll(), /left after the reap/);
  assert.deepEqual(calls.map((c) => c.join(' ')), ['rm -f -a -t 0', 'pod rm -f -a', 'ps -a -q']);
  const empty = new PodmanRemote({ url: 'unix:///x', exec: exec({}, []) });
  await empty.reapAll();
});

test('a container is told to stop with a signal by name, and only with a plain name and a known signal', async () => {
  const calls = [];
  const c = new PodmanRemote({ url: 'unix:///x', exec: exec({}, calls) });
  await c.kill('adam-bench-oc-r1-a1', 'TERM');
  assert.deepEqual(calls[0], ['kill', '--signal', 'TERM', 'adam-bench-oc-r1-a1']);
  await assert.rejects(c.kill('x; rm', 'TERM'), ContainerError);
  await assert.rejects(c.kill('ok', 'STOP'), ContainerError);
  const fake = new FakeContainers();
  await fake.kill('n', 'TERM');
  assert.deepEqual(fake.killed, [['n', 'TERM']]);
});

test('labels and stdin: a run\'s input goes in with -i and never on the command line', () => {
  const args = buildRunArgs({ image: 'registry.example.test/x@sha256:' + 'a'.repeat(64), script: 'cat', network: 'none', input: 'SECRET-TOKEN', labels: { 'bench.slot': '1' } });
  assert.ok(args.includes('-i'));
  assert.ok(!args.join(' ').includes('SECRET-TOKEN'));
  assert.deepEqual(args.slice(args.indexOf('--label'), args.indexOf('--label') + 2), ['--label', 'bench.slot=1']);
  assert.throws(() => buildRunArgs({ image: 'registry.example.test/x:1', script: 'x', network: 'none', labels: { 'Bad Key': 'v' } }), ContainerError);
});
