import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { GLIBC_FILES } from '../lib/glibc.mjs';
import { PINS, coderRef } from '../lib/pins.mjs';
import { RUNTIME } from '../lib/runtime.mjs';
import { Stack, StackError, TREE_READ_TIMEOUT_MS, TREE_REMOVE_TIMEOUT_MS, coderEnvironment, redact, secretsOf, serviceNames } from '../lib/stack.mjs';
import { tmp } from './helpers.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const COMPOSE = path.join(HERE, '..', 'compose.yaml');
const DIGEST = `sha256:${'a'.repeat(64)}`;
const TASK_IMAGE = `registry.example.test/task@${DIGEST}`;

function makeStack(t, { exec, execBuf, baseEnv } = {}) {
  const home = tmp(t);
  const calls = [];
  const logs = [];
  const stack = new Stack({
    composeFile: COMPOSE,
    home,
    meterPort: 18090,
    agentDir: '/h/agent',
    exec: exec ?? (async (file, args, opts) => {
      calls.push({ file, args, opts, envFile: snapshotEnvFiles(home) });
      // The verifier network is the one the harness makes; an existing one is read for its internal flag.
      if (args[0] === 'network' && args[1] === 'inspect') return { exitCode: 0, stdout: 'true\n', stderr: '' };
      return { exitCode: 0, stdout: '', stderr: '' };
    }),
    execBuf,
    log: (e) => logs.push(e),
    baseEnv: baseEnv ?? { PATH: '/usr/bin', HOME: '/home/x', BENCH_MODEL_API_KEY: 'the-real-model-key-123456', ANTHROPIC_API_KEY: 'another-secret-value' },
  });
  return { stack, calls, logs, home };
}

function snapshotEnvFiles(home) {
  const dir = path.join(home, 'stack');
  const out = {};
  try {
    for (const f of fs.readdirSync(dir)) {
      if (f.endsWith('.env')) out[f] = { text: fs.readFileSync(path.join(dir, f), 'utf8'), mode: fs.statSync(path.join(dir, f)).mode & 0o777 };
    }
  } catch {
    // no stack directory yet
  }
  return out;
}

const composeArgs = (c) => c.args.slice(5); // after: compose -f FILE -p NAME

test('up starts the shared services and, for each slot, its git sandbox and meter relay: no arm and no grader is a service', async (t) => {
  const { stack, calls, home } = makeStack(t);
  await stack.up();
  const [c] = calls;
  assert.equal(c.file, 'docker');
  assert.deepEqual(c.args.slice(0, 5), ['compose', '-f', COMPOSE, '-p', 'adam-bench']);
  assert.deepEqual(composeArgs(c), ['up', '-d', '--wait', '--build', ...serviceNames(2)]);
  for (const name of ['gitbox-1', 'gitbox-2', 'meter-1', 'meter-2', 'pub-coder-1', 'pub-coder-2', 'pub-git', 'postgres', 'git-server', 'mock-github']) assert.ok(composeArgs(c).includes(name), name);
  const old = ['pod', 'man'].join('');
  for (const name of [`${old}-1`, `${old}-2`, 'grader', 'coder-1', 'coder-2']) assert.ok(!composeArgs(c).includes(name), name);
  assert.ok(!composeArgs(c).some((n) => n.startsWith('coder-')), 'a coder starts for a run, not with the stack');
  assert.equal(c.opts.env.BENCH_METER_PORT, '18090');
  assert.equal(c.opts.env.BENCH_WORK_1_DIR, path.join(home, 'work', '1'));
  assert.equal(c.opts.env.BENCH_WORK_2_DIR, path.join(home, 'work', '2'));
  assert.ok(fs.statSync(path.join(home, 'work', '1')).isDirectory());
  assert.equal(fs.statSync(path.join(home, 'work', '1')).mode & 0o777, 0o777, 'uid 10001 writes the work directory');
  assert.ok(fs.statSync(path.join(home, 'slots', '2')).isDirectory());
  // the verifier network is made by the stack: compose makes no network that has no service
  assert.ok(calls.some((x) => x.args[0] === 'network' && x.args[1] === 'inspect' && x.args.at(-1) === 'adam-bench_verifier'));
});

test('a compose call gets the variables of the file and none of the model\'s: the key is not in its environment', async (t) => {
  const { stack, calls } = makeStack(t);
  await stack.up();
  const env = calls[0].opts.env;
  assert.ok(!Object.values(env).some((v) => v.includes('the-real-model-key') || v.includes('another-secret-value')));
  // ... and it does get what finds the daemon: an empty environment would pass the check above and break compose.
  const baseEnv = { PATH: '/usr/bin', HOME: '/home/x', DOCKER_HOST: 'unix:///run/user/4242/docker.sock', BENCH_MODEL_API_KEY: 'the-real-model-key-123456' };
  const custom = makeStack(t, { baseEnv });
  await custom.stack.up();
  assert.equal(custom.calls[0].opts.env.DOCKER_HOST, baseEnv.DOCKER_HOST, 'the compose call finds the daemon the stack was built for');
  assert.equal(custom.calls[0].opts.env.PATH, baseEnv.PATH);
});

test('the host child that finds the meter address gets the allow-list too: the key is not in its environment', (t) => {
  // `ip` is a host program, not a docker child, but it is started by every compose call (composeEnv -> meterHostAddress),
  // so it carries the same exposure: a key in its environment is readable with `ps e` for as long as it runs.
  const bin = tmp(t);
  const dump = path.join(bin, 'ip-env.txt');
  const fake = path.join(bin, 'ip');
  fs.writeFileSync(fake, `#!/bin/sh\nenv > '${dump}'\necho '1.1.1.1 via 10.0.0.1 dev eth0 src 10.9.8.7 uid 1000'\n`, { mode: 0o755 });
  const key = 'the-real-model-key-123456';
  const before = { PATH: process.env.PATH, BENCH_MODEL_API_KEY: process.env.BENCH_MODEL_API_KEY };
  process.env.PATH = `${bin}:${before.PATH}`;
  process.env.BENCH_MODEL_API_KEY = key;
  t.after(() => {
    process.env.PATH = before.PATH;
    if (before.BENCH_MODEL_API_KEY === undefined) delete process.env.BENCH_MODEL_API_KEY;
    else process.env.BENCH_MODEL_API_KEY = before.BENCH_MODEL_API_KEY;
  });
  const stack = new Stack({ composeFile: COMPOSE, home: tmp(t), meterPort: 1 });
  assert.equal(stack.meterHostAddress(), '10.9.8.7');
  const seen = fs.readFileSync(dump, 'utf8');
  assert.doesNotMatch(seen, new RegExp(key), 'the key reaches the ip child');
  assert.match(seen, /^PATH=/m, 'the child still finds its program');
});

