// The plan of a run: task blocks, in a seeded and balanced order.
//
// A block is one task through every cell (an agent in an arm). Runs go in blocks so that a stop at the cap leaves
// complete sets, and the pilot is the first blocks of the same order, so its runs are the full run's first runs.
//   * the tasks are shuffled with a seeded generator;
//   * the cells of a block follow a cyclic Latin square: each round of `cells.length` blocks starts from one seeded
//     permutation of the cells and rotates it by one for each block, so in a round every cell stands at every position
//     exactly once (no cell is always the first to meet a cold cache or the end of a long day).
import { mulberry32, seedFrom } from './stats.mjs';

const SAFE = /[^A-Za-z0-9._-]/g;
const byText = (a, b) => (a < b ? -1 : a > b ? 1 : 0);

function shuffle(items, rand) {
  const a = [...items];
  for (let i = a.length - 1; i > 0; i--) {
    const j = Math.floor(rand() * (i + 1));
    [a[i], a[j]] = [a[j], a[i]];
  }
  return a;
}

/** The run id of a cell of a task: stable, plain, and one per task, agent and arm. */
export const runId = (taskId, agent, arm) => `${String(taskId).replace(SAFE, '-')}.${agent}.${arm}`.slice(0, 128);

/** Public tasks are `public`; own tasks of adam-rs's own repository are `own-self`; the other own tasks are `own`. */
export function sourceOf(task, selfRepo) {
  if (task.source === 'own' && selfRepo && task.repo === selfRepo) return 'own-self';
  return task.source ?? 'public';
}

/**
 * @param {object[]} tasks   loaded tasks ({ id, source, repo, language })
 * @param {string[]} agents  e.g. ['adam', 'opencode']
 * @param {string[]} arms    the arm names of the configuration
 * @returns {{ index: number, task: object, cells: {agent: string, arm: string}[] }[]}
 */
export function buildBlocks({ tasks, agents, arms, seed }) {
  const cells = agents.flatMap((agent) => arms.map((arm) => ({ agent, arm }))).sort((x, y) => byText(`${x.agent}/${x.arm}`, `${y.agent}/${y.arm}`));
  const n = cells.length;
  const order = shuffle([...tasks].sort((a, b) => byText(a.id, b.id)), mulberry32(seedFrom(`${seed}:blocks:tasks`)));
  const rounds = [];
  return order.map((task, index) => {
    const round = Math.floor(index / n);
    rounds[round] ??= shuffle(cells, mulberry32(seedFrom(`${seed}:blocks:round:${round}`)));
    const shift = index % n;
    return { index, task, cells: rounds[round].map((_, i) => rounds[round][(i + shift) % n]) };
  });
}

/** The runs of the blocks, in the order they are started. */
export function specsOf(blocks, { selfRepo = null } = {}) {
  return blocks.flatMap((b) =>
    b.cells.map(({ agent, arm }) => ({ run: runId(b.task.id, agent, arm), task: b.task, agent, arm, block: b.index, source: sourceOf(b.task, selfRepo), language: b.task.language ?? null })),
  );
}
