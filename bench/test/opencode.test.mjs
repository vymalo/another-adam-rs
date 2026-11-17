import assert from 'node:assert/strict';
import { execFile, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { InfraError } from '../lib/agents/common.mjs';
import { MODEL, OpenCodeAgent, opencodeConfig, opencodeEnv, opencodeExportScript, opencodeInput, opencodeRunScript, parseEvents } from '../lib/agents/opencode.mjs';
import { FakeContainers } from '../lib/containers.mjs';
import { IntegrityError } from '../lib/npm.mjs';
import { PINS } from '../lib/pins.mjs';
import { createScriptedModel } from '../lib/scripted-model.mjs';
import { loadTask } from '../lib/task.mjs';
import { commitFiles, initRepo } from './gitfix.mjs';
import { tmp } from './helpers.mjs';
import { makeTask } from './taskfix.mjs';

const TOKEN = 'RUNTOKEN0123456789abcdef';
const URL = `http://meter-2:8090/r/${TOKEN}/v1`;
const LIMITS = { limitMs: 50 * 60 * 1000, steps: 150, cancelGraceMs: 30_000 };

// ---- the configuration ----

test('the permission denies the web, anything outside the work tree and every question', () => {
  const c = opencodeConfig({ baseUrl: URL, contextWindow: 131072 });
  assert.deepEqual(c.permission, { '*': 'allow', webfetch: 'deny', websearch: 'deny', external_directory: 'deny', question: 'deny' });
});

test('the configuration: the meter\'s address, the model limits, the same model for the small calls, the configured steps, no updater, no sharing', () => {
  const c = opencodeConfig({ baseUrl: URL, contextWindow: 65536, steps: 90 });
  assert.equal(c.autoupdate, false);
  assert.equal(c.share, 'disabled');
  assert.equal(c.model, MODEL);
  assert.equal(c.small_model, MODEL);
  assert.equal(c.agent.build.steps, 90, 'the step limit is the configured one, the turn limit adam gets');
  const p = c.provider.bench;
  assert.equal(p.npm, '@ai-sdk/openai-compatible');
  assert.equal(p.options.baseURL, URL);
  assert.equal(p.options.apiKey, 'bench-dummy-key', 'the model\'s key is the meter\'s, never here');
  assert.deepEqual(p.models['bench-model'].limit, { context: 65536, output: 8192 });
});

test('the context window is required, and the address must be a meter relay\'s with a run token', () => {
  assert.throws(() => opencodeConfig({ baseUrl: URL }), /context window is required/);
  assert.throws(() => opencodeConfig({ baseUrl: URL, contextWindow: null }), /context window is required/);
  assert.throws(() => opencodeConfig({ baseUrl: 'https://models.example.test/v1', contextWindow: 1024 }), /meter/);
  assert.throws(() => opencodeConfig({ baseUrl: 'http://meter:8090/r/abcdefabcdefabcdef/v1', contextWindow: 1024 }), /meter/);
});

test('finding 13: the config, which holds the run token, goes in on stdin; the environment holds two switches and nothing secret', () => {
  const config = opencodeConfig({ baseUrl: URL, contextWindow: 131072 });
  assert.deepEqual(opencodeEnv(), { OPENCODE_DISABLE_MODELS_FETCH: 'true', OPENCODE_DISABLE_AUTOUPDATE: '1' });
  assert.deepEqual(JSON.parse(opencodeInput({ config })), config);
  assert.ok(opencodeInput({ config }).includes(TOKEN));
  assert.ok(!JSON.stringify(opencodeEnv()).includes(TOKEN));
});

// ---- the command ----

const paths = { bin: '/opt/bench/opencode', stmt: '/bench/in/instruction.md', out: '/bench/out', dir: '/work/workspaces/oc-r1-a1/slot', run: 'r1-a1' };

test('the command: the config from stdin, then OpenCode as the last command, so the container\'s exit status is its own; no exit file, no --dir', () => {
  const script = opencodeRunScript(paths);
  assert.match(script, /OPENCODE_CONFIG_CONTENT=\$\(cat\)\nexport OPENCODE_CONFIG_CONTENT\ncd \/work\/workspaces\/oc-r1-a1\/slot \|\| exit 98\n\/opt\/bench\/opencode run --standalone --format json --model bench\/bench-model --auto --title 'r1-a1' < \/bench\/in\/instruction\.md > \/bench\/out\/events\.jsonl 2> \/bench\/out\/stderr\.txt$/);
  assert.doesNotMatch(script, /--dir|echo \$\?|\/exit/);
});

test('the command takes only plain paths and a plain run id', () => {
  assert.throws(() => opencodeRunScript({ ...paths, run: "x'; rm -rf /; '" }), /plain/);
  assert.throws(() => opencodeRunScript({ ...paths, dir: '/work; rm' }), /plain/);
  assert.throws(() => opencodeRunScript({ ...paths, bin: '/opt/bench/opencode $(x)' }), /plain/);
});

// ---- events ----

const EVENTS = [
  { type: 'step_start', sessionID: 'ses_abc123XYZ', part: { type: 'step-start' } },
  { type: 'tool_use', sessionID: 'ses_abc123XYZ', part: { type: 'tool', tool: 'write' } },
  { type: 'step_finish', sessionID: 'ses_abc123XYZ', part: { tokens: { input: 100, output: 10, reasoning: 3, cache: { write: 2, read: 40 } }, cost: 0 } },
  { type: 'step_finish', sessionID: 'ses_abc123XYZ', part: { tokens: { input: 50, output: 10, reasoning: 0, cache: { write: 0, read: 7 } }, cost: 0 } },
  { type: 'text', sessionID: 'ses_abc123XYZ', part: { text: 'done' } },
];
const jsonl = (events) => `${events.map((e) => JSON.stringify(e)).join('\n')}\n`;

test('the events give the session, the steps and what OpenCode says it used', () => {
  const r = parseEvents(jsonl(EVENTS));
  assert.equal(r.sessionId, 'ses_abc123XYZ');
  assert.deepEqual(r.selfReported, { input: 150, output: 20, cached: 47, calls: 2 });
  assert.equal(r.errors, 0);
});

test('events that are not JSON, a session id that is not plain, and error, retry and wait events are handled', () => {
  const r = parseEvents(`not json\n${JSON.stringify({ type: 'error', sessionID: 'ses_xyz123' })}\n${JSON.stringify({ type: 'step_start', sessionID: '../etc' })}\n${JSON.stringify({ type: 'retry' })}\n${JSON.stringify({ type: 'background_wait' })}\n`);
  assert.deepEqual([r.errors, r.sessionId, r.retries, r.backgroundWaits], [1, 'ses_xyz123', 1, 1]);
  assert.equal(parseEvents('').sessionId, null);
  assert.equal(parseEvents(jsonl([{ type: 'step_start', sessionID: 'ses_a;rm' }])).sessionId, null);
});

// ---- the driver, on a fake container service ----

function setup(t, { runResult = { exitCode: 0 }, outFiles = null, refused = [], binary = {} } = {}) {
  const home = tmp(t);
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  const stackCalls = [];
  const out = { files: new Map(outFiles ?? [['events.jsonl', Buffer.from(jsonl(EVENTS))], ['stderr.txt', Buffer.from(`warn ${URL}\n`)], ['export.json', Buffer.from(JSON.stringify({ info: { id: 'ses_abc123XYZ', url: URL } }))]]), refused };
  const containers = new FakeContainers({ onRun: (spec) => (spec.script.includes(' run --standalone ') ? runResult : { exitCode: 0 }) });
  const slotDir = path.join(tmp(t), 'slots', '2');
  const stack = {
    slotDir: () => slotDir,
    workDir: async (slot, dir, o) => stackCalls.push(['workDir', slot, dir, Boolean(o?.remove)]),
    readFiles: async (slot, dir, names) => {
      stackCalls.push(['readFiles', slot, dir, names]);
      return out;
    },
    extractTree: async (slot, dir, dest) => {
      stackCalls.push(['extractTree', slot, dir]);
      fs.mkdirSync(dest, { recursive: true });
      fs.writeFileSync(path.join(dest, 'README.md'), 'demo\n');
      fs.writeFileSync(path.join(dest, 'hello.txt'), 'hello\n');
    },
  };
  const fetched = [];
  const real = path.join(tmp(t), 'opencode');
  fs.writeFileSync(real, '#!/bin/sh\n', { mode: 0o755 });
  const agent = new OpenCodeAgent({
    pins: { opencode: { ...PINS.opencode, ...binary } },
    home,
    contextWindow: 131072,
    fetchBinary: async ({ pin, dir }) => {
      fetched.push({ pin, dir });
      if (pin.integrity !== PINS.opencode.integrity) throw new IntegrityError('the downloaded package does not match the pinned integrity');
      return { path: real, cached: false, integrity: pin.integrity, version: pin.version };
    },
    now: (() => {
      let n = 0;
      return () => (n += 1000);
    })(),
    pollMs: 5,
  });
  const ctx = {
    run: 'r1',
    attempt: 1,
    slot: 2,
    task,
    base: { dir: '/host/base', sha: 'a'.repeat(40), slug: 'demo-1-aaaaaa', workdir: '/app', containerRepo: '/work/bench-base/base.git' },
    stack,
    containers,
    meter: { token: TOKEN, baseUrl: URL, refusal: () => null },
    paths: { run: tmp(t), scratch: path.join(tmp(t), 'scratch') },
    limits: LIMITS,
    contextWindow: 131072,
    log: () => {},
  };
  return { agent, ctx, containers, stackCalls, task, fetched, slotDir, out };
}

test('prepare: the verified binary and the statement in the slot\'s directory, and a clone of the base in a directory of the volume', async (t) => {
  const { agent, ctx, containers, stackCalls, slotDir } = setup(t);
  await agent.prepare(ctx);
  assert.equal(fs.statSync(path.join(slotDir, 'bin', 'opencode')).mode & 0o777, 0o555);
  assert.equal(fs.readFileSync(path.join(slotDir, 'in-r1-a1', 'instruction.md'), 'utf8'), 'Make the thing work.\n', 'the statement, byte for byte');
  assert.deepEqual(stackCalls.filter((c) => c[0] === 'workDir').map((c) => c.slice(2).join(' ')), ['/work/workspaces/oc-r1-a1 true', '/work/workspaces/oc-r1-a1 false', '/work/bench-out/oc-r1-a1 true', '/work/bench-out/oc-r1-a1 false']);
  const clone = containers.runs[0];
  assert.equal(clone.network, 'none');
  assert.match(clone.script, /git clone -q --no-hardlinks \/work\/bench-base\/base\.git \/work\/workspaces\/oc-r1-a1\/demo-1-aaaaaa/);
  assert.deepEqual(clone.mounts.map((m) => [m.source, m.target, Boolean(m.readOnly)]), [['/work/bench-base', '/work/bench-base', true], ['/work/workspaces/oc-r1-a1', '/work/workspaces/oc-r1-a1', false]]);
  assert.equal(clone.labels['bench.slot'], '2');
});

test('prepare: a binary that does not match its pinned integrity stops the run before any container starts', async (t) => {
  const { agent, ctx, containers } = setup(t, { binary: { integrity: `sha512-${'A'.repeat(86)}==` } });
  await assert.rejects(agent.prepare(ctx), IntegrityError);
  assert.equal(containers.runs.length, 0);
});

test('prepare: the binary is the pinned one, fetched once, under one directory of the harness\'s home', async (t) => {
  const { agent, ctx, fetched } = setup(t);
  await agent.prepare(ctx);
  await agent.prepare(ctx);
  assert.equal(fetched.length, 1);
  assert.deepEqual(fetched[0].pin, PINS.opencode);
  assert.match(fetched[0].dir, new RegExp(`/opencode/${PINS.opencode.version}$`));
});

test('run: the container of the task image by digest, the binary read-only, the config on stdin and nowhere else, the task\'s cpus and memory, a label, the time limit', async (t) => {
  const { agent, ctx, containers, task } = setup(t);
  await agent.prepare(ctx);
  const result = await agent.run(ctx);
  const spec = containers.runs.find((s) => s.script.includes(' run --standalone '));
  assert.equal(spec.image, task.image.ref);
  assert.equal(spec.network, 'default', 'the Podman service\'s own network: the meter relay and nothing else');
  assert.equal(spec.timeoutMs, LIMITS.limitMs);
  assert.deepEqual([spec.cpus, spec.memoryMb], [task.cpus, task.memoryMb]);
  assert.equal(spec.name, 'adam-bench-oc-r1-a1');
  assert.equal(spec.labels['bench.slot'], '2');
  assert.equal(JSON.parse(spec.input).provider.bench.options.baseURL, URL);
  assert.equal(JSON.parse(spec.input).agent.build.steps, 150);
  assert.ok(!JSON.stringify({ ...spec, input: undefined }).includes(TOKEN), 'the run token is on no command line, in no environment and in no script');
  const mounts = Object.fromEntries(spec.mounts.map((m) => [m.target, m]));
  assert.equal(mounts['/opt/bench/opencode'].readOnly, true);
  assert.equal(mounts['/bench/in'].readOnly, true);
  assert.equal(mounts['/bench/out'].source, '/work/bench-out/oc-r1-a1', 'the output is on the volume, where the agent writes and the host does not read');
  assert.equal(result.exit, 'done');
  assert.equal(result.sessionId, 'ses_abc123XYZ');
  assert.deepEqual(result.selfReported, { input: 150, output: 20, cached: 47, calls: 2 });
  const exp = containers.runs.find((s) => s.script.includes('session export'));
  assert.match(exp.script, /session export --standalone 'ses_abc123XYZ'/);
  assert.equal(exp.network, 'none');
  assert.equal(JSON.parse(exp.input).provider.bench.options.baseURL, URL);
});

test('run: the output is read through the stream reader with named files only, and kept as raw JSON without the run token', async (t) => {
  const { agent, ctx, stackCalls } = setup(t);
  await agent.prepare(ctx);
  await agent.run(ctx);
  const read = stackCalls.find((c) => c[0] === 'readFiles');
  assert.deepEqual(read.slice(2), ['/work/bench-out/oc-r1-a1', ['events.jsonl', 'stderr.txt', 'export.json']]);
  for (const f of ['events.jsonl', 'export.json', 'stderr.txt']) assert.ok(!fs.readFileSync(path.join(ctx.paths.run, 'opencode', f), 'utf8').includes(TOKEN), f);
  assert.match(fs.readFileSync(path.join(ctx.paths.run, 'opencode', 'stderr.txt'), 'utf8'), /\[redacted\]/);
  assert.equal(JSON.parse(fs.readFileSync(path.join(ctx.paths.run, 'opencode', 'export.json'), 'utf8')).info.id, 'ses_abc123XYZ');
});

test('finding 6: the exit is the runtime\'s: a file the agent wrote called exit is never read, and a time-out wins over any status', async (t) => {
  const forged = [...setup(t).out.files, ['exit', Buffer.from('0\n')]];
  const failed = setup(t, { runResult: { exitCode: 1 }, outFiles: forged });
  await failed.agent.prepare(failed.ctx);
  assert.equal((await failed.agent.run(failed.ctx)).exit, 'failed', 'the agent wrote 0 to exit; the container said 1');
  assert.ok(!failed.stackCalls.some((c) => c[0] === 'readFiles' && c[3].includes('exit')));

  const slow = setup(t, { runResult: { exitCode: 0, timedOut: true } });
  await slow.agent.prepare(slow.ctx);
  assert.equal((await slow.agent.run(slow.ctx)).exit, 'timeout', 'timed out, whatever the status says');
});

test('finding 6: a symbolic link, FIFO or device where a log should be is refused, logged and kept out of the results', async (t) => {
  const logs = [];
  const { agent, ctx, runDir } = (() => {
    const s = setup(t, { outFiles: [['stderr.txt', Buffer.from('x')]], refused: ['events.jsonl', 'export.json'] });
    s.ctx.log = (e) => logs.push(e);
    return { ...s, runDir: s.ctx.paths.run };
  })();
  await agent.prepare(ctx);
  const r = await agent.run(ctx);
  assert.deepEqual(r.refusedOutput, ['events.jsonl', 'export.json']);
  assert.deepEqual(logs.find((l) => l.event === 'refused_output').names, ['events.jsonl', 'export.json']);
  assert.equal(fs.readFileSync(path.join(runDir, 'opencode', 'refused.json'), 'utf8').trim(), '["events.jsonl","export.json"]');
  assert.equal(fs.readFileSync(path.join(runDir, 'opencode', 'events.jsonl'), 'utf8'), '');
});

test('run: the exit is budget_hit or stopped_by_cap when the meter refused, failed otherwise, and a container that did not start is infrastructure', async (t) => {
  const exits = async ({ refusal = null, runResult = { exitCode: 1 } }) => {
    const s = setup(t, { runResult });
    s.ctx.meter.refusal = () => refusal;
    await s.agent.prepare(s.ctx);
    return (await s.agent.run(s.ctx)).exit;
  };
  assert.equal(await exits({}), 'failed');
  assert.equal(await exits({ refusal: 'budget' }), 'budget_hit');
  assert.equal(await exits({ refusal: 'cap' }), 'stopped_by_cap');
  assert.equal(await exits({ refusal: 'budget', runResult: { exitCode: 0 } }), 'done', 'a refusal the agent got past is not the run\'s end');
  const dead = setup(t, { runResult: { exitCode: 125, stderr: 'Error: image not known\n' }, outFiles: [] });
  await dead.agent.prepare(dead.ctx);
  await assert.rejects(dead.agent.run(dead.ctx), (e) => e instanceof InfraError && e.reason === 'runtime');
});

test('run: a refusal of the meter while OpenCode retries ends the container early, and the run is budget_hit, not a timeout', async (t) => {
  const { agent, ctx, containers } = setup(t);
  let release;
  const held = new Promise((r) => (release = r));
  const real = containers.onRun;
  containers.onRun = async (spec, i) => {
    if (spec.script.includes(' run --standalone ')) {
      await held; // OpenCode retrying a refused call: it does not end by itself
      return { exitCode: 137, timedOut: true };
    }
    return real(spec, i);
  };
  await agent.prepare(ctx);
  ctx.meter.refusal = () => 'budget';
  const done = agent.run(ctx);
  await new Promise((r) => setTimeout(r, 60));
  assert.deepEqual(containers.stopped, ['adam-bench-oc-r1-a1'], 'stopped once, by name');
  release();
  assert.equal((await done).exit, 'budget_hit');
});

test('finding 11 and 9: stop reaps the service and confirms it, and a failure is infrastructure', async (t) => {
  const { agent, ctx, containers } = setup(t);
  await agent.stop(ctx);
  assert.equal(containers.reaped, 1);
  containers.reapAll = async () => {
    throw new Error('containers are left after the reap');
  };
  await assert.rejects(agent.stop(ctx), (e) => e instanceof InfraError && e.reason === 'not-stopped');
});

test('collect: the clone comes out as a tar stream into a directory the harness owns, and becomes the patch', async (t) => {
  const { agent, ctx, stackCalls } = setup(t);
  const app = initRepo(t);
  commitFiles(app, { 'README.md': 'demo\n' }, 'base');
  ctx.base = { ...ctx.base, dir: app, sha: execFileSync('git', ['-C', app, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim() };
  const r = await agent.collect(ctx);
  assert.deepEqual(stackCalls.find((c) => c[0] === 'extractTree').slice(1), [2, '/work/workspaces/oc-r1-a1/demo-1-aaaaaa']);
  assert.match(r.patch.toString('utf8'), /\+\+\+ b\/hello\.txt/);
});

test('finding 5: cleanup removes the volume\'s directories inside the service, reaps it, and removes only what the harness wrote', async (t) => {
  const { agent, ctx, stackCalls, containers, slotDir } = setup(t);
  await agent.prepare(ctx);
  await agent.cleanup(ctx);
  assert.deepEqual(stackCalls.filter((c) => c[0] === 'workDir').slice(-2).map((c) => c.slice(2).join(' ')), ['/work/workspaces/oc-r1-a1 true', '/work/bench-out/oc-r1-a1 true']);
  assert.equal(containers.reaped, 1);
  assert.equal(fs.existsSync(path.join(slotDir, 'in-r1-a1')), false);
});

// ---- the real binary, on this machine, against the scripted model (no container, no model, no network) ----

const BIN = process.env.BENCH_TEST_OPENCODE_BIN;
const live = BIN ? false : 'BENCH_TEST_OPENCODE_BIN is not set (a path to the pinned OpenCode binary)';
const sh = (script, { env, input }) => new Promise((resolve) => {
  const child = execFile('sh', ['-c', script], { env, encoding: 'utf8', timeout: 120_000 }, (err, stdout, stderr) => resolve({ status: err ? (err.code ?? 1) : 0, stdout, stderr }));
  child.stdin.end(input);
});

test('the real OpenCode, with the command, the configuration and the stdin of this driver, does the scripted task and its session is exported by a second process', { skip: live }, async (t) => {
  const model = createScriptedModel({ script: 'agents' });
  const { port } = await model.listen();
  t.after(() => model.close());
  const base = tmp(t);
  const work = tmp(t);
  const repo = initRepo(t);
  commitFiles(repo, { 'README.md': 'demo\n' }, 'base');
  const dir = path.join(work, 'clone');
  execFileSync('git', ['clone', '-q', repo, dir]);
  const out = tmp(t);
  const stmt = path.join(base, 'instruction.md');
  fs.writeFileSync(stmt, 'Write hello.txt containing hello. [mock:hello]\n');
  const config = opencodeConfig({ baseUrl: `http://127.0.0.1:${port}/v1`, contextWindow: 131072, urlPattern: /^http:\/\/127\.0\.0\.1:\d+\/v1$/ });
  const env = { PATH: process.env.PATH, ...opencodeEnv() };
  const input = opencodeInput({ config });
  const ran = await sh(opencodeRunScript({ bin: BIN, stmt, out, dir, run: 'live-1' }), { env, input });
  assert.equal(ran.status, 0, fs.readFileSync(path.join(out, 'stderr.txt'), 'utf8').slice(-600));
  assert.equal(fs.readFileSync(path.join(dir, 'hello.txt'), 'utf8'), 'hello\n');
  const events = parseEvents(fs.readFileSync(path.join(out, 'events.jsonl'), 'utf8'));
  assert.match(events.sessionId, /^ses_/);
  assert.equal(events.errors, 0);
  assert.equal(events.selfReported.calls, 3);
  assert.equal(model.requests.length, 3, 'two steps and the answer: no title call');
  assert.equal(model.requests[0].body.messages.find((m) => m.role === 'user').content, 'Write hello.txt containing hello. [mock:hello]\n', 'the statement reaches the model unchanged');
  const exported = await sh(opencodeExportScript({ bin: BIN, out, sessionId: events.sessionId }), { env, input });
  assert.equal(exported.status, 0);
  assert.equal(JSON.parse(fs.readFileSync(path.join(out, 'export.json'), 'utf8')).info.id, events.sessionId);
});
