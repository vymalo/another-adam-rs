import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { baseScript, prepareBase } from '../lib/base.mjs';
import { FakeContainers } from '../lib/containers.mjs';
import { adapterSymlinks, collectPatch, localGit } from '../lib/grade.mjs';
import { WorktreeError, copyScript, crossCheckBranch, locateWorktree, saveRaw } from '../lib/agents/common.mjs';
import { commitFiles, git, initRepo } from './gitfix.mjs';
import { tmp } from './helpers.mjs';
import { DIGEST, IMAGE, makeTask } from './taskfix.mjs';
import { loadTask } from '../lib/task.mjs';

const sh = (script, env = {}) => execFileSync('sh', ['-c', script], { env: { PATH: process.env.PATH, HOME: tmp({ after() {} }), ...env }, stdio: 'pipe' });

/** A repository as a task image holds it: committed files, an ignored dependency directory with files in it. */
function appRepo(t) {
  const app = initRepo(t);
  commitFiles(app, { 'package.json': '{}\n', 'src/index.js': 'export {};\n', '.gitignore': 'node_modules/\ndist/\n' }, 'base commit');
  fs.mkdirSync(path.join(app, 'node_modules', 'dep'), { recursive: true });
  fs.writeFileSync(path.join(app, 'node_modules', 'dep', 'index.js'), 'dep\n');
  fs.mkdirSync(path.join(app, 'dist'));
  fs.writeFileSync(path.join(app, 'dist', 'out.js'), 'built\n');
  return app;
}

function runBase(t, app) {
  const out = tmp(t);
  const scratch = path.join(tmp(t), 'w');
  sh(baseScript({ workdir: app, out, scratch }), { GIT_CONFIG_NOSYSTEM: '1' });
  return { out, bare: path.join(out, 'base.git'), sha: fs.readFileSync(path.join(out, 'base.sha'), 'utf8').trim() };
}

test('the base script makes a bare repository, on main, with one commit that links the ignored top-level entries to the image\'s', (t) => {
  const app = appRepo(t);
  const original = git(app, 'rev-parse', 'HEAD').trim();
  const { bare, sha } = runBase(t, app);
  assert.match(sha, /^[0-9a-f]{40}$/);
  assert.equal(git(bare, 'rev-parse', 'refs/heads/main').trim(), sha);
  assert.equal(git(bare, 'rev-parse', `${sha}^`).trim(), original, 'the history up to the base is kept, one commit on top');
  const tree = git(bare, 'ls-tree', '-r', sha).split('\n').filter(Boolean);
  const links = tree.filter((l) => l.startsWith('120000'));
  assert.equal(links.length, 2);
  assert.ok(links.some((l) => l.endsWith('\tnode_modules')) && links.some((l) => l.endsWith('\tdist')));
  assert.equal(git(bare, 'cat-file', 'blob', `${sha}:node_modules`), path.join(app, 'node_modules'));
  assert.ok(!tree.some((l) => l.includes('dep/index.js')), 'the dependencies themselves are not copied into the repository');
});

test('the links are the base: the patch of an untouched clone is empty, and a change to a link is left out of it', async (t) => {
  const app = appRepo(t);
  const { bare, sha } = runBase(t, app);
  const clone = path.join(tmp(t), 'c');
  execFileSync('git', ['clone', '-q', bare, clone]);
  assert.equal((await collectPatch({ tree: clone, baseRepo: bare, base: sha, linkPrefix: app })).length, 0);
  fs.writeFileSync(path.join(clone, 'new.txt'), 'x\n');
  fs.rmSync(path.join(clone, 'dist'));
  fs.symlinkSync('/elsewhere', path.join(clone, 'dist'));
  const patch = (await collectPatch({ tree: clone, baseRepo: bare, base: sha, linkPrefix: app })).toString();
  assert.match(patch, /\+\+\+ b\/new\.txt/);
  assert.doesNotMatch(patch, /dist|node_modules/);
  assert.deepEqual((await adapterSymlinks(localGit(bare), sha, app)).sort(), ['dist', 'node_modules']);
});

test('a repository with nothing ignored has no extra commit, and the base is its own head', (t) => {
  const app = initRepo(t);
  const head = commitFiles(app, { 'a.txt': 'a\n' }, 'only commit');
  assert.equal(runBase(t, app).sha, head);
});

test('the base script refuses a path that is not plain', () => {
  assert.throws(() => baseScript({ workdir: '/app; rm -rf /' }), /plain/);
  assert.throws(() => baseScript({ workdir: '/app', out: '/out $(x)' }), /plain/);
});

