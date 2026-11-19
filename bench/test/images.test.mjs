import assert from 'node:assert/strict';
import { test } from 'node:test';
import { HostContainers } from '../lib/containers.mjs';
import { once } from '../lib/images.mjs';

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

test('R14: the host asks whether an image is in its store, and tells a missing image from an error', async () => {
  const seen = [];
  const answers = [0, 1, 125];
  const exec = (bin, args, opts, cb) => {
    seen.push(args);
    const code = answers.length > 0 ? answers.shift() : 0;
    cb(code === 0 ? null : Object.assign(new Error('x'), { code }), '', '');
    return { stdin: { on() {}, end() {} } };
  };
  const p = new HostContainers({ exec });
  assert.equal(await p.hasImage('adam-bench-own/x:1'), true);
  assert.equal(await p.hasImage('adam-bench-own/x:1'), false);
  await assert.rejects(p.hasImage('adam-bench-own/x:1'), /failed \(exit 125\)/);
  assert.deepEqual(seen[0].slice(0, 3), ['image', 'inspect', '--format']);
  assert.equal(seen[0].at(-1), 'adam-bench-own/x:1');
});
