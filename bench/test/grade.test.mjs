import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { ContainerError, FakeContainers, PodmanRemote, buildRunArgs } from '../lib/containers.mjs';
import { CLONE_PATH, checkTask, pinTask } from '../lib/envcheck.mjs';
import { MAX_PATCH_BYTES, PatchTooLarge, collectPatch, gradeOnce, gradePatch, gradeScript, localGit, verifierNetwork } from '../lib/grade.mjs';
import { loadTask } from '../lib/task.mjs';
import { commitFiles, git, initRepo } from './gitfix.mjs';
import { tmp } from './helpers.mjs';
import { DIGEST, IMAGE, makeTask } from './taskfix.mjs';

// ---- patch collection ----

function finalTree(t) {
  const dir = initRepo(t);
  const base = commitFiles(dir, { 'a.txt': 'one\ntwo\n', 'src/b.txt': 'b\n', '.gitignore': 'ignored/\n' }, 'base');
  fs.symlinkSync('/app/node_modules', path.join(dir, 'node_modules'));
  fs.symlinkSync('/etc/hosts', path.join(dir, 'hosts-link'));
  git(dir, 'add', '-A');
  git(dir, 'commit', '-q', '-m', 'adapter links');
  return { dir, base: git(dir, 'rev-parse', 'HEAD').trim(), first: base };
}

test('the patch has edits, untracked files and binaries, and leaves out ignored files and the adapter symlinks', async (t) => {
  const { dir, base } = finalTree(t);
  fs.writeFileSync(path.join(dir, 'a.txt'), 'one\nTWO\n');
  fs.writeFileSync(path.join(dir, 'untracked.txt'), 'new file\n');
  fs.writeFileSync(path.join(dir, 'blob.bin'), Buffer.from([0, 1, 2, 255, 254, 0, 0, 7]));
  fs.mkdirSync(path.join(dir, 'ignored'));
  fs.writeFileSync(path.join(dir, 'ignored', 'x'), 'x');
  fs.rmSync(path.join(dir, 'src', 'b.txt'));
  fs.rmSync(path.join(dir, 'node_modules'));
  fs.symlinkSync('/app/elsewhere', path.join(dir, 'node_modules'));
  fs.rmSync(path.join(dir, 'hosts-link'));
  fs.symlinkSync('/etc/passwd', path.join(dir, 'hosts-link'));

  const patch = (await collectPatch({ git: localGit(dir), base })).toString('utf8');
  assert.match(patch, /\+\+\+ b\/untracked\.txt/);
  assert.match(patch, /\+TWO/);
  assert.match(patch, /GIT binary patch/);
  assert.match(patch, /deleted file mode[^\n]*\n[^\n]*\n--- a\/src\/b\.txt/);
  assert.doesNotMatch(patch, /ignored/);
  assert.doesNotMatch(patch, /node_modules/, 'the adapter symlink is left out even when the agent changed it');
  assert.match(patch, /hosts-link/, 'a symlink to somewhere else is part of the work');

  // the patch applies to a clean copy of the base and gives the same files
  const clean = initRepo(t);
  commitFiles(clean, { 'a.txt': 'one\ntwo\n', 'src/b.txt': 'b\n', '.gitignore': 'ignored/\n' }, 'base');
  fs.symlinkSync('/etc/hosts', path.join(clean, 'hosts-link'));
  git(clean, 'add', '-A');
  git(clean, 'commit', '-q', '-m', 'links');
  const f = path.join(tmp(t), 'p.patch');
  fs.writeFileSync(f, patch);
  git(clean, 'apply', '--whitespace=nowarn', f);
  assert.equal(fs.readFileSync(path.join(clean, 'a.txt'), 'utf8'), 'one\nTWO\n');
  assert.equal(fs.readFileSync(path.join(clean, 'untracked.txt'), 'utf8'), 'new file\n');
  assert.deepEqual([...fs.readFileSync(path.join(clean, 'blob.bin'))], [0, 1, 2, 255, 254, 0, 0, 7]);
  assert.equal(fs.existsSync(path.join(clean, 'src', 'b.txt')), false);
});

test('an untouched tree gives an empty patch', async (t) => {
  const { dir, base } = finalTree(t);
  assert.equal((await collectPatch({ git: localGit(dir), base })).length, 0);
});

