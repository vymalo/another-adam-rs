// What the two drivers share: the agent interface, finding the run's tree, copying it out of the Podman
// service, keeping raw output, and a patch from the final tree.
//
// An agent driver is an object with
//   name                      'adam' | 'opencode'
//   prepare(ctx)              the base is in place, the agent can be started; throws on infrastructure trouble
//   run(ctx)                  runs the agent to its end; returns { exit, wallMs, selfReported, ... }
//   collect(ctx)              the final tree as a patch: { patch: Buffer, ... }
//   cleanup(ctx)              always called; removes what the run left in the stack
// ctx: { run, attempt, slot, task, base, stack, containers, meter: { token, baseUrl, refusal() }, paths: { run, in, out },
//        limitMs, log }
import fs from 'node:fs';
import path from 'node:path';
import { redact } from '../stack.mjs';
import { collectPatch } from '../grade.mjs';

const SAFE_PATH = /^\/[A-Za-z0-9._/@+=-]*$/;

/** The shell that copies a tree (never its top-level `.git`) from `tree` to `out`, readable by everyone. Pure. */
export function copyScript({ tree = '/tree', out = '/out' } = {}) {
  for (const p of [tree, out]) if (!SAFE_PATH.test(p)) throw new Error('a path in the copy script is not a plain absolute path');
  return `set -eu\ncd ${tree}\ntar --exclude=./.git -cf - . | tar -xf - -C ${out}\nchmod -R a+rX ${out}`;
}

/** Copy a tree out of the Podman service's volume into `outDir` (a directory the harness shares with the service). */
export async function copyTreeOut({ containers, image, treePath, outDir, timeoutMs = 10 * 60 * 1000 }) {
  fs.rmSync(outDir, { recursive: true, force: true });
  fs.mkdirSync(outDir, { recursive: true });
  fs.chmodSync(outDir, 0o777);
  const r = await containers.run({
    image,
    script: copyScript(),
    workdir: '/',
    mounts: [
      { source: treePath, target: '/tree', readOnly: true },
      { source: outDir, target: '/out' },
    ],
    network: 'none',
    timeoutMs,
  });
  if (r.exitCode !== 0) throw new Error(`the final tree could not be copied out (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-2).join(' | ')}`);
}

/** The patch of the tree in `treeDir` against the base. The agent's `.git` was never copied and nothing in the tree is run. */
export async function patchOfTree({ treeDir, base, workRoot }) {
  return collectPatch({ tree: treeDir, baseRepo: base.dir, base: base.sha, linkPrefix: base.workdir, workRoot });
}

export class WorktreeError extends Error {
  constructor(code, message) {
    super(message);
    this.name = 'WorktreeError';
    this.code = code;
  }
}

/**
 * The run's worktree, `<workspaces>/<run>/<slot>`, from what the volume holds. The volume is emptied before each
 * run, so there is one run directory; and one slot, or the one named after the repository. Pure.
 * @param {string[]} runs         the entries of /work/workspaces
 * @param {(run: string) => string[]} slotsOf  the entries of /work/workspaces/<run>
 */
export function locateWorktree({ runs, slotsOf, slug, root = '/work/workspaces' }) {
  if (runs.length === 0) throw new WorktreeError('no-worktree', 'the coder made no workspace');
  if (runs.length > 1) throw new WorktreeError('ambiguous', `the volume holds ${runs.length} runs: it was not emptied`);
  const run = runs[0];
  const slots = slotsOf(run);
  if (slots.length === 0) throw new WorktreeError('no-worktree', 'the run has no worktree');
  const named = slots.filter((s) => s === slug || s.startsWith(slug) || slug.startsWith(s));
  const pick = slots.length === 1 ? slots : named;
  if (pick.length !== 1) throw new WorktreeError('ambiguous', `the run has ${slots.length} slots and ${pick.length} of them are the task's repository`);
  return { run, slot: pick[0], path: `${root}/${run}/${pick[0]}` };
}

/**
 * Is a pushed `agent/*` branch the one this run made? The coder names its branch `agent/<the first characters of
 * the run id>`, so the branch's tail must be a prefix of the run directory's name. Returns
 * { branch, sha, consistent } or null when nothing was pushed.
 */
export function crossCheckBranch({ heads, runDir }) {
  const mine = heads.filter((h) => h.ref.startsWith('agent/'));
  if (mine.length === 0) return null;
  const ours = mine.find((h) => runDir.startsWith(h.ref.slice('agent/'.length).replace(/-[0-9a-f]{8}$/, '')));
  const h = ours ?? mine[0];
  return { branch: h.ref, sha: h.sha, consistent: Boolean(ours), pushed: mine.length };
}

/** Keep raw output of a run under its results directory, with the run's secrets taken out. */
export function saveRaw(dir, name, text, secrets = []) {
  fs.mkdirSync(path.dirname(path.join(dir, name)), { recursive: true });
  fs.writeFileSync(path.join(dir, name), redact(text, secrets));
}
