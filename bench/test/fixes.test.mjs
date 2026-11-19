// The fix round after the blind review of slice 2. Each test names the finding (row) it holds.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { ConfigError, normalizeConfig } from '../lib/config.mjs';
import { FakeContainers, buildRunArgs } from '../lib/containers.mjs';
import { checkTask } from '../lib/envcheck.mjs';
import * as G from '../lib/grade.mjs';

const { collectPatch, gradeOnce, gradePatch } = G;
const sanitizePatch = (...a) => G.sanitizePatch(...a);
import { cfgTestRanges, classifyPath, evaluateCommit, gitIn } from '../lib/mine.mjs';
import { ownTestScript } from '../lib/own.mjs';
import { selectTasks } from '../lib/select.mjs';
import { runTasks } from '../lib/tasks-cli.mjs';
import { loadTask } from '../lib/task.mjs';
import { buildReport } from '../lib/report.mjs';
import { UpstreamError, fetchUpstream, parseSums, verifySums } from '../lib/upstream.mjs';
import { commitFiles, git, initRepo, lines, SHA } from './gitfix.mjs';
import { rawConfig, tmp } from './helpers.mjs';
import { DIGEST, IMAGE, makeTask } from './taskfix.mjs';

const REWARD = (spec) => spec.mounts.find((m) => m.target === '/logs/verifier').source;
const loaded = (t, over) => loadTask(makeTask(tmp(t), 'fix-task', over));

// ---- row 1: the verdict does not come from the agent's code alone ----

test('row 1: a reward of 1 with a non-zero exit status is a fail, and suspect', async (t) => {
  const fake = new FakeContainers({ onRun: (spec) => { fs.writeFileSync(path.join(REWARD(spec), 'reward.txt'), '1\n'); return { exitCode: 137 }; } });
  const r = await gradeOnce({ containers: fake, task: loaded(t), workRoot: tmp(t) });
  assert.equal(r.pass, false);
  assert.ok(r.suspect.includes('reward-with-failed-exit'));
});

test('row 1: a reward file that is older than the attempt is not accepted', async (t) => {
  const fake = new FakeContainers({ onRun: (spec) => { const f = path.join(REWARD(spec), 'reward.txt'); fs.writeFileSync(f, '1\n'); fs.utimesSync(f, 1000, 1000); return { exitCode: 0 }; } });
  const r = await gradeOnce({ containers: fake, task: loaded(t), workRoot: tmp(t) });
  assert.equal(r.pass, false);
  assert.equal(r.error, 'stale-reward');
});

test('row 1: every attempt gets an empty directory of its own, read only after the container ended', async (t) => {
  const seen = [];
  const fake = new FakeContainers({ onRun: (spec) => { seen.push({ dir: REWARD(spec), files: fs.readdirSync(REWARD(spec)) }); fs.writeFileSync(path.join(REWARD(spec), 'reward.txt'), '1\n'); return { exitCode: 0 }; } });
  await gradePatch({ containers: fake, task: loaded(t), patchFile: null, workRoot: tmp(t) });
  assert.equal(new Set(seen.map((s) => s.dir)).size, 3);
  assert.ok(seen.every((s) => s.files.length === 0));
});

test('row 1: a patch that mentions the grading machinery is graded as it is and listed as suspect', async (t) => {
  const patch = path.join(tmp(t), 'p.patch');
  fs.writeFileSync(patch, 'diff --git a/build.rs b/build.rs\n--- a/build.rs\n+++ b/build.rs\n@@ -1 +1,2 @@\n x\n+std::fs::write("/logs/verifier/reward.txt", "1");\n');
  const fake = new FakeContainers({ onRun: (spec) => { fs.writeFileSync(path.join(REWARD(spec), 'reward.txt'), '1\n'); return { exitCode: 0 }; } });
  const r = await gradePatch({ containers: fake, task: loaded(t), patchFile: patch, workRoot: tmp(t) });
  assert.ok(r.suspect.includes('mentions-grading'));
  assert.equal(r.patch, patch);
});

// ---- row 2: nothing of the agent's .git is run on the host ----

function agentTree(t) {
  const dir = initRepo(t);
  commitFiles(dir, { 'a.txt': 'one\n', 'notes.txt': 'n\n' }, 'base');
  fs.symlinkSync('/app/node_modules', path.join(dir, 'node_modules'));
  git(dir, 'add', '-A');
  git(dir, 'commit', '-q', '-m', 'links');
  const bare = path.join(tmp(t), 'base.git');
  execFileSync('git', ['clone', '-q', '--bare', dir, bare]);
  return { dir, bare, base: git(dir, 'rev-parse', 'HEAD').trim() };
}