test('a patch above 5 MB stops with PatchTooLarge, and the limit is a parameter', async (t) => {
  const { dir, base } = finalTree(t);
  fs.writeFileSync(path.join(dir, 'big.txt'), 'x'.repeat(MAX_PATCH_BYTES + 1000));
  await assert.rejects(collectPatch({ git: localGit(dir), base }), PatchTooLarge);
  fs.rmSync(path.join(dir, 'big.txt'));
  fs.writeFileSync(path.join(dir, 'small.txt'), 'y'.repeat(2000));
  await assert.rejects(collectPatch({ git: localGit(dir), base, maxBytes: 1000 }), PatchTooLarge);
  assert.ok((await collectPatch({ git: localGit(dir), base })).length > 2000);
});

test('the base must be a full commit id', async (t) => {
  const { dir } = finalTree(t);
  await assert.rejects(collectPatch({ git: localGit(dir), base: 'HEAD' }), /full commit id/);
});

// ---- the podman --remote arguments ----

const SPEC = { image: `${IMAGE}@${DIGEST}`, script: 'echo hi', network: 'none', workdir: '/', mounts: [], name: 'job-1' };

test('the arguments: remote, no network for the agent phase, the image and the script last', () => {
  const a = buildRunArgs({ ...SPEC, cpus: 2, memoryMb: 4096, env: { A: 'b' } }, { connection: 'svc' });
  assert.deepEqual(a.slice(0, 4), ['--remote', '--connection', 'svc', 'run']);
  assert.ok(a.includes('--network=none'));
  assert.ok(a.includes('--rm') && a.includes('--pull=never'));
  assert.deepEqual(a.slice(a.indexOf('--cpus'), a.indexOf('--cpus') + 2), ['--cpus', '2']);
  assert.deepEqual(a.slice(a.indexOf('--memory'), a.indexOf('--memory') + 2), ['--memory', '4096m']);
  assert.ok(a.includes('A=b'));
  assert.deepEqual(a.slice(-5), ['--entrypoint', 'sh', SPEC.image, '-c', 'echo hi']);
  assert.ok(buildRunArgs(SPEC, { url: 'unix:///run/podman.sock' }).join(' ').startsWith('--remote --url unix:///run/podman.sock run'));
});

test('the verifier has the network its task names; none stays none; a name is passed on', () => {
  assert.equal(verifierNetwork({ verifierNetwork: 'public' }), 'default');
  assert.equal(verifierNetwork({ verifierNetwork: 'no-network' }), 'none');
  assert.equal(verifierNetwork({ verifierNetwork: 'bench-verifier' }), 'bench-verifier');
  assert.ok(!buildRunArgs({ ...SPEC, network: 'default' }).some((x) => x.startsWith('--network')));
  assert.ok(buildRunArgs({ ...SPEC, network: 'bench-verifier' }).includes('--network=bench-verifier'));
});

test('a mount is read-only when asked, and an unsafe path or name is refused', () => {
  const a = buildRunArgs({ ...SPEC, mounts: [{ source: '/srv/t/tests', target: '/tests', readOnly: true }, { source: '/srv/t/logs', target: '/logs/verifier' }] });
  assert.ok(a.includes('/srv/t/tests:/tests:ro'));
  assert.ok(a.includes('/srv/t/logs:/logs/verifier'));
  for (const bad of ['/a:b', 'rel', '/a b', '/a,b']) assert.throws(() => buildRunArgs({ ...SPEC, mounts: [{ source: bad, target: '/x' }] }), ContainerError, bad);
  assert.throws(() => buildRunArgs({ ...SPEC, name: 'a b' }), ContainerError);
  assert.throws(() => buildRunArgs({ ...SPEC, network: undefined }), /names its network/);
  assert.throws(() => buildRunArgs({ ...SPEC, image: 'a b' }), ContainerError);
  assert.throws(() => buildRunArgs({ ...SPEC, env: { 'A B': '1' } }), ContainerError);
});

test('PodmanRemote runs the binary with those arguments, and removes a container that timed out', async () => {
  const calls = [];
  const exec = (bin, args, opts, cb) => {
    calls.push({ bin, args, opts });
    if (args.includes('run')) cb(Object.assign(new Error('timeout'), { killed: true, code: null }), '', '');
    else cb(null, '', '');
    return { stdin: { end() {} } };
  };
  const p = new PodmanRemote({ bin: 'podman', connection: 'svc', exec });
  const r = await p.run({ ...SPEC, timeoutMs: 5 });
  assert.equal(r.timedOut, true);
  assert.equal(calls[0].bin, 'podman');
  assert.equal(calls[0].opts.timeout, 5);
  assert.deepEqual(calls[1].args, ['--remote', '--connection', 'svc', 'rm', '-f', 'job-1']);
});

