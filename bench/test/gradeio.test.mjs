// Defects 1 and 8 of the second round: what the host opens that a container could have made, and a grader that mounts
// only a directory of its own.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { FakeContainers } from '../lib/containers.mjs';
import { gradeOnce } from '../lib/grade.mjs';
import { Stack } from '../lib/stack.mjs';
import { loadTask } from '../lib/task.mjs';
import { tmp } from './helpers.mjs';
import { makeTask } from './taskfix.mjs';

const guard = (p, ms = 5000) => Promise.race([p, new Promise((_, reject) => setTimeout(() => reject(new Error(`hung for ${ms} ms`)), ms).unref())]);

test('defect 1: a FIFO where the reward should be does not hang the harness: it is a bad reward', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  const containers = new FakeContainers({
    onRun: (spec) => {
      execFileSync('mkfifo', [path.join(spec.mounts.find((m) => m.target === '/logs/verifier').source, 'reward.txt')]);
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
    const containers = new FakeContainers({ onRun: (spec) => (make(path.join(spec.mounts.find((m) => m.target === '/logs/verifier').source, 'reward.txt')), { exitCode: 0 }) });
    const r = await guard(gradeOnce({ containers, task, workRoot: tmp(t) }));
    assert.equal(r.pass, false);
    assert.equal(r.error, 'bad-reward');
  }
});

// ---- the grader mounts only a directory of its own: the tests and the patch go in, the reward comes out, as tar streams ----

function graderStack(t, { reward = '1\n', rewardKind = 'file' } = {}) {
  const home = tmp(t);
  const execs = [];
  const bufs = [];
  const stack = new Stack({
    composeFile: '/repo/bench/compose.yaml',
    home,
    tasksDir: '/repo/bench/tasks',
    seccomp: '/s',
    meterPort: 1,
    exec: async (file, args, opts) => {
      execs.push({ args: args.slice(5), input: opts?.input });
      return { exitCode: 0, stdout: '', stderr: '' };
    },
    execBuf: async (file, args) => {
      bufs.push(args.slice(5));
      const d = tmp(t);
      if (rewardKind === 'file') fs.writeFileSync(path.join(d, 'reward.txt'), reward);
      else if (rewardKind === 'fifo') execFileSync('mkfifo', [path.join(d, 'reward.txt')]);
      else if (rewardKind === 'link') fs.symlinkSync('/etc/hostname', path.join(d, 'reward.txt'));
      const names = rewardKind === 'none' ? [] : ['reward.txt'];
      const stdout = names.length === 0 ? Buffer.alloc(1024) : execFileSync('tar', ['--format=posix', '-cf', '-', '-C', d, ...names], { maxBuffer: 1 << 24 });
      return { exitCode: 0, stdout, stderr: '', over: false };
    },
  });
  return { stack, execs, bufs };
}

test('defect 8: grading stages the tests and the patch in a directory of the grader\'s own, reads the reward as a tar stream and removes the directory in the service', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  fs.mkdirSync(path.join(task.dir, 'tests'), { recursive: true });
  fs.writeFileSync(path.join(task.dir, 'tests', 'extra.txt'), 'hidden\n');
  const patch = path.join(tmp(t), 'model.patch');
  fs.writeFileSync(patch, 'diff --git a/x b/x\n');
  const { stack, execs, bufs } = graderStack(t);
  const containers = new FakeContainers();
  const r = await gradeOnce({ containers, io: stack.graderIO(), task, patchFile: patch });
  assert.equal(r.pass, true);
  const spec = containers.runs[0];
  const mounts = Object.fromEntries(spec.mounts.map((m) => [m.target, m]));
  assert.deepEqual(Object.keys(mounts).sort(), ['/logs/verifier', '/patch/model.patch', '/tests']);
  assert.match(mounts['/tests'].source, /^\/grade\/[a-f0-9]{16}\/tests$/);
  assert.equal(mounts['/tests'].readOnly, true);
  assert.equal(mounts['/patch/model.patch'].readOnly, true);
  assert.match(mounts['/logs/verifier'].source, /^\/grade\/[a-f0-9]{16}\/logs$/);
  assert.ok(!spec.mounts.some((m) => m.source.startsWith(task.dir) || m.source.startsWith(path.dirname(patch))), 'nothing of the host\'s is mounted');
  // what went in: a tar of the tests and of the patch, through the grader's exec
  const puts = execs.filter((e) => e.args.includes('-xf'));
  assert.equal(puts.length, 2);
  assert.ok(puts.every((e) => e.args.includes('grader') && Buffer.isBuffer(e.input)));
  assert.ok(puts[0].input.includes('hidden'));
  assert.ok(execs.some((e) => e.args.join(' ').includes('mkdir -p') && e.args.join(' ').includes('chmod 777') && e.args.includes('grader')));
  // what came out: the reward, by name, from the grader
  assert.deepEqual(bufs[0].slice(0, 4), ['exec', '-T', 'grader', 'tar']);
  assert.ok(bufs[0].includes('reward.txt'));
  // and what was left is removed by the grader, and checked
  const last = execs.at(-1).args.join(' ');
  assert.match(last, /rm -rf \/grade\/[a-f0-9]{16}; test ! -e \/grade\/[a-f0-9]{16}/);
});

test('defect 8: a FIFO, a link or a missing file where the grading container should have left the reward is not a pass', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  for (const [kind, error] of [['fifo', 'bad-reward'], ['link', 'bad-reward'], ['none', 'no-reward']]) {
    const { stack } = graderStack(t, { rewardKind: kind });
    const r = await guard(gradeOnce({ containers: new FakeContainers(), io: stack.graderIO(), task }));
    assert.deepEqual([r.pass, r.error], [false, error], kind);
  }
  const { stack } = graderStack(t, { reward: '1'.repeat(100) });
  assert.equal((await gradeOnce({ containers: new FakeContainers(), io: stack.graderIO(), task })).error, 'bad-reward');
});

test('defect 8: each grading has its own directory, and the host\'s logs directory is not made at all', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  const work = tmp(t);
  const { stack } = graderStack(t);
  const containers = new FakeContainers();
  await gradeOnce({ containers, io: stack.graderIO(), task, workRoot: work });
  await gradeOnce({ containers, io: stack.graderIO(), task, workRoot: work });
  const dirs = containers.runs.map((s) => s.mounts.find((m) => m.target === '/logs/verifier').source);
  assert.notEqual(dirs[0], dirs[1]);
  assert.deepEqual(fs.readdirSync(work), [], 'no grade-logs directory on the host');
});
