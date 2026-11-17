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

/** The lock file body: ids, checksums, image digests and the seed, and what pins the source. Nothing else. */
export function buildLock({ source, seed, upstreamCommit = null, tasks }) {
  const lock = { schema: 1, source, seed: String(seed) };
  if (upstreamCommit) lock.upstreamCommit = upstreamCommit;
  lock.tasks = tasks.map((t) => ({ id: t.id, repo: t.repo, checksum: t.checksum, image: t.imageDigest ?? null }));
  return lock;
}

export const lockText = (lock) => `${JSON.stringify(lock, null, 2)}\n`;