test('PodmanRemote reads digests and ids, and refuses what is not one', async () => {
  const answers = { digest: `sha256:${'1'.repeat(64)}`, id: '2'.repeat(64) };
  const exec = (bin, args, opts, cb) => {
    cb(null, args.includes('{{.Digest}}') ? `${answers.digest}\n` : `${answers.id}\n`, '');
    return {};
  };
  const p = new PodmanRemote({ exec });
  assert.equal(await p.digestOf('x/y:1'), answers.digest);
  assert.equal(await p.imageId('x/y:1'), `sha256:${answers.id}`);
  answers.digest = 'nonsense';
  await assert.rejects(p.digestOf('x/y:1'), ContainerError);
});

// ---- grading ----

const rewards = (seq) => (spec, i) => {
  const logs = spec.mounts?.find((m) => m.target === '/logs/verifier');
  const v = seq[i % seq.length];
  if (logs && v !== null && typeof v === 'string') fs.writeFileSync(path.join(logs.source, 'reward.txt'), `${v}\n`);
  return typeof v === 'object' && v !== null ? v : { exitCode: 0 };
};

function loaded(t, over) {
  return loadTask(makeTask(tmp(t), 'grade-task', over));
}

test('three grades, two of three pass; one of three fails; unanimous is not flaky, a split is', async (t) => {
  const task = loaded(t);
  const run = async (seq) => gradePatch({ containers: new FakeContainers({ onRun: rewards(seq) }), task, patchFile: '/tmp/p.patch', workRoot: tmp(t) });
  const a = await run(['1', '1', '1']);
  assert.deepEqual([a.pass, a.passes, a.flaky], [true, 3, false]);
  const b = await run(['1', '0', '1']);
  assert.deepEqual([b.pass, b.passes, b.flaky], [true, 2, true]);
  const c = await run(['0', '1', '0']);
  assert.deepEqual([c.pass, c.passes, c.flaky], [false, 1, true]);
  const d = await run(['0', '0', '0']);
  assert.deepEqual([d.pass, d.passes, d.flaky], [false, 0, false]);
  assert.equal(d.attempts.length, 3);
});

test('a grading container: /tests read-only, patch read-only, logs writable, the verifier network, the task image by digest', async (t) => {
  const task = loaded(t);
  const fake = new FakeContainers({ onRun: rewards(['1']) });
  await gradeOnce({ containers: fake, task, patchFile: '/tmp/p.patch', workRoot: tmp(t) });
  const spec = fake.runs[0];
  assert.equal(spec.image, `${IMAGE}@${DIGEST}`);
  assert.equal(spec.network, 'default');
  const by = Object.fromEntries(spec.mounts.map((m) => [m.target, m]));
  assert.equal(by['/tests'].readOnly, true);
  assert.equal(by['/tests'].source, path.join(task.dir, 'tests'));
  assert.equal(by['/patch/model.patch'].readOnly, true);
  assert.equal(by['/logs/verifier'].readOnly, undefined);
  assert.match(spec.script, /git apply/);
  assert.match(spec.script, /bash \/tests\/test\.sh/);
});

test('no patch: nothing mounted for it; a missing reward, a bad one, a patch that does not apply and infra errors are told apart', async (t) => {
  const task = loaded(t);
  const one = async (onRun) => gradeOnce({ containers: new FakeContainers({ onRun }), task, workRoot: tmp(t) });
  const fake = new FakeContainers({ onRun: rewards(['0']) });
  await gradeOnce({ containers: fake, task, workRoot: tmp(t) });
  assert.equal(fake.runs[0].mounts.some((m) => m.target === '/patch/model.patch'), false);
  assert.equal((await one(() => ({ exitCode: 0 }))).error, 'no-reward');
  assert.equal((await one(rewards(['maybe']))).error, 'bad-reward');
  assert.equal((await one(() => ({ exitCode: 97 }))).error, 'apply');
  assert.equal((await one(() => ({ exitCode: 125 }))).error, 'infra');
  assert.equal((await one(() => ({ exitCode: 0, timedOut: true }))).error, 'timeout');
  const ok = await one(rewards(['1']));
  assert.deepEqual([ok.pass, ok.error], [true, null]);
});

test('the scratch directories are removed', async (t) => {
  const root = tmp(t);
  await gradeOnce({ containers: new FakeContainers({ onRun: rewards(['1']) }), task: loaded(t), workRoot: root });
  assert.deepEqual(fs.readdirSync(root), []);
});