test('finding 17: every compose call has a finite time, and one that times out is an error that says so', async (t) => {
  const { stack, calls } = makeStack(t);
  await stack.resetDatabase(1);
  await stack.stopCoder(1).catch(() => {});
  assert.ok(calls.every((c) => c.opts.timeoutMs > 0), 'no call waits for ever');
  const hung = makeStack(t, { exec: async () => ({ exitCode: 137, stdout: '', stderr: '', timedOut: true }) });
  await assert.rejects(hung.stack.down(), /timed out/);
});

test('the coder is a host container of the task image, as uid 10001, under the runtime limits, with its settings in a 0600 file', async (t) => {
  const { stack, calls, logs, home } = makeStack(t);
  stack.waitForCoder = async () => {};
  stack.ensureCoderFiles = async () => '/h/coder-bin';
  // The task image's PATH is the one the coder's commands get: its tools come first, after the coder's own binaries.
  stack.imageEnvOf = async (image) => (image === TASK_IMAGE ? { PATH: '/task/bin:/usr/bin' } : { PATH: '/coder/bin' });
  const bearer = 'bearer-value-aaaaaaaaaaaaaaaa';
  const url = 'http://meter-1:8090/r/tokentokentokentokentoken/v1';
  await stack.startCoder(1, { A2A_BEARER_TOKENS: bearer, MODEL_BASE_URL: url, MODEL_CONTEXT_WINDOW: '131072' }, { image: TASK_IMAGE });
  const run = calls.find((c) => c.args[0] === 'run');
  assert.ok(run, 'docker run was called');
  assert.deepEqual(run.args.slice(0, 2), ['run', '-d']);
  assert.ok(run.args.includes('--network=adam-bench_slot-1'));
  assert.deepEqual(run.args.slice(run.args.indexOf('--user'), run.args.indexOf('--user') + 2), ['--user', '10001:10001']);
  assert.deepEqual(run.args.slice(run.args.indexOf('--cpus'), run.args.indexOf('--cpus') + 2), ['--cpus', String(RUNTIME.cpus)]);
  assert.deepEqual(run.args.slice(run.args.indexOf('--memory'), run.args.indexOf('--memory') + 2), ['--memory', `${RUNTIME.memoryMb}m`]);
  assert.equal(run.args[run.args.length - 1], TASK_IMAGE);
  assert.equal(run.args[run.args.length - 2], '/opt/adam/bin/adam-coder');
  assert.ok(run.args.some((a) => a.endsWith(':/etc/adam/agent:ro')));
  assert.ok(run.args.some((a) => a.endsWith(':/opt/adam/bin:ro')));
  assert.ok(run.args.some((a) => a.endsWith(':/work')));
  assert.ok(!run.args.includes('--read-only'), 'the coder\'s own root stays writable: its temp files (the LSP\'s, a tool\'s) live there, only its binaries and agent folder are read-only');
  assert.ok(!run.args.includes('-p'), 'the coder publishes nothing itself: the relay does');
  // the run's settings are in the file, mode 0600, and in no command line or log
  const file = run.envFile['slot-1.env'];
  assert.equal(file.mode, 0o600);
  assert.match(file.text, /^A2A_BEARER_TOKENS=bearer-value-aaaaaaaaaaaaaaaa$/m);
  assert.match(file.text, /^RUN_ENVIRONMENT=local$/m);
  assert.match(file.text, /^DEVCONTAINER_RUNTIME=off$/m);
  assert.match(file.text, /^MODEL_CONTEXT_WINDOW=131072$/m);
  assert.match(file.text, /^PATH=\/opt\/adam\/bin:\/task\/bin:\/usr\/bin$/m, 'the task image\'s PATH, not the coder image\'s');
  assert.ok(!JSON.stringify(run.args).includes(bearer) && !JSON.stringify(run.args).includes('tokentoken'));
  assert.ok(!JSON.stringify(logs).includes(bearer) && !JSON.stringify(logs).includes('tokentoken'));
  assert.deepEqual(fs.readdirSync(path.join(home, 'stack')).filter((f) => f.endsWith('.env')), []);
});

test('a task image that sets no PATH falls back to the coder image\'s', async (t) => {
  const { stack, calls } = makeStack(t);
  stack.waitForCoder = async () => {};
  stack.ensureCoderFiles = async () => '/h/coder-bin';
  stack.imageEnvOf = async (image) => (image === TASK_IMAGE ? {} : { PATH: '/coder/bin' });
  await stack.startCoder(1, {}, { image: TASK_IMAGE });
  const run = calls.find((c) => c.args[0] === 'run');
  assert.match(run.envFile['slot-1.env'].text, /^PATH=\/opt\/adam\/bin:\/coder\/bin$/m);
});

test('the coder refuses a task image that is not named by digest, and a setting that is not plain', async (t) => {
  const { stack } = makeStack(t);
  stack.waitForCoder = async () => {};
  await assert.rejects(stack.startCoder(1, {}, { image: 'registry.example.test/task:1' }), /by digest/);
  await assert.rejects(stack.startCoder(1, { X: 'a\nb=c' }, { image: TASK_IMAGE }), /not plain/);
  await assert.rejects(stack.startCoder(1, { X: 'a$b' }, { image: TASK_IMAGE }), /not plain/);
  await assert.rejects(stack.startCoder(3, {}, { image: TASK_IMAGE }), /no slot 3/);
});

