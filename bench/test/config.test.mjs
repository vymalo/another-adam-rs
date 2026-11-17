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

// ---- fix round: rows 3 (key), 4, 5, 6, 11, 12 of the review ----

test('row 4: a directory that merely starts with two dots is inside the repository, not outside it', (t) => {
  const repo = fakeRepo(t);
  fs.mkdirSync(path.join(repo, '..cfg'));
  const file = path.join(repo, '..cfg', 'config.json');
  assert.throws(() => assertOutsideRepo(file, 'BENCH_CONFIG', [repo]), (e) => e instanceof ConfigError && /repository/.test(e.message));
  // a sibling of the repository really is outside
  assert.doesNotThrow(() => assertOutsideRepo(path.join(path.dirname(repo), 'elsewhere', 'config.json'), 'BENCH_CONFIG', [repo]));
});

test('row 5: any path under a directory with a .git entry (directory or file) is refused, with no roots given', (t) => {
  const repo = fakeRepo(t);
  const deep = path.join(repo, 'a', 'b', 'config.json'); // the directories do not exist yet
  assert.throws(() => assertOutsideRepo(deep, 'BENCH_HOME'), (e) => e instanceof ConfigError && e.field === 'BENCH_HOME' && /repository/.test(e.message));
  assert.throws(() => assertOutsideRepo(repo, 'BENCH_HOME'), ConfigError, 'the work tree itself');
  const worktree = tmp(t);
  fs.writeFileSync(path.join(worktree, '.git'), 'gitdir: /somewhere/else\n'); // a linked work tree has a .git file
  assert.throws(() => assertOutsideRepo(path.join(worktree, 'out.md'), '--out'), ConfigError);
  const loose = tmp(t);
  assert.doesNotThrow(() => assertOutsideRepo(path.join(loose, 'x', 'ledger.jsonl'), '--ledger'));
  // the same through loadConfig, with no `roots` argument
  fs.mkdirSync(path.join(repo, 'sub'));
  const inside = path.join(repo, 'sub', 'config.json');
  fs.writeFileSync(inside, JSON.stringify(rawConfig()));
  assert.throws(() => loadConfig({ env: { BENCH_CONFIG: inside } }), (e) => e instanceof ConfigError && e.field === 'BENCH_CONFIG');
});

test('row 12: the report refuses --ledger and --out inside a work tree, and BENCH_HOME', (t) => {
  const repo = fakeRepo(t);
  const home = tmp(t);
  const env = { ...process.env, HOME: home, BENCH_HOME: home, BENCH_CONFIG: '' };
  const out = spawnSync(process.execPath, [BENCH, 'report', '--out', path.join(repo, 'report.md')], { env, encoding: 'utf8' });
  assert.equal(out.status, 78, out.stderr);
  assert.match(out.stderr, /--out/);
  assert.ok(!fs.existsSync(path.join(repo, 'report.md')));
  const ledger = spawnSync(process.execPath, [BENCH, 'report', '--ledger', path.join(repo, 'ledger.jsonl')], { env, encoding: 'utf8' });
  assert.equal(ledger.status, 78, ledger.stderr);
  assert.match(ledger.stderr, /--ledger/);
});

test('row 6: a real-looking model id and an arm option named like a counter are accepted', () => {
  const id = 'Qwen3-Coder-480B-A35B-Instruct-FP8';
  const c = normalizeConfig(rawConfig({ modelId: id, modelLabel: id, arms: { a: { token_budget: 5000, note: id, thinking: { budget_tokens: 2048 } } } }));
  assert.equal(c.modelId, id);
  assert.equal(looksLikeKey(id), true, 'the entropy rule is still a rule of looksLikeKey; it is just not applied to ids, labels and arms');
});

test('row 6: a secret-named field holding any non-empty value of any type is refused; known prefixes and long hex still are, in ids and arms', () => {
  refused(rawConfig({ apiKey: 12345678 }), 'apiKey');
  refused(rawConfig({ arms: { a: { password: 42 } } }), 'arms.a.password');
  refused(rawConfig({ arms: { a: { secret: true } } }), 'arms.a.secret');
  refused(rawConfig({ arms: { a: { auth: { nested: 'x' } } } }), 'arms.a.auth');
  refused(rawConfig({ arms: { a: { token: 'abc' } } }), 'arms.a.token');
  assert.doesNotThrow(() => normalizeConfig(rawConfig({ arms: { a: { token: '' } } })), 'an empty value holds no secret');
  refused(rawConfig({ modelId: fakeKey('ghp_') }), 'modelId');
  refused(rawConfig({ arms: { a: { note: '0123456789abcdef0123456789abcdef' } } }), 'arms.a.note');
});

test('row 11: http is for a loopback host only; anything else must be https', () => {
  const bad = (endpoint) =>
    assert.throws(() => normalizeConfig(rawConfig({ endpoint })), (e) => e instanceof ConfigError && e.field === 'endpoint', endpoint);
  bad('http://models.example.test/v1');
  bad('http://10.0.0.5/v1');
  bad('http://127.0.0.1.example.test/v1');
  for (const ok of ['http://127.0.0.1:8080/v1', 'http://localhost:9/v1', 'http://[::1]:9/v1', 'http://127.1.2.3/v1', 'https://models.example.test/v1']) {
    assert.doesNotThrow(() => normalizeConfig(rawConfig({ endpoint: ok })), ok);
  }
});

