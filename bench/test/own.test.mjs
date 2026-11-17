import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { OwnTaskError, loadOwnSpecs, materializeOwn, ownChecksum, ownDockerfile, ownImageTag, ownTestScript, parseOwnSpec, prepareOwnContext } from '../lib/own.mjs';
import { loadTask } from '../lib/task.mjs';
import { commitFiles, git, initRepo, lines } from './gitfix.mjs';
import { tmp } from './helpers.mjs';
import { SHA } from './gitfix.mjs';

const WS = `registry.example.test/workspace@sha256:${SHA('d')}`;

export function ownFixture(t) {
  const clone = initRepo(t);
  commitFiles(clone, { 'src/app.ts': lines(5), 'package.json': '{}\n' }, 'chore: start (#1)');
  const base = git(clone, 'rev-parse', 'HEAD').trim();
  const commit = commitFiles(clone, { 'src/app.ts': `${lines(5)}export const added = 1;\n`, 'src/app.test.ts': 'test("hidden test content", () => {});\n' }, 'feat: add (#2)');
  commitFiles(clone, { 'later.txt': 'a later commit\n' }, 'feat: later (#3)');
  const specDir = path.join(tmp(t), 'own', 'demo-2');
  fs.mkdirSync(specDir, { recursive: true });
  fs.writeFileSync(path.join(specDir, 'instruction.md'), 'Make `added` exist.\n');
  const spec = parseOwnSpec({ id: 'demo-2', repo: 'demo', clone: 'demo', commit, base, language: 'typescript', testCommand: 'npx vitest run src/app.test.ts', toolchain: { env: { NODE_ENV: 'test' }, run: ['npm ci'] } }, 'demo-2');
  fs.writeFileSync(path.join(specDir, 'own.json'), JSON.stringify({ id: 'demo-2', repo: 'demo', clone: 'demo', commit, base, language: 'typescript', testCommand: spec.testCommand }));
  return { clone, base, commit, spec: { ...spec, dir: specDir }, specDir };
}

const GOOD = { id: 'x-1', repo: 'x', clone: 'x', commit: 'a'.repeat(40), base: 'b'.repeat(40), testCommand: 'make test' };

test('an own.json is validated field by field', () => {
  assert.equal(parseOwnSpec(GOOD, 'x-1').language, 'unknown');
  const bad = (over, re) => assert.throws(() => parseOwnSpec({ ...GOOD, ...over }, 'x-1'), (e) => e instanceof OwnTaskError && re.test(e.message));
  bad({ extra: 1 }, /unknown field/);
  bad({ id: '../x' }, /safe task id/);
  bad({ id: 'other' }, /equal the directory/);
  bad({ commit: 'abc' }, /full commit id/);
  bad({ base: 'B'.repeat(40) }, /full commit id/);
  bad({ testCommand: '' }, /testCommand/);
  bad({ testCommand: 'a\nb' }, /testCommand/);
  bad({ clone: 'a b' }, /plain name/);
  bad({ toolchain: { env: { lower: 'x' } } }, /toolchain\.env/);
  bad({ toolchain: { env: { A: 'x$y' } } }, /toolchain\.env/);
  bad({ toolchain: { run: ['a\nb'] } }, /one-line/);
  bad({ toolchain: { other: 1 } }, /holds only/);
});

