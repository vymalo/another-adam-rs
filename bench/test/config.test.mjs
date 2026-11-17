import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { ConfigError, assertOutsideRepo, describeConfig, loadConfig, looksLikeKey, normalizeConfig, readKey } from '../lib/config.mjs';
import { fakeRepo, rawConfig, tmp } from './helpers.mjs';

const BENCH = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'bin', 'bench.mjs');
const fakeKey = (prefix = 'sk') => `${prefix}-${'Qz7'.repeat(14)}`;

function refused(raw, field) {
  assert.throws(
    () => normalizeConfig(raw),
    (e) => e instanceof ConfigError && e.exitCode === 78 && e.field === field,
    `expected a ConfigError on ${field}`,
  );
}

test('a good configuration is normalised to micro-dollars', () => {
  const c = normalizeConfig(rawConfig());
  assert.equal(c.capMicro, 10_000_000);
  assert.equal(c.perRunBudgetMicro, 3_000_000);
  assert.deepEqual(c.prices, { input: 3_000_000, cachedInput: 300_000, cacheWrite: 3_750_000, output: 15_000_000 });
  assert.equal(c.endpoint, 'https://models.example.test/v1');
  assert.equal(c.keyEnv, 'BENCH_MODEL_API_KEY');
  assert.equal(c.concurrency, 2);
  assert.equal(c.timeouts.firstByteMs, 300_000);
});

test('an unknown field is refused, at every level', () => {
  refused(rawConfig({ extra: 1 }), 'extra');
  refused(rawConfig({ prices: { ...rawConfig().prices, bonus: 1 } }), 'prices.bonus');
  refused(rawConfig({ timeouts: { nope: 1 } }), 'timeouts.nope');
});

test('required fields, ranges and shapes are checked and named', () => {
  const raw = rawConfig();
  delete raw.modelLabel;
  refused(raw, 'modelLabel');
  refused(rawConfig({ capUSD: 0 }), 'capUSD');
  refused(rawConfig({ perRunBudgetUSD: 11 }), 'perRunBudgetUSD');
  refused(rawConfig({ prices: { ...rawConfig().prices, outputPerMTok: -1 } }), 'prices.outputPerMTok');
  refused(rawConfig({ arms: {} }), 'arms');
  refused(rawConfig({ arms: { 'Bad Name': {} } }), 'arms.Bad Name');
  refused(rawConfig({ concurrency: 99 }), 'concurrency');
  refused(rawConfig({ keyEnv: 'not an env name' }), 'keyEnv');
});

test('an arm may not change what is compared', () => {
  refused(rawConfig({ arms: { a: { model: 'other' } } }), 'arms.a.model');
  refused(rawConfig({ arms: { a: { stream: false } } }), 'arms.a.stream');
  refused(rawConfig({ arms: { a: { tools: [] } } }), 'arms.a.tools');
});

test('an endpoint holds no credentials, query or fragment, and is http or https', () => {
  for (const bad of ['https://user:pw@models.example.test/v1', 'https://models.example.test/v1?key=abc', 'ftp://models.example.test/v1', 'not a url']) {
    assert.throws(
      () => normalizeConfig(rawConfig({ endpoint: bad })),
      (e) => e instanceof ConfigError && e.field === 'endpoint' && !e.message.includes('pw') && !e.message.includes('abc'),
    );
  }
});

test('a field named like a secret that holds a value is refused, wherever it is', () => {
  refused(rawConfig({ apiKey: 'whatever' }), 'apiKey');
  refused(rawConfig({ arms: { a: { api_key: 'whatever' } } }), 'arms.a.api_key');
  refused(rawConfig({ arms: { a: { headers: { Authorization: 'x' } } } }), 'arms.a.headers.Authorization');
  // a number under a name with "tokens" in it is an ordinary setting
  assert.doesNotThrow(() => normalizeConfig(rawConfig({ arms: { a: { max_tokens: 100 } } })));
});

