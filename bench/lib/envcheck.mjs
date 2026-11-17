// The environment check: a task is kept only if its gold patch passes three times and the empty patch
// fails three times, run from the clone path the agents will use (adam's worktree and OpenCode's clone
// are never at the image's /app, so a task whose tests only work at /app would favour neither fairly).
// No model is involved and nothing is spent.
import os from 'node:os';
import { GRADE_ATTEMPTS, gradeAttempt } from './grade.mjs';

export const CLONE_PATH = '/work/workspaces/bench/slot';

/** The image of a task by digest: a tag-named image is pulled and its digest read. Returns { task, pin }. */
export async function pinTask(containers, task) {
  if (task.image.digest) return { task, pin: null };
  await containers.pull(task.image.ref);
  const digest = await containers.digestOf(task.image.ref);
  const pinned = { ...task, image: { ...task.image, digest, ref: `${task.image.name}@${digest}` } };
  return { task: pinned, pin: { ref: task.image.ref, digest } };
}

async function runSeries(opts, patchFile) {
  const out = [];
  for (let i = 0; i < GRADE_ATTEMPTS; i++) out.push(await gradeAttempt({ ...opts, patchFile, cloneTo: CLONE_PATH }));
  return out;
}

const count = (series, f) => series.filter(f).length;
const broken = (series) => series.some((a) => a.invalid);

/**
 * Check one task. Returns { id, ok, reason, devcontainer, imageDigest, pin }. `reason` is one of
 * no-gold-patch, infra, gold-does-not-apply, gold-fails, gold-flaky, gold-fails-at-app, empty-passes, empty-flaky.
 * The gold also runs once through the real grading path (the working directory /app, the original test.sh).
 */
export async function checkTask({ containers, task: given, workRoot = os.tmpdir(), timeoutMs }) {
  const result = (extra) => ({ id: given.id, ok: false, reason: null, devcontainer: null, imageDigest: null, pin: null, ...extra });
  let pinned;
  try {
    pinned = await pinTask(containers, given);
  } catch (e) {
    return result({ reason: 'infra', detail: e.message });
  }
  const { task, pin } = pinned;
  const base = { imageDigest: task.image.digest, pin };
  if (!task.goldPatch) return result({ ...base, reason: 'no-gold-patch' });

  const probe = await containers.run({
    image: task.image.ref,
    script: `if [ -e ${task.workdir}/.devcontainer ] || [ -e ${task.workdir}/.devcontainer.json ]; then echo devcontainer=yes; else echo devcontainer=no; fi`,
    workdir: '/',
    network: 'none',
    timeoutMs: 120000,
  });
  if (probe.exitCode !== 0 || !/devcontainer=(yes|no)/.test(probe.stdout)) return result({ ...base, reason: 'infra', detail: 'the probe did not run' });
  const devcontainer = probe.stdout.includes('devcontainer=yes');

  const opts = { containers, task, workRoot, timeoutMs };
  const gold = await runSeries(opts, task.goldPatch);
  if (broken(gold)) return result({ ...base, devcontainer, reason: 'infra' });
  const goldPasses = count(gold, (a) => a.pass);
  if (goldPasses !== GRADE_ATTEMPTS) {
    const reason = goldPasses > 0 ? 'gold-flaky' : count(gold, (a) => a.error === 'apply') > 0 ? 'gold-does-not-apply' : 'gold-fails';
    return result({ ...base, devcontainer, reason });
  }
  const real = await gradeAttempt({ ...opts, patchFile: task.goldPatch });
  if (real.invalid) return result({ ...base, devcontainer, reason: 'infra' });
  if (!real.pass) return result({ ...base, devcontainer, reason: 'gold-fails-at-app' });
  const empty = await runSeries(opts, null);
  if (broken(empty)) return result({ ...base, devcontainer, reason: 'infra' });
  const emptyPasses = count(empty, (a) => a.pass);
  if (emptyPasses !== 0) return result({ ...base, devcontainer, reason: emptyPasses === GRADE_ATTEMPTS ? 'empty-passes' : 'empty-flaky' });
  return result({ ...base, ok: true, devcontainer });
}
