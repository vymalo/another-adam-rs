// The OpenCode driver. OpenCode V2 (`@opencode/cli` 2.x: a pinned binary from npm, checked against its integrity hash
// and mounted read-only) runs in a container of the task's own image on the slot's Podman service, in a clone of the
// same base at the same kind of path, with the statement on stdin and the meter as its only way to the model.
//
//   cd <clone> && opencode run --standalone --format json --model bench/bench-model --auto --title <run> < instruction.md
//
// Verified 2026-11-17 against the pinned 2.3.0 binary and a local OpenAI-compatible stub (bench/README.md, "OpenCode
// V2"): `run` has no `--dir` and `--standalone` makes it use a private server; the statement on stdin reaches the model
// byte for byte; `--title` removes the extra title-generation call; `step_finish` carries `tokens` {input, output,
// reasoning, cache: {read, write}} and `cost`; the export is `opencode session export <session>`; `run` waits for the
// background jobs its prompt started, subagents included, with no time limit of its own: the time limit is the harness's.
// It needs no network to load its provider.
//
// What the host trusts (the review of slice 3): the exit status is the container runtime's, never a file the agent could
// write; the JSON stream, the stderr and the export come out through a tar stream with a size cap and are refused if
// they are not regular files; the config, which holds the run token, goes in on stdin and is on no command line.
import fs from 'node:fs';
import path from 'node:path';
import { fetchPinnedBinary } from '../npm.mjs';
import { InfraError, patchOfTree, saveRaw } from './common.mjs';

export const MODEL = 'bench/bench-model';
const METER_URL = /^http:\/\/meter-\d:8090\/r\/[A-Za-z0-9_-]{16,128}\/v1$/;
const SAFE_PATH = /^\/[A-Za-z0-9._/@+=-]*$/;
const SAFE_RUN = /^[A-Za-z0-9._-]{1,64}$/;
const SESSION = /^ses_[A-Za-z0-9]{6,64}$/;
const OUTPUT_TOKENS = 8192;
const OUT_FILES = ['events.jsonl', 'stderr.txt', 'export.json'];

/** The OpenCode configuration of a run (`OPENCODE_CONFIG_CONTENT`). Pure. */
export function opencodeConfig({ baseUrl, contextWindow, steps = 200, urlPattern = METER_URL }) {
  if (!urlPattern.test(baseUrl ?? '')) throw new Error('the model address must be the meter\'s: http://meter-<slot>:8090/r/<run token>/v1');
  if (!Number.isSafeInteger(contextWindow) || contextWindow < 1) throw new Error('the context window is required: both agents are given the same one');
  return {
    $schema: 'https://opencode.ai/config.json',
    autoupdate: false,
    share: 'disabled',
    model: MODEL,
    small_model: MODEL,
    provider: {
      bench: {
        npm: '@ai-sdk/openai-compatible',
        name: 'bench',
        options: { baseURL: baseUrl, apiKey: 'bench-dummy-key' }, // the meter holds the real key
        models: { 'bench-model': { name: 'bench-model', tool_call: true, limit: { context: contextWindow, output: OUTPUT_TOKENS } } },
      },
    },
    permission: { '*': 'allow', webfetch: 'deny', websearch: 'deny', external_directory: 'deny', question: 'deny' },
    agent: { build: { steps } },
  };
}

/** The environment of the container: two switches, nothing secret. */
export function opencodeEnv() {
  return { OPENCODE_DISABLE_MODELS_FETCH: 'true', OPENCODE_DISABLE_AUTOUPDATE: '1' };
}

/** What goes in on stdin: the configuration, which holds the run's token. */
export function opencodeInput({ config }) {
  return `${JSON.stringify(config)}\n`;
}

const plain = (...ps) => {
  for (const p of ps) if (!SAFE_PATH.test(p)) throw new Error('a path in the OpenCode script is not a plain absolute path');
};

const prelude = (out) => [
  'set -u',
  `export HOME=${out}/home XDG_DATA_HOME=${out}/home/data XDG_CONFIG_HOME=${out}/home/config XDG_CACHE_HOME=${out}/home/cache`,
  'mkdir -p "$HOME" "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME"',
  'OPENCODE_CONFIG_CONTENT=$(cat)',
  'export OPENCODE_CONFIG_CONTENT',
];

/** The shell of the run; its last command is OpenCode, so the container's exit status is OpenCode's. Pure. */
export function opencodeRunScript({ bin, stmt, out, dir, run }) {
  plain(bin, stmt, out, dir);
  if (!SAFE_RUN.test(run)) throw new Error('the run id is not plain');
  return [...prelude(out), `cd ${dir} || exit 98`, `${bin} run --standalone --format json --model ${MODEL} --auto --title '${run}' < ${stmt} > ${out}/events.jsonl 2> ${out}/stderr.txt`].join('\n');
}