test('finding 14: an error holds neither the bearer, nor the address, nor the bare token of the address', async (t) => {
  const bearer = 'bearer-value-bbbbbbbbbbbbbbbb';
  const token = 'tokentokentokentokentoken';
  const url = `http://meter-2:8090/r/${token}/v1`;
  const { stack, home } = makeStack(t, { exec: async (file, args) => (args[0] === 'run' ? { exitCode: 1, stdout: '', stderr: `error: variable A2A_BEARER_TOKENS=${bearer} is invalid\npath /r/${token}/v1 refused\nfull ${url}` } : { exitCode: 0, stdout: '', stderr: '' }) });
  stack.waitForCoder = async () => {};
  stack.ensureCoderFiles = async () => '/h/coder-bin';
  stack.coderImageEnv = async () => ({ PATH: '/usr/bin' });
  await assert.rejects(stack.startCoder(2, { A2A_BEARER_TOKENS: bearer, MODEL_BASE_URL: url }, { image: TASK_IMAGE }), (e) => e instanceof StackError && !e.message.includes(bearer) && !e.message.includes(token) && !e.message.includes(url) && /exit 1/.test(e.message));
  assert.deepEqual(fs.readdirSync(path.join(home, 'stack')).filter((f) => f.endsWith('.env')), []);
  const parts = secretsOf(url);
  assert.ok(parts.has(url) && parts.has(token), 'the address and the token in it');
  assert.equal(redact(`a ${token} b`, parts), 'a [redacted] b');
});

test('finding 15: an env file a killed harness left is removed at the next start', async (t) => {
  const { stack, home } = makeStack(t);
  fs.mkdirSync(path.join(home, 'stack'), { recursive: true });
  fs.writeFileSync(path.join(home, 'stack', 'slot-1.env'), 'A2A_BEARER_TOKENS=left-behind-bearer\n', { mode: 0o600 });
  fs.writeFileSync(path.join(home, 'stack', 'keep.txt'), 'x');
  assert.deepEqual(stack.removeStaleEnvFiles(), ['slot-1.env']);
  fs.writeFileSync(path.join(home, 'stack', 'slot-2.env'), 'x');
  await stack.up();
  assert.deepEqual(fs.readdirSync(path.join(home, 'stack')).filter((f) => f.endsWith('.env')), []);
  assert.ok(fs.existsSync(path.join(home, 'stack', 'keep.txt')));
});

test('a setting that is not plain, a slot that does not exist and a path outside the work directory are refused', async (t) => {
  const { stack, calls } = makeStack(t);
  stack.waitForCoder = async () => {};
  await assert.rejects(stack.startCoder(1, { X: 'a"b' }, { image: TASK_IMAGE }), /not plain/);
  await assert.rejects(stack.startCoder(1, { X: "a'b" }, { image: TASK_IMAGE }), /not plain/);
  await assert.rejects(stack.startCoder(1, { X: 'a\\b' }, { image: TASK_IMAGE }), /not plain/);
  await assert.rejects(stack.startCoder(1, { 'bad key': 'x' }, { image: TASK_IMAGE }), /not plain/);
  await assert.rejects(stack.startCoder(0, {}, { image: TASK_IMAGE }), /no slot 0/);
  await assert.rejects(stack.listDir(1, '/etc'), /plain path/);
  await assert.rejects(stack.listDir(1, '/work/../etc'), /plain path/);
  await assert.rejects(stack.workDir(1, '/work/workspaces/a/b/c'), /directly under/);
  await assert.rejects(stack.workDir(1, '/etc/x', { remove: true }), /directly under/);
  await assert.rejects(stack.readFiles(1, '/work/../etc', ['a']), /plain path/);
  await assert.rejects(stack.extractTree(1, '/work/x', tmp(t)), /not a worktree/);
  await assert.rejects(stack.deleteRepo('../x'), /plain/);
  assert.ok(!calls.some((c) => c.args[0] === 'run'), 'no coder was started');
});

test('redact removes long secrets and leaves short words', () => {
  assert.equal(redact('a secret-value-12345 b', ['secret-value-12345']), 'a [redacted] b');
  assert.equal(redact('a abc b', ['abc']), 'a abc b');
});

test('the database of a slot is reset by postgres; the work directory is emptied by a helper container as uid 10001 (finding 22)', async (t) => {
  const outputs = [];
  const { stack, calls } = makeStack(t, {
    exec: async (file, args) => {
      outputs.push(args);
      return { exitCode: 0, stdout: '', stderr: '' };
    },
  });
  await stack.resetDatabase(2);
  await stack.resetWork(1);
  const flat = outputs.map((a) => a.join(' '));
  assert.match(flat[0], /exec -T postgres psql -U postgres .*DROP DATABASE IF EXISTS adam_bench_2 WITH \(FORCE\) -c CREATE DATABASE adam_bench_2/);
  const helper = outputs.find((a) => a[0] === 'run' && a.some((x) => x.includes('/work')));
  assert.ok(helper, 'the work directory is emptied by a container');
  assert.ok(helper.includes('0:0'), 'the cleanup helper is root inside the container, the host user under rootless Docker');
  assert.ok(helper.includes('--network=none'));
  const script = helper.at(-1);
  assert.match(script, /find \/work -mindepth 1 -maxdepth 1 ! -name \.gitbox -exec rm -rf \{\} \+ ; mkdir -p \/work\/workspaces \/work\/\.gitbox && chmod 777 \/work\/workspaces \/work\/\.gitbox/);
  assert.doesNotMatch(script, /rm -rf \/work\/workspaces \/work\/git/);
  void calls;
});

