// Tests that kill the mutations of the sabotage pass (bench2-sabotage), each named for what it holds.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { FakeContainers } from '../lib/containers.mjs';
import { collectPatch, gradeOnce, gradePatch, sanitizePatch } from '../lib/grade.mjs';
import { cfgTestRanges } from '../lib/mine.mjs';
import { runTasks } from '../lib/tasks-cli.mjs';
import { UpstreamError, cloneArgs, fetchUpstream } from '../lib/upstream.mjs';
import { commitFiles, git, initRepo } from './gitfix.mjs';
import { rawConfig, tmp } from './helpers.mjs';
import { IMAGE, makeTask } from './taskfix.mjs';
import { loadTask } from '../lib/task.mjs';

const logsOf = (spec) => spec.mounts.find((m) => m.target === '/logs/verifier').source;
const task = (t) => loadTask(makeTask(tmp(t), 'mut-task'));
const writer = (reward, exitCode = 0) => new FakeContainers({ onRun: (spec) => { if (reward !== null) fs.writeFileSync(path.join(logsOf(spec), 'reward.txt'), reward); return { exitCode }; } });

// ---- the reward is exactly 1 ----

test('the reward is exactly 1, or 0, with at most one newline: "1 0", "1\\n0", "10", " 1" and "1\\n\\n" are not a pass', async (t) => {
  for (const bad of ['1 0', '1\n0', '10', ' 1', '1\n\n', '1\r\n', '01', 'yes', '']) {
    const r = await gradeOnce({ containers: writer(bad), task: task(t), workRoot: tmp(t) });
    assert.equal(r.pass, false, JSON.stringify(bad));
    assert.ok(['bad-reward', 'no-reward'].includes(r.error), JSON.stringify(bad));
  }
  for (const good of ['1', '1\n']) assert.equal((await gradeOnce({ containers: writer(good), task: task(t), workRoot: tmp(t) })).pass, true);
  assert.equal((await gradeOnce({ containers: writer('0\n'), task: task(t), workRoot: tmp(t) })).error, null);
});

// ---- infrastructure errors ----

test('exits 98, 125, 126 and 127 are infrastructure: retried twice, then invalid', async (t) => {
  for (const code of [98, 125, 126, 127]) {
    const fake = writer(null, code);
    const r = await gradePatch({ containers: fake, task: task(t), patchFile: null, workRoot: tmp(t) });
    assert.equal(r.invalid, true, String(code));
    assert.equal(fake.runs.length, 9, `${code}: three attempts, three tries each`);
    assert.equal(r.pass, false);
  }
});

test('a container timeout is retried, and then the grade is invalid', async (t) => {
  const fake = new FakeContainers({ onRun: () => ({ exitCode: 0, timedOut: true }) });
  const one = await gradeOnce({ containers: fake, task: task(t), workRoot: tmp(t) });
  assert.equal(one.error, 'timeout');
  const r = await gradePatch({ containers: new FakeContainers({ onRun: () => ({ exitCode: 0, timedOut: true }) }), task: task(t), patchFile: null, workRoot: tmp(t) });
  assert.deepEqual([r.invalid, r.pass], [true, false]);
  assert.equal(r.attempts.every((a) => a.invalid), true);
});

test('an invalid grade never passes, even when two attempts did', async (t) => {
  let n = 0;
  const fake = new FakeContainers({ onRun: (spec) => {
    const i = n++;
    if (i >= 2) return { exitCode: 125 };
    fs.writeFileSync(path.join(logsOf(spec), 'reward.txt'), '1\n');
    return { exitCode: 0 };
  } });
  const r = await gradePatch({ containers: fake, task: task(t), patchFile: null, workRoot: tmp(t) });
  assert.deepEqual([r.passes, r.invalid, r.pass, r.flaky], [2, true, false, false]);
});

// ---- stripping and suspects ----

const sec = (p, body = '+x\n') => `diff --git a/${p} b/${p}\n--- a/${p}\n+++ b/${p}\n@@ -1 +1,2 @@\n y\n${body}`;

test('jest, tox, nox, mocha and karma configuration changes are stripped', () => {
  const names = ['jest.config.js', 'jest.setup.ts', 'tox.ini', 'noxfile.py', '.mocharc.json', 'mocha.opts', 'karma.conf.js', 'web/jest.config.mjs', 'vitest.setup.ts'];
  const r = sanitizePatch(names.map((n) => sec(n)).join('') + sec('src/ok.js'));
  assert.deepEqual(r.removed, [...names].sort());
  assert.match(r.patch, /src\/ok\.js/);
});

