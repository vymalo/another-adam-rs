// The adam-coder driver: the coder image the pins name, on settings that already exist (no code change), driven
// over A2A, its worktree read from the volume.
//
//   prepare  agent folder of the image's revision without mcp.json, the base pushed to git-server, an empty
//            database and work volume, the slot's coder recreated for this run (model address with the run's
//            meter token, a random A2A bearer, the task image as the devcontainer's default image)
//   run      SendStreamingMessage, then polling every 10 s decides (lib/a2a.mjs): one fixed reply to a first
//            `input-required`, CancelTask on the second one, at the time limit, or when the meter refuses
//   collect  the run's worktree (the one new directory of the emptied volume) copied out by a helper container
//            of the task image and turned into a patch; the pushed agent/<run> branch is the cross-check
//   cleanup  the coder stopped and the scratch removed
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { A2AClient, driveTask } from '../a2a.mjs';
import { ensureAgentFolder, hostRepoUrl, lsRemoteHeads, pushBase, stackRepoUrl } from '../gitops.mjs';
import { parseImageRef } from '../task.mjs';
import { WorktreeError, copyTreeOut, crossCheckBranch, locateWorktree, patchOfTree, saveRaw } from './common.mjs';

/** The coder's per-run settings. Pure. Every value is plain: a settings file has no quoting. */
export function adamSettings({ meterBaseUrl, bearer, taskImage, contextWindow = null }) {
  const ref = parseImageRef(taskImage);
  if (!ref || ref.tag !== null || !ref.digest) throw new Error('the default environment image must be named by digest only (name@sha256:...)');
  const env = {
    MODEL_BASE_URL: meterBaseUrl,
    A2A_BEARER_TOKENS: bearer,
    DEVCONTAINER_DEFAULT_IMAGE: taskImage,
  };
  if (contextWindow !== null) env.MODEL_CONTEXT_WINDOW = String(contextWindow);
  return env;
}

/** What adam is told: the one line of preamble it needs to know where the repository is, then the statement. */
export function adamMessage({ repoUrl, statement }) {
  return `In ${repoUrl} (base branch main), do this task:\n\n${statement}`;
}

export class AdamAgent {
  name = 'adam';

  constructor({ pins, home, repoDir, contextWindow = null, drive = driveTask, push = pushBase, heads = lsRemoteHeads, newBearer = () => crypto.randomBytes(24).toString('base64url'), makeClient = (o) => new A2AClient(o), ensureFolder = ensureAgentFolder, now = Date.now, sleep }) {
    Object.assign(this, { pins, home, repoDir, contextWindow, drive, push, heads, newBearer, makeClient, ensureFolder, now, sleep });
    this.agentDir = null;
    this.clients = new Map(); // run -> client; the bearer is inside it and nowhere else
  }

  async prepare(ctx) {
    const { stack, slot, base } = ctx;
    this.agentDir ??= await this.ensureFolder({ repoDir: this.repoDir, revision: this.pins.coder.revision, dest: path.join(this.home, 'agent-folder', this.pins.coder.revision.slice(0, 12)) });
    stack.agentDir = this.agentDir;
    await this.push({ dir: base.dir, url: hostRepoUrl(stack.ports.git, base.slug) });
    await stack.resetDatabase(slot);
    await stack.resetWork(slot);
    const bearer = this.newBearer();
    const env = adamSettings({ meterBaseUrl: ctx.meter.baseUrl, bearer, taskImage: ctx.task.image.ref, contextWindow: this.contextWindow });
    await stack.startCoder(slot, env);
    this.clients.set(ctx.run, this.makeClient({ url: stack.coderUrl(slot), bearer }));
    fs.mkdirSync(ctx.paths.out, { recursive: true });
    fs.chmodSync(ctx.paths.out, 0o777);
  }

  async run(ctx) {
    const client = this.clients.get(ctx.run);
    if (!client) throw new Error('adam: run() before prepare()');
    const text = adamMessage({ repoUrl: stackRepoUrl(ctx.base.slug), statement: ctx.task.instruction });
    const started = this.now();
    const r = await this.drive({ client, text, messageId: `bench-${ctx.run}-a${ctx.attempt}`.replace(/[^A-Za-z0-9._-]/g, '-'), limitMs: ctx.limitMs, refusal: ctx.meter.refusal, now: this.now, ...(this.sleep ? { sleep: this.sleep } : {}), log: ctx.log });
    const wallMs = this.now() - started;
    saveRaw(path.join(ctx.paths.run, 'adam'), 'task.json', `${JSON.stringify({ exit: r.exit, taskId: r.taskId, finalState: r.finalState, nudges: r.nudges, polls: r.polls, task: r.lastTask ?? null }, null, 2)}\n`, [ctx.meter.token]);
    return { exit: r.exit, wallMs, taskId: r.taskId, nudges: r.nudges, polls: r.polls, selfReported: r.selfReported, cancelFailed: r.cancelFailed };
  }

  async collect(ctx) {
    const { stack, slot } = ctx;
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
    const treeDir = path.join(ctx.paths.out, 'tree');
    await copyTreeOut({ containers: ctx.containers, image: ctx.task.image.ref, treePath: found.path, outDir: treeDir });
    const patch = await patchOfTree({ treeDir, base: ctx.base, workRoot: path.dirname(ctx.paths.out) });
    fs.rmSync(treeDir, { recursive: true, force: true });
    let branch = null;
    try {
      branch = crossCheckBranch({ heads: await this.heads({ dir: ctx.base.dir, url: hostRepoUrl(stack.ports.git, ctx.base.slug) }), runDir: found.run });
    } catch {
      branch = { error: 'the branches could not be listed' };
    }
    return { patch, notes: { worktree: { run: found.run, slot: found.slot }, branch } };
  }

  async cleanup(ctx) {
    this.clients.delete(ctx.run);
    try {
      await ctx.stack.stopCoder(ctx.slot);
    } finally {
      fs.rmSync(ctx.paths.out, { recursive: true, force: true });
    }
  }
}