test('the Dockerfile: the workspace image by digest, the repository at /app, the pins', () => {
  const d = ownDockerfile({ workspaceImage: WS, toolchain: { env: { RUSTUP_TOOLCHAIN: '1.94.1' }, run: ['rustup toolchain install 1.94.1'] } });
  assert.equal(d.split('\n')[0], `FROM ${WS}`);
  assert.match(d, /COPY repo\/ \/app\//);
  assert.match(d, /ENV RUSTUP_TOOLCHAIN="1\.94\.1"/);
  assert.match(d, /RUN rustup toolchain install 1\.94\.1/);
  assert.match(d, /WORKDIR \/app\nUSER 10001/);
  assert.throws(() => ownDockerfile({ workspaceImage: 'registry.example.test/workspace:latest' }), /by digest/);
  assert.equal(ownDockerfile({ workspaceImage: WS }), ownDockerfile({ workspaceImage: WS, toolchain: { env: {}, run: [] } }));
});

test('the test script resets the hidden test files, applies the patch from /tests and writes the reward', (t) => {
  const s = ownTestScript({ testFiles: ['src/a.test.ts', 'tests/it.rs'], testCommand: 'cargo test --locked' });
  assert.match(s, /\/logs\/verifier\/reward\.txt/);
  assert.match(s, /git checkout -q HEAD -- "\$f"/);
  assert.match(s, /git apply --whitespace=nowarn \/tests\/test_patch\.patch/);
  assert.match(s, /\( cargo test --locked \)/);
  const f = path.join(tmp(t), 't.sh');
  fs.writeFileSync(f, s);
  execFileSync('bash', ['-n', f]);
  assert.throws(() => ownTestScript({ testFiles: ["a'b"], testCommand: 'x' }), OwnTaskError);
});

test('the build context holds the history up to the base and nothing later', (t) => {
  const { clone, base, commit } = ownFixture(t);
  const out = path.join(tmp(t), 'ctx', 'repo');
  prepareOwnContext({ cloneDir: clone, base, outDir: out });
  assert.equal(git(out, 'rev-parse', 'HEAD').trim(), base);
  assert.throws(() => git(out, 'cat-file', '-e', commit), 'the commit with the tests is not there');
  assert.equal(git(out, 'for-each-ref').trim(), '');
  assert.equal(git(out, 'remote').trim(), '');
  assert.equal(fs.existsSync(path.join(out, 'src', 'app.test.ts')), false);
  assert.equal(fs.existsSync(path.join(out, 'later.txt')), false);
  assert.match(git(out, 'status', '--porcelain'), /^$/);
});

test('materializing makes a valid task: hidden tests read from the clone, gold without them', (t) => {
  const { clone, spec, base } = ownFixture(t);
  const out = path.join(tmp(t), 'm', 'demo-2');
  const { tag, testFiles } = materializeOwn({ spec, cloneDir: clone, outDir: out, workspaceImage: WS });
  assert.deepEqual(testFiles, ['src/app.test.ts']);
  assert.equal(tag, ownImageTag(spec));
  assert.ok(tag.endsWith(base.slice(0, 12)));
  const hidden = fs.readFileSync(path.join(out, 'tests', 'test_patch.patch'), 'utf8');
  assert.match(hidden, /hidden test content/);
  const gold = fs.readFileSync(path.join(out, 'solution', 'solution.patch'), 'utf8');
  assert.match(gold, /export const added = 1/);
  assert.doesNotMatch(gold, /hidden test content/);
  assert.equal(fs.readFileSync(path.join(out, 'instruction.md'), 'utf8'), 'Make `added` exist.\n');
  assert.throws(() => loadTask(out), /not pinned by digest/);
  const task = loadTask(out, { pins: { [tag]: `sha256:${SHA('e')}` }, source: 'own' });
  assert.equal(task.image.ref, `${tag.split(':')[0]}@sha256:${SHA('e')}`);
  assert.equal(task.language, 'typescript');
  assert.equal(task.repo, 'demo');
  assert.ok(task.goldPatch);
});

test('the gold and the hidden tests, applied to the base, give the commit', (t) => {
  const { clone, spec, commit, base } = ownFixture(t);
  const out = path.join(tmp(t), 'm', 'demo-2');
  materializeOwn({ spec, cloneDir: clone, outDir: out, workspaceImage: WS });
  const work = tmp(t);
  execFileSync('git', ['clone', '-q', clone, work]);
  git(work, 'checkout', '-q', '--detach', base);
  git(work, 'apply', path.join(out, 'solution', 'solution.patch'));
  git(work, 'apply', path.join(out, 'tests', 'test_patch.patch'));
  git(work, 'add', '-A');
  assert.equal(git(work, 'diff', '--cached', '--stat', commit).trim(), '');
});

test('a base that is not the first parent, or a commit with no test, is refused', (t) => {
  const { clone, spec } = ownFixture(t);
  const out = path.join(tmp(t), 'm', 'x');
  assert.throws(() => materializeOwn({ spec: { ...spec, base: 'f'.repeat(40) }, cloneDir: clone, outDir: out, workspaceImage: WS }), /not the first parent|unknown revision|failed/);
  assert.throws(() => materializeOwn({ spec: { ...spec, commit: git(clone, 'rev-parse', 'HEAD').trim(), base: git(clone, 'rev-parse', 'HEAD^').trim() }, cloneDir: clone, outDir: out, workspaceImage: WS }), /no test file/);
});

test('loadOwnSpecs reads the specs, and names a statement the owner has not written', (t) => {
  const { specDir } = ownFixture(t);
  const root = path.dirname(specDir);
  const ok = loadOwnSpecs(root);
  assert.equal(ok.specs.length, 1);
  assert.equal(ok.errors.length, 0);
  fs.writeFileSync(path.join(specDir, 'instruction.md'), '  \n');
  assert.match(loadOwnSpecs(root).errors[0].problems[0], /the owner writes it/);
  fs.rmSync(path.join(specDir, 'own.json'));
  assert.match(loadOwnSpecs(root).errors[0].problems[0], /own\.json/);
  assert.deepEqual(loadOwnSpecs(path.join(root, 'nothing')), { specs: [], errors: [] });
});

test('the checksum covers the statement and the pins', (t) => {
  const { spec, specDir } = ownFixture(t);
  const a = ownChecksum(spec);
  assert.match(a, /^[0-9a-f]{64}$/);
  fs.appendFileSync(path.join(specDir, 'instruction.md'), 'more\n');
  assert.notEqual(ownChecksum(spec), a);
});
