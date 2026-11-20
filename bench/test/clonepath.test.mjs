// The environment check's clone path and the verifier's egress probe, run for real on this machine: the grading
// script `gradeScript` makes is executed with its container paths mapped to a temporary directory, on a fixture that is
// a tiny Node project with an ignored `node_modules` and `config.json`, as an image's /app has them.
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { test } from 'node:test';
import { FakeContainers } from '../lib/containers.mjs';
import { CLONE_PATH, checkTask } from '../lib/envcheck.mjs';
import { EGRESS_PROBE, gradeOnce, gradeScript } from '../lib/grade.mjs';
import { loadTask } from '../lib/task.mjs';
import { commitFiles, git } from './gitfix.mjs';
import { tmp } from './helpers.mjs';
import { makeTask } from './taskfix.mjs';

/**
 * A task's image, on disk: `app` is its repository at the base, with what the image installed and git ignores
 * (`node_modules/helper`, a module that requires the project's sources by a relative path, and `config.json`). The test
 * script writes `config.json` (as NodeBB's run_script.sh does) and passes only if `helper` returns the patched value.
 */
function fixture(t) {
  const root = tmp(t);
  const app = path.join(root, 'image', 'app');
  fs.mkdirSync(app, { recursive: true });
  git(app, 'init', '-q', '-b', 'main');
  commitFiles(app, { '.gitignore': 'node_modules/\nconfig.json\n', 'src/value.js': "module.exports = 'base';\n" }, 'base');
  fs.mkdirSync(path.join(app, 'node_modules', 'helper'), { recursive: true });
  fs.writeFileSync(path.join(app, 'node_modules', 'helper', 'package.json'), '{"name":"helper","main":"index.js"}\n');
  fs.writeFileSync(path.join(app, 'node_modules', 'helper', 'index.js'), "module.exports = () => require('../../src/value.js');\n");
  fs.writeFileSync(path.join(app, 'config.json'), '{"image":true}\n');
  fs.writeFileSync(path.join(app, 'src', 'value.js'), "module.exports = 'gold';\n");
  const gold = path.join(root, 'gold.patch');
  fs.writeFileSync(gold, git(app, 'diff'));
  git(app, 'checkout', '-q', '--', 'src/value.js');
  const logs = path.join(root, 'reward-logs');
  const tests = path.join(root, 't');
  fs.mkdirSync(logs);
  fs.mkdirSync(tests);
  fs.writeFileSync(
    path.join(tests, 'test.sh'),
    [
      '#!/bin/bash',
      `cd ${app} || exit 1`,
      `echo '{"written":"by the test"}' > config.json`,
      `v=$(node -e "process.stdout.write(require('helper')())")`,
      `if [ "$v" = gold ]; then echo 1 > ${logs}/reward.txt; else echo 0 > ${logs}/reward.txt; fi`,
      '',
    ].join('\n'),
  );
  return { root, app, gold, logs, tests, slot: path.join(root, 'work', 'workspaces', 'bench', 'slot') };
}

const run = (file, args, opts) => new Promise((resolve) => execFile(file, args, opts, (err, stdout, stderr) => resolve({ code: err ? err.code : 0, stdout, stderr })));

