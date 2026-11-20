// Seeded, stratified selection of tasks, and the lock files.
//
// Strata are repositories. Every stratum gets an equal share of `total`, capped at `perRepoCap` (and at its
// own size and at `repoCaps[repo]`); what a capped stratum cannot take goes to the others, one task at a
// time, in a seeded order. Inside a stratum a seeded shuffle of the sorted ids picks the tasks. The same
// seed gives the same list; another seed, in general, another.
import { mulberry32, seedFrom } from './stats.mjs';

function shuffle(items, rand) {
  const a = [...items];
  for (let i = a.length - 1; i > 0; i--) {
    const j = Math.floor(rand() * (i + 1));
    [a[i], a[j]] = [a[j], a[i]];
  }
  return a;
}

/**
 * candidates: [{ id, repo, ... }]. Returns the chosen ones sorted by repo then id.
 * Options: seed (string or integer), scope (a label mixed into the seed), total, perRepoCap, repoCaps.
 */
export function selectTasks(candidates, { seed, scope = 'tasks', total, perRepoCap, repoCaps = {} }) {
  if (!Number.isInteger(total) || total < 0) throw new RangeError('total must be a whole number');
  if (!Number.isInteger(perRepoCap) || perRepoCap < 1) throw new RangeError('perRepoCap must be a whole number above 0');
  const rand = mulberry32(seedFrom(`${seed}:${scope}`));
  const strata = new Map();
  for (const c of [...candidates].sort((x, y) => (x.id < y.id ? -1 : x.id > y.id ? 1 : 0))) {
    if (!strata.has(c.repo)) strata.set(c.repo, []);
    strata.get(c.repo).push(c);
  }
  const repos = [...strata.keys()].sort();
  const room = Object.fromEntries(repos.map((r) => [r, Math.min(strata.get(r).length, perRepoCap, repoCaps[r] ?? Infinity)]));
  const quota = Object.fromEntries(repos.map((r) => [r, 0]));
  let left = Math.min(total, repos.reduce((s, r) => s + room[r], 0));
  while (left > 0) {
    const open = repos.filter((r) => quota[r] < room[r]);
    const order = shuffle(open, rand);
    for (const r of order) {
      if (left === 0) break;
      quota[r]++;
      left--;
    }
  }
  const chosen = [];
  for (const r of repos) chosen.push(...shuffle(strata.get(r), rand).slice(0, quota[r]));
  return chosen.sort((x, y) => (x.repo < y.repo ? -1 : x.repo > y.repo ? 1 : x.id < y.id ? -1 : x.id > y.id ? 1 : 0));
}

/**
 * The order in which the public tasks are walked, by `tasks check` and by `tasks select` alike: every task once, a function of
 * the tasks' ids and repositories, the seed and the scope alone (never of a verdict). A seeded shuffle inside each repository,
 * then rounds: each round takes the next task of every repository that still has one, the repositories in a seeded order.
 */
export function selectionOrder(tasks, { seed, scope = 'public' }) {
  const rand = mulberry32(seedFrom(`${seed}:${scope}`));
  const strata = new Map();
  for (const c of [...tasks].sort((x, y) => (x.id < y.id ? -1 : x.id > y.id ? 1 : 0))) {
    if (!strata.has(c.repo)) strata.set(c.repo, []);
    strata.get(c.repo).push(c);
  }
  const queues = new Map([...strata.keys()].sort().map((r) => [r, shuffle(strata.get(r), rand)]));
  const order = [];
  for (let live = [...queues.keys()]; live.length > 0; live = live.filter((r) => queues.get(r).length > 0)) {
    for (const r of shuffle(live, rand)) order.push(queues.get(r).shift());
  }
  return order;
}

/** What is still wanted from a walk: `total` kept in all, at most `perRepo` of each repository. */
export class Quota {
  #perRepo;
  #total;
  #byRepo = new Map();
  kept = 0;
  constructor({ total, perRepo }) {
    if (!Number.isInteger(total) || total < 0) throw new RangeError('total must be a whole number');
    if (!Number.isInteger(perRepo) || perRepo < 1) throw new RangeError('perRepo must be a whole number above 0');
    this.#total = total;
    this.#perRepo = perRepo;
  }
  get done() {
    return this.kept >= this.#total;
  }
  /** Is a task of this repository still worth a check? */
  open(repo) {
    return !this.done && (this.#byRepo.get(repo) ?? 0) < this.#perRepo;
  }
  take(repo) {
    this.#byRepo.set(repo, (this.#byRepo.get(repo) ?? 0) + 1);
    this.kept++;
  }
}

/** The tasks of `order` that `isKept` accepts, the first ones that fit the quota: what a full check selects. */
export function pickInOrder(order, isKept, opts) {
  const q = new Quota(opts);
  const chosen = [];
  for (const t of order) {
    if (q.done) break;
    if (q.open(t.repo) && isKept(t)) {
      q.take(t.repo);
      chosen.push(t);
    }
  }
  return chosen;
}

/** The lock file body: ids, checksums, image digests, each task's agent-path attribute and the seed, and what pins the source. Nothing else. */
export function buildLock({ source, seed, upstreamCommit = null, tasks }) {
  const lock = { schema: 1, source, seed: String(seed) };
  if (upstreamCommit) lock.upstreamCommit = upstreamCommit;
  lock.tasks = tasks.map((t) => ({ id: t.id, repo: t.repo, checksum: t.checksum, image: t.imageDigest ?? null, agentPath: t.agentPath ?? 'not-run' }));
  return lock;
}

export const lockText = (lock) => `${JSON.stringify(lock, null, 2)}\n`;
