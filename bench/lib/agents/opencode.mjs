// The OpenCode driver. OpenCode (a pinned binary from npm, checked against its integrity hash and mounted read-only)
// runs in a container of the task's own image on the same Podman service as adam's commands, in a clone of the same
// base at the same kind of path, with the statement on stdin and the meter as its only way to the model.
//
//   opencode run --format json --model bench/bench-model --auto --title <run> --dir <clone> < instruction.md
//
// Verified 2026-11-17 against the pinned binary (1.19.5) and a local OpenAI-compatible stub: the statement on stdin
// reaches the model byte for byte (as arguments, a statement with a space is wrapped in quotes); `--title` removes
// the extra title-generation call; `step_finish` carries `tokens` {total, input, output, reasoning, cache: {write,
// read}} and `cost`; `opencode export <session>` prints the session as JSON ({info, messages}); the stream has only
// the main session (a subagent's calls are in the export). The home directory is on the shared output directory so
// that the export can run in a second container after a run that was killed at the time limit.
import fs from 'node:fs';
import path from 'node:path';
import { fetchPinnedBinary } from '../npm.mjs';
import { copyTreeOut, patchOfTree, saveRaw } from './common.mjs';

export const MODEL = 'bench/bench-model';
const METER_URL = /^http:\/\/meter:8090\/r\/[A-Za-z0-9_-]{16,128}\/v1$/;
const SAFE_PATH = /^\/[A-Za-z0-9._/@+=-]*$/;
const SAFE_RUN = /^[A-Za-z0-9._-]{1,64}$/;
const SESSION = /^ses_[A-Za-z0-9]{6,64}$/;
const OUTPUT_TOKENS = 8192;

/** The OpenCode configuration of a run (`OPENCODE_CONFIG_CONTENT`). Pure. */
export function opencodeConfig({ baseUrl, contextWindow = 131072, steps = 200, urlPattern = METER_URL }) {
  if (!urlPattern.test(baseUrl ?? '')) throw new Error('the model address must be the meter\'s: http://meter:8090/r/<run token>/v1');
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
    permission: { '*': 'allow', webfetch: 'deny', websearch: 'deny', external_directory: 'deny' },
    agent: { build: { steps } },
  };
}

/** The environment of the container, besides the home directory the script sets. */
export function opencodeEnv({ config }) {
  return {
    OPENCODE_CONFIG_CONTENT: JSON.stringify(config),
    OPENCODE_DISABLE_MODELS_FETCH: 'true',
    OPENCODE_DISABLE_AUTOUPDATE: '1',
  };
}

const plain = (...ps) => {
  for (const p of ps) if (!SAFE_PATH.test(p)) throw new Error('a path in the OpenCode script is not a plain absolute path');
};

const homeLines = (out) => [`export HOME=${out}/home XDG_DATA_HOME=${out}/home/data XDG_CONFIG_HOME=${out}/home/config XDG_CACHE_HOME=${out}/home/cache`, 'mkdir -p "$HOME" "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME"'];

/** The shell of the run. Pure. */
export function opencodeRunScript({ bin, stmt, out, dir, run }) {
  plain(bin, stmt, out, dir);
  if (!SAFE_RUN.test(run)) throw new Error('the run id is not plain');
  return [
    'set -u',
    ...homeLines(out),
    `cd ${dir} || exit 98`,
    `${bin} run --format json --model ${MODEL} --auto --title '${run}' --dir ${dir} < ${stmt} > ${out}/events.jsonl 2> ${out}/stderr.txt`,
    `echo $? > ${out}/exit`,
  ].join('\n');
}

/** The shell of the export, in a container of its own (the run's may have been killed). Pure. */
export function opencodeExportScript({ bin, out, sessionId }) {
  plain(bin, out);
  if (!SESSION.test(sessionId)) throw new Error('the session id is not plain');
  return ['set -u', ...homeLines(out), `${bin} export '${sessionId}' > ${out}/export.json 2> ${out}/export.err`].join('\n');
}

/** What the JSON events say: the session, the steps, what OpenCode says it used, the errors. Pure. */
export function parseEvents(text) {
  let sessionId = null;
  let calls = 0;
  let errors = 0;
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
    if (e?.type === 'step_finish') {
      calls += 1;
      const t = e.part?.tokens ?? {};
      tokens.input += Number.isFinite(t.input) ? t.input : 0;
      tokens.output += Number.isFinite(t.output) ? t.output : 0;
      tokens.cached += Number.isFinite(t.cache?.read) ? t.cache.read : 0;
    }
  }
  return { sessionId, errors, selfReported: { ...tokens, calls } };
}

const readOr = (file, fallback = '') => {
  try {
    return fs.readFileSync(file, 'utf8');
  } catch {
    return fallback;
  }
};

export class OpenCodeAgent {
  name = 'opencode';

  constructor({ pins, home, fetchBinary = fetchPinnedBinary, now = Date.now, contextWindow }) {
    this.pins = pins;
    this.home = home;
    this.fetchBinary = fetchBinary;
    this.now = now;
    this.contextWindow = contextWindow;
    this.binary = null;
  }