test('resetWork keeps the names it is told to (a task base), and refuses a name that is not plain', async (t) => {
  const outputs = [];
  const { stack } = makeStack(t, {
    exec: async (file, args) => {
      outputs.push(args);
      return { exitCode: 0, stdout: '', stderr: '' };
    },
  });
  fs.mkdirSync(path.join(stack.workRoot(1), 'bench-base'), { recursive: true });
  await stack.resetWork(1, { keep: ['bench-base'] });
  const script = outputs.find((a) => a[0] === 'run').at(-1);
  assert.match(script, /find \/work -mindepth 1 -maxdepth 1 ! -name \.gitbox ! -name bench-base -exec rm -rf \{\} \+/);
  assert.ok(fs.existsSync(path.join(stack.workRoot(1), 'bench-base')), 'the kept base is still there');
  await assert.rejects(stack.resetWork(1, { keep: ['../etc'] }), /not plain/);
});

test('a directory of the work directory is made on the host, removed by a helper as uid 10001 and checked', async (t) => {
  const seen = [];
  const { stack } = makeStack(t, { exec: async (file, args) => (seen.push(args), { exitCode: 0, stdout: '', stderr: '' }) });
  await stack.workDir(1, '/work/workspaces/oc-r1-a1');
  assert.ok(fs.statSync(path.join(stack.workRoot(1), 'workspaces/oc-r1-a1')).isDirectory());
  assert.equal(fs.statSync(path.join(stack.workRoot(1), 'workspaces/oc-r1-a1')).mode & 0o777, 0o777);
  await stack.workDir(1, '/work/workspaces/oc-r1-a1', { remove: true });
  const script = seen.find((a) => a.join(' ').includes('rm -rf'))?.at(-1);
  assert.match(script, /chmod -R u\+rwX \/work\/workspaces\/oc-r1-a1 2>\/dev\/null; rm -rf \/work\/workspaces\/oc-r1-a1; test ! -e \/work\/workspaces\/oc-r1-a1/);
  const stuck = makeStack(t, { exec: async () => ({ exitCode: 1, stdout: '', stderr: 'test: failed' }) });
  await assert.rejects(stuck.stack.workDir(1, '/work/workspaces/x', { remove: true }), /exit 1/);
});

/** A tar, as `tar -cf -` inside a container writes it. */
const tarOf = (files) => {
  const d = fs.mkdtempSync(path.join(fs.realpathSync('/tmp'), 'bench-tar-'));
  for (const [name, content] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(d, name)), { recursive: true });
    if (typeof content === 'object') fs.symlinkSync(content.link, path.join(d, name));
    else fs.writeFileSync(path.join(d, name), content);
  }
  const out = execFileSync('tar', ['--format=posix', '-cf', '-', '-C', d, '.'], { maxBuffer: 1 << 26 });
  fs.rmSync(d, { recursive: true, force: true });
  return out;
};

test('finding 5 and 6: files come out as a tar stream from a helper container on the host, by name, and a link where a log should be is refused', async (t) => {
  const seen = [];
  const { stack } = makeStack(t, {
    execBuf: async (file, args) => {
      seen.push(args);
      return { exitCode: 0, stdout: tarOf({ 'events.jsonl': '{}\n', 'export.json': { link: '/home/owner/.ssh/id_rsa' }, 'other.txt': 'o' }), stderr: '', over: false };
    },
  });
  const { files, refused } = await stack.readFiles(1, '/work/bench-out/oc-r1-a1', ['events.jsonl', 'stderr.txt', 'export.json']);
  assert.equal(seen[0][0], 'run');
  assert.ok(seen[0].includes('--network=none'));
  assert.ok(seen[0].includes('10001:10001'));
  assert.ok(seen[0].includes(`${stack.workRoot(1)}:/work:ro`));
  assert.deepEqual(seen[0].slice(-9), ['--format=posix', '-cf', '-', '--ignore-failed-read', '-C', '/work/bench-out/oc-r1-a1', 'events.jsonl', 'stderr.txt', 'export.json']);
  assert.deepEqual([...files.keys()], ['events.jsonl']);
  assert.deepEqual(refused, ['export.json']);
});

test('a log over its cap is refused and the others are read; an archive over the total cap is the only error (defect 3)', async (t) => {
  const huge = makeStack(t, { execBuf: async () => ({ exitCode: 0, stdout: tarOf({ 'events.jsonl': 'x'.repeat(5000), 'stderr.txt': 'ok' }), stderr: '', over: false }) });
  const got = await huge.stack.readFiles(1, '/work/bench-out/x', ['events.jsonl', 'stderr.txt'], { maxFileBytes: 1000 });
  assert.deepEqual([[...got.files.keys()], got.refused], [['stderr.txt'], ['events.jsonl']]);
  const big = makeStack(t, { execBuf: async () => ({ exitCode: 0, stdout: Buffer.alloc(0), stderr: '', over: true }) });
  await assert.rejects(big.stack.readFiles(1, '/work/bench-out/x', ['events.jsonl']), /larger than the limit/);
});

test('defect 2, through the real parser: a FIFO where a log should be does not end the run: it is refused, and the other logs are read', async (t) => {
  const d = tmp(t);
  fs.writeFileSync(path.join(d, 'stderr.txt'), 'warn');
  fs.writeFileSync(path.join(d, 'export.json'), '{}');
  execFileSync('mkfifo', [path.join(d, 'events.jsonl')]);
  const tar = execFileSync('tar', ['--format=posix', '-cf', '-', '-C', d, 'events.jsonl', 'stderr.txt', 'export.json'], { maxBuffer: 1 << 26 });
  const { stack } = makeStack(t, { execBuf: async () => ({ exitCode: 0, stdout: tar, stderr: '', over: false }) });
  const got = await stack.readFiles(1, '/work/bench-out/x', ['events.jsonl', 'stderr.txt', 'export.json']);
  assert.deepEqual([[...got.files.keys()].sort(), got.refused], [['export.json', 'stderr.txt'], ['events.jsonl']]);
});

