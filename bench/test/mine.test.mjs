import assert from 'node:assert/strict';
import { test } from 'node:test';
import { cfgTestRanges, classifyPath, evaluateCommit, gitIn, mine, parseHunks, touchesInlineTests } from '../lib/mine.mjs';
import { commitFiles, git, initRepo, lines } from './gitfix.mjs';

const RS = `pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn sub(a: i32, b: i32) -> i32 {
    a - b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(1, 2), 3);
    }
}
`;

function fixture(t) {
  const dir = initRepo(t);
  commitFiles(dir, { 'Cargo.toml': '[package]\nname = "demo"\n', 'src/lib.rs': RS, 'src/app.ts': lines(10), 'package.json': '{}\n', 'Cargo.lock': 'v1\n', 'README.md': 'hi\n' }, 'chore: start (#1)');
  return dir;
}

const judge = (dir, c, extra) => evaluateCommit(gitIn(dir), c, extra);

test('classifyPath: tests, lockfiles, documentation and sources', () => {
  for (const p of ['tests/a.rs', 'src/__tests__/x.ts', 'web/e2e/a.spec.ts', 'a/b.test.ts', 'a/b.spec.js', 'pkg/x_test.go', 'x/test_y.py', 'a/y_test.py']) assert.equal(classifyPath(p), 'test', p);
  for (const p of ['Cargo.lock', 'web/pnpm-lock.yaml', 'go.sum', 'package-lock.json']) assert.equal(classifyPath(p), 'lock', p);
  for (const p of ['README.md', 'docs/x.yaml', 'a/b.png']) assert.equal(classifyPath(p), 'doc', p);
  for (const p of ['src/lib.rs', 'web/app.tsx', 'pkg/x.go', 'deploy/values.yaml']) assert.equal(classifyPath(p), 'source', p);
});

test('a commit that passes every rule is a candidate, with its base, tests and a hint', (t) => {
  const dir = fixture(t);
  const c = commitFiles(dir, { 'src/app.ts': `${lines(10)}export const x = 1;\n`, 'src/app.test.ts': 'test("x", () => {});\n' }, 'feat(app): add x (#12)');
  const r = judge(dir, c);
  assert.equal(r.ok, true, r.reasons.join());
  assert.equal(r.candidate.id, 'repo-12');
  assert.equal(r.candidate.pr, 12);
  assert.equal(r.candidate.base, git(dir, 'rev-parse', `${c}^`).trim());
  assert.deepEqual(r.candidate.testFiles, ['src/app.test.ts']);
  assert.deepEqual(r.candidate.sourceFiles, ['src/app.ts']);
  assert.equal(r.candidate.language, 'typescript');
  assert.equal(r.candidate.nonTestLines, 1);
  assert.match(r.candidate.testCommandHint, /^cd \S* ?&?&? ?npx vitest run|npx vitest run src\/app\.test\.ts/);
});

test('each rule refuses its commit', (t) => {
  const dir = fixture(t);
  const refused = (files, message, reason, extra) => {
    const c = commitFiles(dir, files, message);
    const r = judge(dir, c, extra);
    assert.equal(r.ok, false, message);
    assert.ok(r.reasons.includes(reason), `${message}: ${reason} not in ${r.reasons}`);
  };
  const good = { 'src/app.ts': lines(10, 'changed'), 'src/app.test.ts': 'test("y", () => {});\n' };
  refused({ ...good, 'src/app.ts': lines(10, 'again') }, 'feat: no pull request number', 'no-pr-number');
  refused({ 'src/app.ts': lines(10, 'b'), 'src/app.test.ts': 'test("z", () => {});\n' }, 'docs: words (#2)', 'type-docs');
  refused({ 'src/app.ts': lines(10, 'c'), 'src/app.test.ts': 'test("w", () => {});\n' }, 'chore(deps): bump (#3)', 'type-chore');
  refused({ 'src/app.ts': lines(10, 'd'), 'src/app.test.ts': 'test("v", () => {});\n' }, 'Revert "feat: x" (#4)', 'type-revert');
  refused({ 'src/app.ts': lines(10, 'e') }, 'fix: source only (#5)', 'no-tests');
  refused({ 'src/app.test.ts': 'test("u", () => {});\n' }, 'test: tests only (#6)', 'no-source');
  refused({ 'src/app.ts': lines(500, 'big'), 'src/app.test.ts': 'test("t", () => {});\n' }, 'feat: too big (#7)', 'too-large');
  refused({ 'src/app.ts': lines(10, 'f'), 'src/app.test.ts': 'test("s", () => {});\n', 'Cargo.lock': 'v2\n' }, 'feat: touches a lockfile (#8)', 'lockfile');
  refused({ 'src/lib.rs': RS.replace('assert_eq!(add(1, 2), 3);', 'assert_eq!(add(2, 2), 4);'), 'tests/it.rs': 'fn main() {}\n' }, 'test: edits an inline test (#9)', 'inline-tests');
});