/** The shell of the export, in a container of its own (the run's may have been killed). Pure. */
export function opencodeExportScript({ bin, out, sessionId }) {
  plain(bin, out);
  if (!SESSION.test(sessionId)) throw new Error('the session id is not plain');
  return [...prelude(out), `${bin} session export --standalone '${sessionId}' > ${out}/export.json 2> ${out}/export.err`].join('\n');
}

/** What the JSON events say: the session, the steps, what OpenCode says it used, the errors. Pure. */
export function parseEvents(text) {
  let sessionId = null;
  let calls = 0;
  let errors = 0;
  let retries = 0;
  let backgroundWaits = 0;
  const tokens = { input: 0, output: 0, cached: 0 };
  for (const line of String(text).split('\n')) {
    let e;
    try {
      e = JSON.parse(line);
    } catch {
      continue;
    }
    if (typeof e?.sessionID === 'string' && SESSION.test(e.sessionID)) sessionId ??= e.sessionID;
    if (e?.type === 'error') errors += 1;
    if (e?.type === 'retry') retries += 1;
    if (e?.type === 'background_wait') backgroundWaits += 1;
    if (e?.type === 'step_finish') {
      calls += 1;
      const t = e.part?.tokens ?? {};
      tokens.input += Number.isFinite(t.input) ? t.input : 0;
      tokens.output += Number.isFinite(t.output) ? t.output : 0;
      tokens.cached += Number.isFinite(t.cache?.read) ? t.cache.read : 0;
    }
  }
  return { sessionId, errors, retries, backgroundWaits, selfReported: { ...tokens, calls } };
}

const text = (files, name) => (files.has(name) ? files.get(name).toString('utf8') : '');

export class OpenCodeAgent {
  name = 'opencode';

  constructor({ pins, home, fetchBinary = fetchPinnedBinary, now = Date.now, contextWindow, pollMs = 10_000 }) {
    Object.assign(this, { pins, home, fetchBinary, now, contextWindow, pollMs });
    this.binary = null;
  }

