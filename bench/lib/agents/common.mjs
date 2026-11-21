// What the two drivers share: the agent interface, finding the run's tree, keeping raw output, and a patch from the
// final tree.
//
// An agent driver is an object with
//   name                      'adam' | 'opencode'
//   prepare(ctx)              the base is in place, the agent can be started; throws on infrastructure trouble
//   run(ctx)                  runs the agent to its end; returns { exit, wallMs, selfReported, ... }
//   stop(ctx)                 the agent and everything it started are stopped and that is confirmed, or it throws
//   collect(ctx)              the final tree as a patch: { patch: Buffer, ... }; only after stop()
//   cleanup(ctx)              always called; removes what the run left in the stack
// ctx: { run, attempt, slot, task, base, stack, containers, meter: { token, baseUrl, refusal() }, paths: { run, in, scratch },
//        limits: { limitMs, steps, cancelGraceMs }, contextWindow, log }
// The host never reads a file a container wrote and never deletes one: trees and logs come out as tar streams
// (lib/tarstream.mjs through lib/stack.mjs) and what is left in a service is removed by the service.
import fs from 'node:fs';
import path from 'node:path';
import { collectPatch, localGit } from '../grade.mjs';
import { redact } from '../stack.mjs';

/** The harness's infrastructure, not the agent, is what went wrong: the run is `Failed_infra` with this reason. */
export class InfraError extends Error {
  constructor(reason, message = reason) {
    super(message);
    this.name = 'InfraError';
    this.reason = reason;
  }
}

/** The patch of the tree in `treeDir` (a directory the harness made) against the base. Nothing in the tree is run. */
/**
 * Folders only a package manager or a tool writes. The agent's tree is read without the ones the base does not track: they are
 * never part of a change, and one of them can be far over the size a tree may be (element-web's node_modules failed collect,
 * 2026-11-21). One the base tracks (a vendored node_modules) is read, or its files would look deleted.
 */
export const HEAVY_DIRS = Object.freeze(['node_modules', '.mypy_cache', '.pytest_cache', '__pycache__', '.ruff_cache', '.tox', '.nox']);
const heavyOfBase = new Map();

/** The names of HEAVY_DIRS that no file of the base is under, at any depth. */
export async function untrackedHeavyDirs(base, git = localGit) {
  const key = `${base.dir}@${base.sha}`;
  if (!heavyOfBase.has(key)) {
    const files = (await git(base.dir)(['ls-tree', '-r', '-z', '--name-only', base.sha])).toString('utf8').split('\0');
    const tracked = new Set();
    for (const f of files) for (const part of f.split('/').slice(0, -1)) if (HEAVY_DIRS.includes(part)) tracked.add(part);
    heavyOfBase.set(key, HEAVY_DIRS.filter((d) => !tracked.has(d)));
  }
  return heavyOfBase.get(key);
}

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
 * { branch, sha, consistent } or null when nothing was pushed. The run has a repository of its own, so every
 * branch in it is this run's.
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
