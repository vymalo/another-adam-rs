import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ContainerError, HostContainers } from '../lib/containers.mjs';
import { checkNetworks } from '../lib/tasks-cli.mjs';

/** A fake docker CLI over a set of networks: name -> { internal }. */
function fakeDocker(existing = {}) {
  const nets = { ...existing };
  const calls = [];
  const docker = async (args) => {
    calls.push(args.join(' '));
    const [, verb, ...rest] = args[0] === 'network' ? args : [null, null];
    if (args[0] !== 'network') return { exitCode: 125, stdout: '', stderr: 'unexpected' };
    if (verb === 'inspect') {
      const name = rest[rest.length - 1];
      return name in nets ? { exitCode: 0, stdout: `${nets[name].internal}\n`, stderr: '' } : { exitCode: 1, stdout: '', stderr: 'No such network' };
    }
    if (verb === 'create') {
      nets[rest[rest.length - 1]] = { internal: rest.includes('--internal') };
      return { exitCode: 0, stdout: '', stderr: '' };
    }
    if (verb === 'rm') {
      delete nets[rest[0]];
      return { exitCode: 0, stdout: '', stderr: '' };
    }
    return { exitCode: 1, stdout: '', stderr: '' };
  };
  return { docker, nets, calls };
}
const task = (verifierNetwork) => ({ id: verifierNetwork, verifierNetwork });

test('a check makes the verifier (internal) and egress networks the stack names, and removes what it made', async () => {
  const f = fakeDocker();
  const net = await checkNetworks({ docker: f.docker, tasks: [task('public'), task('default'), task('none')] });
  assert.deepEqual(f.nets, { 'adam-bench_verifier': { internal: true }, 'adam-bench_egress': { internal: false } });
  assert.deepEqual(net.aliases, { verifier: 'adam-bench_verifier', egress: 'adam-bench_egress' });
  await net.cleanup();
  assert.deepEqual(f.nets, {});
});

test('a network that was already there is used and left alone', async () => {
  const f = fakeDocker({ 'adam-bench_verifier': { internal: true }, 'adam-bench_egress': { internal: false } });
  const net = await checkNetworks({ docker: f.docker, tasks: [task('public'), task('default')] });
  await net.cleanup();
  assert.equal(f.calls.some((c) => c.startsWith('network create') || c.startsWith('network rm')), false);
});

test('a verifier network that exists without the internal flag is refused, as the stack refuses it', async () => {
  const f = fakeDocker({ 'adam-bench_verifier': { internal: false } });
  await assert.rejects(checkNetworks({ docker: f.docker, tasks: [task('default')] }), (e) => e instanceof ContainerError && /not internal/.test(e.message));
});

test('tasks that need no network touch no network; a named network that is absent fails fast and says what to do', async () => {
  const f = fakeDocker();
  const none = await checkNetworks({ docker: f.docker, tasks: [task('none'), task('no-network')] });
  await none.cleanup();
  assert.deepEqual(f.calls, []);
  await assert.rejects(checkNetworks({ docker: f.docker, tasks: [task('bench-verifier')] }), (e) => e instanceof ContainerError && /bench-verifier/.test(e.message) && /docker network create/.test(e.message));
  assert.deepEqual(f.nets, {});
});

test('what a failed setup made is removed again', async () => {
  const f = fakeDocker();
  await assert.rejects(checkNetworks({ docker: f.docker, tasks: [task('public'), task('bench-verifier')] }));
  assert.deepEqual(f.nets, {});
});

test('the containers of a check start a public verifier on the stack\'s egress network, not on a bare name', async () => {
  const f = fakeDocker();
  const net = await checkNetworks({ docker: f.docker, tasks: [task('public'), task('default')] });
  const runs = [];
  const invoke = async (args) => { runs.push(args.join(' ')); return { exitCode: 0, stdout: '', stderr: '' }; };
  const c = new HostContainers({ aliases: net.aliases, invoke });
  const image = `registry.example.test/t@sha256:${'a'.repeat(64)}`;
  await c.run({ image, script: 'true', network: 'egress' });
  await c.run({ image, script: 'true', network: 'verifier' });
  assert.match(runs[0], /--network=adam-bench_egress /);
  assert.match(runs[1], /--network=adam-bench_verifier /);
  assert.ok(Object.keys(f.nets).includes('adam-bench_egress'), 'and the network exists while the check runs');
});
