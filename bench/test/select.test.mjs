import assert from 'node:assert/strict';
import { test } from 'node:test';
import { lintStatement, splitDiff, withoutInterface } from '../lib/lint.mjs';
import { Quota, buildLock, lockText, pickInOrder, selectTasks, selectionOrder } from '../lib/select.mjs';

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

test('the lock holds ids, checksums, image digests, the agent-path attribute and the seed, nothing else', () => {
  const lock = buildLock({ source: 'public', seed: 7, upstreamCommit: 'c'.repeat(40), tasks: [{ id: 'x', repo: 'r', checksum: 'h', imageDigest: 'sha256:d', agentPath: 'fail', instruction: 'SECRET TEXT', dir: '/somewhere' }] });
  assert.deepEqual(lock, { schema: 1, source: 'public', seed: '7', upstreamCommit: 'c'.repeat(40), tasks: [{ id: 'x', repo: 'r', checksum: 'h', image: 'sha256:d', agentPath: 'fail' }] });
  assert.equal(lockText(lock).includes('SECRET TEXT'), false);
  assert.equal(lockText(lock).includes('/somewhere'), false);
});

// ---- the selection order, shared by `tasks check` and `tasks select` (public source) ----

const bigPool = (repos = 14, each = 12) => {
  const out = [];
  for (let r = 0; r < repos; r++) for (let i = 0; i < each; i++) out.push({ id: `r${String(r).padStart(2, '0')}-${String(i).padStart(2, '0')}`, repo: `r${String(r).padStart(2, '0')}` });
  return out;
};

test('the selection order holds every task once, does not depend on the input order and is seeded', () => {
  const o = { seed: 'one', scope: 'public' };
  const order = selectionOrder(bigPool(), o);
  assert.equal(order.length, 14 * 12);
  assert.equal(new Set(ids(order)).size, order.length);
  assert.deepEqual(ids(selectionOrder([...bigPool()].reverse(), o)), ids(order));
  assert.notDeepEqual(ids(selectionOrder(bigPool(), { ...o, seed: 'two' })), ids(order));
  assert.notDeepEqual(ids(selectionOrder(bigPool(), { ...o, scope: 'own' })), ids(order), 'the scope is part of the seed');
});

test('the selection order is stratified: the first round of it takes one task of every repository', () => {
  const order = selectionOrder(bigPool(5, 6), { seed: 3, scope: 'public' });
  assert.deepEqual(new Set(order.slice(0, 5).map((t) => t.repo)).size, 5);
  assert.deepEqual(new Set(order.slice(5, 10).map((t) => t.repo)).size, 5);
});

test('a quota stops at the total, skips a full repository and says when it is done', () => {
  const q = new Quota({ total: 3, perRepo: 2 });
  assert.deepEqual([q.open('a'), q.done], [true, false]);
  q.take('a');
  q.take('a');
  assert.equal(q.open('a'), false, 'a repository at its cap is skipped');
  assert.equal(q.open('b'), true);
  q.take('b');
  assert.deepEqual([q.kept, q.done, q.open('c')], [3, true, false]);
  assert.throws(() => new Quota({ total: 1, perRepo: 0 }), RangeError);
});

test('property: a walk that stops at the target picks exactly what a full check would, for any seed and any verdicts', () => {
  const rows = bigPool();
  for (let seed = 1; seed <= 40; seed++) {
    // verdicts: a seed-dependent mix, some repositories nearly all dropped
    const keptSet = new Set(rows.filter((t, i) => (i * 7 + seed * 13 + Number(t.repo.slice(1)) * 3) % (3 + (Number(t.repo.slice(1)) % 4)) !== 0).map((t) => t.id));
    for (const total of [1, 10, 40, 80, 500]) {
      const opts = { total, perRepo: 9 };
      const order = selectionOrder(rows, { seed, scope: 'public' });
      const full = pickInOrder(order, (t) => keptSet.has(t.id), opts).map((t) => t.id);
      // the partial walk: only the tasks it needs are checked
      const q = new Quota(opts);
      const checked = new Set();
      for (const t of order) {
        if (q.done) break;
        if (!q.open(t.repo)) continue;
        checked.add(t.id);
        if (keptSet.has(t.id)) q.take(t.repo);
      }
      const partial = pickInOrder(order, (t) => checked.has(t.id) && keptSet.has(t.id), opts).map((t) => t.id);
      assert.deepEqual(partial, full, `seed ${seed}, total ${total}`);
      assert.ok(full.length <= total);
      const per = {};
      for (const id of full) per[id.slice(0, 3)] = (per[id.slice(0, 3)] ?? 0) + 1;
      assert.ok(Object.values(per).every((n) => n <= 9), 'at most 9 per repository');
      if (total === 40 || total === 80) assert.ok(checked.size < rows.length, 'it did not walk every task');
    }
  }
});

test('pickInOrder takes the first kept ones in order, capped per repository', () => {
  const order = [{ id: '1', repo: 'a' }, { id: '2', repo: 'a' }, { id: '3', repo: 'b' }, { id: '4', repo: 'a' }, { id: '5', repo: 'b' }];
  assert.deepEqual(ids(pickInOrder(order, () => true, { total: 4, perRepo: 2 })), ['1', '2', '3', '5']);
  assert.deepEqual(ids(pickInOrder(order, (t) => t.id !== '2', { total: 10, perRepo: 2 })), ['1', '3', '4', '5']);
});