test('row 2: a filter, a textconv, an fsmonitor and a hook of the agent\'s .git run nothing', async (t) => {
  const { dir, bare, base } = agentTree(t);
  const marks = tmp(t);
  const bin = tmp(t);
  const trap = (n) => {
    const f = path.join(bin, `${n}.sh`);
    fs.writeFileSync(f, `#!/bin/sh\ntouch ${path.join(marks, n)}\ncat\n`, { mode: 0o755 });
    return f;
  };
  fs.appendFileSync(path.join(dir, '.git', 'config'), `[filter "x"]\n\tclean = ${trap('PWNED')}\n[diff "y"]\n\ttextconv = ${trap('TEXTCONV')}\n[core]\n\tfsmonitor = ${trap('FSMONITOR')}\n\thooksPath = ${bin}\n`);
  fs.copyFileSync(trap('HOOK'), path.join(bin, 'pre-commit'));
  fs.writeFileSync(path.join(dir, '.gitattributes'), `* filter=x\n*.txt diff=y\n`);
  fs.writeFileSync(path.join(dir, 'a.txt'), 'one\ntwo\n');
  const patch = (await collectPatch({ tree: dir, baseRepo: bare, base })).toString('utf8');
  assert.deepEqual(fs.readdirSync(marks), [], 'nothing was executed');
  assert.match(patch, /\+two/);
});

test('row 2: the patch is made from the files of the tree, symlinks are not followed, the agent\'s .git is not read', async (t) => {
  const { dir, bare, base } = agentTree(t);
  const outside = tmp(t);
  fs.writeFileSync(path.join(outside, 'secret'), 'secret outside the tree\n');
  fs.symlinkSync(outside, path.join(dir, 'escape'));
  fs.writeFileSync(path.join(dir, 'new.txt'), 'new\n');
  const patch = (await collectPatch({ tree: dir, baseRepo: bare, base })).toString('utf8');
  assert.doesNotMatch(patch, /secret outside the tree/);
  assert.match(patch, /\+\+\+ b\/new\.txt/);
  assert.match(patch, /new file mode 120000/, 'a symlink is a link in the patch');
});

// ---- row 3: the agent's patch may not change the tests or their machinery ----

const section = (p, body = '+x\n') => `diff --git a/${p} b/${p}\n--- a/${p}\n+++ b/${p}\n@@ -1 +1,2 @@\n y\n${body}`;

test('row 3: conftest.py, test paths, pytest.ini, .gitattributes and the hidden tests are stripped; sources stay', () => {
  const patch = [section('src/app.py'), section('tests/conftest.py'), section('conftest.py'), section('pytest.ini'), section('pkg/tests/test_x.py'), section('.gitattributes'), section('docs/hidden_spec.txt'), section('vitest.config.ts'), section('src/app_tests.rs')].join('');
  const r = sanitizePatch(patch, { hiddenTests: ['docs/hidden_spec.txt'] });
  assert.deepEqual(r.removed.sort(), ['.gitattributes', 'conftest.py', 'docs/hidden_spec.txt', 'pkg/tests/test_x.py', 'pytest.ini', 'src/app_tests.rs', 'tests/conftest.py', 'vitest.config.ts']);
  assert.match(r.patch, /src\/app\.py/);
  assert.doesNotMatch(r.patch, /conftest|pytest\.ini|gitattributes/);
});

test('row 3: a change that configures the tests is kept and flagged suspect', () => {
  const cargo = section('Cargo.toml', '+[[test]]\n+name = "x"\n+harness = false\n');
  const plain = section('Cargo.toml', '+serde = "1"\n');
  assert.ok(sanitizePatch(cargo).suspect.includes('test-configuration:Cargo.toml'));
  assert.match(sanitizePatch(cargo).patch, /harness = false/);
  assert.deepEqual(sanitizePatch(plain).suspect, []);
  assert.ok(sanitizePatch(section('package.json', '+  "test": "true",\n')).suspect.includes('test-configuration:package.json'));
  assert.ok(sanitizePatch(section('pyproject.toml', '+[tool.pytest.ini_options]\n')).suspect.includes('test-configuration:pyproject.toml'));
});