test('defect 3, through the real parser: a FIFO, a device or a big file in the work tree is skipped and recorded, and the rest of the tree is extracted', async (t) => {
  const d = tmp(t);
  fs.mkdirSync(path.join(d, 'src'));
  fs.writeFileSync(path.join(d, 'src', 'a.txt'), 'a');
  fs.writeFileSync(path.join(d, 'huge.bin'), 'x'.repeat(5000));
  execFileSync('mkfifo', [path.join(d, 'src', 'pipe')]);
  const tar = execFileSync('tar', ['--format=posix', '-cf', '-', '-C', d, '.'], { maxBuffer: 1 << 26 });
  const { stack } = makeStack(t, { execBuf: async () => ({ exitCode: 0, stdout: tar, stderr: '', over: false }) });
  const dest = path.join(tmp(t), 'tree');
  const { refused } = await stack.extractTree(1, '/work/workspaces/r/s', dest, { maxFileBytes: 1000 });
  assert.deepEqual(refused.map((r) => r.name).sort(), ['huge.bin', 'src/pipe']);
  assert.equal(fs.readFileSync(path.join(dest, 'src', 'a.txt'), 'utf8'), 'a');
  assert.equal(fs.existsSync(path.join(dest, 'src', 'pipe')), false);
});

test('a tree comes out as a tar stream into a directory the harness makes, without its top-level .git, links kept as links and never followed', async (t) => {
  const seen = [];
  const { stack } = makeStack(t, {
    execBuf: async (file, args) => {
      seen.push(args);
      return { exitCode: 0, stdout: tarOf({ 'src/a.txt': 'a\n', node_modules: { link: '/app/node_modules' } }), stderr: '', over: false };
    },
  });
  const dest = path.join(tmp(t), 'tree');
  fs.mkdirSync(dest);
  fs.writeFileSync(path.join(dest, 'stale'), 'old');
  await stack.extractTree(1, '/work/workspaces/run-1/slot', dest);
  assert.deepEqual(seen[0].slice(-7), ['--format=posix', '-cf', '-', '--exclude=./.git', '-C', '/work/workspaces/run-1/slot', '.']);
  assert.equal(fs.readFileSync(path.join(dest, 'src', 'a.txt'), 'utf8'), 'a\n');
  assert.equal(fs.readlinkSync(path.join(dest, 'node_modules')), '/app/node_modules');
  assert.equal(fs.existsSync(path.join(dest, 'stale')), false);
});

test('a tree whose tar has a name that leaves the directory is refused, and nothing outside is written', async (t) => {
  const evil = Buffer.alloc(1536);
  evil.write('../escape.txt', 0);
  evil.write('0000644\0', 100);
  evil.write('00000000001\0', 124);
  evil.write('0', 156);
  evil.write('ustar\0', 257);
  let sum = 0;
  for (let i = 0; i < 512; i++) sum += i >= 148 && i < 156 ? 0x20 : evil[i];
  evil.write(`${sum.toString(8).padStart(6, '0')}\0 `, 148);
  evil.write('x', 512);
  const { stack } = makeStack(t, { execBuf: async () => ({ exitCode: 0, stdout: evil, stderr: '', over: false }) });
  const base = tmp(t);
  await assert.rejects(stack.extractTree(1, '/work/workspaces/r/s', path.join(base, 'tree')), /leaves the target/);
  assert.equal(fs.existsSync(path.join(base, 'escape.txt')), false);
});

test('finding 1: the base is pushed by a one-shot container of the stack, with the script as its only argument, and a run\'s repository is deleted by the git server', async (t) => {
  const seen = [];
  const { stack } = makeStack(t, { exec: async (file, args) => (seen.push(args.slice(5)), { exitCode: 0, stdout: '', stderr: '' }) });
  await stack.pushBase(2, 'echo pushed');
  await stack.deleteRepo('demo-1-aaaaaa-1b2c3d4e');
  const run = seen.find((a) => a[0] === 'run');
  assert.deepEqual(run, ['run', '--rm', '-T', '--no-deps', 'pusher-2', 'echo pushed']);
  const del = seen.find((a) => a[0] === 'exec');
  assert.deepEqual(del.slice(0, 5), ['exec', '-T', 'git-server', 'sh', '-c']);
  assert.match(del[5], /rm -rf \/srv\/git\/bench\/demo-1-aaaaaa-1b2c3d4e\.git; test ! -e \/srv\/git\/bench\/demo-1-aaaaaa-1b2c3d4e\.git/);
});

test('finding 11: stopping a coder removes it and confirms that none is left', async (t) => {
  const answers = [{ exitCode: 0, stdout: '', stderr: '' }, { exitCode: 0, stdout: '', stderr: '' }];
  const { stack } = makeStack(t, { exec: async () => answers.shift() });
  await stack.stopCoder(1);
  const alive = [{ exitCode: 0, stdout: '', stderr: '' }, { exitCode: 0, stdout: 'abc123def456\n', stderr: '' }];
  const b = makeStack(t, { exec: async () => alive.shift() });
  await assert.rejects(b.stack.stopCoder(1), /not stopped/);
});

test('the containers of a slot and of the grader are host containers with their own labels and networks', async (t) => {
  const { stack } = makeStack(t);
  const slot = stack.containers(2);
  assert.deepEqual(slot.labels, { 'bench.project': 'adam-bench', 'bench.slot': '2', 'bench.role': 'run' });
  assert.equal(slot.aliases.slot, 'adam-bench_slot-2');
  assert.equal(slot.prefix, 'adam-bench_');
  assert.deepEqual(stack.graderContainers().labels, { 'bench.project': 'adam-bench', 'bench.role': 'grader' });
  const grader = stack.graderContainers();
  assert.equal(grader.prefix, '', 'a network the owner named in tasks.verifierNetworks is used as it is');
  assert.equal(grader.aliases.verifier, 'adam-bench_verifier');
  assert.equal(grader.aliases.egress, 'adam-bench_egress');
  assert.deepEqual(stack.coderContainers(1).labels, { 'bench.project': 'adam-bench', 'bench.slot': '1', 'bench.role': 'coder' });
});

