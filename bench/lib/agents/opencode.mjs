// The OpenCode driver. OpenCode V2 (`@opencode/cli` 2.x: a pinned binary from npm, checked against its integrity hash
// and mounted read-only) runs in a container of the task's own image on the host's Docker, on the slot's network, in a
// clone of the same base at the same kind of path, with the statement on stdin and the meter as its only way to the
// model. It runs as uid 10001 and under the same limits as the coder (lib/runtime.mjs).
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
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { PermanentError, summarize, withStderr } from '../errors.mjs';
import { SAFE_DIRECTORY_LINES } from '../gitops.mjs';
import { glibcMounts, lacksLoader } from '../glibc.mjs';
import { RUN_ID_MAX } from '../plan.mjs';
import { fetchPinnedBinary } from '../npm.mjs';
import { redact } from '../stack.mjs';
import { runtimeLimits, runtimeUser } from '../runtime.mjs';
import { InfraError, patchOfTree, saveRaw } from './common.mjs';

export const MODEL = 'bench/bench-model';
const METER_URL = /^http:\/\/meter-\d:8090\/r\/[A-Za-z0-9_-]{16,128}\/v1$/;
const SAFE_PATH = /^\/[A-Za-z0-9._/@+=-]*$/;
// The title is `<run id>-a<attempt>`: the longest run id, `-a` and up to six digits.
const SAFE_RUN = new RegExp(`^[A-Za-z0-9._-]{1,${RUN_ID_MAX + 8}}$`);
const SESSION = /^ses_[A-Za-z0-9]{6,64}$/;
const OUTPUT_TOKENS = 8192;
export const SHARED_PROMPT_FILE = 'shared-prompt.md';
const OUT_FILES = ['events.jsonl', 'stderr.txt', 'export.json'];