  #names(ctx) {
    const parent = `/work/workspaces/oc-${ctx.run.replace(/[^A-Za-z0-9._-]/g, '-')}`;
    return { parent, dir: `${parent}/${ctx.base.slug}` };
  }

  async prepare(ctx) {
    this.binary ??= await this.fetchBinary({ pin: this.pins.opencode, dir: path.join(this.home, 'opencode', this.pins.opencode.version) });
    for (const d of [ctx.paths.in, ctx.paths.out]) {
      fs.rmSync(d, { recursive: true, force: true });
      fs.mkdirSync(d, { recursive: true });
    }
    fs.chmodSync(ctx.paths.in, 0o755);
    fs.chmodSync(ctx.paths.out, 0o777);
    fs.writeFileSync(path.join(ctx.paths.in, 'instruction.md'), ctx.task.instruction, { mode: 0o644 });
    const { parent, dir } = this.#names(ctx);
    await ctx.stack.workspaceDir(ctx.slot, parent);
    const base = path.dirname(ctx.base.dir);
    const r = await ctx.containers.run({
      image: ctx.task.image.ref,
      script: ['set -eu', "export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0='*'", `git clone -q --no-hardlinks ${ctx.base.dir} ${dir}`, `git -C ${dir} checkout -q main`, `chmod -R a+rwX ${parent}`].join('\n'),
      workdir: '/',
      mounts: [
        { source: base, target: base, readOnly: true },
        { source: parent, target: parent },
      ],
      network: 'none',
      timeoutMs: 10 * 60 * 1000,
    });
    if (r.exitCode !== 0) throw new Error(`the clone for OpenCode failed (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-2).join(' | ')}`);
  }

  #mounts(ctx, { dir, parent }) {
    return [
      { source: this.binary.path, target: '/opt/bench/opencode', readOnly: true },
      { source: ctx.paths.in, target: '/bench/in', readOnly: true },
      { source: ctx.paths.out, target: '/bench/out' },
      { source: parent, target: parent },
    ].filter(Boolean);
  }

  async run(ctx) {
    const names = this.#names(ctx);
    const config = opencodeConfig({ baseUrl: ctx.meter.baseUrl, contextWindow: this.contextWindow });
    const env = opencodeEnv({ config });
    const mounts = this.#mounts(ctx, names);
    const started = this.now();
    const r = await ctx.containers.run({
      image: ctx.task.image.ref,
      script: opencodeRunScript({ bin: '/opt/bench/opencode', stmt: '/bench/in/instruction.md', out: '/bench/out', dir: names.dir, run: `${ctx.run}-a${ctx.attempt}`.replace(/[^A-Za-z0-9._-]/g, '-') }),
      workdir: names.dir,
      env,
      mounts,
      network: 'default', // the Podman service's own network: the meter, and nothing else
      timeoutMs: ctx.limitMs,
    });
    const wallMs = this.now() - started;
    const out = ctx.paths.out;
    const exitText = readOr(path.join(out, 'exit'), '').trim();
    if (exitText === '' && !r.timedOut && [125, 126, 127].includes(r.exitCode)) throw new Error(`the OpenCode container did not start (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-2).join(' | ')}`);
    const eventsText = readOr(path.join(out, 'events.jsonl'));
    const events = parseEvents(eventsText);

    let exportText = '';
    if (events.sessionId) {
      const e = await ctx.containers.run({
        image: ctx.task.image.ref,
        script: opencodeExportScript({ bin: '/opt/bench/opencode', out: '/bench/out', sessionId: events.sessionId }),
        workdir: '/',
        env: opencodeEnv({ config }),
        mounts,
        network: 'none',
        timeoutMs: 5 * 60 * 1000,
      });
      exportText = e.exitCode === 0 ? readOr(path.join(out, 'export.json')) : '';
    }
    const secrets = [ctx.meter.token];
    const keep = path.join(ctx.paths.run, 'opencode');
    saveRaw(keep, 'events.jsonl', eventsText, secrets);
    saveRaw(keep, 'stderr.txt', readOr(path.join(out, 'stderr.txt')).slice(-20_000), secrets);
    saveRaw(keep, 'export.json', exportText, secrets);

    const refusal = ctx.meter.refusal();
    const finished = exitText === '0' && events.errors === 0;
    let exit;
    if (r.timedOut) exit = 'timeout';
    else if (finished) exit = 'done';
    else if (refusal) exit = refusal === 'cap' ? 'stopped_by_cap' : 'budget_hit';
    else exit = 'failed';
    return { exit, wallMs, sessionId: events.sessionId, selfReported: events.selfReported, exportKept: exportText !== '', processExit: exitText === '' ? null : Number(exitText) };
  }

  async collect(ctx) {
    const { dir } = this.#names(ctx);
    const treeDir = path.join(ctx.paths.out, 'tree');
    await copyTreeOut({ containers: ctx.containers, image: ctx.task.image.ref, treePath: dir, outDir: treeDir });
    const patch = await patchOfTree({ treeDir, base: ctx.base, workRoot: path.dirname(ctx.paths.out) });
    fs.rmSync(treeDir, { recursive: true, force: true });
    return { patch, notes: {} };
  }

  async cleanup(ctx) {
    const { parent } = this.#names(ctx);
    try {
      await ctx.stack.workspaceDir(ctx.slot, parent, { remove: true });
    } finally {
      for (const d of [ctx.paths.in, ctx.paths.out]) fs.rmSync(d, { recursive: true, force: true });
    }
  }
}