test('the size rule counts only non-test source lines and takes its limit', (t) => {
  const dir = fixture(t);
  const c = commitFiles(dir, { 'src/app.ts': lines(30, 'n'), 'src/app.test.ts': lines(900, 't') }, 'feat: big tests (#20)');
  assert.equal(judge(dir, c).ok, true, 'a large test change is fine');
  const d = commitFiles(dir, { 'src/app.ts': lines(30, 'm'), 'src/app.test.ts': lines(5, 'u') }, 'feat: small limit (#21)');
  assert.ok(judge(dir, d, { maxNonTestLines: 10 }).reasons.includes('too-large'));
});

test('a Rust change outside the cfg(test) module passes, inside it is refused', (t) => {
  const dir = fixture(t);
  const outside = commitFiles(dir, { 'src/lib.rs': RS.replace('a + b', 'a.wrapping_add(b)'), 'tests/it.rs': 'fn main() {}\n' }, 'fix: outside (#30)');
  assert.equal(judge(dir, outside).ok, true);
  const inside = commitFiles(dir, { 'src/lib.rs': RS.replace('a + b', 'a.wrapping_add(b)').replace('add(1, 2), 3', 'add(1, 2), 4'), 'tests/it.rs': 'fn main() { }\n' }, 'fix: inside (#31)');
  assert.ok(judge(dir, inside).reasons.includes('inline-tests'));
  assert.match(judge(dir, outside).candidate.testCommandHint, /cargo test --locked/);
});

test('a merge commit and the root commit are not squash commits', (t) => {
  const dir = fixture(t);
  git(dir, 'checkout', '-q', '-b', 'side');
  commitFiles(dir, { 'side.txt': 'x\n' }, 'feat: side (#40)');
  git(dir, 'checkout', '-q', 'main');
  commitFiles(dir, { 'main.txt': 'x\n' }, 'feat: main (#41)');
  git(dir, 'merge', '-q', '--no-ff', '-m', 'Merge pull request (#42)', 'side');
  const merge = git(dir, 'rev-parse', 'HEAD').trim();
  assert.ok(judge(dir, merge).reasons.includes('not-squash'));
  const root = git(dir, 'rev-list', '--max-parents=0', 'HEAD').trim();
  assert.ok(judge(dir, root).reasons.includes('not-squash'));
});

test('mine walks the first-parent history, keeps the good and counts the refusals', (t) => {
  const dir = fixture(t);
  commitFiles(dir, { 'src/app.ts': lines(10, 'p'), 'src/app.test.ts': 'test("a", () => {});\n' }, 'feat: one (#50)');
  commitFiles(dir, { 'src/app.ts': lines(10, 'q') }, 'fix: two (#51)');
  commitFiles(dir, { 'src/app.ts': lines(10, 'r'), 'src/app.test.ts': 'test("b", () => {});\n' }, 'feat: three (#52)');
  const { candidates, refused } = mine({ dir, repoName: 'demo' });
  assert.deepEqual(candidates.map((c) => c.pr), [52, 50]);
  assert.equal(refused['no-tests'], 1);
  assert.equal(mine({ dir, repoName: 'demo', limit: 1 }).candidates.length, 1);
});

test('cfgTestRanges and touchesInlineTests handle braces in strings, comments and char literals', () => {
  const text = `fn a() {}\n#[cfg(test)]\nmod tests {\n    // }\n    const S: &str = "}";\n    const C: char = '}';\n    fn t() {}\n}\nfn after() {}\n`;
  assert.deepEqual(cfgTestRanges(text), [[2, 8]]);
  assert.deepEqual(cfgTestRanges('#[cfg(test)]\nuse x::y;\nfn z() {}\n'), [[1, 2]]);
  assert.deepEqual(parseHunks('@@ -3,0 +4,2 @@\n+a\n+b\n@@ -9 +10 @@\n'), [{ oldStart: 3, oldLen: 0, newStart: 4, newLen: 2 }, { oldStart: 9, oldLen: 1, newStart: 10, newLen: 1 }]);
  assert.equal(touchesInlineTests({ hunks: [{ oldStart: 1, oldLen: 0, newStart: 9, newLen: 1 }], oldText: '', newText: text }), false);
  assert.equal(touchesInlineTests({ hunks: [{ oldStart: 5, oldLen: 1, newStart: 5, newLen: 0 }], oldText: text, newText: 'fn a() {}\n' }), true, 'a deletion inside is read from the old text');
});
