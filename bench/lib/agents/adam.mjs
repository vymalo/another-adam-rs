// The adam-coder driver: the coder image the pins name, on settings that already exist (no code change), driven
// over A2A, its worktree read from the volume.
//
//   prepare  agent folder of the image's revision without mcp.json and with the configured turn limit, an empty
//            database, the base pushed to a repository of this run's own on the git server by a container with no
//            secret, the slot's coder recreated for this run (model address with the run's meter token, a random
//            A2A bearer, the task image as the devcontainer's default image); its git runs in the git sandbox and the
//            coder is on no network that has internet
//   run      SendStreamingMessage, then polling every 10 s decides (lib/a2a.mjs): one fixed reply to a first
//            `input-required`, CancelTask on the second one, at the time limit, or when the meter refuses; then the
//            harness requires that a container of the task image ran (`no-task-container` otherwise)
//   stop     the coder removed and confirmed, the devcontainers it made removed and confirmed
//   collect  only after stop: the run's worktree as a tar stream, turned into a patch; the pushed agent/<run>
//            branch of the run's own repository is the cross-check
//   cleanup  the run's repository deleted, the scratch removed
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { A2AClient, driveTask } from '../a2a.mjs';
import { externalFailure } from '../classify.mjs';
import { ensureAgentFolder, hostRepoUrl, lsRemoteHeads, pushScript, runRepoName, stackRepoUrl } from '../gitops.mjs';
import { parseImageRef } from '../task.mjs';
import { InfraError, WorktreeError, crossCheckBranch, locateWorktree, patchOfTree, saveRaw } from './common.mjs';

/** The coder's per-run settings. Pure. Every value is plain: a settings file has no quoting. */
export function adamSettings({ meterBaseUrl, bearer, taskImage, contextWindow }) {
  const ref = parseImageRef(taskImage);
  if (!ref || ref.tag !== null || !ref.digest) throw new Error('the default environment image must be named by digest only (name@sha256:...)');
  if (!Number.isSafeInteger(contextWindow) || contextWindow < 1) throw new Error('the context window is required: both agents are given the same one');
  return {
    MODEL_BASE_URL: meterBaseUrl,
    A2A_BEARER_TOKENS: bearer,
    DEVCONTAINER_DEFAULT_IMAGE: taskImage,
    MODEL_CONTEXT_WINDOW: String(contextWindow),
  };
}

/** What adam is told: the one line of preamble it needs to know where the repository is, then the statement. */
export function adamMessage({ repoUrl, statement }) {
  return `In ${repoUrl} (base branch main), do this task:\n\n${statement}`;
}

/** Did a container of the task's image run in the slot's service? `containers` is its listing. */
export function ranTaskImage(containers, task) {
  return containers.some((c) => c.image.includes(task.image.digest) || c.imageId === task.image.digest.replace(/^sha256:/, '') || c.imageId === task.image.digest);
}

export class AdamAgent {
  name = 'adam';

  constructor({ pins, home, repoDir, contextWindow, drive = driveTask, heads = lsRemoteHeads, newBearer = () => crypto.randomBytes(24).toString('base64url'), makeClient = (o) => new A2AClient(o), ensureFolder = ensureAgentFolder, now = Date.now, sleep }) {
    Object.assign(this, { pins, home, repoDir, contextWindow, drive, heads, newBearer, makeClient, ensureFolder, now, sleep });
    this.agentDir = null;
    this.runs = new Map(); // run -> { client, bearer, repo }; the bearer is here and in the client, nowhere else
  }

  async prepare(ctx) {
    const { stack, slot, base } = ctx;
    const steps = ctx.limits.steps;
    const dest = path.join(this.home, 'agent-folder', `${this.pins.coder.revision.slice(0, 12)}-t${steps}`);
    this.agentDir = await this.ensureFolder({ repoDir: this.repoDir, revision: this.pins.coder.revision, dest, steps });
    stack.agentDir = this.agentDir;
    const repo = runRepoName(base.slug, ctx.run, ctx.attempt);
    this.runs.set(ctx.run, { repo });
    await stack.resetDatabase(slot);
    await stack.pushBase(slot, pushScript({ base: base.containerRepo, url: stackRepoUrl(repo) }));
    const bearer = this.newBearer();
    const env = adamSettings({ meterBaseUrl: ctx.meter.baseUrl, bearer, taskImage: ctx.task.image.ref, contextWindow: this.contextWindow });
    await stack.startCoder(slot, env);
    Object.assign(this.runs.get(ctx.run), { client: this.makeClient({ url: stack.coderUrl(slot), bearer }), bearer });
    fs.mkdirSync(ctx.paths.scratch, { recursive: true });
  }

