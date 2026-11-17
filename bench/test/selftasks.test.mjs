import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { SELF_TASKS, taskMarker } from '../lib/agent-script.mjs';
import { buildArgsFlags, ContainerError, FakeContainers } from '../lib/containers.mjs';
import { SELF_IDS, dryRunConfig, materializeSelf, parseKill } from '../lib/dryrun.mjs';
import { normalizeConfig } from '../lib/config.mjs';
import { hiddenTestPaths, sanitizePatch } from '../lib/grade.mjs';
import { loadTask } from '../lib/task.mjs';
import { git } from './gitfix.mjs';
import { tmp } from './helpers.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const SELF = path.join(HERE, '..', 'tasks', 'self');
const DIGEST = `sha256:${'d'.repeat(64)}`;
const pins = (id) => ({ [`adam-bench-self/${id}:1`]: DIGEST });

test('the self-test tasks are the three the scripted model knows, each valid in the format of the others', () => {
  assert.deepEqual([...SELF_IDS].sort(), Object.keys(SELF_TASKS).sort());
  assert.deepEqual(fs.readdirSync(SELF).filter((n) => !n.startsWith('README')).sort(), [...SELF_IDS].sort());
  for (const id of SELF_IDS) {
    const task = loadTask(path.join(SELF, id), { pins: pins(id), source: 'self' });
    assert.equal(task.agentNetwork, 'none');
    assert.equal(task.verifierNetwork, 'none');
    assert.equal(task.workdir, '/app');
    assert.equal(task.image.digest, DIGEST);
    assert.equal(taskMarker([{ role: 'user', content: task.instruction }]), id, `${id}'s statement carries its marker`);
    assert.ok(task.goldPatch.endsWith('solution.patch'));
  }
});

test('the statements hold no private value and no word of the market', () => {
  for (const id of SELF_IDS) {
    const text = fs.readdirSync(path.join(SELF, id), { recursive: true }).filter((f) => fs.statSync(path.join(SELF, id, f)).isFile()).map((f) => fs.readFileSync(path.join(SELF, id, f), 'utf8')).join('\n');
    assert.doesNotMatch(text, /https?:\/\/(?!models\.example\.test)[a-z0-9.-]+\.(com|io|ai|net)\b/i);
  }
});