test('row 3: grading mounts the stripped patch and records what it removed', async (t) => {
  const patch = path.join(tmp(t), 'p.patch');
  fs.writeFileSync(patch, section('src/app.py') + section('tests/conftest.py', '+import os\n'));
  let mounted = null;
  const fake = new FakeContainers({ onRun: (spec) => { mounted = fs.readFileSync(spec.mounts.find((m) => m.target === '/patch/model.patch').source, 'utf8'); fs.writeFileSync(path.join(REWARD(spec), 'reward.txt'), '1\n'); return { exitCode: 0 }; } });
  const r = await gradePatch({ containers: fake, task: loaded(t), patchFile: patch, workRoot: tmp(t) });
  assert.doesNotMatch(mounted, /conftest/);
  assert.match(mounted, /src\/app\.py/);
  assert.deepEqual(r.removed, ['tests/conftest.py']);
});

// ---- row 4: the whole upstream tree is verified ----

const sha = (s) => execFileSync('sha256sum', { input: s, encoding: 'utf8' }).split(' ')[0];

test('row 4: a file the checksums do not list is refused, a symlink anywhere is refused, a path that is not normal is refused', (t) => {
  const dir = tmp(t);
  fs.mkdirSync(path.join(dir, 'tests'));
  fs.writeFileSync(path.join(dir, 'tests', 'test.sh'), 'ok');
  const sums = parseSums(`${sha('ok')}  tests/test.sh\n`);
  verifySums(dir, sums);
  fs.writeFileSync(path.join(dir, 'tests', 'conftest.py'), 'import evil');
  assert.throws(() => verifySums(dir, sums), /not listed/);
  fs.rmSync(path.join(dir, 'tests', 'conftest.py'));
  const elsewhere = tmp(t);
  fs.writeFileSync(path.join(elsewhere, 'test.sh'), 'ok');
  const dir2 = tmp(t);
  fs.symlinkSync(elsewhere, path.join(dir2, 'tests'));
  assert.throws(() => verifySums(dir2, sums), /symlink/);
  assert.throws(() => parseSums(`${sha('a')}  a//b\n`), UpstreamError);
  assert.throws(() => parseSums(`${sha('a')}  a/./b\n`), UpstreamError);
  assert.throws(() => parseSums(`${sha('a')}  a/b/../c\n`), UpstreamError);
});

test('row 4: a reused upstream directory is cleaned and its origin must be the configured one', (t) => {
  const work = initRepo(t);
  fs.mkdirSync(path.join(work, 'v2', 'one'), { recursive: true });
  fs.writeFileSync(path.join(work, 'v2', 'one', 'task.toml'), 'x');
  fs.writeFileSync(path.join(work, 'v2', 'SHA256SUMS'), `${sha('x')}  one/task.toml\n`);
  const commit = commitFiles(work, {}, 'tasks');
  const bare = path.join(tmp(t), 'u.git');
  execFileSync('git', ['clone', '-q', '--bare', work, bare]);
  const dir = path.join(tmp(t), 'clone');
  fetchUpstream({ url: bare, commit, dir });
  fs.writeFileSync(path.join(dir, 'v2', 'one', 'conftest.py'), 'import evil');
  fetchUpstream({ url: bare, commit, dir });
  assert.equal(fs.existsSync(path.join(dir, 'v2', 'one', 'conftest.py')), false, 'the leftover file is cleaned');
  assert.throws(() => fetchUpstream({ url: path.join(tmp(t), 'other.git'), commit, dir }), /origin/);
});

// ---- row 5: the miner ----

test('row 5: cfg attributes are read for the token test, in all, any and file-level form, and not under not', () => {
  assert.deepEqual(cfgTestRanges('#[cfg(all(test, unix))]\nmod t {\n}\n'), [[1, 3]]);
  assert.deepEqual(cfgTestRanges('#[cfg(any(test, feature = "x"))]\nmod t {\n}\n'), [[1, 3]]);
  assert.deepEqual(cfgTestRanges('#[cfg(not(test))]\nfn f() {}\n'), []);
  assert.deepEqual(cfgTestRanges('#[cfg(feature = "testing")]\nfn f() {}\n'), []);
  assert.deepEqual(cfgTestRanges('#![cfg(test)]\nfn a() {}\n'), [[1, 3]]);
  assert.deepEqual(cfgTestRanges('#[cfg(test)]\nmod t {\n    const S: &str = r#"a"b}"#;\n}\nfn after() {}\n'), [[1, 4]]);
  assert.deepEqual(cfgTestRanges('#[cfg(test)]\nmod t {\n    const S: &str = r##"x"#}"##;\n}\nfn after() {}\n'), [[1, 4]]);
});