  async run(ctx) {
    const state = this.runs.get(ctx.run);
    if (!state?.client) throw new Error('adam: run() before prepare()');
    const text = adamMessage({ repoUrl: stackRepoUrl(state.repo), statement: ctx.task.instruction });
    const r = await this.drive({ client: state.client, text, messageId: `bench-${ctx.run}-a${ctx.attempt}`.replace(/[^A-Za-z0-9._-]/g, '-'), limitMs: ctx.limits.limitMs, graceMs: ctx.limits.cancelGraceMs, refusal: ctx.meter.refusal, now: this.now, ...(this.sleep ? { sleep: this.sleep } : {}), log: ctx.log });
    saveRaw(path.join(ctx.paths.run, 'adam'), 'task.json', `${JSON.stringify({ exit: r.exit, taskId: r.taskId, finalState: r.finalState, nudges: r.nudges, polls: r.polls, task: r.lastTask ?? null }, null, 2)}\n`, [ctx.meter.token, state.bearer]);
    // The run must have used the task's image: a coder whose runtime failed goes on in its own container and degrades quietly.
    if (r.exit !== 'infra' && !ranTaskImage(await ctx.containers.listContainers(), ctx.task)) throw new InfraError('no-task-container', 'no container of the task image ran in the Podman service');
    return { exit: r.exit, wallMs: r.elapsedMs, taskId: r.taskId, nudges: r.nudges, polls: r.polls, selfReported: r.selfReported, cancelFailed: r.cancelFailed, finalState: r.finalState, externalFailure: r.exit === 'failed' && externalFailure(r.lastTask) };
  }

  /** The coder is removed and its devcontainers are removed, and both are confirmed. */
  async stop(ctx) {
    try {
      await ctx.stack.stopCoder(ctx.slot);
      await ctx.containers.reapAll();
    } catch (e) {
      throw new InfraError('not-stopped', e.message);
    }
  }

  async collect(ctx) {
    const { stack, slot } = ctx;
    const state = this.runs.get(ctx.run);
    const runs = await stack.listDir(slot, '/work/workspaces');
    const slots = {};
    for (const run of runs) slots[run] = await stack.listDir(slot, `/work/workspaces/${run}`);
    let found;
    try {
      found = locateWorktree({ runs, slotsOf: (r) => slots[r], slug: ctx.base.slug });
    } catch (e) {
      if (e instanceof WorktreeError && e.code === 'no-worktree') return { patch: Buffer.alloc(0), notes: { worktree: null, reason: e.message } };
      throw e;
    }
    const treeDir = path.join(ctx.paths.scratch, 'tree');
    const ex = await stack.extractTree(slot, found.path, treeDir);
    const patch = await patchOfTree({ treeDir, base: ctx.base, workRoot: ctx.paths.scratch });
    fs.rmSync(treeDir, { recursive: true, force: true });
    let branch = null;
    try {
      branch = crossCheckBranch({ heads: await this.heads({ url: hostRepoUrl(stack.ports.git, state.repo) }), runDir: found.run });
    } catch {
      branch = { error: 'the branches could not be listed' };
    }
    return { patch, notes: { worktree: { run: found.run, slot: found.slot }, branch, refused: ex?.refused ?? [] } };
  }

  async cleanup(ctx) {
    const state = this.runs.get(ctx.run);
    this.runs.delete(ctx.run);
    const errors = [];
    const attempt = async (f) => {
      try {
        await f();
      } catch (e) {
        errors.push(e);
      }
    };
    await attempt(() => ctx.stack.stopCoder(ctx.slot));
    if (state?.repo) await attempt(() => ctx.stack.deleteRepo(state.repo));
    await attempt(() => ctx.containers.reapAll());
    fs.rmSync(ctx.paths.scratch, { recursive: true, force: true });
    if (errors.length > 0) throw errors[0];
  }
}
