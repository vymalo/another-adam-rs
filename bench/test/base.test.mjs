import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { BASE_MARKER, BaseCache, baseKey, baseScript, makeContainerBase, makeHostBase, packedRef, parseBaseMarker } from '../lib/base.mjs';
import { repoSlug } from '../lib/gitops.mjs';
import { extractEntries, parseTar } from '../lib/tarstream.mjs';
import { FakeContainers } from '../lib/containers.mjs';
import { adapterSymlinks, collectPatch, localGit } from '../lib/grade.mjs';
import { WorktreeError, crossCheckBranch, locateWorktree, saveRaw } from '../lib/agents/common.mjs';
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
  return { out, bare: path.join(out, 'base.git'), tree: path.join(out, 'tree'), sha: git(path.join(out, 'base.git'), 'rev-parse', 'refs/heads/main').trim() };
}

test('the base script makes a bare repository on main, with one commit that links the ignored top-level entries, and a tree without .git', (t) => {
  const app = appRepo(t);
  const original = git(app, 'rev-parse', 'HEAD').trim();
  const { bare, tree, sha } = runBase(t, app);
  assert.match(sha, /^[0-9a-f]{40}$/);
  assert.equal(git(bare, 'rev-parse', `${sha}^`).trim(), original, 'the history up to the base is kept, one commit on top');
  const links = git(bare, 'ls-tree', '-r', sha).split('\n').filter((l) => l.startsWith('120000'));
  assert.deepEqual(links.map((l) => l.split('\t')[1]).sort(), ['dist', 'node_modules']);
  assert.equal(git(bare, 'cat-file', 'blob', `${sha}:node_modules`), path.join(app, 'node_modules'));
  assert.equal(fs.readFileSync(path.join(tree, 'src', 'index.js'), 'utf8'), 'export {};\n');
  assert.equal(fs.existsSync(path.join(tree, '.git')), false);
  assert.equal(fs.readlinkSync(path.join(tree, 'node_modules')), path.join(app, 'node_modules'));
});

test('the base script refuses a path that is not plain', () => {
  assert.throws(() => baseScript({ workdir: '/app; rm -rf /' }), /plain/);
  assert.throws(() => baseScript({ workdir: '/app', out: '/out $(x)' }), /plain/);
});

/** What the real stack does for the base: the container's tree comes out as a tar stream into a directory the host owns. */
function fakeStack(produced, calls) {
  return {
    workDir: async (slot, dir, o) => calls.push(['workDir', slot, dir, Boolean(o?.remove)]),
    extractTree: async (slot, dir, dest) => {
      calls.push(['extractTree', slot, dir]);
      const tar = execFileSync('tar', ['--format=posix', '-cf', '-', '-C', produced.tree, '.'], { maxBuffer: 1 << 28 });
      extractEntries(parseTar(tar, { allowSymlinks: true }), dest);
    },
  };
}

test('the base is made in the slot\'s volume in the task image with no network, then rebuilt on the host from a tar stream by a repository the harness owns', async (t) => {
  const hostRoot = tmp(t);
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  const app = appRepo(t);
  const produced = runBase(t, app);
  const calls = [];
  const containers = new FakeContainers();
  const stack = fakeStack(produced, calls);
  await makeContainerBase({ containers, stack, slot: 2, task });
  const base = await makeHostBase({ stack, slot: 2, task, hostRoot });
  const spec = containers.runs[0];
  assert.deepEqual([spec.image, spec.network, spec.workdir], [`${IMAGE}@${DIGEST}`, 'none', '/']);
  assert.deepEqual(spec.mounts, [{ source: '/work/bench-base', target: '/out' }]);
  assert.match(spec.script, /git clone -q --no-hardlinks \/app /);
  // the marker is the content a clone would read and the tree the host rebuilds, not a name
  assert.match(spec.script, /base-marker/);
  assert.match(spec.script, /rev-parse refs\/heads\/main/);
  assert.deepEqual(calls.map((c) => c.slice(0, 4).join(' ')), ['workDir 2 /work/bench-base true', 'workDir 2 /work/bench-base false', 'extractTree 2 /work/bench-base/tree']);
  assert.equal(base.containerRepo, '/work/bench-base/base.git');
  assert.notEqual(base.dir, base.containerRepo);
  assert.equal(git(base.dir, 'rev-parse', 'HEAD').trim(), base.sha);
  assert.equal(git(base.dir, 'rev-parse', 'HEAD^{tree}').trim(), base.tree);
  assert.equal(base.tree, git(produced.bare, 'rev-parse', 'HEAD^{tree}').trim(), 'the host tree is the container\'s');
  assert.equal(base.slug, repoSlug('demo-1'));
  // the host's repository is the base for patch collection: untouched is empty, a new file is the patch, the links are left out
  fs.cpSync(base.dir, path.join(hostRoot, 'work'), { recursive: true });
  assert.equal((await collectPatch({ tree: path.join(hostRoot, 'work'), baseRepo: base.dir, base: base.sha, linkPrefix: app })).length, 0);
  fs.writeFileSync(path.join(hostRoot, 'work', 'new.txt'), 'x\n');
  assert.match((await collectPatch({ tree: path.join(hostRoot, 'work'), baseRepo: base.dir, base: base.sha, linkPrefix: app })).toString(), /\+\+\+ b\/new\.txt/);
  assert.deepEqual((await adapterSymlinks(localGit(base.dir), base.sha, app)).sort(), ['dist', 'node_modules']);
});