test('a pure rename out of or into a test path is stripped', () => {
  const rename = (from, to) => `diff --git a/${from} b/${to}\nsimilarity index 100%\nrename from ${from}\nrename to ${to}\n`;
  assert.deepEqual(sanitizePatch(rename('tests/x.py', 'src/y.py')).removed, ['tests/x.py']);
  assert.deepEqual(sanitizePatch(rename('src/y.py', 'tests/x.py')).removed, ['tests/x.py']);
  assert.match(sanitizePatch(rename('src/a.py', 'src/b.py')).patch, /rename to src\/b\.py/);
});

test('each grading name makes a patch suspect: reward.txt, /logs/verifier, /tests/, test.sh', () => {
  for (const line of ['+open("reward.txt")', '+cd /logs/verifier', '+cat /tests/x', '+bash test.sh']) {
    assert.ok(sanitizePatch(sec('src/a.rs', `${line}\n`)).suspect.includes('mentions-grading'), line);
  }
  assert.deepEqual(sanitizePatch(sec('src/a.rs', '+fn main() {}\n')).suspect, []);
  assert.deepEqual(sanitizePatch(sec('src/a.rs', '-test.sh removed line\n')).suspect, [], 'only added lines count');
});

test('setup.cfg [tool:pytest] and [pytest] are flagged suspect; tox.ini is stripped', () => {
  assert.deepEqual(sanitizePatch(sec('setup.cfg', '+[tool:pytest]\n')).suspect, ['test-configuration:setup.cfg']);
  assert.deepEqual(sanitizePatch(sec('setup.cfg', '+[pytest]\n')).suspect, ['test-configuration:setup.cfg']);
  assert.deepEqual(sanitizePatch(sec('tox.ini', '+[pytest]\n')).removed, ['tox.ini']);
});

// ---- collecting ----

function agentTree(t, files = {}) {
  const dir = initRepo(t);
  commitFiles(dir, { 'a.txt': 'one\n', '.gitignore': 'ignored/\n', ...files }, 'base');
  const bare = path.join(tmp(t), 'base.git');
  execFileSync('git', ['clone', '-q', '--bare', dir, bare]);
  return { dir, bare, base: git(dir, 'rev-parse', 'HEAD').trim() };
}

test('a nested repository is not entered', async (t) => {
  const { dir, bare, base } = agentTree(t);
  const nested = path.join(dir, 'vendor', 'lib');
  fs.mkdirSync(nested, { recursive: true });
  git(nested, 'init', '-q', '-b', 'main');
  fs.writeFileSync(path.join(nested, 'f.txt'), 'nested\n');
  git(nested, 'add', '-A');
  git(nested, 'commit', '-q', '-m', 'n');
  fs.writeFileSync(path.join(dir, 'a.txt'), 'one\ntwo\n');
  const patch = (await collectPatch({ tree: dir, baseRepo: bare, base })).toString('utf8');
  assert.doesNotMatch(patch, /vendor/);
  assert.match(patch, /\+two/);
});

test('the base\'s .gitignore decides what the patch holds, not the agent\'s', async (t) => {
  const { dir, bare, base } = agentTree(t);
  fs.writeFileSync(path.join(dir, '.gitignore'), '*.txt\n');
  fs.writeFileSync(path.join(dir, 'new.txt'), 'hidden by the agent\'s .gitignore\n');
  fs.mkdirSync(path.join(dir, 'ignored'));
  fs.writeFileSync(path.join(dir, 'ignored', 'x'), 'un-ignored by the agent\n');
  const patch = (await collectPatch({ tree: dir, baseRepo: bare, base })).toString('utf8');
  assert.match(patch, /new\.txt/, 'the agent\'s .gitignore does not hide a file');
  assert.doesNotMatch(patch, /ignored\/x/, 'the agent\'s .gitignore does not un-ignore one');
  assert.doesNotMatch(patch, /\+\+\+ b\/\.gitignore/);
});

// ---- the host's own git configuration runs nothing ----

