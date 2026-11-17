import assert from 'node:assert/strict';
import { test } from 'node:test';
import { buildBlocks, runId, sourceOf, specsOf } from '../lib/plan.mjs';

const tasks = (n, extra = {}) => Array.from({ length: n }, (_, i) => ({ id: `task-${String(i + 1).padStart(2, '0')}`, source: 'public', repo: 'r', language: 'go', ...extra }));
const AGENTS = ['adam', 'opencode'];
const ARMS = ['max', 'off'];
const cellName = (c) => `${c.agent}/${c.arm}`;

test('the same seed gives the same blocks, another seed another order', () => {
  const a = buildBlocks({ tasks: tasks(12), agents: AGENTS, arms: ARMS, seed: '7' });
  const b = buildBlocks({ tasks: [...tasks(12)].reverse(), agents: AGENTS, arms: ARMS, seed: '7' });
  assert.deepEqual(a.map((x) => [x.task.id, x.cells.map(cellName)]), b.map((x) => [x.task.id, x.cells.map(cellName)]), 'the input order does not matter');
  const c = buildBlocks({ tasks: tasks(12), agents: AGENTS, arms: ARMS, seed: '8' });
  assert.notDeepEqual(a.map((x) => x.task.id), c.map((x) => x.task.id));
});

test('every block holds every cell once, and every task is in exactly one block', () => {
  const blocks = buildBlocks({ tasks: tasks(9), agents: AGENTS, arms: ARMS, seed: '1' });
  assert.equal(blocks.length, 9);
  assert.deepEqual(blocks.map((b) => b.index), [...Array(9).keys()]);
  for (const b of blocks) assert.deepEqual(b.cells.map(cellName).sort(), ['adam/max', 'adam/off', 'opencode/max', 'opencode/off']);
  assert.deepEqual(blocks.map((b) => b.task.id).sort(), tasks(9).map((t) => t.id));
});

test('balanced: in each round of four blocks every cell stands at every position exactly once', () => {
  const blocks = buildBlocks({ tasks: tasks(12), agents: AGENTS, arms: ARMS, seed: '3' });
  for (let round = 0; round < 3; round++) {
    const chunk = blocks.slice(round * 4, round * 4 + 4);
    for (let pos = 0; pos < 4; pos++) {
      assert.deepEqual(chunk.map((b) => cellName(b.cells[pos])).sort(), ['adam/max', 'adam/off', 'opencode/max', 'opencode/off'], `round ${round} position ${pos}`);
    }
  }
  // so, over the whole run, each cell is first the same number of times
  const firsts = {};
  for (const b of blocks) firsts[cellName(b.cells[0])] = (firsts[cellName(b.cells[0])] ?? 0) + 1;
  assert.deepEqual(Object.values(firsts), [3, 3, 3, 3]);
});

test('a partial last round is still balanced to within one', () => {
  const blocks = buildBlocks({ tasks: tasks(10), agents: AGENTS, arms: ARMS, seed: '3' });
  const firsts = {};
  for (const b of blocks) firsts[cellName(b.cells[0])] = (firsts[cellName(b.cells[0])] ?? 0) + 1;
  assert.ok(Math.max(...Object.values(firsts)) - Math.min(...Object.values(firsts)) <= 1);
});

test('specs follow the blocks, with a plain unique run id and the block, source and language', () => {
  const blocks = buildBlocks({ tasks: tasks(3), agents: AGENTS, arms: ARMS, seed: '1' });
  const specs = specsOf(blocks);
  assert.equal(specs.length, 12);
  assert.equal(new Set(specs.map((s) => s.run)).size, 12);
  assert.deepEqual(specs.slice(0, 4).map((s) => s.block), [0, 0, 0, 0]);
  assert.ok(specs.every((s) => /^[A-Za-z0-9._:-]{1,128}$/.test(s.run)));
  assert.equal(specs[0].run, runId(blocks[0].task.id, blocks[0].cells[0].agent, blocks[0].cells[0].arm));
  assert.equal(runId('a b/c', 'adam', 'max'), 'a-b-c.adam.max');
  assert.equal(specs[0].language, 'go');
});

test('adam-rs\'s own tasks are told apart from the other own tasks and from the public ones', () => {
  assert.equal(sourceOf({ source: 'public', repo: 'vymalo/another-adam-rs' }, 'vymalo/another-adam-rs'), 'public');
  assert.equal(sourceOf({ source: 'own', repo: 'vymalo/another-adam-rs' }, 'vymalo/another-adam-rs'), 'own-self');
  assert.equal(sourceOf({ source: 'own', repo: 'vymalo/other' }, 'vymalo/another-adam-rs'), 'own');
  assert.equal(sourceOf({ source: 'own', repo: 'vymalo/another-adam-rs' }, null), 'own');
  const specs = specsOf(buildBlocks({ tasks: [{ id: 't1', source: 'own', repo: 'x/self', language: 'rust' }], agents: ['adam'], arms: ['max'], seed: '1' }), { selfRepo: 'x/self' });
  assert.equal(specs[0].source, 'own-self');
});

test('the pilot is a prefix: the first blocks of the plan, so its runs are the full run\'s first runs', () => {
  const blocks = buildBlocks({ tasks: tasks(8), agents: AGENTS, arms: ARMS, seed: '5' });
  const all = specsOf(blocks).map((s) => s.run);
  const pilot = specsOf(blocks.slice(0, 3)).map((s) => s.run);
  assert.deepEqual(all.slice(0, pilot.length), pilot);
});