test('prepareBase runs the script in the task image, with no network and the output mounted, and keeps the result by digest', async (t) => {
  const home = tmp(t);
  const tasks = tmp(t);
  const task = loadTask(makeTask(tasks, 'demo-1'));
  const app = appRepo(t);
  const produced = runBase(t, app);
  const containers = new FakeContainers({
    onRun: (spec) => {
      const out = spec.mounts.find((m) => m.target === '/out').source;
      fs.cpSync(produced.out, out, { recursive: true });
      return { exitCode: 0 };
    },
  });
  const base = await prepareBase({ containers, task, root: home });
  assert.equal(containers.runs.length, 1);
  const spec = containers.runs[0];
  assert.deepEqual([spec.image, spec.network, spec.workdir], [`${IMAGE}@${DIGEST}`, 'none', '/']);
  assert.match(spec.script, /git clone -q --no-hardlinks \/app /);
  assert.equal(base.sha, produced.sha);
  assert.ok(fs.existsSync(base.dir));
  const again = await prepareBase({ containers, task, root: home });
  assert.equal(containers.runs.length, 1, 'kept: the image is the same');
  assert.deepEqual(again, base);
});

test('prepareBase says why it failed, and keeps nothing', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-2'));
  const containers = new FakeContainers({ onRun: () => ({ exitCode: 3, stderr: 'fatal: not a git repository\n' }) });
  await assert.rejects(prepareBase({ containers, task, root: tmp(t) }), /could not be made \(exit 3\).*not a git repository/);
});

test('the copy script copies a tree without its top-level .git, keeps links as links and makes it readable', (t) => {
  const tree = tmp(t);
  fs.mkdirSync(path.join(tree, '.git'));
  fs.writeFileSync(path.join(tree, '.git', 'config'), '[core]\n\tfsmonitor = x\n');
  fs.mkdirSync(path.join(tree, 'src', 'deep'), { recursive: true });
  fs.writeFileSync(path.join(tree, 'src', 'deep', 'a.txt'), 'a\n', { mode: 0o600 });
  fs.symlinkSync('/app/node_modules', path.join(tree, 'node_modules'));
  const out = tmp(t);
  sh(copyScript({ tree, out }));
  assert.equal(fs.existsSync(path.join(out, '.git')), false);
  assert.equal(fs.readFileSync(path.join(out, 'src', 'deep', 'a.txt'), 'utf8'), 'a\n');
  assert.equal(fs.readlinkSync(path.join(out, 'node_modules')), '/app/node_modules');
  assert.ok((fs.statSync(path.join(out, 'src', 'deep', 'a.txt')).mode & 0o044) === 0o044);
  assert.throws(() => copyScript({ tree: '/tree; x' }), /plain/);
});

test('the run\'s worktree is the one slot of the one run directory, or the one named after the repository', () => {
  const slotsOf = (map) => (r) => map[r];
  assert.deepEqual(locateWorktree({ runs: ['run-1'], slotsOf: slotsOf({ 'run-1': ['demo-1a2b3c'] }), slug: 'demo-1a2b3c' }), { run: 'run-1', slot: 'demo-1a2b3c', path: '/work/workspaces/run-1/demo-1a2b3c' });
  assert.equal(locateWorktree({ runs: ['r'], slotsOf: slotsOf({ r: ['other', 'demo-1a2b3c'] }), slug: 'demo-1a2b3c' }).slot, 'demo-1a2b3c');
  assert.throws(() => locateWorktree({ runs: [], slotsOf: slotsOf({}), slug: 'x' }), (e) => e instanceof WorktreeError && e.code === 'no-worktree');
  assert.throws(() => locateWorktree({ runs: ['a', 'b'], slotsOf: slotsOf({}), slug: 'x' }), (e) => e.code === 'ambiguous');
  assert.throws(() => locateWorktree({ runs: ['r'], slotsOf: slotsOf({ r: ['p', 'q'] }), slug: 'x-1' }), (e) => e.code === 'ambiguous');
  assert.throws(() => locateWorktree({ runs: ['r'], slotsOf: slotsOf({ r: [] }), slug: 'x' }), (e) => e.code === 'no-worktree');
});

test('the pushed branch is the cross-check: it is the run\'s when its name starts the run directory\'s', () => {
  const heads = [{ ref: 'agent/0191a2b3', sha: 'a'.repeat(40) }];
  assert.deepEqual(crossCheckBranch({ heads, runDir: '0191a2b3-7c4d-4e5f' }), { branch: 'agent/0191a2b3', sha: 'a'.repeat(40), consistent: true, pushed: 1 });
  assert.equal(crossCheckBranch({ heads, runDir: 'ffff0000-1111' }).consistent, false);
  assert.equal(crossCheckBranch({ heads: [], runDir: 'x' }), null);
  assert.equal(crossCheckBranch({ heads: [{ ref: 'main', sha: 'b'.repeat(40) }], runDir: 'x' }), null);
});

test('raw output is kept with the run\'s secrets taken out', (t) => {
  const dir = tmp(t);
  saveRaw(dir, 'raw/events.jsonl', '{"url":"http://meter:8090/r/SECRETTOKEN0123456789/v1"}\n', ['SECRETTOKEN0123456789']);
  assert.ok(!fs.readFileSync(path.join(dir, 'raw', 'events.jsonl'), 'utf8').includes('SECRETTOKEN'));
});