test('the grading script applies the patch in the working directory; the check clones to the agents\' path', () => {
  const pristine = gradeScript({ workdir: '/app' });
  assert.match(pristine, /cd \/app/);
  assert.match(pristine, /\[ -s \/patch\/model\.patch \]/);
  assert.doesNotMatch(pristine, /git clone/);
  const clone = gradeScript({ workdir: '/app', cloneTo: CLONE_PATH });
  assert.match(clone, new RegExp(`git clone -q --no-hardlinks /app ${CLONE_PATH}`));
  assert.match(clone, new RegExp(`cd ${CLONE_PATH}`));
  assert.match(clone, /sed 's#\/app/);
  assert.match(clone, /ln -s \/app/);
});

test('the sed of the check rewrites the working directory and leaves other paths alone', (t) => {
  const dir = tmp(t);
  const script = gradeScript({ workdir: '/app', cloneTo: '/clone/x' });
  const sed = /(sed '[^\n]*') \/tests\/test\.sh/.exec(script)[1];
  const input = path.join(dir, 'in.sh');
  fs.writeFileSync(input, 'cd /app\ncd /app/sub\nls /application /tests/x\nA=/app\n');
  const out = execFileSync('sh', ['-c', `${sed} ${input}`], { encoding: 'utf8' });
  assert.equal(out, 'cd /clone/x\ncd /clone/x/sub\nls /application /tests/x\nA=/clone/x\n');
});

// ---- the environment check ----

/** A fake runtime whose answer depends on whether the gold or the empty patch is mounted. */
function oracle({ gold = ['1', '1', '1'], empty = ['0', '0', '0'], devcontainer = false, probeFails = false } = {}) {
  const counts = { gold: 0, empty: 0 };
  return new FakeContainers({
    onRun: (spec) => {
      if (!spec.mounts) return probeFails ? { exitCode: 125 } : { exitCode: 0, stdout: `devcontainer=${devcontainer ? 'yes' : 'no'}\n` };
      const withPatch = spec.mounts.some((m) => m.target === '/patch/model.patch');
      const seq = withPatch ? gold : empty;
      const v = seq[counts[withPatch ? 'gold' : 'empty']++ % seq.length];
      if (v === 'apply') return { exitCode: 97 };
      fs.writeFileSync(path.join(spec.mounts.find((m) => m.target === '/logs/verifier').source, 'reward.txt'), `${v}\n`);
      return { exitCode: 0 };
    },
  });
}

test('the check keeps a task whose gold passes three times and empty fails three times, and records the .devcontainer', async (t) => {
  const task = loaded(t);
  const c = oracle({ devcontainer: true });
  const r = await checkTask({ containers: c, task, workRoot: tmp(t) });
  assert.deepEqual([r.ok, r.reason, r.devcontainer, r.imageDigest], [true, null, true, DIGEST]);
  assert.equal(c.runs.length, 1 + 3 + 3);
  assert.ok(c.runs.slice(1).every((s) => /git clone/.test(s.script)), 'always from the agents\' clone path');
  assert.equal(c.runs[0].network, 'none');
});

test('the check drops a task for its reason', async (t) => {
  const reason = async (opts, over) => (await checkTask({ containers: oracle(opts), task: loaded(t, over), workRoot: tmp(t) })).reason;
  assert.equal(await reason({ gold: ['0', '0', '0'] }), 'gold-fails');
  assert.equal(await reason({ gold: ['1', '0', '1'] }), 'gold-flaky');
  assert.equal(await reason({ gold: ['apply'] }), 'gold-does-not-apply');
  assert.equal(await reason({ empty: ['1', '1', '1'] }), 'empty-passes');
  assert.equal(await reason({ empty: ['0', '1', '0'] }), 'empty-flaky');
  assert.equal(await reason({ probeFails: true }), 'infra');
  assert.equal(await reason({}, { 'solution/solution.patch': null, 'solution/notes.txt': 'x' }), 'no-gold-patch');
});

test('a tag-named image is pulled and pinned by its digest', async (t) => {
  const dir = makeTask(tmp(t), 'tagged-task', { 'environment/Dockerfile': `FROM ${IMAGE}:v1\n` });
  const task = loadTask(dir, { requireDigest: false });
  const fake = new FakeContainers({ digests: { [`${IMAGE}:v1`]: `sha256:${'c'.repeat(64)}` } });
  const { task: pinned, pin } = await pinTask(fake, task);
  assert.deepEqual(fake.pulled, [`${IMAGE}:v1`]);
  assert.equal(pinned.image.ref, `${IMAGE}@sha256:${'c'.repeat(64)}`);
  assert.deepEqual(pin, { ref: `${IMAGE}:v1`, digest: `sha256:${'c'.repeat(64)}` });
  assert.equal((await pinTask(fake, pinned)).pin, null);
});