/** Run a grading script here: its container paths become the fixture's. Returns { code, reward }. */
async function gradeHere(f, { patch, cloneTo = null, probe = [] }) {
  const map = {
    '/logs/verifier': f.logs,
    '/patch/model.patch': patch ?? path.join(f.root, 'no.patch'),
    '/tests/test.sh': path.join(f.tests, 'test.sh'),
    '/tmp/bench-test.sh': path.join(f.root, 'bench-test.sh'),
  };
  const script = gradeScript({ workdir: f.app, cloneTo, probe }).replace(/\/logs\/verifier|\/patch\/model\.patch|\/tests\/test\.sh|\/tmp\/bench-test\.sh/g, (m) => map[m]);
  fs.rmSync(path.join(f.logs, 'reward.txt'), { force: true });
  const r = await run('sh', ['-c', script], { env: { PATH: process.env.PATH, HOME: f.root, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' }, timeout: 60_000 });
  let reward = null;
  try {
    reward = fs.readFileSync(path.join(f.logs, 'reward.txt'), 'utf8').trim();
  } catch {
    reward = null;
  }
  return { code: r.code, reward, stderr: r.stderr };
}

test('the clone path grades the tree /app grades: the gold passes and the empty patch fails on both', async (t) => {
  for (const cloneTo of [null, 'slot']) {
    const g = fixture(t);
    const gold = await gradeHere(g, { patch: g.gold, cloneTo: cloneTo && g.slot });
    assert.deepEqual([gold.code, gold.reward], [0, '1'], `gold, ${cloneTo ?? '/app'}: ${gold.stderr}`);
    const e = fixture(t);
    const empty = await gradeHere(e, { cloneTo: cloneTo && e.slot });
    assert.deepEqual([empty.code, empty.reward], [0, '0'], `empty, ${cloneTo ?? '/app'}: ${empty.stderr}`);
  }
});

test('a module under an ignored node_modules resolves the clone\'s sources, never the image\'s (Node follows a link to its real path)', async (t) => {
  const f = fixture(t);
  await gradeHere(f, { patch: f.gold, cloneTo: f.slot });
  const helper = path.join(f.slot, 'node_modules', 'helper');
  assert.equal(fs.lstatSync(path.join(f.slot, 'node_modules')).isSymbolicLink(), false, 'node_modules is the clone\'s own');
  assert.equal(fs.realpathSync(helper), helper);
});

test('a run on the clone path writes nothing into the image\'s tree', async (t) => {
  const f = fixture(t);
  await gradeHere(f, { patch: f.gold, cloneTo: f.slot });
  assert.equal(fs.readFileSync(path.join(f.app, 'config.json'), 'utf8'), '{"image":true}\n', 'the test script\'s config.json went to the clone');
  assert.equal(fs.readFileSync(path.join(f.app, 'src', 'value.js'), 'utf8'), "module.exports = 'base';\n");
  assert.equal(git(f.app, 'status', '--porcelain'), '');
  assert.equal(fs.readFileSync(path.join(f.slot, 'config.json'), 'utf8'), '{"written":"by the test"}\n');
});

test('the check\'s clone path copies the image\'s whole tree, links nothing and clones nothing', () => {
  const s = gradeScript({ workdir: '/app', cloneTo: CLONE_PATH });
  assert.match(s, new RegExp(`cp -a /app/\\. ${CLONE_PATH}/`));
  assert.doesNotMatch(s, /ln -s/);
  assert.doesNotMatch(s, /git clone/);
});

// ---- the egress probe ----

/** A port that listens (the server is closed with the test) and one that does not. */
async function ports(t) {
  const server = net.createServer((c) => c.destroy());
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  t.after(() => server.close());
  const gone = net.createServer();
  await new Promise((r) => gone.listen(0, '127.0.0.1', r));
  const closed = gone.address().port;
  await new Promise((r) => gone.close(r));
  return { open: server.address().port, closed };
}

test('the probe passes when every host answers, and an unreachable one is an infrastructure exit (98) before anything is graded', async (t) => {
  const { open, closed } = await ports(t);
  const ok = fixture(t);
  const a = await gradeHere(ok, { patch: ok.gold, probe: [`127.0.0.1:${open}`] });
  assert.deepEqual([a.code, a.reward], [0, '1'], a.stderr);
  const bad = fixture(t);
  const b = await gradeHere(bad, { patch: bad.gold, probe: [`127.0.0.1:${open}`, `127.0.0.1:${closed}`] });
  assert.deepEqual([b.code, b.reward], [98, null]);
  assert.match(b.stderr, new RegExp(`does not reach 127\\.0\\.0\\.1:${closed}`));
  assert.equal(git(bad.app, 'status', '--porcelain'), '', 'the patch was not applied');
});

test('a probe entry is a plain host and port, nothing a shell would read', () => {
  for (const bad of ['a b:1', 'x;rm:1', '$(id):443', 'host', 'host:', 'host:99999', '-x:1']) assert.throws(() => gradeScript({ probe: [bad] }), /probe/, bad);
  assert.doesNotThrow(() => gradeScript({ probe: EGRESS_PROBE }));
});

test('only a public verifier is probed: its grading container checks the registries before the patch', async (t) => {
  const scripts = {};
  for (const mode of ['public', 'default', 'none']) {
    const task = loadTask(makeTask(tmp(t), `net-${mode}`, { 'task.toml': `[agent]\nnetwork_mode = "none"\n[verifier]\nnetwork_mode = "${mode}"\n` }));
    const fake = new FakeContainers({ onRun: () => ({ exitCode: 0 }) });
    await gradeOnce({ containers: fake, task, workRoot: tmp(t) });
    scripts[mode] = fake.runs[0];
  }
  assert.equal(scripts.public.network, 'egress');
  for (const hp of EGRESS_PROBE) assert.ok(scripts.public.script.includes(hp), hp);
  assert.ok(scripts.public.script.indexOf('/dev/tcp') < scripts.public.script.indexOf('/patch/model.patch'), 'the probe runs before the patch is applied');
  assert.doesNotMatch(scripts.default.script, /\/dev\/tcp/);
  assert.doesNotMatch(scripts.none.script, /\/dev\/tcp/);
});

test('a public task checked on a network with no route out is infra, never gold-fails', async (t) => {
  const task = loadTask(makeTask(tmp(t), 'needs-npm', { 'task.toml': '[agent]\nnetwork_mode = "none"\n[verifier]\nnetwork_mode = "public"\n' }));
  // The network is broken: the probe cannot connect; a test script that ran anyway would fail its npm install.
  const fake = new FakeContainers({
    onRun: (spec) => {
      if (!spec.mounts) return { stdout: 'devcontainer=no\n' };
      if (/\/dev\/tcp/.test(spec.script)) return { exitCode: 98 };
      fs.writeFileSync(path.join(spec.mounts.find((m) => m.target === '/logs/verifier').source, 'reward.txt'), '0\n');
      return { exitCode: 0 };
    },
  });
  const r = await checkTask({ containers: fake, task, workRoot: tmp(t) });
  assert.deepEqual([r.ok, r.reason], [false, 'infra']);
});