test('row 5: more test paths and exact lockfile names', () => {
  for (const p of ['src/tests.rs', 'src/foo_tests.rs', 'src/testing/mod.rs', 'src/test_utils.rs', 'pkg/testutil.go', 'a/b_tests.py']) assert.equal(classifyPath(p), 'test', p);
  for (const p of ['deno.lock', 'a/pnpm-lock.yml', 'Package.resolved', 'mix.lock', 'x/custom.lock', 'bun.lockb']) assert.equal(classifyPath(p), 'lock', p);
  assert.equal(classifyPath('Cargo.lock.bak'), 'source', 'not a lockfile by name, and not one the install reads');
});

test('row 5: a binary source file is over the limit; ci, style, build and refactor subjects are refused', (t) => {
  const dir = initRepo(t);
  commitFiles(dir, { 'src/a.ts': lines(3) }, 'chore: start (#1)');
  const judge = (c) => evaluateCommit(gitIn(dir), c);
  const bin = commitFiles(dir, { 'src/blob.wasm': 'AAAA\0\0\0\u0001\u0002', 'src/a.test.ts': 'test(1)\n' }, 'feat: binary (#2)');
  fs.writeFileSync(path.join(dir, 'src', 'blob2.wasm'), Buffer.from([0, 1, 2, 3, 0, 0, 255]));
  const bin2 = commitFiles(dir, { 'src/blob2.wasm': Buffer.from([0, 1, 2, 3, 0, 0, 255]).toString('latin1'), 'src/a.test.ts': 'test(2)\n' }, 'feat: binary two (#3)');
  assert.ok(judge(bin).reasons.includes('too-large') || judge(bin2).reasons.includes('too-large'));
  let n = 10;
  for (const type of ['ci', 'style', 'build', 'refactor']) {
    const c = commitFiles(dir, { 'src/a.ts': lines(n++), 'src/a.test.ts': `test(${n})\n` }, `${type}: something (#${n})`);
    assert.ok(judge(c).reasons.includes(`type-${type}`), type);
  }
});

// ---- row 6: a check result belongs to the task it was made for ----

test('row 6: a task that changed after its check is not selected', async (t) => {
  const dir = tmp(t);
  const config = path.join(dir, 'config.json');
  const work = initRepo(t);
  const root = path.join(work, 'v2');
  makeTask(root, 'instance_Org__Alpha-aaaaaaa1', { 'environment/Dockerfile': `FROM ${IMAGE}:v1\n` });
  const sums = [];
  const walk = (d) => { for (const e of fs.readdirSync(d, { withFileTypes: true })) { const p = path.join(d, e.name); if (e.isDirectory()) walk(p); else sums.push(`${sha(fs.readFileSync(p))}  ${path.relative(root, p)}`); } };
  walk(root);
  fs.writeFileSync(path.join(root, 'SHA256SUMS'), `${sums.join('\n')}\n`);
  const commit = commitFiles(work, {}, 'tasks');
  const bare = path.join(tmp(t), 'u.git');
  execFileSync('git', ['clone', '-q', '--bare', work, bare]);
  fs.writeFileSync(config, JSON.stringify(rawConfig({ tasks: { upstreamUrl: bare, upstreamCommit: commit } })));
  const env = { BENCH_CONFIG: config, BENCH_HOME: path.join(dir, 'home'), BENCH_TASKS_DIR: path.join(dir, 'rt') };
  const out = [];
  const run = (argv, make) => runTasks(argv, { env, out: (l) => out.push(l), err: () => {}, make });
  const fake = new FakeContainers({ onRun: (spec) => { if (!spec.mounts) return { stdout: 'devcontainer=no\n' }; fs.writeFileSync(path.join(REWARD(spec), 'reward.txt'), spec.mounts.some((m) => m.target === '/patch/model.patch') ? '1\n' : '0\n'); return {}; } });
  await run(['check'], () => fake);
  await run(['select', '--source', 'public']);
  assert.match(out.join('\n'), /public: 1 of 1 selected/);
  // the checked image digest moves: the result no longer fits
  const file = path.join(env.BENCH_HOME, 'tasks', 'check.json');
  const results = JSON.parse(fs.readFileSync(file, 'utf8'));
  const key = Object.keys(results)[0];
  assert.ok(results[key].checksum, 'the result records the checksum');
  results[key].checksum = 'f'.repeat(64);
  fs.writeFileSync(file, JSON.stringify(results));
  out.length = 0;
  await run(['select', '--source', 'public']);
  assert.match(out.join('\n'), /public: 0 of 0 selected/);
});