test('makeContainerBase says why it failed', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-2'));
  const containers = new FakeContainers({ onRun: () => ({ exitCode: 3, stderr: 'fatal: not a git repository\n' }) });
  await assert.rejects(makeContainerBase({ containers, stack: fakeStack({}, []), slot: 1, task }), /could not be made \(exit 3\).*not a git repository/);
});

/**
 * A stack whose slot volumes remember what a base build wrote (the fake containers only record the script): a build in
 * progress is materialised by the next look, with the marker and the ref a real build would write.
 */
function cacheStack(produced, calls, volumes, content, extract = () => produced.tree) {
  const vol = (slot) => {
    if (!volumes.has(slot)) volumes.set(slot, { marker: null, ref: null, packed: null, names: new Set(), pending: false });
    return volumes.get(slot);
  };
  const materialize = (v) => {
    if (!v.pending) return;
    v.pending = false;
    v.names = new Set(['base.git', 'tree', BASE_MARKER]);
    v.marker = `${content.commit} ${content.tree}\n`;
    v.ref = `${content.commit}\n`;
    v.packed = null;
  };
  return {
    workDir: async (slot, dir, o) => {
      calls.push(['workDir', slot, dir, Boolean(o?.remove)]);
      if (o?.remove) {
        const v = vol(slot);
        v.marker = null;
        v.ref = null;
        v.names.clear();
        v.pending = true;
      }
    },
    listDir: async (slot) => {
      const v = vol(slot);
      materialize(v);
      return [...v.names];
    },
    readFiles: async (slot) => {
      const v = vol(slot);
      materialize(v);
      const files = new Map();
      if (v.marker) files.set(BASE_MARKER, Buffer.from(v.marker));
      if (v.ref) files.set('base.git/refs/heads/main', Buffer.from(v.ref));
      if (v.packed) files.set('base.git/packed-refs', Buffer.from(v.packed));
      return { files, refused: [] };
    },
    extractTree: async (slot, dir, dest) => {
      const tree = extract();
      calls.push(['extractTree', slot, dir, tree]);
      const tar = execFileSync('tar', ['--format=posix', '-cf', '-', '-C', tree, '.'], { maxBuffer: 1 << 28 });
      extractEntries(parseTar(tar, { allowSymlinks: true }), dest);
    },
  };
}