/** Run a task's test script on this machine, in a copy of its repository, with the paths rewritten. Fixture scripts only. */
function reward(t, id, { gold, scripted = null }) {
  const work = tmp(t);
  const repo = path.join(work, 'app');
  fs.cpSync(path.join(SELF, id, 'repo'), repo, { recursive: true });
  git(repo, 'init', '-q', '-b', 'main');
  git(repo, 'add', '-A');
  git(repo, '-c', 'user.name=t', '-c', 'user.email=t@example.test', 'commit', '-q', '-m', 'base');
  if (gold) git(repo, 'apply', path.join(SELF, id, 'solution', 'solution.patch'));
  if (scripted) fs.writeFileSync(path.join(repo, scripted.file), scripted.content);
  const logs = path.join(work, 'logs');
  fs.mkdirSync(logs);
  const tests = path.join(SELF, id, 'tests');
  const script = fs.readFileSync(path.join(tests, 'test.sh'), 'utf8').replaceAll('/logs/verifier', logs).replaceAll('/tests', tests).replaceAll('/app', repo);
  const f = path.join(work, 'test.sh');
  fs.writeFileSync(f, script);
  spawnSync('bash', [f], { env: { PATH: process.env.PATH, HOME: work, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null' }, encoding: 'utf8' });
  return fs.readFileSync(path.join(logs, 'reward.txt'), 'utf8').trim();
}

test('each self-test task: the gold change passes and the unchanged repository fails', (t) => {
  for (const id of SELF_IDS) {
    assert.equal(reward(t, id, { gold: true }), '1', `${id}: gold`);
    assert.equal(reward(t, id, { gold: false }), '0', `${id}: empty`);
  }
});

test('what the scripted model writes passes hello and fix-add and fails wrong', (t) => {
  for (const id of SELF_IDS) assert.equal(reward(t, id, { gold: false, scripted: SELF_TASKS[id] }), id === 'wrong' ? '0' : '1', id);
});

test('the hidden test of fix-add is read from its patch, and the agent\'s change to it is stripped', () => {
  const task = loadTask(path.join(SELF, 'fix-add'), { pins: pins('fix-add') });
  assert.deepEqual(hiddenTestPaths(task), ['add.test.mjs']);
  const edit = (p) => `diff --git a/${p} b/${p}\n--- a/${p}\n+++ b/${p}\n@@ -1 +1 @@\n-a\n+b\n`;
  const r = sanitizePatch(edit('add.mjs') + edit('add.test.mjs'), { hiddenTests: hiddenTestPaths(task) });
  assert.deepEqual(r.removed, ['add.test.mjs']);
  assert.match(r.patch, /add\.mjs/);
});

test('materializing a self-test task: a task directory with no repo, and a build context with the repository and one commit', async (t) => {
  const home = tmp(t);
  const m = await materializeSelf({ srcDir: path.join(SELF, 'fix-add'), home });
  assert.equal(m.tag, 'adam-bench-self/fix-add:1');
  assert.equal(fs.existsSync(path.join(m.taskDir, 'repo')), false);
  assert.ok(fs.existsSync(path.join(m.taskDir, 'tests', 'test.sh')));
  assert.equal(fs.readFileSync(path.join(m.context, 'repo', 'add.mjs'), 'utf8'), 'export const add = (a, b) => a - b;\n');
  assert.equal(git(path.join(m.context, 'repo'), 'rev-list', '--count', 'HEAD').trim(), '1');
  assert.match(fs.readFileSync(m.dockerfile, 'utf8'), /^FROM \$\{BASE_IMAGE\}$/m);
  const task = loadTask(m.taskDir, { pins: pins('fix-add') });
  assert.equal(task.id, 'fix-add');
  const again = await materializeSelf({ srcDir: path.join(SELF, 'fix-add'), home });
  assert.equal(again.context, m.context, 'made again from scratch');
});

test('the image of a self-test task is built from a base image named at build time, and the arguments are plain', async () => {
  const containers = new FakeContainers();
  await containers.build('/ctx', 'adam-bench-self/hello:1', '/ctx/Dockerfile', { BASE_IMAGE: `registry.example.test/base@${DIGEST}` });
  assert.deepEqual(containers.built[0].buildArgs, { BASE_IMAGE: `registry.example.test/base@${DIGEST}` });
  assert.deepEqual(buildArgsFlags({ A: 'b' }), ['--build-arg', 'A=b']);
  assert.deepEqual(buildArgsFlags(), []);
  for (const bad of [{ 'A B': 'x' }, { A: 'x y' }, { A: 'x$y' }, { A: 'x"y' }]) assert.throws(() => buildArgsFlags(bad), ContainerError);
  await assert.rejects(containers.build('/c', 't', '/c/D', { A: 'x y' }), ContainerError);
});

test('the dry run\'s configuration is valid, made up, and names the scripted model only', () => {
  const raw = dryRunConfig({ endpoint: 'http://127.0.0.1:1/v1' });
  const config = normalizeConfig(raw);
  assert.equal(config.modelId, 'example-model');
  assert.equal(config.endpoint, 'http://127.0.0.1:1/v1');
  assert.match(config.tasks.workspaceImage, /@sha256:[0-9a-f]{64}$/);
  assert.equal(normalizeConfig(dryRunConfig({ endpoint: 'http://127.0.0.1:1/v1', perRunBudgetUSD: 34, capUSD: 100 })).perRunBudgetMicro, 34_000_000);
});

test('the kill setting is a run and a number of calls, and anything else is none', () => {
  assert.deepEqual(parseKill('fix-add-opencode-max:2'), { run: 'fix-add-opencode-max', calls: 2 });
  for (const bad of [undefined, '', 'x', 'x:', ':2', 'a b:2', 'x:y', 'x:1000']) assert.equal(parseKill(bad), null, String(bad));
});

test('the command line: a bad phase is a usage error and a results directory in the repository is refused', (t) => {
  const bench = path.join(HERE, '..', 'bin', 'bench.mjs');
  const run = (args, env = {}) => spawnSync(process.execPath, [bench, ...args], { env: { ...process.env, ...env }, encoding: 'utf8' });
  assert.equal(run(['dry-run', '--phase', 'nope'], { BENCH_HOME: tmp(t) }).status, 64);
  assert.equal(run(['doctor', '--bogus'], { BENCH_HOME: tmp(t) }).status, 64);
  const inside = run(['dry-run'], { BENCH_HOME: path.join(HERE, 'results') });
  assert.equal(inside.status, 78);
  assert.equal(run(['doctor'], { BENCH_HOME: path.join(HERE, 'results') }).status, 78);
  void execFileSync;
});