// ---- row 7: the verifier's network is an allow-list ----

test('row 7: host and unknown network names are refused; a listed name is accepted', (t) => {
  const toml = (n) => `[agent]\nnetwork_mode = "none"\n[verifier]\nnetwork_mode = "${n}"\n`;
  for (const bad of ['host', 'bridge-x', 'container']) {
    assert.throws(() => loadTask(makeTask(tmp(t), 'net', { 'task.toml': toml(bad) })), /network_mode/, bad);
  }
  for (const ok of ['none', 'default', 'public']) assert.equal(loadTask(makeTask(tmp(t), 'net', { 'task.toml': toml(ok) })).verifierNetwork, ok);
  assert.equal(loadTask(makeTask(tmp(t), 'net', { 'task.toml': toml('bench-verifier') }), { allowedNetworks: ['bench-verifier'] }).verifierNetwork, 'bench-verifier');
  assert.throws(() => buildRunArgs({ image: `${IMAGE}@${DIGEST}`, script: 'x', network: 'host' }), /host/);
});

// ---- row 8: infrastructure errors are retried, then invalid ----

test('row 8: an infrastructure error is retried twice and then the grade is invalid, never a fail', async (t) => {
  const task = loaded(t);
  let calls = 0;
  const fake = new FakeContainers({ onRun: () => { calls++; return { exitCode: 125 }; } });
  const r = await gradePatch({ containers: fake, task, patchFile: null, workRoot: tmp(t) });
  assert.equal(r.invalid, true);
  assert.equal(r.pass, false);
  assert.equal(calls, 3 * 3, 'three attempts, each tried three times');
});

test('row 8: an error that goes away on the retry is not held against the patch', async (t) => {
  let n = 0;
  const fake = new FakeContainers({ onRun: (spec) => { if (n++ === 0) return { exitCode: 125 }; fs.writeFileSync(path.join(REWARD(spec), 'reward.txt'), '1\n'); return { exitCode: 0 }; } });
  const r = await gradePatch({ containers: fake, task: loaded(t), patchFile: null, workRoot: tmp(t) });
  assert.deepEqual([r.pass, r.invalid, r.passes], [true, false, 3]);
});

// ---- row 9: the gold also passes through the real grading path ----

test('row 9: a task whose gold passes only in the clone path is dropped', async (t) => {
  const fake = new FakeContainers({ onRun: (spec) => {
    if (!spec.mounts) return { stdout: 'devcontainer=no\n' };
    const gold = spec.mounts.some((m) => m.target === '/patch/model.patch');
    const real = !/git clone/.test(spec.script);
    fs.writeFileSync(path.join(REWARD(spec), 'reward.txt'), gold && !real ? '1\n' : '0\n');
    return {};
  } });
  const r = await checkTask({ containers: fake, task: loaded(t), workRoot: tmp(t) });
  assert.deepEqual([r.ok, r.reason], [false, 'gold-fails-at-app']);
  assert.ok(fake.runs.some((s) => !/git clone/.test(s.script) && s.mounts?.some((m) => m.target === '/patch/model.patch')));
});

// ---- row 10 ----

test('row 10: rm -f has a -- before the path', () => {
  assert.match(ownTestScript({ testFiles: ['-rf'], testCommand: 'true' }), /rm -f -- "\$f"/);
});

// ---- row 11: URLs ----

test('row 11: a URL that begins with - or has the wrong scheme is refused', (t) => {
  for (const tasks of [{ upstreamUrl: '--upload-pack=touch' }, { upstreamUrl: 'ftp://x/y' }, { verifierNetworks: ['host'] }, { upstreamDir: '/absolute' }, { workspaceImage: 'x:1' }]) {
    assert.throws(() => normalizeConfig(rawConfig({ tasks })), ConfigError, JSON.stringify(tasks));
  }
  for (const tasks of [{ upstreamUrl: 'https://example.test/x.git' }, { upstreamUrl: '/srv/up.git' }, { verifierNetworks: ['bench-verifier'] }, { upstreamDir: 'v2' }]) normalizeConfig(rawConfig({ tasks }));
  assert.throws(() => fetchUpstream({ url: '--upload-pack=touch', commit: 'a'.repeat(40), dir: path.join(tmp(t), 'x') }), UpstreamError);
});