test('the cache makes a task base once per (task, image, workdir): the slot copy and the host repository are reused', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-3'));
  const produced = runBase(t, appRepo(t));
  const content = { commit: produced.sha, tree: git(produced.bare, 'rev-parse', 'HEAD^{tree}').trim() };
  const calls = [];
  const volumes = new Map();
  const containers = new FakeContainers();
  const stack = cacheStack(produced, calls, volumes, content);
  const cache = new BaseCache();
  const hostRoot = path.join(tmp(t), 'bases', baseKey(task));
  const builds = () => containers.runs.length;
  const extracts = () => calls.filter((c) => c[0] === 'extractTree').length;

  const first = await cache.prepare({ containers, stack, slot: 1, task, hostRoot });
  assert.equal(builds(), 1);
  assert.equal(extracts(), 1);
  assert.ok(first.dir.startsWith(hostRoot), 'the host repository lives under the key, not under a run');
  assert.equal(git(first.dir, 'rev-parse', 'HEAD').trim(), first.sha);
  assert.equal(first.tree, content.tree, 'the host tree is the container\'s');
  const again = await cache.prepare({ containers, stack, slot: 1, task, hostRoot });
  assert.equal(builds(), 1, 'the slot copy is reused while its marker records the same content');
  assert.equal(extracts(), 1);
  assert.deepEqual([again.dir, again.sha], [first.dir, first.sha]);
  // Another slot has no copy: its own container side is made, the host's repository is not made again.
  const other = await cache.prepare({ containers, stack, slot: 2, task, hostRoot });
  assert.equal(builds(), 2);
  assert.equal(extracts(), 1);
  assert.deepEqual([other.dir, other.sha], [first.dir, first.sha]);
  // A branch changed in the slot (a commit added to the bare repository) is not trusted: the base is remade from the image.
  const edited = volumes.get(2);
  edited.ref = `${'f'.repeat(40)}\n`;
  await cache.prepare({ containers, stack, slot: 2, task, hostRoot });
  assert.equal(builds(), 3);
  assert.equal(extracts(), 1);
  // Two tasks that differ only in the working directory are two bases, with two host repositories.
  const elsewhere = { ...task, workdir: '/src' };
  assert.notEqual(baseKey(elsewhere), baseKey(task));
  const separate = await cache.prepare({ containers, stack, slot: 3, task: elsewhere, hostRoot: path.join(tmp(t), 'bases', baseKey(elsewhere)) });
  assert.notEqual(separate.dir, first.dir);
  assert.equal(builds(), 4);
  assert.equal(extracts(), 2);
  // A tree edited after the host copy was made is never read again in this process: the copy is the base.
  fs.writeFileSync(path.join(produced.tree, 'src', 'index.js'), 'tampered\n');
  const warm = await cache.prepare({ containers, stack, slot: 1, task, hostRoot });
  assert.equal(builds(), 4);
  assert.equal(extracts(), 2);
  assert.deepEqual([warm.sha, warm.tree], [first.sha, first.tree]);
  // A volume with no base at all is remade too.
  volumes.delete(1);
  await cache.prepare({ containers, stack, slot: 1, task, hostRoot });
  assert.equal(builds(), 5);
  assert.equal(extracts(), 2);
});

test('the cache reads a packed ref, so a base packed by git gc is not rebuilt every run', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-4'));
  const produced = runBase(t, appRepo(t));
  const content = { commit: produced.sha, tree: git(produced.bare, 'rev-parse', 'HEAD^{tree}').trim() };
  const calls = [];
  const volumes = new Map();
  const containers = new FakeContainers();
  const stack = cacheStack(produced, calls, volumes, content);
  const cache = new BaseCache();
  const hostRoot = path.join(tmp(t), 'bases', baseKey(task));
  const first = await cache.prepare({ containers, stack, slot: 1, task, hostRoot });
  // `git pack-refs`/`git gc`: the loose ref is gone, the packed one has it.
  const v = volumes.get(1);
  v.ref = null;
  v.packed = `# pack-refs with: peeled fully-peeled sorted\n${content.commit} refs/heads/main\n`;
  const again = await cache.prepare({ containers, stack, slot: 1, task, hostRoot });
  assert.equal(containers.runs.length, 1, 'a packed ref is read, not rebuilt');
  assert.equal(calls.filter((c) => c[0] === 'extractTree').length, 1);
  assert.deepEqual([again.sha, again.tree], [first.sha, first.tree]);
  assert.equal(packedRef(v.packed, 'refs/heads/main'), content.commit);
  assert.equal(packedRef(v.packed, 'refs/tags/other'), '');
});

/** A repository with a different tree from `appRepo`'s: the marker's tree is not what the slot's tree holds. */
function otherRepo(t) {
  const app = appRepo(t);
  commitFiles(app, { 'src/index.js': 'export const other = 1;\n' }, 'another tree');
  return app;
}