test('row 3: a key with CR, LF, a control character or a space is refused by name, never by value', () => {
  const c = normalizeConfig(rawConfig());
  for (const bad of ['abc\ndef', 'abc\r\ndef', 'abc def', 'abc\tdef', 'abc\u0000def', 'abc\u007fdef', ' leading', 'trailing\n']) {
    assert.throws(
      () => readKey(c, { BENCH_MODEL_API_KEY: bad }),
      (e) => e instanceof ConfigError && e.exitCode === 78 && e.message.includes('BENCH_MODEL_API_KEY') && !e.message.includes('def') && !e.message.includes('leading') && !e.message.includes('trailing'),
      JSON.stringify(bad),
    );
  }
  assert.equal(readKey(c, { BENCH_MODEL_API_KEY: 'sk-abc.DEF_123' }), 'sk-abc.DEF_123');
});

// ---- mutation round ----

test('a root with no .git still refuses root/..cfg/config.json', (t) => {
  const root = tmp(t);
  assert.throws(() => assertOutsideRepo(path.join(root, '..cfg', 'config.json'), 'BENCH_CONFIG', [root]), ConfigError);
});

test('a symlink outside that points into a work tree is refused, with no roots', (t) => {
  const repo = fakeRepo(t);
  fs.mkdirSync(path.join(repo, 'sub'));
  fs.writeFileSync(path.join(repo, 'sub', 'config.json'), '{}');
  const outside = tmp(t);
  fs.symlinkSync(path.join(repo, 'sub'), path.join(outside, 'linkdir'));
  assert.throws(() => assertOutsideRepo(path.join(outside, 'linkdir', 'config.json'), 'BENCH_CONFIG', []), ConfigError);
});

test('only the loopback names are loopback: localhost-prefixed hosts are not', () => {
  for (const bad of ['http://localhost.models.example.test/v1', 'http://localhostx/v1', 'http://xlocalhost/v1']) {
    assert.throws(() => normalizeConfig(rawConfig({ endpoint: bad })), (e) => e instanceof ConfigError && e.field === 'endpoint', bad);
  }
});

test('the value of a refused secret field is in neither the message nor the command line\'s stderr', (t) => {
  const value = 'hunter2-unique-value';
  assert.throws(() => normalizeConfig(rawConfig({ arms: { a: { api_key: value } } })), (e) => e instanceof ConfigError && !e.message.includes(value));
  const dir = tmp(t);
  const file = path.join(dir, 'config.json');
  fs.writeFileSync(file, JSON.stringify(rawConfig({ arms: { a: { api_key: value } } })));
  const r = spawnSync(process.execPath, [BENCH, 'config'], { env: { ...process.env, BENCH_CONFIG: file, HOME: dir }, encoding: 'utf8' });
  assert.equal(r.status, 78);
  assert.ok(!r.stderr.includes(value) && !r.stdout.includes(value));
});

test('a counter name excuses a number only: a string under auth_token_max or api_key_limit is refused', () => {
  refused(rawConfig({ arms: { a: { auth_token_max: 'abc' } } }), 'arms.a.auth_token_max');
  refused(rawConfig({ arms: { a: { api_key_limit: 'abc' } } }), 'arms.a.api_key_limit');
  refused(rawConfig({ arms: { a: { token_budget: true } } }), 'arms.a.token_budget');
  assert.doesNotThrow(() => normalizeConfig(rawConfig({ arms: { a: { token_budget: 5000 } } })));
});

test('the context window is required, because both agents are given the same value (finding 19)', () => {
  const raw = rawConfig();
  delete raw.contextWindow;
  refused(raw, 'contextWindow');
  refused(rawConfig({ contextWindow: null }), 'contextWindow');
  assert.equal(normalizeConfig(rawConfig({ contextWindow: 65536 })).contextWindow, 65536);
});

test('the limits of a run are one set of values in the configuration: minutes, steps and the cancel grace', () => {
  const d = normalizeConfig(rawConfig());
  assert.deepEqual(d.limits, { minutes: 50, steps: 200, cancelGraceSeconds: 60, limitMs: 50 * 60 * 1000, cancelGraceMs: 60_000 });
  const c = normalizeConfig(rawConfig({ limits: { minutes: 10, steps: 80, cancelGraceSeconds: 5 } }));
  assert.deepEqual([c.limits.limitMs, c.limits.steps, c.limits.cancelGraceMs], [600_000, 80, 5000]);
  refused(rawConfig({ limits: { minutes: 0 } }), 'limits.minutes');
  refused(rawConfig({ limits: { steps: 1.5 } }), 'limits.steps');
  refused(rawConfig({ limits: { hours: 1 } }), 'limits.hours');
});
