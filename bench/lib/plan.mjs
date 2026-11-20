// The plan of a run: task blocks, in a seeded and balanced order.
//
// A block is one task through every cell (an agent in an arm). Runs go in blocks so that a stop at the cap leaves
// complete sets, and the pilot is the first blocks of the same order, so its runs are the full run's first runs.
//   * the tasks are shuffled with a seeded generator;
//   * the cells of a block follow a cyclic Latin square: each round of `cells.length` blocks starts from one seeded
//     permutation of the cells and rotates it by one for each block, so in a round every cell stands at every position
//     exactly once (no cell is always the first to meet a cold cache or the end of a long day).
import crypto from 'node:crypto';
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

/**
 * The run id of a cell of a task, unique by construction: the task part is the id as it is when it is plain and short enough,
 * else the sanitised id, cut to fit, with a short hash of the whole id; `.agent.arm` is never cut. A pilot run has the prefix `pilot.`.
 */
export function runId(taskId, agent, arm, { prefix = '' } = {}) {
  const id = String(taskId);
  const suffix = `.${agent}.${arm}`;
  const room = 128 - prefix.length - suffix.length;
  const plain = id.replace(SAFE, '-');
  if (plain === id && id.length <= room) return `${prefix}${id}${suffix}`;
  const hash = crypto.createHash('sha256').update(id).digest('hex').slice(0, 8);
  return `${prefix}${plain.slice(0, room - hash.length - 1)}-${hash}${suffix}`;
}

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

/**
 * The runs of the blocks, in the order they are started. `pilot` marks the pilot's runs (phase `pilot`, ids prefixed `pilot.`): the
 * full run runs the same tasks again under the confirmed budget, and the pilot's results are never in the rates.
 */
export function specsOf(blocks, { selfRepo = null, pilot = false } = {}) {
  const specs = blocks.flatMap((b) =>
    b.cells.map(({ agent, arm }) => ({
      run: runId(b.task.id, agent, arm, { prefix: pilot ? 'pilot.' : '' }),
      task: b.task,
      agent,
      arm,
      block: b.index,
      phase: pilot ? 'pilot' : 'run',
      source: sourceOf(b.task, selfRepo),
      language: b.task.language ?? null,
      agentPath: b.task.agentPath ?? null,
    })),
  );
  const seen = new Set();
  for (const s of specs) {
    if (seen.has(s.run)) throw new Error(`two runs would have the same run id (${s.run}): the task ids collide`);
    seen.add(s.run);
  }
  return specs;
}