test('the coder\'s address is the loopback port it publishes; the meter relay is the slot\'s', () => {
  const stack = new Stack({ composeFile: COMPOSE, home: '/h', meterPort: 1, ports: { coder1: 20001, coder2: 20002 } });
  assert.equal(stack.coderUrl(1), 'http://127.0.0.1:20001/');
  assert.equal(stack.coderUrl(2), 'http://127.0.0.1:20002/');
  assert.equal(stack.meterHost(2), 'meter-2');
  assert.equal(stack.slotDir(1), '/h/slots/1');
  assert.equal(stack.workRoot(1), '/h/work/1');
  assert.throws(() => new Stack({ composeFile: 'compose.yaml', home: '/h', meterPort: 1 }), /absolute/);
});

test('the coder environment is the one the plan names: local commands, no devcontainer runtime, the sandbox socket', () => {
  const env = coderEnvironment({ slot: 1, publicUrl: 'http://127.0.0.1:18081/', databaseUrl: 'postgres://postgres:postgres@postgres:5432/adam_bench_1' });
  assert.equal(env.RUN_ENVIRONMENT, 'local');
  assert.equal(env.DEVCONTAINER_RUNTIME, 'off');
  assert.equal(env.GIT_SANDBOX_SOCKET, '/work/.gitbox/git.sock');
  assert.equal(env.ALLOWED_REPO_HOSTS, 'git-server');
  assert.equal(env.GITHUB_API_URL, 'http://mock-github:8080');
  assert.equal(env.DATABASE_URL, 'postgres://postgres:postgres@postgres:5432/adam_bench_1');
  assert.equal(env.PUBLIC_URL, 'http://127.0.0.1:18081/');
  assert.match(env.PATH, /^\/opt\/adam\/bin:/);
});

// ---- the compose file itself, rendered by `docker compose config` (no daemon needed) ----

function rendered() {
  const env = { ...process.env, BENCH_WORK_1_DIR: '/tmp/bw/1', BENCH_WORK_2_DIR: '/tmp/bw/2', BENCH_METER_HOST: '10.0.0.1', BENCH_METER_PORT: '8090', BENCH_GIT_PORT: '18083' };
  try {
    return JSON.parse(execFileSync('docker', ['compose', '-f', COMPOSE, '--profile', '*', 'config', '--format', 'json'], { env, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }));
  } catch {
    return null;
  }
}
const config = rendered();
// A missing compose is a skip, and a failure where the docker tests are required.
const compose = { skip: config || process.env.BENCH_TEST_REQUIRE_DOCKER === '1' ? false : 'docker compose is not installed here' };
const members = (net) => Object.entries(config.services).filter(([, s]) => Object.hasOwn(s.networks ?? {}, net)).map(([n]) => n).sort();

test('compose: the stack is rendered (a failure under BENCH_TEST_REQUIRE_DOCKER=1)', compose, () => {
  assert.ok(config, 'docker compose config did not render the file');
});

test('finding 3: each network has exactly the members the docs say, and gradings share one verifier network', compose, () => {
  assert.deepEqual(members('slot-1'), ['git-server', 'meter-1', 'mock-github', 'postgres', 'pub-coder-1']);
  assert.deepEqual(members('slot-2'), ['git-server', 'meter-2', 'mock-github', 'postgres', 'pub-coder-2']);
  assert.deepEqual(members('gitpub-net'), ['git-server', 'pub-git', 'pusher-1', 'pusher-2']);
  assert.deepEqual(members('egress'), ['meter-1', 'meter-2', 'pub-coder-1', 'pub-coder-2', 'pub-git']);
  // Compose drops a declared network that no service is on; the stack makes the verifier network itself (`up`).
  assert.equal(config.networks.verifier, undefined);
  assert.equal(members('verifier').length, 0, 'the gradings are host containers, not compose services');
  assert.match(fs.readFileSync(COMPOSE, 'utf8'), /^  verifier:\n    internal: true$/m);
  for (const n of ['slot-1', 'slot-2', 'gitpub-net']) assert.equal(config.networks[n].internal, true, n);
  assert.ok(!config.networks.egress.internal);
  assert.equal(config.networks['grader-net'], undefined);
  for (const n of ['agentnet-1', 'agentnet-2']) assert.equal(config.networks[n], undefined, n);
});

test('up makes the verifier network internal, leaves an internal one alone, and refuses one that is not internal', async (t) => {
  // The network is the property that a grading has no route out. A stale `adam-bench_verifier` that is not internal
  // (made by an older harness or by hand) would silently give every grading the internet, so it is refused, never used.
  const created = [];
  const daemon = (internal) =>
    makeStack(t, {
      exec: async (file, args) => {
        if (args[0] === 'network' && args[1] === 'inspect') {
          return internal === null ? { exitCode: 1, stdout: '', stderr: 'no such network' } : { exitCode: 0, stdout: `${internal}\n`, stderr: '' };
        }
        if (args[0] === 'network' && args[1] === 'create') created.push(args);
        return { exitCode: 0, stdout: '', stderr: '' };
      },
    });
  await daemon(null).stack.up();
  assert.deepEqual(created, [['network', 'create', '--internal', 'adam-bench_verifier']], 'the missing network is made internal');
  await daemon(true).stack.up();
  assert.deepEqual(created, [['network', 'create', '--internal', 'adam-bench_verifier']], 'an internal network is left alone');
  await assert.rejects(daemon(false).stack.up(), /adam-bench_verifier network exists and is not internal/);
});

test('a grading that needs the internet is alone in its window: two windows never overlap', async () => {
  // The `egress` network is the one with a route out; a verifier that asks for it grades alone, and the lock is what
  // makes "alone" true (`graderWindow` is its exclusive side, `graderShared` the shared one).
  const stack = new Stack({ composeFile: COMPOSE, home: '/h', meterPort: 1 });
  const events = [];
  const work = (name, ms = 20) => async () => {
    events.push(`${name} start`);
    await new Promise((r) => setTimeout(r, ms));
    events.push(`${name} end`);
  };
  await Promise.all([stack.graderWindow(work('a')), stack.graderWindow(work('b'))]);
  assert.deepEqual(events, ['a start', 'a end', 'b start', 'b end']);
});