test('a slot whose tree is not its marker is rebuilt from the image, and the host copy with it', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-5'));
  const slotTree = runBase(t, appRepo(t)); // what the slot's tree holds (edited before the host copy)
  const imageTree = runBase(t, otherRepo(t)); // what the marker records, and what a rebuild makes
  const content = { commit: imageTree.sha, tree: git(imageTree.bare, 'rev-parse', 'HEAD^{tree}').trim() };
  assert.notEqual(content.tree, git(slotTree.bare, 'rev-parse', 'HEAD^{tree}').trim());
  const volumes = new Map([[1, { marker: `${content.commit} ${content.tree}\n`, ref: `${content.commit}\n`, packed: null, names: new Set(['base.git', 'tree', BASE_MARKER]), pending: false }]]);
  const calls = [];
  const containers = new FakeContainers();
  let extracted = 0;
  const stack = cacheStack(slotTree, calls, volumes, content, () => (extracted++ === 0 ? slotTree.tree : imageTree.tree));
  const cache = new BaseCache();
  const hostRoot = path.join(tmp(t), 'bases', baseKey(task));
  const base = await cache.prepare({ containers, stack, slot: 1, task, hostRoot });
  assert.equal(containers.runs.length, 1, 'the slot copy is made again from the image');
  assert.deepEqual(calls.filter((c) => c[0] === 'extractTree').map((c) => c[3]), [slotTree.tree, imageTree.tree], 'the host copy is made again, from the rebuilt tree');
  assert.equal(base.tree, content.tree, 'the returned base is the one the marker records');
  // The rebuilt base is the cached one now.
  await cache.prepare({ containers, stack, slot: 1, task, hostRoot });
  assert.equal(containers.runs.length, 1);
  assert.equal(calls.filter((c) => c[0] === 'extractTree').length, 2);
});

test('a slot whose tree never matches its marker is refused, not silently used', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'demo-6'));
  const slotTree = runBase(t, appRepo(t));
  const imageTree = runBase(t, otherRepo(t));
  const content = { commit: imageTree.sha, tree: git(imageTree.bare, 'rev-parse', 'HEAD^{tree}').trim() };
  const volumes = new Map([[1, { marker: `${content.commit} ${content.tree}\n`, ref: `${content.commit}\n`, packed: null, names: new Set(['base.git', 'tree', BASE_MARKER]), pending: false }]]);
  const calls = [];
  const containers = new FakeContainers();
  const stack = cacheStack(slotTree, calls, volumes, content, () => slotTree.tree);
  const cache = new BaseCache();
  await assert.rejects(
    cache.prepare({ containers, stack, slot: 1, task, hostRoot: path.join(tmp(t), 'bases', baseKey(task)) }),
    /does not match its marker/,
  );
  assert.equal(containers.runs.length, 1);
  assert.equal(calls.filter((c) => c[0] === 'extractTree').length, 2);
});

test('the key of a base names the task, its image and its working directory, and refuses a task without them', () => {
  const a = { id: 'demo-1', image: { ref: 'ghcr.io/x/y@sha256:aa' }, workdir: '/app' };
  assert.equal(baseKey(a), baseKey({ ...a }));
  assert.notEqual(baseKey(a), baseKey({ ...a, id: 'demo-2' }));
  assert.notEqual(baseKey(a), baseKey({ ...a, image: { ref: 'ghcr.io/x/y@sha256:bb' } }));
  assert.notEqual(baseKey(a), baseKey({ ...a, workdir: '/src' }));
  assert.match(baseKey(a), /^demo-1-[0-9a-f]{16}$/);
  assert.throws(() => baseKey({ id: '', image: a.image, workdir: '/app' }), /task id, an image reference and a working directory/);
  assert.throws(() => baseKey({ id: 'demo-1', workdir: '/app' }), /task id, an image reference and a working directory/);
  assert.throws(() => baseKey({ id: 'demo-1', image: a.image }), /task id, an image reference and a working directory/);
});

test('a marker is a bare repository\'s HEAD and a tree digest, or nothing', () => {
  const commit = 'a'.repeat(40);
  const tree = 'b'.repeat(40);
  assert.deepEqual(parseBaseMarker(`${commit} ${tree}\n`), { commit, tree });
  assert.deepEqual(parseBaseMarker(`${commit} ${tree}`), { commit, tree });
  assert.equal(parseBaseMarker('demo-1-0123456789abcdef'), null);
  assert.equal(parseBaseMarker(`${commit} not-a-tree`), null);
  assert.equal(parseBaseMarker(''), null);
  assert.equal(parseBaseMarker(undefined), null);
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
