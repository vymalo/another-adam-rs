// What a grading opens that a container could have made, and the directory a grading gets: the host's own, one per
// attempt, mounted into the grading container only.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { FakeContainers } from '../lib/containers.mjs';
import { gradeOnce, hostGradeIO } from '../lib/grade.mjs';
import { loadTask } from '../lib/task.mjs';
import { tmp } from './helpers.mjs';
import { makeTask } from './taskfix.mjs';

const guard = (p, ms = 5000) => Promise.race([p, new Promise((_, reject) => setTimeout(() => reject(new Error(`hung for ${ms} ms`)), ms).unref())]);
const rewardDir = (spec) => spec.mounts.find((m) => m.target === '/logs/verifier').source;

test('defect 1: a FIFO where the reward should be does not hang the harness: it is a bad reward', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  const containers = new FakeContainers({
    onRun: (spec) => {
      execFileSync('mkfifo', [path.join(rewardDir(spec), 'reward.txt')]);
      return { exitCode: 0 };
    },
  });
  const r = await guard(gradeOnce({ containers, task, workRoot: tmp(t) }));
  assert.equal(r.pass, false);
  assert.equal(r.error, 'bad-reward');
  assert.equal(r.reward, 'invalid');
});

test('defect 1: a directory, a device-like or a huge file where the reward should be is a bad reward too', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  for (const make of [(f) => fs.mkdirSync(f), (f) => fs.writeFileSync(f, '1'.repeat(1000))]) {
    const containers = new FakeContainers({ onRun: (spec) => (make(path.join(rewardDir(spec), 'reward.txt')), { exitCode: 0 }) });
    const r = await guard(gradeOnce({ containers, task, workRoot: tmp(t) }));
    assert.equal(r.pass, false);
    assert.equal(r.error, 'bad-reward');
  }
});

test('a grading stages the tests and the patch in a directory of the harness\'s, read-only in that container only, and removes it', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  fs.mkdirSync(path.join(task.dir, 'tests'), { recursive: true });
  fs.writeFileSync(path.join(task.dir, 'tests', 'extra.txt'), 'hidden\n');
  const patch = path.join(tmp(t), 'model.patch');
  fs.writeFileSync(patch, 'diff --git a/x b/x\n');
  const work = tmp(t);
  const containers = new FakeContainers({ onRun: (spec) => (fs.writeFileSync(path.join(rewardDir(spec), 'reward.txt'), '1\n'), { exitCode: 0 }) });
  const r = await gradeOnce({ containers, io: hostGradeIO(work), task, patchFile: patch });
  assert.equal(r.pass, true);
  const spec = containers.runs[0];
  const mounts = Object.fromEntries(spec.mounts.map((m) => [m.target, m]));
  assert.deepEqual(Object.keys(mounts).sort(), ['/logs/verifier', '/patch/model.patch', '/tests']);
  assert.equal(mounts['/tests'].source, path.join(task.dir, 'tests'));
  assert.equal(mounts['/tests'].readOnly, true);
  assert.equal(mounts['/patch/model.patch'].source, path.resolve(patch));
  assert.equal(mounts['/patch/model.patch'].readOnly, true);
  assert.equal(mounts['/logs/verifier'].readOnly, undefined);
  assert.equal(fs.existsSync(mounts['/logs/verifier'].source), false, 'the logs directory is removed after the attempt');
});

test('each grading attempt has its own directory, and none of them is made before it', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  const work = tmp(t);
  const containers = new FakeContainers({ onRun: (spec) => (fs.writeFileSync(path.join(rewardDir(spec), 'reward.txt'), '1\n'), { exitCode: 0 }) });
  await gradeOnce({ containers, io: hostGradeIO(work), task, workRoot: work });
  await gradeOnce({ containers, io: hostGradeIO(work), task, workRoot: work });
  const dirs = containers.runs.map(rewardDir);
  assert.notEqual(dirs[0], dirs[1]);
  assert.deepEqual(fs.readdirSync(work), [], 'no grade-logs directory is left');
});

test('a reward the grading container made a link to a file of the host is not a reward', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  const hostFile = path.join(tmp(t), 'one.txt');
  fs.writeFileSync(hostFile, '1\n');
  const containers = new FakeContainers({
    onRun: (spec) => {
      fs.symlinkSync(hostFile, path.join(rewardDir(spec), 'reward.txt'));
      return { exitCode: 0 };
    },
  });
  const r = await gradeOnce({ containers, task, workRoot: tmp(t) });
  assert.equal(r.pass, false);
  assert.notEqual(r.reward, '1');
});