test('shared gradings run together; a window waits for them, and a grading that arrives after the window waits for it', async () => {
  const stack = new Stack({ composeFile: COMPOSE, home: '/h', meterPort: 1 });
  const events = [];
  const work = (name, ms = 20) => async () => {
    events.push(`${name} start`);
    await new Promise((r) => setTimeout(r, ms));
    events.push(`${name} end`);
  };
  const g1 = stack.graderShared(work('g1'));
  const g2 = stack.graderShared(work('g2'));
  await new Promise((r) => setTimeout(r, 5));
  const w = stack.graderWindow(work('window'));
  const g3 = stack.graderShared(work('g3'));
  await Promise.all([g1, g2, w, g3]);
  const at = (e) => events.indexOf(e);
  assert.ok(at('g1 start') < at('g2 end') && at('g2 start') < at('g1 end'), 'the two shared gradings overlap');
  assert.ok(at('window start') > at('g1 end') && at('window start') > at('g2 end'), 'the window opens only when they have ended');
  assert.ok(at('g3 start') > at('window end'), 'a grading that came after the window waits until it is closed');
});

test('a window that fails releases the lock, and a shared grading that fails does not hold it', async () => {
  const stack = new Stack({ composeFile: COMPOSE, home: '/h', meterPort: 1 });
  await assert.rejects(stack.graderWindow(async () => { throw new Error('grading failed'); }), /grading failed/);
  assert.equal(await stack.graderWindow(async () => 'next'), 'next');
  await assert.rejects(stack.graderShared(async () => { throw new Error('shared failed'); }), /shared failed/);
  assert.equal(await stack.graderWindow(async () => 'after'), 'after');
});

test('finding 2: only the relays are on a network with the internet; no sandbox, git server or database is', compose, () => {
  for (const name of ['gitbox-1', 'gitbox-2', 'git-server', 'postgres', 'mock-github', 'pusher-1', 'pusher-2']) {
    const nets = Object.keys(config.services[name].networks ?? {});
    assert.ok(!nets.includes('egress') && !nets.includes('default'), `${name} is on ${nets}`);
  }
  const target = (n) => config.services[n].environment.RELAY_TARGET;
  assert.deepEqual(['coder-1:8080', 'coder-2:8080', 'git-server:8080'], [target('pub-coder-1'), target('pub-coder-2'), target('pub-git')]);
  assert.match(target('meter-1'), /^10\.0\.0\.1:\d+$/);
});

test('finding 2: the git sandbox has no secret, no network, a read-only root and the work directory; the coder is told where its socket is', compose, () => {
  for (const slot of [1, 2]) {
    const g = config.services[`gitbox-${slot}`];
    assert.equal(g.network_mode, 'none');
    assert.equal(g.read_only, true);
    assert.deepEqual(g.cap_drop, ['ALL']);
    assert.deepEqual(Object.keys(g.environment).sort(), ['GIT_SANDBOX_LISTEN', 'GIT_SANDBOX_ROOT']);
    assert.deepEqual(g.volumes.map((v) => `${v.source}:${v.target}`), [`/tmp/bw/${slot}:/work`]);
    assert.equal(g.user, '10001:10001');
    assert.equal(g.environment.GIT_SANDBOX_LISTEN, '/work/.gitbox/git.sock');
  }
  assert.equal(coderEnvironment({ slot: 1, publicUrl: 'http://127.0.0.1:1/', databaseUrl: 'postgres://x' }).GIT_SANDBOX_SOCKET, '/work/.gitbox/git.sock');
});

test('compose: every container of the stack that is ours runs the pinned coder image, tag and digest, the same as bench/pins.json', compose, () => {
  for (const name of ['meter-1', 'meter-2', 'pub-coder-1', 'pub-coder-2', 'pub-git', 'gitbox-1', 'gitbox-2', 'pusher-1', 'pusher-2']) assert.equal(config.services[name].image, coderRef(PINS), name);
  const images = new Set(Object.values(config.services).map((s) => s.image).filter(Boolean));
  assert.deepEqual([...images].filter((i) => !i.startsWith('ghcr.io/vymalo/another-adam-rs/coder')).sort(), ['adam-bench-git-server', 'postgres:16', 'wiremock/wiremock:3.13.2'].sort().filter((i) => images.has(i)));
});

test('compose: no privileged, no cap_add, no devices, no Docker socket, and no model key anywhere', compose, () => {
  const text = JSON.stringify(config);
  for (const [name, s] of Object.entries(config.services)) {
    assert.ok(!s.privileged && !s.cap_add && !s.devices, name);
    for (const v of s.volumes ?? []) assert.ok(!/docker\.sock/.test(v.source ?? ''), `${name} mounts a Docker socket`);
  }
  assert.doesNotMatch(text, /sk-[A-Za-z0-9]{10,}/);
  for (const n of ['gitbox-1', 'gitbox-2', 'meter-1', 'meter-2', 'pub-coder-1', 'pub-coder-2', 'pub-git', 'pusher-1', 'pusher-2']) assert.equal(config.services[n].user, '10001:10001', n);
});

test('compose: the git server publishes nothing itself, the relay does; the database is the slot\'s own', compose, () => {
  assert.equal(config.services['git-server'].environment.AUTO_CREATE_OWNERS, 'bench');
  assert.ok(!config.services['git-server'].ports, 'the git server publishes nothing itself: the relay does');
  assert.match(fs.readFileSync(path.join(HERE, '..', 'postgres-init.sql'), 'utf8'), /adam_bench_1[\s\S]*adam_bench_2/);
  assert.deepEqual(Object.keys(config.services['meter-1'].networks).sort(), ['egress', 'slot-1']);
  assert.deepEqual(Object.keys(config.services['meter-2'].networks).sort(), ['egress', 'slot-2']);
});

test('C: only the relays publish ports, and only on the loopback', compose, () => {
  const published = Object.entries(config.services).filter(([, svc]) => svc.ports).map(([n]) => n).sort();
  assert.deepEqual(published, ['pub-coder-1', 'pub-coder-2', 'pub-git']);
  for (const n of published) for (const port of config.services[n].ports) assert.equal(port.host_ip, '127.0.0.1', n);
});