function withEnv(t, vars) {
  const before = {};
  for (const [k, v] of Object.entries(vars)) {
    before[k] = process.env[k];
    process.env[k] = v;
  }
  t.after(() => {
    for (const [k, v] of Object.entries(before)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  });
}

/** An executable that records that it ran, then passes its input through. */
function trap(bin, marks, name) {
  const f = path.join(bin, `${name}.sh`);
  fs.writeFileSync(f, `#!/bin/sh\ntouch ${path.join(marks, name)}\ncat\n`, { mode: 0o755 });
  return f;
}

const FILTER = (cmd) => `[filter "x"]\n\tclean = ${cmd}\n`;

async function runs(t, name, setup, { agentAttributes = true } = {}) {
  const { dir, bare, base } = agentTree(t);
  if (agentAttributes) fs.writeFileSync(path.join(dir, '.gitattributes'), '* filter=x\n');
  fs.writeFileSync(path.join(dir, 'a.txt'), 'one\ntwo\n');
  const marks = tmp(t);
  const bin = tmp(t);
  const vars = setup({ marks, bin, trap: (n) => trap(bin, marks, n) });
  withEnv(t, vars);
  await collectPatch({ tree: dir, baseRepo: bare, base });
  assert.deepEqual(fs.readdirSync(marks), [], `${name}: nothing ran`);
}

test('row 2: host-side git configuration runs nothing: system, global, HOME, XDG, GIT_CONFIG_COUNT and GIT_CONFIG_PARAMETERS', async (t) => {
  await runs(t, 'GIT_CONFIG_SYSTEM', ({ bin, trap: tr }) => {
    const f = path.join(bin, 'system.cfg');
    fs.writeFileSync(f, FILTER(tr('SYSTEM')));
    return { GIT_CONFIG_SYSTEM: f };
  });
  await runs(t, 'GIT_CONFIG_GLOBAL', ({ bin, trap: tr }) => {
    const f = path.join(bin, 'global.cfg');
    fs.writeFileSync(f, FILTER(tr('GLOBAL')));
    return { GIT_CONFIG_GLOBAL: f };
  });
  await runs(t, 'HOME', ({ bin, trap: tr }) => {
    fs.writeFileSync(path.join(bin, '.gitconfig'), FILTER(tr('HOME')));
    return { HOME: bin, GIT_CONFIG_GLOBAL: '' };
  });
  await runs(t, 'XDG_CONFIG_HOME', ({ bin, trap: tr }) => {
    fs.mkdirSync(path.join(bin, 'git'));
    fs.writeFileSync(path.join(bin, 'git', 'config'), FILTER(tr('XDG')));
    return { XDG_CONFIG_HOME: bin, HOME: tmp(t), GIT_CONFIG_GLOBAL: '' };
  });
  await runs(t, 'GIT_CONFIG_COUNT', ({ trap: tr }) => ({ GIT_CONFIG_COUNT: '1', GIT_CONFIG_KEY_0: 'filter.x.clean', GIT_CONFIG_VALUE_0: tr('COUNT') }));
  await runs(t, 'GIT_CONFIG_PARAMETERS', ({ trap: tr }) => ({ GIT_CONFIG_PARAMETERS: `'filter.x.clean=${tr('PARAMETERS')}'` }));
});

test('row 2: a configuration that arrives through a repository template (fsmonitor, hooks, attributes) runs nothing', async (t) => {
  await runs(t, 'template fsmonitor', ({ bin, trap: tr }) => {
    const tpl = path.join(bin, 'tpl');
    fs.mkdirSync(tpl);
    fs.writeFileSync(path.join(tpl, 'config'), `[core]\n\tfsmonitor = ${tr('FSMONITOR')}\n`);
    return { GIT_TEMPLATE_DIR: tpl };
  });
  await runs(t, 'template hooks', ({ bin, trap: tr }) => {
    const tpl = path.join(bin, 'tpl');
    fs.mkdirSync(path.join(tpl, 'hooks'), { recursive: true });
    fs.copyFileSync(tr('HOOK'), path.join(tpl, 'hooks', 'post-checkout'));
    fs.chmodSync(path.join(tpl, 'hooks', 'post-checkout'), 0o755);
    return { GIT_TEMPLATE_DIR: tpl };
  });
  await runs(t, 'template hooksPath', ({ bin, trap: tr }) => {
    const hooks = path.join(bin, 'hk');
    fs.mkdirSync(hooks);
    fs.copyFileSync(tr('HOOKPATH'), path.join(hooks, 'post-checkout'));
    fs.chmodSync(path.join(hooks, 'post-checkout'), 0o755);
    const tpl = path.join(bin, 'tpl');
    fs.mkdirSync(tpl);
    fs.writeFileSync(path.join(tpl, 'config'), `[core]\n\thooksPath = ${hooks}\n`);
    return { GIT_TEMPLATE_DIR: tpl };
  });
  await runs(t, 'template attributesFile', ({ bin, trap: tr }) => {
    const attrs = path.join(bin, 'attrs');
    fs.writeFileSync(attrs, '* filter=y\n');
    const tpl = path.join(bin, 'tpl');
    fs.mkdirSync(tpl);
    fs.writeFileSync(path.join(tpl, 'config'), `[core]\n\tattributesFile = ${attrs}\n[filter "y"]\n\tclean = ${tr('ATTRIBUTES')}\n`);
    return { GIT_TEMPLATE_DIR: tpl };
  }, { agentAttributes: false });
});

// ---- upstream ----

const sha = (s) => execFileSync('sha256sum', { input: s, encoding: 'utf8' }).split(' ')[0];

function upstreamRepo(t, extra = {}) {
  const work = initRepo(t);
  fs.mkdirSync(path.join(work, 'v2', 'one'), { recursive: true });
  fs.writeFileSync(path.join(work, 'v2', 'one', 'task.toml'), 'x');
  fs.writeFileSync(path.join(work, 'v2', 'SHA256SUMS'), `${sha('x')}  one/task.toml\n`);
  fs.writeFileSync(path.join(work, '.gitignore'), '*.log\n');
  for (const [rel, text] of Object.entries(extra)) {
    fs.mkdirSync(path.dirname(path.join(work, rel)), { recursive: true });
    fs.writeFileSync(path.join(work, rel), text);
  }
  const commit = commitFiles(work, {}, 'tasks');
  const bare = path.join(tmp(t), 'u.git');
  execFileSync('git', ['clone', '-q', '--bare', work, bare]);
  return { bare, commit };
}

test('a task directory no SHA256SUMS line names is refused', (t) => {
  const { bare, commit } = upstreamRepo(t, { 'v2/two/task.toml': 'y', 'v2/two/instruction.md': 'z' });
  assert.throws(() => fetchUpstream({ url: bare, commit, dir: path.join(tmp(t), 'c') }), (e) => e instanceof UpstreamError && /not listed/.test(e.message));
});

test('a reused clone is cleaned of ignored files and nested repositories', (t) => {
  const { bare, commit } = upstreamRepo(t);
  const dir = path.join(tmp(t), 'c');
  fetchUpstream({ url: bare, commit, dir });
  fs.writeFileSync(path.join(dir, 'v2', 'cache.log'), 'ignored');
  fs.mkdirSync(path.join(dir, 'v2', 'nested'));
  git(path.join(dir, 'v2', 'nested'), 'init', '-q');
  fetchUpstream({ url: bare, commit, dir });
  assert.equal(fs.existsSync(path.join(dir, 'v2', 'cache.log')), false, 'an ignored file is removed');
  assert.equal(fs.existsSync(path.join(dir, 'v2', 'nested')), false, 'a nested repository is removed');
});

test('git clone gets -- before the URL and the directory', () => {
  for (const url of ['/srv/u.git', 'https://example.test/u.git']) {
    const a = cloneArgs(url, '/work/dir');
    assert.deepEqual(a.slice(-3), ['--', url, '/work/dir']);
    assert.equal(a.includes('--filter=blob:none'), url.startsWith('https'));
  }
});

// ---- the miner ----

test('not(all(test, unix)) is not test code, and a byte raw string is skipped', () => {
  assert.deepEqual(cfgTestRanges('#[cfg(not(all(test, unix)))]\nfn f() {}\n'), []);
  assert.deepEqual(cfgTestRanges('#[cfg(all(not(test), unix))]\nfn f() {}\n'), []);
  assert.deepEqual(cfgTestRanges('#[cfg(test)]\nmod t {\n    const B: &[u8] = br#"a"}"#;\n}\nfn after() {}\n'), [[1, 4]]);
});

// ---- selection ----

test('select refuses a task whose image digest changed after its check', async (t) => {
  const dir = tmp(t);
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
  const config = path.join(dir, 'config.json');
  fs.writeFileSync(config, JSON.stringify(rawConfig({ tasks: { upstreamUrl: bare, upstreamCommit: commit } })));
  const env = { BENCH_CONFIG: config, BENCH_HOME: path.join(dir, 'home'), BENCH_TASKS_DIR: path.join(dir, 'rt') };
  const out = [];
  const run = (argv, make) => runTasks(argv, { env, out: (l) => out.push(l), err: () => {}, make });
  const fake = new FakeContainers({ onRun: (spec) => { if (!spec.mounts) return { stdout: 'devcontainer=no\n' }; fs.writeFileSync(path.join(logsOf(spec), 'reward.txt'), spec.mounts.some((m) => m.target === '/patch/model.patch') ? '1\n' : '0\n'); return {}; } });
  await run(['check'], () => fake);
  await run(['select', '--source', 'public']);
  assert.match(out.join('\n'), /public: 1 of 1 selected/);
  const file = path.join(env.BENCH_HOME, 'tasks', 'check.json');
  const results = JSON.parse(fs.readFileSync(file, 'utf8'));
  const key = Object.keys(results)[0];
  results[key].imageDigest = `sha256:${'9'.repeat(64)}`;
  fs.writeFileSync(file, JSON.stringify(results));
  out.length = 0;
  await run(['select', '--source', 'public']);
  assert.match(out.join('\n'), /public: 0 of 0 selected/);
});