test('a key-like value is refused anywhere but the endpoint, and never quoted', () => {
  const key = fakeKey();
  for (const raw of [rawConfig({ modelLabel: key }), rawConfig({ modelId: key }), rawConfig({ arms: { a: { note: key } } }), rawConfig({ arms: { a: { list: [key] } } })]) {
    assert.throws(
      () => normalizeConfig(raw),
      (e) => e instanceof ConfigError && e.exitCode === 78 && !e.message.includes(key) && /key/.test(e.message),
    );
  }
  assert.equal(looksLikeKey('example-model-large-v2'), false);
  assert.equal(looksLikeKey('accounts/example/models/llama-v3p1-70b-instruct'), false);
  assert.equal(looksLikeKey('Example Model'), false);
  assert.equal(looksLikeKey('a'.repeat(32).replace(/a/g, '0')), true, 'long hex is a key');
  assert.equal(looksLikeKey(fakeKey('ghp_')), true);
});

test('a configuration inside the repository is refused, also through a symlink', (t) => {
  const repo = fakeRepo(t);
  const outside = tmp(t);
  fs.mkdirSync(path.join(repo, 'sub'));
  const inside = path.join(repo, 'sub', 'config.json');
  fs.writeFileSync(inside, JSON.stringify(rawConfig()));
  assert.throws(() => loadConfig({ env: { BENCH_CONFIG: inside }, roots: [repo] }), (e) => e instanceof ConfigError && e.field === 'BENCH_CONFIG' && /repository/.test(e.message));
  const link = path.join(outside, 'link.json');
  fs.symlinkSync(inside, link);
  assert.throws(() => loadConfig({ env: { BENCH_CONFIG: link }, roots: [repo] }), (e) => e instanceof ConfigError && /repository/.test(e.message));
  const fine = path.join(outside, 'config.json');
  fs.writeFileSync(fine, JSON.stringify(rawConfig()));
  assert.equal(loadConfig({ env: { BENCH_CONFIG: fine }, roots: [repo] }).modelLabel, 'Example Model');
  assert.throws(() => assertOutsideRepo(path.join(repo, 'results'), 'BENCH_HOME', [repo]), ConfigError);
});

test('the file must be JSON and must not hold the value of the key variable', (t) => {
  const dir = tmp(t);
  const file = path.join(dir, 'config.json');
  fs.writeFileSync(file, '{ nope');
  assert.throws(() => loadConfig({ env: { BENCH_CONFIG: file }, roots: [] }), (e) => e instanceof ConfigError && /JSON/.test(e.message) && !e.message.includes('nope'));
  const secret = 'a-value-that-is-in-the-environment';
  fs.writeFileSync(file, JSON.stringify(rawConfig({ modelLabel: `label ${secret}` })));
  assert.throws(
    () => loadConfig({ env: { BENCH_CONFIG: file, BENCH_MODEL_API_KEY: secret }, roots: [] }),
    (e) => e instanceof ConfigError && !e.message.includes(secret),
  );
});

test('the key comes only from the environment variable the configuration names', () => {
  const c = normalizeConfig(rawConfig({ keyEnv: 'MY_KEY_VAR' }));
  assert.equal(readKey(c, { MY_KEY_VAR: 'abc' }), 'abc');
  assert.throws(() => readKey(c, {}), (e) => e instanceof ConfigError && e.field === 'keyEnv' && e.message.includes('MY_KEY_VAR'));
});

test('describing a configuration shows no model id, no URL path, no prices', () => {
  const text = describeConfig(normalizeConfig(rawConfig({ endpoint: 'https://models.example.test/private/path/v1' })));
  assert.match(text, /Example Model/);
  assert.match(text, /models\.example\.test/);
  assert.doesNotMatch(text, /private\/path|example-model|3\.75/);
});

test('the command line exits 78 on a bad configuration and names the field, never the value', (t) => {
  const dir = tmp(t);
  const file = path.join(dir, 'config.json');
  const key = fakeKey();
  fs.writeFileSync(file, JSON.stringify(rawConfig({ modelLabel: key })));
  const r = spawnSync(process.execPath, [BENCH, 'config'], { env: { ...process.env, BENCH_CONFIG: file, HOME: dir }, encoding: 'utf8' });
  assert.equal(r.status, 78);
  assert.match(r.stderr, /modelLabel/);
  assert.ok(!r.stderr.includes(key) && !r.stdout.includes(key));
  fs.writeFileSync(file, JSON.stringify(rawConfig()));
  const ok = spawnSync(process.execPath, [BENCH, 'config'], { env: { ...process.env, BENCH_CONFIG: file, HOME: dir }, encoding: 'utf8' });
  assert.equal(ok.status, 0);
  assert.doesNotMatch(ok.stdout, /example-model/);
});
