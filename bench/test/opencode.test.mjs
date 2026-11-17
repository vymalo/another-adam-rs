import assert from 'node:assert/strict';
import { execFile, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { OpenCodeAgent, opencodeConfig, opencodeEnv, opencodeExportScript, opencodeRunScript, parseEvents, MODEL } from '../lib/agents/opencode.mjs';
import { FakeContainers } from '../lib/containers.mjs';
import { IntegrityError, sri } from '../lib/npm.mjs';
import { PINS } from '../lib/pins.mjs';
import { createScriptedModel } from '../lib/scripted-model.mjs';
import { loadTask } from '../lib/task.mjs';
import { commitFiles, initRepo, git } from './gitfix.mjs';
import { tmp } from './helpers.mjs';
import { makeTask } from './taskfix.mjs';

const URL = 'http://meter:8090/r/RUNTOKEN0123456789abcdef/v1';

// ---- the configuration ----

test('the permission denies the web and anything outside the work tree, and allows the rest', () => {
  const c = opencodeConfig({ baseUrl: URL, contextWindow: 131072 });
  assert.deepEqual(c.permission, { '*': 'allow', webfetch: 'deny', websearch: 'deny', external_directory: 'deny', question: 'deny' });
  assert.equal(c.permission.webfetch, 'deny');
  assert.equal(c.permission.websearch, 'deny');
  assert.equal(c.permission.external_directory, 'deny');
});

test('the configuration: the meter\'s address, the model limits, the same model for the small calls, 200 steps, no updater, no sharing', () => {
  const c = opencodeConfig({ baseUrl: URL, contextWindow: 65536 });
  assert.equal(c.autoupdate, false);
  assert.equal(c.share, 'disabled');
  assert.equal(c.model, 'bench/bench-model');
  assert.equal(c.small_model, 'bench/bench-model');
  assert.equal(c.agent.build.steps, 200);
  const p = c.provider.bench;
  assert.equal(p.npm, '@ai-sdk/openai-compatible');
  assert.equal(p.options.baseURL, URL);
  assert.equal(p.options.apiKey, 'bench-dummy-key', 'the model\'s key is the meter\'s, never here');
  assert.deepEqual(p.models['bench-model'].limit, { context: 65536, output: 8192 });
  assert.equal(p.models['bench-model'].tool_call, true);
  assert.equal(MODEL, 'bench/bench-model');
  assert.equal(opencodeConfig({ baseUrl: URL }).provider.bench.models['bench-model'].limit.context, 131072);
});

test('the address must be the meter\'s plain address with a run token', () => {
  assert.throws(() => opencodeConfig({ baseUrl: 'https://models.example.test/v1' }), /meter/);
  assert.throws(() => opencodeConfig({ baseUrl: 'http://meter:8090/v1' }), /meter/);
});

test('the environment turns off the model list fetch and the updater and carries the configuration', () => {
  const config = opencodeConfig({ baseUrl: URL });
  const env = opencodeEnv({ config });
  assert.equal(env.OPENCODE_DISABLE_MODELS_FETCH, 'true');
  assert.equal(env.OPENCODE_DISABLE_AUTOUPDATE, '1');
  assert.deepEqual(JSON.parse(env.OPENCODE_CONFIG_CONTENT), config);
  assert.deepEqual(Object.keys(env).sort(), ['OPENCODE_CONFIG_CONTENT', 'OPENCODE_DISABLE_AUTOUPDATE', 'OPENCODE_DISABLE_MODELS_FETCH']);
});

// ---- the command ----

const paths = { bin: '/opt/bench/opencode', stmt: '/bench/in/instruction.md', out: '/bench/out', dir: '/work/workspaces/oc-r1-a1/slot', run: 'r1-a1' };

test('the command: json events, the fixed model, auto-approve, the run as title, the directory, the statement on stdin', () => {
  const script = opencodeRunScript(paths);
  assert.match(script, /cd \/work\/workspaces\/oc-r1-a1\/slot \|\| exit 98\n\/opt\/bench\/opencode run --standalone --format json --model bench\/bench-model --auto --title 'r1-a1' < \/bench\/in\/instruction\.md > \/bench\/out\/events\.jsonl 2> \/bench\/out\/stderr\.txt/);
  assert.doesNotMatch(script, /--dir/);
  assert.match(script, /echo \$\? > \/bench\/out\/exit/);
  assert.doesNotMatch(script, /--variant|--session|--continue/);
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
  { type: 'step_finish', sessionID: 'ses_abc123XYZ', part: { tokens: { total: 110, input: 100, output: 10, reasoning: 3, cache: { write: 2, read: 40 } }, cost: 0 } },
  { type: 'step_finish', sessionID: 'ses_abc123XYZ', part: { tokens: { total: 60, input: 50, output: 10, reasoning: 0, cache: { write: 0, read: 7 } }, cost: 0 } },
  { type: 'text', sessionID: 'ses_abc123XYZ', part: { text: 'done' } },
];
const jsonl = (events) => `${events.map((e) => JSON.stringify(e)).join('\n')}\n`;

test('the events give the session, the steps and what OpenCode says it used', () => {
  const r = parseEvents(jsonl(EVENTS));
  assert.equal(r.sessionId, 'ses_abc123XYZ');
  assert.deepEqual(r.selfReported, { input: 150, output: 20, cached: 47, calls: 2 });
  assert.equal(r.errors, 0);
});

test('events that are not JSON, a session id that is not plain, and error events are handled', () => {
  const r = parseEvents(`not json\n${JSON.stringify({ type: 'error', sessionID: 'ses_xyz123', error: { name: 'UnknownError' } })}\n${JSON.stringify({ type: 'step_start', sessionID: '../etc' })}\n`);
  assert.equal(r.errors, 1);
  assert.equal(r.sessionId, 'ses_xyz123');
  assert.equal(parseEvents('').sessionId, null);
  assert.equal(parseEvents(jsonl([{ type: 'step_start', sessionID: 'ses_a;rm' }])).sessionId, null);
});

// ---- the driver, on a fake container service ----

function setup(t, { runResult = { exitCode: 0 }, exitFile = '0\n', binary = {} } = {}) {
  const home = tmp(t);
  const task = loadTask(makeTask(tmp(t), 'demo-1'));
  const stackCalls = [];
  const containers = new FakeContainers({
    onRun: (spec) => {
      const out = spec.mounts?.find((m) => m.target === '/bench/out')?.source;
      if (/ run --standalone --format json/.test(spec.script)) {
        fs.writeFileSync(path.join(out, 'events.jsonl'), jsonl(EVENTS));
        fs.writeFileSync(path.join(out, 'stderr.txt'), `warn ${URL}\n`);
        fs.writeFileSync(path.join(out, 'exit'), exitFile);
        return runResult;
      }
      if (/ session export /.test(spec.script)) fs.writeFileSync(path.join(out, 'export.json'), JSON.stringify({ info: { id: 'ses_abc123XYZ', url: URL }, messages: [] }));
      return { exitCode: 0 };
    },
  });
  const stack = {
    containers: () => containers,
    workspaceDir: async (slot, dir, o) => stackCalls.push(['workspaceDir', slot, dir, Boolean(o?.remove)]),
  };
  const fetched = [];
  const agent = new OpenCodeAgent({
    pins: { opencode: { ...PINS.opencode, ...binary } },
    home,
    fetchBinary: async ({ pin, dir }) => {
      fetched.push({ pin, dir });
      if (pin.integrity !== PINS.opencode.integrity) throw new IntegrityError('the downloaded package does not match the pinned integrity');
      return { path: path.join(dir, 'opencode'), cached: false, integrity: pin.integrity, version: pin.version };
    },
    now: (() => {
      let n = 0;
      return () => (n += 1000);
    })(),
  });
  const runDir = tmp(t);
  const ctx = {
    run: 'r1',
    attempt: 1,
    slot: 2,
    task,
    base: { dir: '/base/base.git', sha: 'a'.repeat(40), slug: 'demo-1-aaaaaa', workdir: '/app' },
    stack,
    containers,
    meter: { token: 'RUNTOKEN0123456789abcdef', baseUrl: URL, refusal: () => null },
    paths: { run: runDir, in: path.join(tmp(t), 'in'), out: path.join(tmp(t), 'out') },
    limitMs: 50 * 60 * 1000,
    log: () => {},
  };
  return { agent, ctx, containers, stackCalls, runDir, task, fetched };
}

test('prepare: the verified binary, the statement and a clone of the base in a directory of the volume', async (t) => {
  const { agent, ctx, containers, stackCalls } = setup(t);
  await agent.prepare(ctx);
  assert.deepEqual(stackCalls.slice(0, 2), [['workspaceDir', 2, '/work/workspaces/oc-r1-a1', true], ['workspaceDir', 2, '/work/workspaces/oc-r1-a1', false]]);
  assert.equal(fs.readFileSync(path.join(ctx.paths.in, 'instruction.md'), 'utf8'), 'Make the thing work.\n', 'the statement, byte for byte');
  const clone = containers.runs[0];
  assert.equal(clone.network, 'none');
  assert.match(clone.script, /git clone -q --no-hardlinks \/base\/base\.git \/work\/workspaces\/oc-r1-a1\/demo-1-aaaaaa/);
  assert.deepEqual(clone.mounts.map((m) => [m.source, m.target, Boolean(m.readOnly)]).sort(), [['/base', '/base', true], ['/work/workspaces/oc-r1-a1', '/work/workspaces/oc-r1-a1', false]].sort());
});

test('prepare: a binary that does not match its pinned integrity stops the run before any container starts', async (t) => {
  const { agent, ctx, containers, fetched } = setup(t, { binary: { integrity: sri(Buffer.from('some other package')) } });
  await assert.rejects(agent.prepare(ctx), IntegrityError);
  assert.equal(containers.runs.length, 0);
  assert.equal(fetched.length, 1);
});

test('prepare: the binary is the pinned one, kept under one directory of the harness\'s home', async (t) => {
  const { agent, ctx, fetched, ctx: _ } = setup(t);
  await agent.prepare(ctx);
  await agent.prepare(ctx);
  assert.equal(fetched.length, 1, 'fetched once for the harness');
  assert.deepEqual(fetched[0].pin, PINS.opencode);
  assert.match(fetched[0].dir, new RegExp(`/opencode/${PINS.opencode.version}$`));
});

test('run: the container of the task image, the binary read-only, the statement on stdin, the meter\'s network and the time limit', async (t) => {
  const { agent, ctx, containers, runDir } = setup(t);
  await agent.prepare(ctx);
  const result = await agent.run(ctx);
  const spec = containers.runs.find((s) => / run --standalone --format json/.test(s.script));
  assert.equal(spec.image, ctx.task.image.ref);
  assert.equal(spec.network, 'default', 'the Podman service\'s own network: the meter and nothing else');
  assert.equal(spec.timeoutMs, 50 * 60 * 1000);
  assert.equal(spec.workdir, '/work/workspaces/oc-r1-a1/demo-1-aaaaaa');
  const mounts = Object.fromEntries(spec.mounts.map((m) => [m.target, m]));
  assert.equal(mounts['/opt/bench/opencode'].readOnly, true);
  assert.equal(mounts['/bench/in'].readOnly, true);
  assert.equal(mounts['/bench/out'].readOnly, undefined);
  assert.equal(JSON.parse(spec.env.OPENCODE_CONFIG_CONTENT).provider.bench.options.baseURL, URL);
  assert.equal(result.exit, 'done');
  assert.equal(result.sessionId, 'ses_abc123XYZ');
  assert.deepEqual(result.selfReported, { input: 150, output: 20, cached: 47, calls: 2 });
  assert.ok(result.wallMs > 0);
  const exp = containers.runs.find((s) => / session export /.test(s.script));
  assert.match(exp.script, /session export --standalone 'ses_abc123XYZ'/);
  assert.equal(exp.network, 'none');
  assert.ok(fs.existsSync(path.join(runDir, 'opencode', 'export.json')), 'the export is kept as JSON');
  assert.equal(JSON.parse(fs.readFileSync(path.join(runDir, 'opencode', 'export.json'), 'utf8')).info.id, 'ses_abc123XYZ');
});

test('run: the run token is in none of the files kept', async (t) => {
  const { agent, ctx, runDir } = setup(t);
  await agent.prepare(ctx);
  await agent.run(ctx);
  for (const f of ['events.jsonl', 'export.json', 'stderr.txt']) {
    const text = fs.readFileSync(path.join(runDir, 'opencode', f), 'utf8');
    assert.ok(!text.includes('RUNTOKEN0123456789abcdef'), f);
  }
  assert.match(fs.readFileSync(path.join(runDir, 'opencode', 'stderr.txt'), 'utf8'), /\[redacted\]/);
});

test('run: the exit is timeout, budget_hit, stopped_by_cap, failed or done, as the container and the meter say', async (t) => {
  const exits = async ({ runResult, refusal = null, exitFile = '0\n' }) => {
    const { agent, ctx } = setup(t, { runResult, exitFile });
    ctx.meter.refusal = () => refusal;
    await agent.prepare(ctx);
    return (await agent.run(ctx)).exit;
  };
  assert.equal(await exits({ runResult: { exitCode: 0 } }), 'done');
  assert.equal(await exits({ runResult: { exitCode: 137, timedOut: true }, exitFile: '' }), 'timeout');
  assert.equal(await exits({ runResult: { exitCode: 0 }, refusal: 'budget', exitFile: '1\n' }), 'budget_hit');
  assert.equal(await exits({ runResult: { exitCode: 0 }, refusal: 'cap', exitFile: '1\n' }), 'stopped_by_cap');
  assert.equal(await exits({ runResult: { exitCode: 0 }, exitFile: '1\n' }), 'failed');
  assert.equal(await exits({ runResult: { exitCode: 0 }, refusal: 'budget', exitFile: '0\n' }), 'done', 'a refusal the agent got past is not the run\'s end');
});

test('run: a container that did not start is an infrastructure error, not a failed run', async (t) => {
  const { agent, ctx } = setup(t, { runResult: { exitCode: 125, stderr: 'Error: image not known\n' } });
  await agent.prepare(ctx);
  fs.rmSync(path.join(ctx.paths.out, 'exit'), { force: true });
  const real = ctx.containers.onRun;
  ctx.containers.onRun = (spec, i) => (/ run --standalone --format json/.test(spec.script) ? { exitCode: 125, stderr: 'Error: image not known\n' } : real(spec, i));
  await assert.rejects(agent.run(ctx), /did not start \(exit 125\)/);
});

test('cleanup removes the directory the run made on the volume, and its own scratch', async (t) => {
  const { agent, ctx, stackCalls } = setup(t);
  await agent.prepare(ctx);
  await agent.cleanup(ctx);
  assert.deepEqual(stackCalls.at(-1), ['workspaceDir', 2, '/work/workspaces/oc-r1-a1', true]);
  assert.equal(fs.existsSync(ctx.paths.in), false);
});

// ---- the real binary, on this machine, against the scripted model (no container, no model, no network) ----

const BIN = process.env.BENCH_TEST_OPENCODE_BIN;
const live = BIN ? false : 'BENCH_TEST_OPENCODE_BIN is not set (a path to the pinned OpenCode binary)';

test('the real OpenCode, with the command and configuration this driver builds, does the scripted task and exports its session', { skip: live }, async (t) => {
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
  const config = opencodeConfig({ baseUrl: `http://127.0.0.1:${port}/v1`, urlPattern: /^http:\/\/127\.0\.0\.1:\d+\/v1$/ });
  const env = { PATH: process.env.PATH, ...opencodeEnv({ config }) };
  const script = opencodeRunScript({ bin: BIN, stmt, out, dir, run: 'live-1' });
  const ran = await new Promise((resolve) => execFile('sh', ['-c', script], { env, encoding: 'utf8', timeout: 120_000 }, (err, stdout, stderr) => resolve({ status: err ? (err.code ?? 1) : 0, stdout, stderr })));
  assert.equal(ran.status, 0, ran.stderr);
  assert.equal(fs.readFileSync(path.join(out, 'exit'), 'utf8').trim(), '0', fs.readFileSync(path.join(out, 'stderr.txt'), 'utf8').slice(-600) + fs.readFileSync(path.join(out, 'events.jsonl'), 'utf8').slice(-600));
  assert.equal(fs.readFileSync(path.join(dir, 'hello.txt'), 'utf8'), 'hello\n');
  const events = parseEvents(fs.readFileSync(path.join(out, 'events.jsonl'), 'utf8'));
  assert.match(events.sessionId, /^ses_/);
  assert.equal(events.errors, 0);
  assert.equal(events.selfReported.calls, 3);
  assert.equal(model.requests.length, 3, 'two steps and the answer: no title call');
  assert.ok(model.requests.every((r) => r.body.model === 'bench-model' || r.body.model === 'example-model' || typeof r.body.model === 'string'));
  assert.ok(model.requests.every((r) => r.body.tools.some((x) => x.function.name === 'write')));
  const user = model.requests[0].body.messages.find((m) => m.role === 'user').content;
  assert.equal(user, 'Write hello.txt containing hello. [mock:hello]\n', 'the statement reaches the model unchanged');
  void git;
  const exported = await new Promise((resolve) => execFile('sh', ['-c', opencodeExportScript({ bin: BIN, out, sessionId: events.sessionId })], { env, encoding: 'utf8', timeout: 60_000 }, (err) => resolve(err ? 1 : 0)));
  assert.equal(exported, 0);
  assert.equal(JSON.parse(fs.readFileSync(path.join(out, 'export.json'), 'utf8')).info.id, events.sessionId, 'the session is exported by a second process');
});

test('run: a refusal of the meter while OpenCode retries ends the container early, and the run is budget_hit, not a timeout', async (t) => {
  const { agent, ctx, containers } = setup(t, { runResult: { exitCode: 137, timedOut: true }, exitFile: '' });
  agent.pollMs = 5;
  let release;
  const held = new Promise((r) => (release = r));
  const real = containers.onRun;
  containers.onRun = async (spec, i) => {
    if (/ run --standalone --format json/.test(spec.script)) {
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

test('the events count retries and the waits for background jobs that OpenCode V2 reports', () => {
  const r = parseEvents(`${JSON.stringify({ type: 'retry', sessionID: 'ses_abc123' })}\n${JSON.stringify({ type: 'background_wait', sessionID: 'ses_abc123' })}\n`);
  assert.deepEqual([r.retries, r.backgroundWaits], [1, 1]);
});