test('BENCH_COMPOSE_BUILD=0 starts the stack without --build; unset or 1 keeps it; any other value is refused', async (t) => {
  const off = makeStack(t, { baseEnv: { BENCH_COMPOSE_BUILD: '0' } });
  await off.stack.up();
  assert.deepEqual(composeArgs(off.calls[0]), ['up', '-d', '--wait', ...serviceNames(2)]);
  for (const env of [{}, { BENCH_COMPOSE_BUILD: '1' }]) {
    const on = makeStack(t, { baseEnv: env });
    await on.stack.up();
    assert.ok(composeArgs(on.calls[0]).includes('--build'));
  }
  assert.throws(() => makeStack(t, { baseEnv: { BENCH_COMPOSE_BUILD: 'no' } }), StackError);
});

test('the images compose builds are the ones the stack lists: every service with a build in compose.yaml, named <project>-<service>', (t) => {
  const text = fs.readFileSync(COMPOSE, 'utf8');
  const built = [...text.matchAll(/^ {2}([a-z0-9-]+):\n(?: {4}.*\n)*? {4}build:/gm)].map((m) => `adam-bench-${m[1]}`);
  assert.deepEqual(makeStack(t).stack.builtImages(), built);
  assert.deepEqual(built, ['adam-bench-git-server']);
});

test('the pusher\'s scratch directory is made on the slot\'s work volume before the push, even when the push then fails', async (t) => {
  const seenDir = [];
  const ref = {};
  const made = makeStack(t, { exec: async (file, args) => {
    if (args.includes('pusher-1')) {
      seenDir.push(fs.existsSync(path.join(ref.stack.workRoot(1), 'pusher-scratch')));
      return { exitCode: 128, stdout: '', stderr: 'boom' };
    }
    return { exitCode: 0, stdout: '', stderr: '' };
  } });
  ref.stack = made.stack;
  await assert.rejects(made.stack.pushBase(1, 'true'));
  assert.deepEqual(seenDir, [true], 'the directory the compose file mounts exists when the pusher starts');
});

test('compose: the pusher keeps a read-only root and the base read-only, and its scratch is a disk-backed bind of the work volume, not a tmpfs', compose, () => {
  for (const n of [1, 2]) {
    const s = config.services[`pusher-${n}`];
    assert.equal(s.read_only, true);
    const byTarget = Object.fromEntries(s.volumes.map((v) => [v.target, v]));
    assert.equal(byTarget['/work'].read_only, true);
    assert.equal(byTarget['/scratch'].type, 'bind');
    assert.ok(!byTarget['/scratch'].read_only);
    assert.equal(byTarget['/scratch'].source, `/tmp/bw/${n}/pusher-scratch`);
    const tmpfs = Object.keys(s.tmpfs ?? {}).concat(Array.isArray(s.tmpfs) ? s.tmpfs : []).join(' ');
    assert.ok(!tmpfs.includes('/scratch'));
    assert.ok(!(s.environment && Object.keys(s.environment).some((k) => /KEY|TOKEN|SECRET/.test(k))));
  }
});

test('ensureGlibc copies the loader and the libraries out of the pinned coder image once, following links, and marks the directory', async (t) => {
  const seen = [];
  const { stack } = makeStack(t, {
    exec: async (file, args) => {
      seen.push(args);
      if (args[0] === 'cp') fs.writeFileSync(args[args.length - 1], '');
      return { exitCode: 0, stdout: 'id\n', stderr: '' };
    },
  });
  const dir = await stack.ensureGlibc();
  assert.deepEqual(fs.readdirSync(dir).sort(), ['.image', ...GLIBC_FILES].sort());
  assert.equal((fs.statSync(path.join(dir, 'ld-linux-x86-64.so.2')).mode & 0o111) !== 0, true, 'the loader is executable');
  assert.equal(fs.readFileSync(path.join(dir, '.image'), 'utf8').trim(), coderRef(PINS));
  const cps = seen.filter((a) => a[0] === 'cp');
  assert.equal(cps.length, GLIBC_FILES.length);
  for (const a of cps) {
    assert.equal(a[1], '-L', 'a symbolic link is copied as the file it names');
    assert.match(a[2], /^adam-bench-extract-[0-9a-f]+:\/usr\/lib\/x86_64-linux-gnu\/[A-Za-z0-9._+-]+$/);
  }
  assert.ok(!seen.some((a) => a[0] === 'run' || a[0] === 'start'), 'the image is never run');
  assert.equal(await stack.ensureGlibc(), dir, 'a second call copies nothing');
  assert.equal(seen.filter((a) => a[0] === 'cp').length, GLIBC_FILES.length);
});

test('emptying a slot and removing a directory may take TREE_REMOVE_TIMEOUT_MS, reading a tree TREE_READ_TIMEOUT_MS (a large node_modules timed out at 120 s)', async (t) => {
  const seen = [];
  const { stack } = makeStack(t, {
    exec: async (file, args, opts) => {
      seen.push({ args, opts });
      return { exitCode: 0, stdout: '', stderr: '' };
    },
    execBuf: async (file, args, opts) => {
      seen.push({ args, opts, buf: true });
      return { exitCode: 0, stdout: Buffer.alloc(1024), stderr: '', over: false };
    },
  });
  await stack.resetWork(1);
  await stack.workDir(1, '/work/bench-base', { remove: true });
  const helpers = seen.filter((c) => c.args[0] === 'run' && !c.buf);
  assert.equal(helpers.length, 2);
  for (const h of helpers) assert.equal(h.opts.timeoutMs, TREE_REMOVE_TIMEOUT_MS);
  assert.ok(TREE_REMOVE_TIMEOUT_MS >= 15 * 60 * 1000 && TREE_READ_TIMEOUT_MS >= 5 * 60 * 1000);
  await stack.streamFrom(1, ['-cf', '-', '-C', '/work', '.'], { maxBytes: 4096 });
  assert.equal(seen.find((c) => c.buf).opts.timeoutMs, TREE_READ_TIMEOUT_MS);
});