// ---- row 12: numeric options ----

function cli(t, tasks = {}) {
  const dir = tmp(t);
  const f = path.join(dir, 'c.json');
  fs.writeFileSync(f, JSON.stringify(rawConfig({ tasks })));
  const err = [];
  return { err, run: (argv) => runTasks(argv, { env: { BENCH_CONFIG: f, BENCH_HOME: path.join(dir, 'h'), BENCH_TASKS_DIR: path.join(dir, 'r') }, out: () => {}, err: (l) => err.push(l) }) };
}

test('row 12: a bad number is a usage error, not a stack trace', async (t) => {
  const c = cli(t);
  for (const argv of [['select', '--total', 'abc'], ['select', '--total', '0'], ['select', '--total', '-3'], ['select', '--total', '1.5'], ['mine', '--limit', 'x']]) {
    assert.equal(await c.run(argv), 64, argv.join(' '));
  }
  assert.match(c.err.join('\n'), /positive whole number/);
});

test('row 12: no selfRepo means no cap of its own', async (t) => {
  const c = cli(t, { selfRepo: null });
  assert.equal(await c.run(['select', '--source', 'own']), 0);
});

// ---- row 13 ----

test('row 13: a symlink anywhere in a task directory is refused', (t) => {
  const root = tmp(t);
  const outside = path.join(tmp(t), 'outside.sh');
  fs.writeFileSync(outside, '#!/bin/bash\necho 1 > /logs/verifier/reward.txt\n');
  const dir = makeTask(root, 'linked', { 'tests/test.sh': null });
  fs.mkdirSync(path.join(dir, 'tests'), { recursive: true });
  fs.symlinkSync(outside, path.join(dir, 'tests', 'test.sh'));
  assert.throws(() => loadTask(dir), /symlink/);
  const dir2 = makeTask(root, 'linked2');
  fs.symlinkSync('/etc', path.join(dir2, 'solution', 'etc'));
  assert.throws(() => loadTask(dir2), /symlink/);
});

// ---- the seed golden ----

test('selectTasks: a fixed input and seed give a fixed list (a golden)', () => {
  const pool = [];
  for (const [repo, n] of [['a', 6], ['b', 6], ['c', 2]]) for (let i = 0; i < n; i++) pool.push({ id: `${repo}${i}`, repo });
  const ids = selectTasks(pool, { seed: 'golden', scope: 'public', total: 8, perRepoCap: 4 }).map((x) => x.id);
  assert.deepEqual(ids, GOLDEN);
});

const GOLDEN = ['a2', 'a4', 'a5', 'b0', 'b1', 'b3', 'c0', 'c1'];
void SHA;

// ---- the report: invalid grades and suspect patches ----

test('report: an invalid grade is left out of the rates and listed; a suspect patch is listed with its path and what was stripped', () => {
  const run = (id, task, result) => [
    { kind: 'state', run: id, state: 'Planned', task, agent: 'a', arm: 'max', source: 'public', language: 'go' },
    { kind: 'state', run: id, state: 'Reserved', reservedMicro: 1000 },
    { kind: 'state', run: id, state: 'Preparing' },
    { kind: 'state', run: id, state: 'Running' },
    { kind: 'state', run: id, state: 'Collecting' },
    { kind: 'state', run: id, state: 'Grading' },
    { kind: 'state', run: id, state: 'Done', result },
  ];
  const records = [{ kind: 'setup', modelLabel: 'Example Model', seed: 1 }, ...run('R1', 'T1', { pass: true, exit: 'done' }), ...run('R2', 'T2', { pass: false, invalid: true, exit: 'done' }), ...run('R3', 'T3', { pass: true, exit: 'done', suspect: ['mentions-grading'], removed: ['tests/conftest.py'], patch: '/runs/R3/model.patch' })];
  const text = buildReport(records);
  assert.match(text, /R2 \| Done \(invalid grade\)/);
  assert.match(text, /\| a \| max \| 2 \| 2 \|/, 'two counted runs: the invalid one is not a fail');
  assert.match(text, /## Suspect patches/);
  assert.match(text, /R3 \| T3 \| a \/ max \| mentions-grading \| tests\/conftest\.py \| \/runs\/R3\/model\.patch/);
  assert.match(text, /cannot be made tamper-proof/);
});
