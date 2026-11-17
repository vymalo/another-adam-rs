import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { TaskError, dockerfileFrom, listTasks, loadTask, parseImageRef, repoFromId } from '../lib/task.mjs';
import { tmp } from './helpers.mjs';
import { DIGEST, TOML, makeTask } from './taskfix.mjs';

const IMAGE = 'registry.example.test/tasks/img';

const problems = (dir, opts) => {
  try {
    loadTask(dir, opts);
  } catch (e) {
    assert.ok(e instanceof TaskError, String(e));
    return e.problems.join(' | ');
  }
  return assert.fail('the task should be refused');
};

test('a good task loads, with its image pinned by digest', (t) => {
  const dir = makeTask(tmp(t), 'instance_Org__Repo-abcdef0123');
  const task = loadTask(dir);
  assert.equal(task.image.ref, `${IMAGE}@${DIGEST}`);
  assert.equal(task.agentNetwork, 'none');
  assert.equal(task.verifierNetwork, 'public');
  assert.equal(task.workdir, '/app');
  assert.equal(task.repo, 'Org/Repo');
  assert.ok(task.goldPatch.endsWith('solution.patch'));
});

test('every missing part is refused and named', (t) => {
  const root = tmp(t);
  for (const [name, re] of [
    ['instruction.md', /instruction\.md is missing/],
    ['task.toml', /task\.toml is missing/],
    ['environment/Dockerfile', /environment\/ is missing/],
    ['tests/test.sh', /tests\/test\.sh is missing/],
    ['solution/solution.patch', /solution\/ is missing/],
  ]) {
    const id = `missing-${name.replace(/\W/g, '-')}`;
    assert.match(problems(makeTask(root, id, { [name]: null })), re, name);
  }
});

test('an empty statement, an empty solution and a test script with no reward are refused', (t) => {
  const root = tmp(t);
  assert.match(problems(makeTask(root, 'a1', { 'instruction.md': '  \n' })), /instruction\.md is empty/);
  assert.match(problems(makeTask(root, 'a2', { 'tests/test.sh': 'echo hi\n' })), /does not write/);
  fs.mkdirSync(path.join(root, 'a3', 'solution'), { recursive: true });
  fs.writeFileSync(path.join(root, 'a3', 'instruction.md'), 'x\n');
  assert.match(problems(path.join(root, 'a3')), /solution\/ is empty|task\.toml is missing/);
});

test('a task.toml it cannot read is refused with its line', (t) => {
  assert.match(problems(makeTask(tmp(t), 'bad-toml', { 'task.toml': '[agent]\nnetwork_mode = ???\n' })), /task\.toml line 2/);
});

test('an image not pinned by digest is refused, unless a pin is given', (t) => {
  const root = tmp(t);
  const dir = makeTask(root, 'tagged', { 'environment/Dockerfile': `FROM ${IMAGE}:v1\n` });
  assert.match(problems(dir), /not pinned by digest/);
  assert.equal(loadTask(dir, { pins: { [`${IMAGE}:v1`]: DIGEST } }).image.ref, `${IMAGE}@${DIGEST}`);
  assert.equal(loadTask(dir, { requireDigest: false }).image.digest, null);
  assert.match(problems(dir, { pins: { [`${IMAGE}:v1`]: 'sha256:short' } }), /not pinned by digest/);
  assert.match(problems(makeTask(root, 'bare', { 'environment/Dockerfile': `FROM ${IMAGE}\n` })), /not pinned by digest/);
  assert.match(problems(makeTask(root, 'var', { 'environment/Dockerfile': 'FROM ${BASE}\n' })), /not a plain reference/);
  assert.match(problems(makeTask(root, 'badsum', { 'environment/Dockerfile': `FROM ${IMAGE}@sha256:abc\n` })), /not a plain reference/);
});

test('the image may be named by task.toml instead of a Dockerfile', (t) => {
  const root = tmp(t);
  const dir = makeTask(root, 'viatoml', { 'environment/Dockerfile': null, 'environment/keep': '', 'task.toml': `${TOML}docker_image = "${IMAGE}@${DIGEST}"\n` });
  assert.equal(loadTask(dir).image.digest, DIGEST);
});

test('an agent network mode other than none is refused, and so is a missing one', (t) => {
  const root = tmp(t);
  assert.match(problems(makeTask(root, 'net1', { 'task.toml': TOML.replace('no-network', 'public') })), /network_mode must be "none"/);
  assert.match(problems(makeTask(root, 'net2', { 'task.toml': '[verifier]\nnetwork_mode = "public"\n' })), /network_mode must be "none"/);
  assert.equal(loadTask(makeTask(root, 'net3', { 'task.toml': TOML.replace('no-network', 'none') })).agentNetwork, 'none');
});

test('bad resource numbers and working directories are refused', (t) => {
  const root = tmp(t);
  assert.match(problems(makeTask(root, 'r1', { 'task.toml': TOML.replace('cpus = 1', 'cpus = 0') })), /cpus must be a positive number/);
  assert.match(problems(makeTask(root, 'r2', { 'task.toml': `${TOML}workdir = "../x"\n` })), /workdir must be an absolute path/);
});

test('an unsafe directory name is refused', (t) => {
  const root = tmp(t);
  fs.mkdirSync(path.join(root, '.hidden'));
  assert.throws(() => loadTask(path.join(root, '.hidden')), TaskError);
});

test('several problems are named together', (t) => {
  const dir = makeTask(tmp(t), 'many', { 'instruction.md': null, 'tests/test.sh': null });
  const p = problems(dir);
  assert.match(p, /instruction\.md is missing/);
  assert.match(p, /tests\/test\.sh is missing/);
});

test('listTasks loads what is good and reports what is not', (t) => {
  const root = tmp(t);
  makeTask(root, 'v2/good-one');
  makeTask(root, 'v2/bad-one', { 'instruction.md': null });
  const { tasks, errors } = listTasks(root);
  assert.deepEqual(tasks.map((x) => x.id), ['good-one']);
  assert.equal(errors[0].id, 'bad-one');
});

test('helpers: image references, FROM lines, repository names', () => {
  assert.deepEqual(parseImageRef('a.b/c/d:1.2@sha256:' + 'f'.repeat(64)), { name: 'a.b/c/d', tag: '1.2', digest: `sha256:${'f'.repeat(64)}` });
  assert.deepEqual(parseImageRef('localhost:5000/x:t'), { name: 'localhost:5000/x', tag: 't', digest: null });
  assert.equal(parseImageRef('a b'), null);
  assert.equal(parseImageRef('a@b'), null);
  assert.equal(dockerfileFrom('# c\n\nFROM --platform=linux/amd64 img:1 AS base\nFROM other\n'), 'img:1');
  assert.equal(dockerfileFrom('RUN x\n'), null);
  assert.equal(repoFromId('instance_NodeBB__NodeBB-8168c6c'), 'NodeBB/NodeBB');
  assert.equal(repoFromId('plain'), null);
});