  #names(ctx) {
    const safe = ctx.run.replace(/[^A-Za-z0-9._-]/g, '-');
    const parent = `/work/workspaces/oc-${safe}-a${ctx.attempt}`;
    return { parent, dir: `${parent}/${ctx.base.slug}`, out: `/work/bench-out/oc-${safe}-a${ctx.attempt}`, inDir: path.join(this.slotDir(ctx), `in-${safe}-a${ctx.attempt}`), container: `adam-bench-oc-${safe}-a${ctx.attempt}`.slice(0, 63) };
  }

  slotDir(ctx) {
    return ctx.stack.slotDir(ctx.slot);
  }

  async prepare(ctx) {
    this.binary ??= await this.fetchBinary({ pin: this.pins.opencode, dir: path.join(this.home, 'opencode', this.pins.opencode.version) });
    const names = this.#names(ctx);
    // What the service gets from the harness: the binary and the statement, in the slot's directory, which it mounts read-only.
    const bin = path.join(this.slotDir(ctx), 'bin');
    fs.mkdirSync(bin, { recursive: true, mode: 0o755 });
    const target = path.join(bin, 'opencode');
    if (!fs.existsSync(target) || fs.statSync(target).size !== fs.statSync(this.binary.path).size) {
      fs.rmSync(target, { force: true });
      fs.copyFileSync(this.binary.path, target);
      fs.chmodSync(target, 0o555);
    }
    fs.rmSync(names.inDir, { recursive: true, force: true });
    fs.mkdirSync(names.inDir, { recursive: true, mode: 0o755 });
    fs.writeFileSync(path.join(names.inDir, 'instruction.md'), ctx.task.instruction, { mode: 0o644 });
    for (const d of [names.parent, names.out]) {
      await ctx.stack.workDir(ctx.slot, d, { remove: true }); // a killed attempt may have left it
      await ctx.stack.workDir(ctx.slot, d);
    }
    const r = await ctx.containers.run({
      image: ctx.task.image.ref,
      script: ['set -eu', "export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0='*'", `git clone -q --no-hardlinks ${ctx.base.containerRepo} ${names.dir}`, `git -C ${names.dir} checkout -q main`, `chmod -R a+rwX ${names.parent}`].join('\n'),
      workdir: '/',
      mounts: [
        { source: path.dirname(ctx.base.containerRepo), target: path.dirname(ctx.base.containerRepo), readOnly: true },
        { source: names.parent, target: names.parent },
      ],
      network: 'none',
      timeoutMs: 10 * 60 * 1000,
      labels: { 'bench.slot': String(ctx.slot), 'bench.run': names.container.slice(14) },
    });
    if (r.exitCode !== 0) throw new Error(`the clone for OpenCode failed (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-2).join(' | ')}`);
  }

  async run(ctx) {
    const names = this.#names(ctx);
    const config = opencodeConfig({ baseUrl: ctx.meter.baseUrl, contextWindow: this.contextWindow, steps: ctx.limits.steps });
    const input = opencodeInput({ config });
    const mounts = [
      { source: path.join(this.slotDir(ctx), 'bin', 'opencode'), target: '/opt/bench/opencode', readOnly: true },
      { source: names.inDir, target: '/bench/in', readOnly: true },
      { source: names.out, target: '/bench/out' },
      { source: names.parent, target: names.parent },
    ];
    const base = { image: ctx.task.image.ref, workdir: '/', env: opencodeEnv(), mounts, input, cpus: ctx.task.cpus, memoryMb: ctx.task.memoryMb, labels: { 'bench.slot': String(ctx.slot), 'bench.run': names.container.slice(14) } };
    // The image the container runs is the task's, by digest: asserted here, before the container starts.
    if (!/@sha256:[0-9a-f]{64}$/.test(base.image)) throw new InfraError('no-task-container', 'the task image is not named by digest');
    // A refusal of the meter is a limit like time: OpenCode retries a refused call, so the harness ends the container.
    let refused = false;
    const watcher = setInterval(() => {
      if (!refused && ctx.meter.refusal()) {
        refused = true;
        ctx.containers.stop(names.container).catch(() => {});
      }
    }, this.pollMs);
    const started = this.now();
    let r;
    try {
      r = await ctx.containers.run({ ...base, name: names.container, script: opencodeRunScript({ bin: '/opt/bench/opencode', stmt: '/bench/in/instruction.md', out: '/bench/out', dir: names.dir, run: `${ctx.run}-a${ctx.attempt}`.replace(/[^A-Za-z0-9._-]/g, '-') }), network: 'default', timeoutMs: ctx.limits.limitMs });
    } finally {
      clearInterval(watcher);
    }
    const wallMs = this.now() - started;
    let got = await ctx.stack.readFiles(ctx.slot, names.out, OUT_FILES);
    const eventsText = text(got.files, 'events.jsonl');
    const events = parseEvents(eventsText);
    if (!r.timedOut && !refused && eventsText === '' && [125, 126, 127].includes(r.exitCode)) throw new InfraError('runtime', `the OpenCode container did not start (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-2).join(' | ')}`);

    if (events.sessionId) {
      const e = await ctx.containers.run({ ...base, name: `${names.container}-x`.slice(0, 63), script: opencodeExportScript({ bin: '/opt/bench/opencode', out: '/bench/out', sessionId: events.sessionId }), network: 'none', timeoutMs: 5 * 60 * 1000 });
      if (e.exitCode === 0) got = await ctx.stack.readFiles(ctx.slot, names.out, OUT_FILES);
    }
    const secrets = [ctx.meter.token];
    const keep = path.join(ctx.paths.run, 'opencode');
    for (const name of OUT_FILES) saveRaw(keep, name, text(got.files, name).slice(name === 'stderr.txt' ? -20_000 : 0), secrets);
    if (got.refused.length > 0) {
      ctx.log({ event: 'refused_output', names: got.refused });
      saveRaw(keep, 'refused.json', `${JSON.stringify(got.refused)}\n`);
    }

    const refusal = ctx.meter.refusal();
    let exit;
    if (refused) exit = refusal === 'cap' ? 'stopped_by_cap' : 'budget_hit';
    else if (r.timedOut) exit = 'timeout'; // the runtime's word, before any status
    else if (r.exitCode === 0 && events.errors === 0) exit = 'done';
    else if (refusal) exit = refusal === 'cap' ? 'stopped_by_cap' : 'budget_hit';
    else exit = 'failed';
    return { exit, wallMs, sessionId: events.sessionId, selfReported: events.selfReported, retries: events.retries, backgroundWaits: events.backgroundWaits, exportKept: got.files.has('export.json'), processExit: r.exitCode, refusedOutput: got.refused };
  }

  /** The container ran with --rm and has ended; nothing of the run may be left in the service. */
  async stop(ctx) {
    try {
      await ctx.containers.reapAll();
    } catch (e) {
      throw new InfraError('not-stopped', e.message);
    }
  }

  async collect(ctx) {
    const names = this.#names(ctx);
    const treeDir = path.join(ctx.paths.scratch, 'tree');
    await ctx.stack.extractTree(ctx.slot, names.dir, treeDir);
    const patch = await patchOfTree({ treeDir, base: ctx.base, workRoot: ctx.paths.scratch });
    fs.rmSync(treeDir, { recursive: true, force: true });
    return { patch, notes: {} };
  }

  async cleanup(ctx) {
    const names = this.#names(ctx);
    const errors = [];
    for (const d of [names.parent, names.out]) {
      try {
        await ctx.stack.workDir(ctx.slot, d, { remove: true });
      } catch (e) {
        errors.push(e);
      }
    }
    try {
      await ctx.containers.reapAll();
    } catch (e) {
      errors.push(e);
    }
    fs.rmSync(names.inDir, { recursive: true, force: true });
    fs.rmSync(ctx.paths.scratch, { recursive: true, force: true });
    if (errors.length > 0) throw errors[0];
  }
}