/** The OpenCode configuration of a run (`OPENCODE_CONFIG_CONTENT`). Pure. */
export function opencodeConfig({ baseUrl, contextWindow, steps = 200, urlPattern = METER_URL, instructions = [] }) {
  if (!urlPattern.test(baseUrl ?? '')) throw new Error('the model address must be the meter\'s: http://meter-<slot>:8090/r/<run token>/v1');
  if (!Number.isSafeInteger(contextWindow) || contextWindow < 1) throw new Error('the context window is required: both agents are given the same one');
  return {
    $schema: 'https://opencode.ai/config.json',
    autoupdate: false,
    share: 'disabled',
    // The shared prompt (lib/shared-prompt.mjs): a file in the run's read-only input directory, added to OpenCode's own system prompt.
    ...(instructions.length > 0 ? { instructions } : {}),
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

/**
 * The container name of an attempt: at most 61 characters (the name of Docker is 63 and the export container adds `-x`),
 * the run id when it fits and else its start and a hash of the whole id, so two long ids never share a name. Pure.
 */
export function containerName(run, attempt) {
  const safe = String(run).replace(/[^A-Za-z0-9._-]/g, '-');
  const head = 'adam-bench-oc-';
  const tail = `-a${attempt}`;
  const room = 61 - head.length - tail.length;
  if (safe.length <= room) return `${head}${safe}${tail}`;
  const hash = crypto.createHash('sha256').update(String(run)).digest('hex').slice(0, 8);
  return `${head}${safe.slice(0, room - hash.length - 1)}-${hash}${tail}`;
}

/** The shell that clones the base into the run's directory, in the task image, as the runtime user. Pure. */
export function opencodeCloneScript({ repo, dir }) {
  if (!SAFE_PATH.test(repo) || !SAFE_PATH.test(dir)) throw new Error('a path in the OpenCode clone script is not a plain absolute path');
  return ['set -eu', ...SAFE_DIRECTORY_LINES, `git clone -q --no-hardlinks ${repo} ${dir}`, `git -C ${dir} checkout -q main`].join('\n');
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

const STDERR_CAP = 1048576;

/**
 * The shell of the run. OpenCode runs in the background under a TERM trap (the polite stop of the time limit reaches it
 * and exits 143), its stderr goes through a pipe that keeps the first megabyte and drops the rest, and the script's
 * exit status is OpenCode's. Pure.
 */
export function opencodeRunScript({ bin, stmt, out, dir, run }) {
  plain(bin, stmt, out, dir);
  if (!SAFE_RUN.test(run)) throw new PermanentError('the run id is not plain');
  return [
    ...prelude(out),
    `E=${out}/.stderr.fifo`,
    'rm -f "$E"; mkfifo "$E" || exit 97',
    `( head -c ${STDERR_CAP} > ${out}/stderr.txt; cat > /dev/null ) < "$E" &`,
    'DRAIN=$!',
    `cd ${dir} || exit 98`,
    `${bin} run --standalone --format json --model ${MODEL} --auto --title '${run}' < ${stmt} > ${out}/events.jsonl 2> "$E" &`,
    'OC=$!',
    `trap 'kill -TERM "$OC" 2>/dev/null' TERM INT`,
    'wait "$OC"; S=$?',
    'if kill -0 "$OC" 2>/dev/null; then wait "$OC"; S=$?; fi',
    'wait "$DRAIN" 2>/dev/null',
    'exit $S',
  ].join('\n');
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
      // One definition of `input` for both agents and the meter: every input token, the cached ones included (`cached` is a
      // part of it). OpenCode's own `tokens.input` leaves the cache reads and writes out (7,488 against 43,904 cached in a
      // pilot run, where adam's counts both), so they are added back here.
      const num = (v) => (Number.isFinite(v) ? v : 0);
      tokens.input += num(t.input) + num(t.cache?.read) + num(t.cache?.write);
      tokens.output += num(t.output);
      tokens.cached += num(t.cache?.read);
    }
  }
  return { sessionId, errors, retries, backgroundWaits, selfReported: { ...tokens, calls } };
}

const sha512 = (file) => crypto.createHash('sha512').update(fs.readFileSync(file)).digest('hex');
const text = (files, name) => (files.has(name) ? files.get(name).toString('utf8') : '');

export class OpenCodeAgent {
  constructor({ name = 'opencode', pins, home, fetchBinary = fetchPinnedBinary, now = Date.now, contextWindow, pollMs = 10_000, sharedPrompt = null }) {
    Object.assign(this, { name, pins, home, fetchBinary, now, contextWindow, pollMs, sharedPrompt });
    this.binary = null;
    this.binarySha = null;
    this.glibcDir = null;
    this.lacking = new Map();
  }

  #names(ctx) {
    const safe = ctx.run.replace(/[^A-Za-z0-9._-]/g, '-');
    const parent = `/work/workspaces/oc-${safe}-a${ctx.attempt}`;
    return { parent, dir: `${parent}/${ctx.base.slug}`, out: `/work/bench-out/oc-${safe}-a${ctx.attempt}`, inDir: path.join(this.slotDir(ctx), `in-${safe}-a${ctx.attempt}`), container: containerName(ctx.run, ctx.attempt) };
  }

  slotDir(ctx) {
    return ctx.stack.slotDir(ctx.slot);
  }

  /** An empty directory of the harness's own, mounted read-only over the image's global skill directories. */
  #globals(ctx) {
    return path.join(this.slotDir(ctx), 'no-globals');
  }

  /** Does the task image need the harness's glibc? Asked once for each image, and the runtime is only read from the stack then. */
  async #needsGlibc(ctx) {
    const image = ctx.task.image.ref;
    if (!this.lacking.has(image)) this.lacking.set(image, await lacksLoader({ containers: ctx.containers, image }));
    if (this.lacking.get(image)) this.glibcDir ??= await ctx.stack.ensureGlibc();
    return this.lacking.get(image);
  }

  async prepare(ctx) {
    this.binary ??= await this.fetchBinary({ pin: this.pins.opencode, dir: path.join(this.home, 'opencode', this.pins.opencode.version) });
    const names = this.#names(ctx);
    // What the service gets from the harness: the binary and the statement, in the slot's directory, which it mounts read-only.
    const bin = path.join(this.slotDir(ctx), 'bin');
    fs.mkdirSync(bin, { recursive: true, mode: 0o755 });
    const target = path.join(bin, 'opencode');
    // What is in the slot is the verified binary or it is replaced: compared by hash, never by size.
    this.binarySha ??= sha512(this.binary.path);
    if (!fs.existsSync(target) || sha512(target) !== this.binarySha) {
      fs.rmSync(target, { force: true });
      fs.copyFileSync(this.binary.path, target);
      fs.chmodSync(target, 0o555);
    }
    // The image's global skill directories are hidden from the clone's upward search: the run mounts this empty
    // directory, read-only, over `/.claude` and `/.opencode`. A repository's own `.claude/skills` inside the clone is
    // its own text and stays visible.
    const globals = this.#globals(ctx);
    fs.rmSync(globals, { recursive: true, force: true });
    fs.mkdirSync(globals, { recursive: true, mode: 0o755 });
    fs.rmSync(names.inDir, { recursive: true, force: true });
    fs.mkdirSync(names.inDir, { recursive: true, mode: 0o755 });
    fs.writeFileSync(path.join(names.inDir, 'instruction.md'), ctx.task.instruction, { mode: 0o644 });
    if (this.sharedPrompt) fs.writeFileSync(path.join(names.inDir, SHARED_PROMPT_FILE), this.sharedPrompt, { mode: 0o644 });
    for (const d of [names.parent, names.out]) {
      await ctx.stack.workDir(ctx.slot, d, { remove: true }); // a killed attempt may have left it
      await ctx.stack.workDir(ctx.slot, d);
    }
    const r = await ctx.containers.run({
      image: ctx.task.image.ref,
      script: opencodeCloneScript({ repo: ctx.base.containerRepo, dir: names.dir }),
      workdir: '/',
      user: runtimeUser(),
      mounts: [{ source: ctx.stack.workRoot(ctx.slot), target: '/work' }],
      network: 'none',
      timeoutMs: 10 * 60 * 1000,
      labels: { 'bench.slot': String(ctx.slot), 'bench.run': names.container.slice(14) },
    });
    if (r.exitCode !== 0) throw new PermanentError(`the clone for OpenCode failed (exit ${r.exitCode}): ${summarize(r.stderr)}`, { stderr: r.stderr });
    await this.#needsGlibc(ctx); // found here, before the run, and not in the middle of it
  }

  async run(ctx) {
    const names = this.#names(ctx);
    const config = opencodeConfig({ baseUrl: ctx.meter.baseUrl, contextWindow: this.contextWindow, steps: ctx.limits.steps, instructions: this.sharedPrompt ? [`/bench/in/${SHARED_PROMPT_FILE}`] : [] });
    const input = opencodeInput({ config });
    const globals = this.#globals(ctx);
    const outHost = path.join(ctx.stack.workRoot(ctx.slot), 'bench-out', path.basename(names.out));
    const mounts = [
      { source: path.join(this.slotDir(ctx), 'bin', 'opencode'), target: '/opt/bench/opencode', readOnly: true },
      { source: globals, target: '/.claude', readOnly: true },
      { source: globals, target: '/.opencode', readOnly: true },
      { source: names.inDir, target: '/bench/in', readOnly: true },
      { source: outHost, target: '/bench/out' },
      { source: ctx.stack.workRoot(ctx.slot), target: '/work' },
      ...(await this.#needsGlibc(ctx) ? glibcMounts(this.glibcDir) : []),
    ];
    const base = { image: ctx.task.image.ref, workdir: '/', user: runtimeUser(), env: opencodeEnv(), mounts, input, ...runtimeLimits(), labels: { 'bench.slot': String(ctx.slot), 'bench.run': names.container.slice(14) } };
    // The image the container runs is the task's, by digest: asserted here, before the container starts.
    if (!/@sha256:[0-9a-f]{64}$/.test(base.image)) throw new InfraError('no-task-container', 'the task image is not named by digest');
    // The end of a run, for the time limit and for a refusal of the meter (OpenCode retries a refused call, so it would
    // not end by itself): a polite stop (SIGTERM), then the configured grace, then a forced stop. The same grace adam gets.
    let ended = null;
    let graceTimer = null;
    const end = (why) => {
      if (ended) return;
      ended = why;
      ctx.containers.kill(names.container, 'TERM').catch(() => {});
      graceTimer = setTimeout(() => ctx.containers.stop(names.container).catch(() => {}), ctx.limits.cancelGraceMs);
    };
    const limitTimer = setTimeout(() => end('timeout'), ctx.limits.limitMs);
    const watcher = setInterval(() => {
      if (ctx.meter.refusal()) end('refusal');
    }, this.pollMs);
    const started = this.now();
    let r;
    try {
      r = await ctx.containers.run({ ...base, name: names.container, script: opencodeRunScript({ bin: '/opt/bench/opencode', stmt: '/bench/in/instruction.md', out: '/bench/out', dir: names.dir, run: `${ctx.run}-a${ctx.attempt}`.replace(/[^A-Za-z0-9._-]/g, '-') }), network: 'slot' });
    } finally {
      clearTimeout(limitTimer);
      clearTimeout(graceTimer);
      clearInterval(watcher);
    }
    const wallMs = this.now() - started;
    let got = await ctx.stack.readFiles(ctx.slot, names.out, OUT_FILES);
    const eventsText = text(got.files, 'events.jsonl');
    const events = parseEvents(eventsText);
    const secrets = [ctx.meter.token];
    const keep = path.join(ctx.paths.run, 'opencode');
    if (ended === null && eventsText === '' && [125, 126, 127].includes(r.exitCode)) {
      // The shell's own message ("not found") goes to the pipe that becomes stderr.txt, not to the container's stderr: it is
      // read from there, kept in the run's directory and carried to `prepare-stderr.txt`, as a failed prepare is.
      const said = [r.stderr, text(got.files, 'stderr.txt')].filter((x) => x && x.trim() !== '').join('\n');
      for (const name of OUT_FILES) if (got.files.has(name)) saveRaw(keep, name, text(got.files, name).slice(name === 'stderr.txt' ? -20_000 : 0), secrets);
      throw withStderr(new InfraError('runtime', `the OpenCode container did not start (exit ${r.exitCode}): ${summarize(redact(said, secrets))}`), redact(said, secrets));
    }
    if (events.sessionId) {
      const e = await ctx.containers.run({ ...base, name: `${names.container}-x`, script: opencodeExportScript({ bin: '/opt/bench/opencode', out: '/bench/out', sessionId: events.sessionId }), network: 'none', timeoutMs: 5 * 60 * 1000 });
      if (e.exitCode === 0) got = await ctx.stack.readFiles(ctx.slot, names.out, OUT_FILES);
    }
    for (const name of OUT_FILES) saveRaw(keep, name, text(got.files, name).slice(name === 'stderr.txt' ? -20_000 : 0), secrets);
    if (got.refused.length > 0) {
      ctx.log({ event: 'refused_output', names: got.refused });
      saveRaw(keep, 'refused.json', `${JSON.stringify(got.refused)}\n`);
    }

    const refusal = ctx.meter.refusal();
    let exit;
    if (ended === 'refusal') exit = refusal === 'cap' ? 'stopped_by_cap' : 'budget_hit';
    else if (ended === 'timeout') exit = 'timeout'; // the harness's own clock, before any status
    else if (r.exitCode === 0 && events.errors === 0) exit = 'done';
    else if (refusal) exit = refusal === 'cap' ? 'stopped_by_cap' : 'budget_hit';
    else exit = 'failed';
    return { exit, wallMs, sessionId: events.sessionId, selfReported: events.selfReported, retries: events.retries, backgroundWaits: events.backgroundWaits, exportKept: got.files.has('export.json'), processExit: r.exitCode, record: { limits: 'runtime', outputRefused: got.refused } };
  }

  /** The container ran with --rm and has ended; nothing of the run may be left on the host. */
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
    const ex = await ctx.stack.extractTree(ctx.slot, names.dir, treeDir);
    const patch = await patchOfTree({ treeDir, base: ctx.base, workRoot: ctx.paths.scratch });
    fs.rmSync(treeDir, { recursive: true, force: true });
    return { patch, notes: { refused: ex?.refused ?? [] } };
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
