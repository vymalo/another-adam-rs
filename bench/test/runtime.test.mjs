// The host runtime of the bench: one store, one user for both arms, one set of limits, and the runtime in the
// configuration fingerprint. And the scan that keeps the harness off the old container service: no file under
// bench/ names it, outside the decision records (which are in docs/decisions).
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { configFingerprint } from '../lib/budget.mjs';
import { normalizeConfig } from '../lib/config.mjs';
import { PINS } from '../lib/pins.mjs';
import { RUNTIME, runtimeLimits, runtimeUser } from '../lib/runtime.mjs';
import { rawConfig } from './helpers.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.join(HERE, '..', '..');
const config = normalizeConfig(rawConfig({ capUSD: 10 }));
const LOCKS = ['{"public":1}', ''];
const fp = (runtime) => configFingerprint({ config, pins: PINS, lockTexts: LOCKS, key: 'key-for-tests-0001', ...(runtime ? { runtime } : {}) });

test('both arms run as uid 10001 and get the same CPU and memory from one setting', () => {
  assert.equal(RUNTIME.uid, 10001);
  assert.equal(RUNTIME.gid, 10001);
  assert.equal(RUNTIME.cpus, 4);
  assert.equal(RUNTIME.memoryMb, 12 * 1024);
  assert.equal(runtimeUser(), '10001:10001');
  assert.deepEqual(runtimeLimits(), { cpus: 4, memoryMb: 12 * 1024 });
});

test('the fingerprint has a runtime field, and a change in the runtime changes the digest and is named', () => {
  const base = fp();
  assert.equal(typeof base.fields.runtime, 'string');
  const changed = fp({ ...RUNTIME, uid: 1000 });
  assert.notEqual(changed.fields.runtime, base.fields.runtime);
  assert.notEqual(changed.digest, base.digest);
  for (const change of [{ cpus: 2 }, { memoryMb: 4096 }, { store: 'service' }, { coder: 'image' }, { host: 'other' }]) {
    assert.notEqual(fp({ ...RUNTIME, ...change }).digest, base.digest, JSON.stringify(change));
  }
  // the endpoint and the cap are still not in it
  const other = configFingerprint({ config: { ...config, endpoint: 'https://other.example.test/v1', capMicro: 99_000_000 }, pins: PINS, lockTexts: LOCKS, key: 'key-for-tests-0001' });
  assert.equal(other.digest, base.digest);
});

test('no file under bench/ names the container service the harness used to run (the decision records live in docs/decisions)', () => {
  const word = ['pod', 'man'].join('');
  const r = spawnSync('git', ['grep', '-i', '-l', word, '--', 'bench'], { cwd: REPO, encoding: 'utf8' });
  assert.ok(r.status === 0 || r.status === 1, r.stderr);
  assert.equal(r.status === 1 ? '' : r.stdout.trim(), '', `bench/ still names ${word}: ${r.stdout}`);
});
