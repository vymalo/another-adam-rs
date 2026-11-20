// The environment check: a task is kept when its gold patch passes three times and the empty patch fails three times at
// /app, the working directory of the image and the path the task's own tooling assumes (the grading of an agent's patch
// runs there too, in a fresh container). Whether the gold also passes at the agents' working path (the clone path, on a copy
// of the image's tree) is recorded as the task's `agentPath` attribute and never drops a task: a task's own parser may
// assume /app (NodeBB's `/tests/parser.py` strips only a literal `/app/` prefix), which says nothing about its fairness,
// since both arms work at the same agent path. No model is involved and nothing is spent.
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

const runSeries = async (opts, patchFile) => {
  const out = [];
  for (let i = 0; i < GRADE_ATTEMPTS; i++) out.push(await gradeAttempt({ ...opts, patchFile }));
  return out;
};

const count = (series, f) => series.filter(f).length;
const broken = (series) => series.some((a) => a.invalid);

/**
 * The one gold run at the clone path, for the attribute: 'pass', 'fail' (with why: `tests-fail`, or the grade's error such
 * as `apply`) or 'not-run' (an infrastructure error or a timeout that stayed: it says nothing about the task).
 */
async function probeAgentPath(opts, patchFile) {
  const a = await gradeAttempt({ ...opts, patchFile, cloneTo: CLONE_PATH });
  if (a.invalid) return { agentPath: 'not-run', agentPathReason: a.error ?? 'infra' };
  return a.pass ? { agentPath: 'pass', agentPathReason: null } : { agentPath: 'fail', agentPathReason: a.error ?? 'tests-fail' };
}

/**
 * Check one task. Returns { id, ok, reason, devcontainer, imageDigest, pin, agentPath, agentPathReason }. `reason` is one of
 * no-gold-patch, infra, gold-does-not-apply, gold-fails, gold-flaky, empty-passes, empty-flaky: all from the runs at /app,
 * the real grading path (the original test.sh in the image's working directory). A kept task then runs its gold once at the
 * clone path; a dropped one has agentPath `not-run` (reason `dropped`), and so has one with no run (`infra`).
 */
export async function checkTask({ containers, task: given, workRoot = os.tmpdir(), timeoutMs, io }) {
  const result = (extra) => ({ id: given.id, ok: false, reason: null, devcontainer: null, imageDigest: null, pin: null, agentPath: 'not-run', agentPathReason: 'dropped', ...extra });
  let pinned;
  try {
    pinned = await pinTask(containers, given);
  } catch (e) {
    return result({ reason: 'infra', agentPathReason: 'infra', detail: e.message });
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
  if (probe.exitCode !== 0 || !/devcontainer=(yes|no)/.test(probe.stdout)) return result({ ...base, reason: 'infra', agentPathReason: 'infra', detail: 'the probe did not run' });
  const devcontainer = probe.stdout.includes('devcontainer=yes');
  const infra = () => result({ ...base, devcontainer, reason: 'infra', agentPathReason: 'infra' });

  const opts = { containers, task, workRoot, timeoutMs, io };
  const gold = await runSeries(opts, task.goldPatch);
  if (broken(gold)) return infra();
  const goldPasses = count(gold, (a) => a.pass);
  if (goldPasses !== GRADE_ATTEMPTS) {
    const reason = goldPasses > 0 ? 'gold-flaky' : count(gold, (a) => a.error === 'apply') > 0 ? 'gold-does-not-apply' : 'gold-fails';
    return result({ ...base, devcontainer, reason });
  }
  const empty = await runSeries(opts, null);
  if (broken(empty)) return infra();
  const emptyPasses = count(empty, (a) => a.pass);
  if (emptyPasses !== 0) return result({ ...base, devcontainer, reason: emptyPasses === GRADE_ATTEMPTS ? 'empty-passes' : 'empty-flaky' });
  return result({ ...base, ok: true, devcontainer, ...(await probeAgentPath(opts, task.goldPatch)) });
}
