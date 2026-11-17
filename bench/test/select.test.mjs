import assert from 'node:assert/strict';
import { test } from 'node:test';
import { lintStatement, splitDiff, withoutInterface } from '../lib/lint.mjs';
import { buildLock, lockText, selectTasks } from '../lib/select.mjs';

const GOLD = `diff --git a/src/limits.rs b/src/limits.rs
index 111..222 100644
--- a/src/limits.rs
+++ b/src/limits.rs
@@ -3,2 +3,5 @@ fn existing_helper() {
   let value = existing_helper();
-  old_name();
+  clamp_to_window(value);
+  let retryBudget = 3;
+  plain words only
diff --git a/src/fresh_module.rs b/src/fresh_module.rs
new file mode 100644
--- /dev/null
+++ b/src/fresh_module.rs
@@ -0,0 +1 @@
+pub fn make_it() {}
`;

test('the lint warns on an identifier or a path only the gold change adds', () => {
  const statement = 'The caller needs clamp_to_window and a retryBudget. Add src/fresh_module.rs. Keep existing_helper and plain words.';
  const w = lintStatement(statement, GOLD);
  assert.deepEqual(w.map((x) => `${x.kind}:${x.token}`), ['identifier:clamp_to_window', 'identifier:fresh_module', 'identifier:retryBudget', 'path:src/fresh_module.rs']);
});

test('what the diff also shows as context or removed is not a giveaway; plain words never are', () => {
  const w = lintStatement('Use existing_helper and old_name here, in src/limits.rs. Words like only and plain.', GOLD);
  assert.deepEqual(w, []);
});

test('an Interface section is exempt, the rest of the statement is not', () => {
  const statement = '# Task\nFix it.\n\n## Interface\nThe tests call `clamp_to_window(value)` and `make_it`.\n\n## Notes\nNo giveaways here.\n';
  assert.deepEqual(lintStatement(statement, GOLD), []);
  assert.equal(withoutInterface(statement).includes('clamp_to_window'), false);
  assert.equal(withoutInterface(statement).includes('No giveaways'), true, 'the section ends at the next heading of its level');
  assert.equal(lintStatement('Please add clamp_to_window.\n\n## Interface\nnothing\n', GOLD).length, 1);
});

test('a token inside a longer name is not a match', () => {
  assert.deepEqual(lintStatement('see my_clamp_to_window_x', GOLD), []);
});

test('splitDiff separates added lines from the rest', () => {
  const { added, other } = splitDiff(GOLD);
  assert.ok(added.includes('clamp_to_window(value);'));
  assert.ok(other.includes('old_name();'));
  assert.ok(!other.includes('clamp_to_window'));
});

const pool = () => {
  const out = [];
  for (const [repo, n] of [['a', 20], ['b', 20], ['c', 3], ['d', 12]]) for (let i = 0; i < n; i++) out.push({ id: `${repo}-${String(i).padStart(2, '0')}`, repo });
  return out;
};
const ids = (l) => l.map((x) => x.id);

test('the same seed gives the same list, another seed another', () => {
  const o = { seed: 'one', total: 20, perRepoCap: 9 };
  assert.deepEqual(ids(selectTasks(pool(), o)), ids(selectTasks(pool(), o)));
  assert.deepEqual(ids(selectTasks([...pool()].reverse(), o)), ids(selectTasks(pool(), o)), 'the order of the input does not matter');
  assert.notDeepEqual(ids(selectTasks(pool(), o)), ids(selectTasks(pool(), { ...o, seed: 'two' })));
  assert.notDeepEqual(ids(selectTasks(pool(), o)), ids(selectTasks(pool(), { ...o, scope: 'own' })), 'the scope is part of the seed');
});

test('shares are equal and capped; a small stratum gives its room to the others', () => {
  const chosen = selectTasks(pool(), { seed: 's', total: 30, perRepoCap: 9 });
  assert.equal(chosen.length, 30);
  const per = {};
  for (const c of chosen) per[c.repo] = (per[c.repo] ?? 0) + 1;
  assert.equal(per.c, 3, 'c has only 3');
  assert.ok(per.a <= 9 && per.b <= 9 && per.d <= 9);
  assert.equal(per.a + per.b + per.c + per.d, 30);
  assert.deepEqual(per, { a: 9, b: 9, c: 3, d: 9 });
  const fewer = selectTasks(pool(), { seed: 's', total: 12, perRepoCap: 9 });
  const p2 = {};
  for (const c of fewer) p2[c.repo] = (p2[c.repo] ?? 0) + 1;
  assert.deepEqual(Object.values(p2).sort(), [3, 3, 3, 3]);
});

test('a total above what the caps allow selects what they allow, and a per-repository cap overrides', () => {
  assert.equal(selectTasks(pool(), { seed: 1, total: 500, perRepoCap: 6 }).length, 6 + 6 + 3 + 6);
  const chosen = selectTasks(pool(), { seed: 1, total: 500, perRepoCap: 6, repoCaps: { a: 2 } });
  assert.equal(chosen.filter((c) => c.repo === 'a').length, 2);
  assert.equal(selectTasks([], { seed: 1, total: 5, perRepoCap: 6 }).length, 0);
});

test('the output is sorted by repository and id and holds no duplicates', () => {
  const chosen = selectTasks(pool(), { seed: 9, total: 40, perRepoCap: 9 });
  assert.deepEqual(ids(chosen), [...ids(chosen)].sort());
  assert.equal(new Set(ids(chosen)).size, chosen.length);
});

test('bad options are refused', () => {
  assert.throws(() => selectTasks([], { seed: 1, total: -1, perRepoCap: 1 }), RangeError);
  assert.throws(() => selectTasks([], { seed: 1, total: 1, perRepoCap: 0 }), RangeError);
});

test('the lock holds ids, checksums, image digests and the seed, nothing else', () => {
  const lock = buildLock({ source: 'public', seed: 7, upstreamCommit: 'c'.repeat(40), tasks: [{ id: 'x', repo: 'r', checksum: 'h', imageDigest: 'sha256:d', instruction: 'SECRET TEXT', dir: '/somewhere' }] });
  assert.deepEqual(lock, { schema: 1, source: 'public', seed: '7', upstreamCommit: 'c'.repeat(40), tasks: [{ id: 'x', repo: 'r', checksum: 'h', image: 'sha256:d' }] });
  assert.equal(lockText(lock).includes('SECRET TEXT'), false);
  assert.equal(lockText(lock).includes('/somewhere'), false);
});
