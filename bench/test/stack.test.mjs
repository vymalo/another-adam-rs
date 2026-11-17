import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { PINS, coderRef } from '../lib/pins.mjs';
import { PODMAN_URL, Stack, StackError, redact, secretsOf, serviceNames } from '../lib/stack.mjs';
import { tmp } from './helpers.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const COMPOSE = path.join(HERE, '..', 'compose.yaml');

function makeStack(t, { exec, execBuf, spawn, baseEnv } = {}) {
  const home = tmp(t);
  const calls = [];
  const logs = [];
  const stack = new Stack({
    composeFile: COMPOSE,
    home,
    tasksDir: '/repo/bench/tasks',
    seccomp: '/repo/dev/podman/seccomp.json',
    meterPort: 18090,
    exec: exec ?? (async (file, args, opts) => {
      calls.push({ file, args, opts, envFile: snapshotEnvFiles(home) });
      return { exitCode: 0, stdout: '', stderr: '' };
    }),
    execBuf,
    spawn,
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

test('up starts the shared services, the grader and, for each slot, its Podman service, git sandbox and relays', async (t) => {
  const { stack, calls, home } = makeStack(t);
  await stack.up();
  const [c] = calls;
  assert.equal(c.file, 'docker');
  assert.deepEqual(c.args.slice(0, 5), ['compose', '-f', COMPOSE, '-p', 'adam-bench']);
  assert.deepEqual(composeArgs(c), ['up', '-d', '--wait', '--build', ...serviceNames(2)]);
  for (const name of ['grader', 'gitbox-1', 'gitbox-2', 'meter-1', 'meter-2', 'pub-coder-1', 'pub-coder-2', 'pub-git', 'podman-1', 'podman-2']) assert.ok(composeArgs(c).includes(name), name);
  assert.ok(!composeArgs(c).some((n) => n.startsWith('coder-')), 'a coder starts for a run, not with the stack');
  assert.equal(c.opts.env.BENCH_METER_PORT, '18090');
  assert.equal(c.opts.env.BENCH_SLOT_1_DIR, path.join(home, 'slots', '1'));
  assert.equal(c.opts.env.PODMAN_SECCOMP, '/repo/dev/podman/seccomp.json');
  assert.ok(fs.statSync(path.join(home, 'slots', '2')).isDirectory());
});

test('a compose call gets the variables of the file and none of the model\'s: the key is not in its environment', async (t) => {
  const { stack, calls } = makeStack(t);
  await stack.up();
  const env = calls[0].opts.env;
  assert.ok(!Object.values(env).some((v) => v.includes('the-real-model-key') || v.includes('another-secret-value')));
});

test('finding 17: every compose call has a finite time, and one that times out is an error that says so', async (t) => {
  const { stack, calls } = makeStack(t);
  await stack.resetDatabase(1);
  await stack.stopCoder(1).catch(() => {});
  assert.ok(calls.every((c) => c.opts.timeoutMs > 0), 'no call waits for ever');
  const hung = makeStack(t, { exec: async () => ({ exitCode: 137, stdout: '', stderr: '', timedOut: true }) });
  await assert.rejects(hung.stack.down(), /timed out/);
});

test('the coder\'s settings are in a file of mode 0600 while it is created, never in a command line, and the file is gone after', async (t) => {
  const { stack, calls, logs, home } = makeStack(t);
  const bearer = 'bearer-value-aaaaaaaaaaaaaaaa';
  const url = 'http://meter-1:8090/r/tokentokentokentokentoken/v1';
  await stack.startCoder(1, { A2A_BEARER_TOKENS: bearer, MODEL_BASE_URL: url, DEVCONTAINER_DEFAULT_IMAGE: 'registry.example.test/img@sha256:' + 'a'.repeat(64) });
  const [c] = calls;
  assert.deepEqual(composeArgs(c), ['up', '-d', '--force-recreate', '--no-deps', '--wait', 'coder-1']);
  const file = c.envFile['slot-1.env'];
  assert.equal(file.mode, 0o600);
  assert.match(file.text, /^A2A_BEARER_TOKENS=bearer-value-aaaaaaaaaaaaaaaa$/m);
  assert.ok(!JSON.stringify(c.args).includes(bearer) && !JSON.stringify(c.args).includes('tokentoken'));
  assert.ok(!JSON.stringify(logs).includes(bearer) && !JSON.stringify(logs).includes('tokentoken'));
  assert.deepEqual(fs.readdirSync(path.join(home, 'stack')).filter((f) => f.endsWith('.env')), []);
});

test('finding 14: an error holds neither the bearer, nor the address, nor the bare token of the address', async (t) => {
  const bearer = 'bearer-value-bbbbbbbbbbbbbbbb';
  const token = 'tokentokentokentokentoken';
  const url = `http://meter-2:8090/r/${token}/v1`;
  const { stack, home } = makeStack(t, { exec: async () => ({ exitCode: 1, stdout: '', stderr: `error: variable A2A_BEARER_TOKENS=${bearer} is invalid\npath /r/${token}/v1 refused\nfull ${url}` }) });
  await assert.rejects(stack.startCoder(2, { A2A_BEARER_TOKENS: bearer, MODEL_BASE_URL: url }), (e) => e instanceof StackError && !e.message.includes(bearer) && !e.message.includes(token) && !e.message.includes(url) && /exit 1/.test(e.message));
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

test('a setting that is not plain, a slot that does not exist and a path outside the volume are refused', async (t) => {
  const { stack, calls } = makeStack(t);
  for (const v of ['a\nb=c', 'a$b', 'a"b', "a'b", 'a\\b']) await assert.rejects(stack.startCoder(1, { X: v }), /not plain/);
  await assert.rejects(stack.startCoder(1, { 'bad key': 'x' }), /not plain/);
  await assert.rejects(stack.startCoder(3, { X: 'y' }), /no slot 3/);
  await assert.rejects(stack.startCoder(0, { X: 'y' }), /no slot 0/);
  await assert.rejects(stack.listDir(1, '/etc'), /plain path/);
  await assert.rejects(stack.listDir(1, '/work/../etc'), /plain path/);
  await assert.rejects(stack.workDir(1, '/work/workspaces/a/b/c'), /directly under/);
  await assert.rejects(stack.workDir(1, '/etc/x', { remove: true }), /directly under/);
  await assert.rejects(stack.readFiles(1, '/work/../etc', ['a']), /plain path/);
  await assert.rejects(stack.extractTree(1, '/work/x', tmp(t)), /not a worktree/);
  await assert.rejects(stack.deleteRepo('../x'), /plain/);
  assert.equal(calls.length, 0, 'nothing was run');
});

test('redact removes long secrets and leaves short words', () => {
  assert.equal(redact('a secret-value-12345 b', ['secret-value-12345']), 'a [redacted] b');
  assert.equal(redact('a abc b', ['abc']), 'a abc b');
});

test('the database of a slot is reset by postgres; the work volume by the slot\'s service, which keeps the sandbox\'s directory and checks what is left (finding 22)', async (t) => {
  const outputs = [];
  const { stack, calls } = makeStack(t, {
    exec: async (file, args) => {
      outputs.push(args);
      return { exitCode: 0, stdout: args.join(' ').includes('ls -1A') ? 'workspaces\n.gitbox\n' : '', stderr: '' };
    },
  });
  await stack.resetDatabase(2);
  await stack.resetWork(1);
  const flat = outputs.map((a) => a.slice(5).join(' '));
  assert.match(flat[0], /exec -T postgres psql -U postgres .*DROP DATABASE IF EXISTS adam_bench_2 WITH \(FORCE\); CREATE DATABASE adam_bench_2;/);
  assert.match(flat[1], /exec -T podman-1 sh -c find \/work -mindepth 1 -maxdepth 1 ! -name \.gitbox -exec rm -rf \{\} \+ ; mkdir -p \/work\/workspaces \/work\/\.gitbox/);
  assert.doesNotMatch(flat[1], /rm -rf \/work\/workspaces \/work\/git/, 'everything under /work goes, not a list of directories');
  void calls;
  const leftover = makeStack(t, { exec: async (file, args) => ({ exitCode: 0, stdout: args.join(' ').includes('ls -1A') ? 'workspaces\nstaging\n' : '', stderr: '' }) });
  await assert.rejects(leftover.stack.resetWork(1), /still holds staging/);
});

test('finding 5: a directory of the volume is removed by the service as its owner and checked to be gone', async (t) => {
  const seen = [];
  const { stack } = makeStack(t, { exec: async (file, args) => (seen.push(args.slice(5).join(' ')), { exitCode: 0, stdout: '', stderr: '' }) });
  await stack.workDir(1, '/work/workspaces/oc-r1-a1', { remove: true });
  assert.match(seen[0], /exec -T podman-1 sh -c chmod -R u\+rwX \/work\/workspaces\/oc-r1-a1 2>\/dev\/null; rm -rf \/work\/workspaces\/oc-r1-a1; test ! -e \/work\/workspaces\/oc-r1-a1/);
  await stack.workDir(2, '/work/bench-out/oc-r1-a1');
  assert.match(seen[1], /mkdir -p \/work\/bench-out\/oc-r1-a1 && chmod 777 \/work\/bench-out\/oc-r1-a1/);
  const stuck = makeStack(t, { exec: async () => ({ exitCode: 1, stdout: '', stderr: 'test: failed' }) });
  await assert.rejects(stuck.stack.workDir(1, '/work/workspaces/x', { remove: true }), /exit 1/);
});

/** A tar, as `tar -cf -` inside a service writes it. */
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

test('finding 5 and 6: files come out of a service as a tar stream, by name, and a link where a log should be is refused', async (t) => {
  const seen = [];
  const { stack } = makeStack(t, {
    execBuf: async (file, args) => {
      seen.push(args.slice(5));
      return { exitCode: 0, stdout: tarOf({ 'events.jsonl': '{}\n', 'export.json': { link: '/home/owner/.ssh/id_rsa' }, 'other.txt': 'o' }), stderr: '', over: false };
    },
  });
  const { files, refused } = await stack.readFiles(1, '/work/bench-out/oc-r1-a1', ['events.jsonl', 'stderr.txt', 'export.json']);
  assert.deepEqual(seen[0], ['exec', '-T', 'podman-1', 'tar', '--format=posix', '-cf', '-', '--ignore-failed-read', '-C', '/work/bench-out/oc-r1-a1', 'events.jsonl', 'stderr.txt', 'export.json']);
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
      seen.push(args.slice(5));
      return { exitCode: 0, stdout: tarOf({ 'src/a.txt': 'a\n', node_modules: { link: '/app/node_modules' } }), stderr: '', over: false };
    },
  });
  const dest = path.join(tmp(t), 'tree');
  fs.mkdirSync(dest);
  fs.writeFileSync(path.join(dest, 'stale'), 'old');
  await stack.extractTree(1, '/work/workspaces/run-1/slot', dest);
  assert.deepEqual(seen[0], ['exec', '-T', 'podman-1', 'tar', '--format=posix', '-cf', '-', '--exclude=./.git', '-C', '/work/workspaces/run-1/slot', '.']);
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
  assert.deepEqual(seen[0], ['run', '--rm', '-T', '--no-deps', 'pusher-2', 'echo pushed']);
  assert.deepEqual(seen[1].slice(0, 5), ['exec', '-T', 'git-server', 'sh', '-c']);
  assert.match(seen[1][5], /rm -rf \/srv\/git\/bench\/demo-1-aaaaaa-1b2c3d4e\.git; test ! -e \/srv\/git\/bench\/demo-1-aaaaaa-1b2c3d4e\.git/);
});

test('finding 11: stopping a coder removes it and confirms that none is left', async (t) => {
  const answers = [{ exitCode: 0, stdout: '', stderr: '' }, { exitCode: 0, stdout: '', stderr: '' }];
  const { stack } = makeStack(t, { exec: async () => answers.shift() });
  await stack.stopCoder(1);
  const alive = [{ exitCode: 0, stdout: '', stderr: '' }, { exitCode: 0, stdout: 'abc123def456\n', stderr: '' }];
  const b = makeStack(t, { exec: async () => alive.shift() });
  await assert.rejects(b.stack.stopCoder(1), /not stopped/);
});

test('the Podman client of a slot, and the grader\'s, run inside their services, through compose exec, on the service\'s own socket', async (t) => {
  const spawned = [];
  const { stack } = makeStack(t, {
    spawn: (file, args, opts, cb) => {
      spawned.push({ file, args });
      cb(null, 'sha256:' + 'c'.repeat(64) + '\n', '');
      return { stdin: { end() {}, on() {} } };
    },
  });
  assert.equal(await stack.containers(2).digestOf('registry.example.test/img:1'), 'sha256:' + 'c'.repeat(64));
  await stack.graderContainers().digestOf('registry.example.test/img:1');
  assert.deepEqual(spawned[0].args.slice(0, 9), ['compose', '-f', COMPOSE, '-p', 'adam-bench', 'exec', '-T', 'podman-2', 'podman']);
  assert.deepEqual(spawned[0].args.slice(9, 12), ['--remote', '--url', PODMAN_URL]);
  assert.equal(spawned[1].args[7], 'grader');
});

test('the egress window connects a service to the network with the internet and takes it away, whatever happens', async (t) => {
  const calls = [];
  const id = 'f'.repeat(12);
  const { stack } = makeStack(t, {
    exec: async (file, args) => {
      calls.push([file, ...args.filter((a) => !a.startsWith('/') && a !== COMPOSE)]);
      return { exitCode: 0, stdout: args.includes('ps') ? `${id}\n` : '', stderr: '' };
    },
  });
  await stack.pullWindow(1, async () => calls.push('pulling'));
  await assert.rejects(stack.graderWindow(async () => { throw new Error('grading failed'); }), /grading failed/);
  const nets = calls.filter((c) => c[1] === 'network').map((c) => c.slice(1).join(' '));
  assert.deepEqual(nets, [`network connect adam-bench_egress ${id}`, `network disconnect adam-bench_egress ${id}`, `network connect adam-bench_egress ${id}`, `network disconnect adam-bench_egress ${id}`]);
  assert.ok(calls.some((c) => c.includes('grader') && c.includes('ps')), 'the grader\'s window is the grader\'s own container');
});

test('R7: two windows on the grader never overlap: one connects, runs and disconnects before the next connects', async (t) => {
  const events = [];
  const id = 'f'.repeat(12);
  const { stack } = makeStack(t, {
    exec: async (file, args) => {
      if (args.includes('network')) events.push(`${args[args.indexOf('network') + 1]}`);
      return { exitCode: 0, stdout: args.includes('ps') ? `${id}\n` : '', stderr: '' };
    },
  });
  const slow = (name) => async () => {
    events.push(`${name} start`);
    await new Promise((r) => setTimeout(r, 20));
    events.push(`${name} end`);
  };
  await Promise.all([stack.graderWindow(slow('a')), stack.graderWindow(slow('b'))]);
  assert.deepEqual(events, ['connect', 'a start', 'a end', 'disconnect', 'connect', 'b start', 'b end', 'disconnect']);
});

test('R7: no grading runs on the grader while a window is open, and gradings without a window run together', async (t) => {
  const events = [];
  const id = 'f'.repeat(12);
  const { stack } = makeStack(t, {
    exec: async (file, args) => {
      if (args.includes('network')) events.push(args[args.indexOf('network') + 1]);
      return { exitCode: 0, stdout: args.includes('ps') ? `${id}\n` : '', stderr: '' };
    },
  });
  const work = (name, ms = 20) => async () => {
    events.push(`${name} start`);
    await new Promise((r) => setTimeout(r, ms));
    events.push(`${name} end`);
  };
  // two gradings are in flight, a pull asks for the window, a third grading arrives after the pull asked
  const g1 = stack.graderShared(work('g1'));
  const g2 = stack.graderShared(work('g2'));
  await new Promise((r) => setTimeout(r, 5));
  const pull = stack.graderWindow(work('pull'));
  const g3 = stack.graderShared(work('g3'));
  await Promise.all([g1, g2, pull, g3]);
  const at = (e) => events.indexOf(e);
  assert.ok(at('g1 start') < at('g2 end') && at('g2 start') < at('g1 end'), 'gradings overlap');
  assert.ok(at('connect') > at('g1 end') && at('connect') > at('g2 end'), 'the window opens only when the gradings have ended');
  assert.ok(at('g3 start') > at('disconnect'), 'a grading that came after the pull waits until the window is closed');
  assert.ok(at('pull start') > at('connect') && at('pull end') < at('disconnect'));
});

test('R7: a window that fails still releases the grader to the next one', async (t) => {
  const id = 'f'.repeat(12);
  const { stack } = makeStack(t, { exec: async (file, args) => ({ exitCode: 0, stdout: args.includes('ps') ? `${id}\n` : '', stderr: '' }) });
  await assert.rejects(stack.graderWindow(async () => { throw new Error('pull failed'); }), /pull failed/);
  assert.equal(await stack.graderWindow(async () => 'next'), 'next');
  assert.equal(await stack.graderShared(async () => 'shared'), 'shared');
});

test('the coder\'s address is the loopback port the compose file publishes, through the relay', () => {
  const stack = new Stack({ composeFile: COMPOSE, home: '/h', tasksDir: '/t', seccomp: '/s', meterPort: 1, ports: { coder1: 20001, coder2: 20002 } });
  assert.equal(stack.coderUrl(1), 'http://127.0.0.1:20001/');
  assert.equal(stack.coderUrl(2), 'http://127.0.0.1:20002/');
  assert.equal(stack.meterHost(2), 'meter-2');
  assert.equal(stack.slotDir(1), '/h/slots/1');
  assert.throws(() => new Stack({ composeFile: 'compose.yaml', home: '/h', tasksDir: '/t', seccomp: '/s', meterPort: 1 }), /absolute/);
});

// ---- the compose file itself, rendered by `docker compose config` (no daemon needed) ----

function rendered() {
  const env = { ...process.env, BENCH_HOME: '/tmp/bh', BENCH_TASKS_DIR: '/tmp/bt', BENCH_STACK_DIR: '/tmp/bs', BENCH_AGENT_DIR: '/tmp/ba', BENCH_METER_PORT: '8090', PODMAN_SECCOMP: '/tmp/seccomp.json', BENCH_SLOT_1_DIR: '/tmp/bh/slots/1', BENCH_SLOT_2_DIR: '/tmp/bh/slots/2' };
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

test('finding 3: each network has exactly the members the docs say', compose, () => {
  assert.deepEqual(members('agentnet-1'), ['meter-1', 'podman-1']);
  assert.deepEqual(members('agentnet-2'), ['meter-2', 'podman-2']);
  assert.deepEqual(members('slot-1'), ['coder-1', 'git-server', 'meter-1', 'mock-github', 'postgres', 'pub-coder-1']);
  assert.deepEqual(members('slot-2'), ['coder-2', 'git-server', 'meter-2', 'mock-github', 'postgres', 'pub-coder-2']);
  assert.deepEqual(members('gitpub-net'), ['git-server', 'pub-git', 'pusher-1', 'pusher-2']);
  assert.deepEqual(members('grader-net'), ['grader']);
  for (const n of ['agentnet-1', 'agentnet-2', 'slot-1', 'slot-2', 'gitpub-net', 'grader-net']) assert.equal(config.networks[n].internal, true, n);
  assert.ok(!config.networks.egress.internal);
});

test('finding 2: only the relays are on a network with the internet; no coder, sandbox, git server, database or Podman service is', compose, () => {
  assert.deepEqual(members('egress'), ['meter-1', 'meter-2', 'pub-coder-1', 'pub-coder-2', 'pub-git']);
  for (const name of ['coder-1', 'coder-2', 'gitbox-1', 'gitbox-2', 'podman-1', 'podman-2', 'grader', 'git-server', 'postgres', 'mock-github', 'pusher-1', 'pusher-2']) {
    const nets = Object.keys(config.services[name].networks ?? {});
    assert.ok(!nets.includes('egress') && !nets.includes('default'), `${name} is on ${nets}`);
  }
  // a relay forwards to one fixed address
  const target = (n) => config.services[n].environment.RELAY_TARGET;
  assert.deepEqual(['coder-1:8080', 'coder-2:8080', 'git-server:8080'], [target('pub-coder-1'), target('pub-coder-2'), target('pub-git')]);
  assert.match(target('meter-1'), /^host\.docker\.internal:\d+$/);
});

test('finding 2: the git sandbox has no secret, no network, a read-only root and the work volume; the coder is told where its socket is', compose, () => {
  for (const slot of [1, 2]) {
    const g = config.services[`gitbox-${slot}`];
    assert.equal(g.network_mode, 'none');
    assert.equal(g.read_only, true);
    assert.deepEqual(g.cap_drop, ['ALL']);
    assert.deepEqual(Object.keys(g.environment).sort(), ['GIT_SANDBOX_LISTEN', 'GIT_SANDBOX_ROOT']);
    assert.deepEqual(g.volumes.map((v) => `${v.source}:${v.target}`), [`work-${slot}:/work`]);
    assert.equal(g.user, '10001:10001');
    assert.equal(config.services[`coder-${slot}`].environment.GIT_SANDBOX_SOCKET, '/work/.gitbox/git.sock');
    assert.equal(g.environment.GIT_SANDBOX_LISTEN, '/work/.gitbox/git.sock');
  }
});

test('defect 8 and finding 7: an agent\'s Podman service mounts its own work volume and one read-only directory, never $BENCH_HOME or the tasks', compose, () => {
  for (const slot of [1, 2]) {
    const mounts = config.services[`podman-${slot}`].volumes.map((v) => `${v.source}:${v.target}${v.read_only ? ':ro' : ''}`).sort();
    assert.deepEqual(mounts, [`/tmp/bh/slots/${slot}:/tmp/bh/slots/${slot}:ro`, `podman-socket-${slot}:/run/podman`, `podman-storage-${slot}:/home/agent/.local/share/containers`, `work-${slot}:/work`].sort());
  }
  const grader = config.services.grader.volumes.map((v) => `${v.source}:${v.target}`).sort();
  assert.deepEqual(grader, ['grader-socket:/run/podman', 'grader-storage:/home/agent/.local/share/containers'].sort(), 'the grader mounts nothing of the host\'s (defect 8)');
  assert.ok(config.services.grader.tmpfs.some((t) => t.startsWith('/grade:')), 'a grading\'s files are in memory of the grader');
  assert.ok(!config.services['podman-1'].volumes.some((v) => v.target === '/tmp/bt' || v.target === '/tmp/bh'));
});

test('compose: every container of the stack that is ours runs the pinned coder image, tag and digest, the same as bench/pins.json', compose, () => {
  for (const name of ['coder-1', 'coder-2', 'meter-1', 'meter-2', 'pub-coder-1', 'pub-coder-2', 'pub-git', 'gitbox-1', 'gitbox-2', 'pusher-1', 'pusher-2']) assert.equal(config.services[name].image, coderRef(PINS), name);
  const images = new Set(Object.values(config.services).map((s) => s.image).filter(Boolean));
  assert.deepEqual([...images].filter((i) => !i.startsWith('ghcr.io/vymalo/another-adam-rs/coder')).sort(), ['adam-rs/podman:dev', 'adam-bench-git-server', 'postgres:16', 'wiremock/wiremock:3.13.2'].sort().filter((i) => images.has(i)));
});

test('compose: no privileged, no cap_add, no devices, no Docker socket, and no model key anywhere', compose, () => {
  const text = JSON.stringify(config);
  for (const [name, s] of Object.entries(config.services)) {
    assert.ok(!s.privileged && !s.cap_add && !s.devices, name);
    for (const v of s.volumes ?? []) assert.ok(!/docker\.sock/.test(v.source ?? ''), `${name} mounts a Docker socket`);
  }
  assert.doesNotMatch(text, /sk-[A-Za-z0-9]{10,}/);
  for (const n of ['podman-1', 'podman-2', 'grader']) {
    assert.equal(config.services[n].cpus, 4);
    assert.equal(Number(config.services[n].mem_limit), 12 * 1024 ** 3);
  }
  assert.equal(config.services['coder-1'].environment.MODEL_API_KEY, 'bench-dummy-key');
});

test('compose: both coders carry the settings the plan names, each its own database, a deployment id, and only the stack\'s git server is allowed', compose, () => {
  for (const slot of [1, 2]) {
    const e = config.services[`coder-${slot}`].environment;
    assert.equal(e.ALLOWED_REPO_HOSTS, 'git-server');
    assert.equal(e.ALLOW_LOCAL_REPOS, 'true');
    assert.equal(e.GITHUB_API_URL, 'http://mock-github:8080');
    assert.equal(e.WORKSPACE_SWEEP_SECS, '0');
    assert.equal(e.DEVCONTAINER_RUNTIME, 'podman');
    assert.equal(e.DEVCONTAINER_PREPULL, 'false');
    assert.equal(e.DEVCONTAINER_NETWORK, 'inherit');
    assert.equal(e.DEVCONTAINER_DEPLOYMENT_ID, `bench-slot-${slot}`);
    assert.match(e.DATABASE_URL, new RegExp(`/adam_bench_${slot}$`));
    assert.match(fs.readFileSync(COMPOSE, 'utf8'), new RegExp(`slot-${slot}\\.env\\n\\s+required: false`));
  }
  assert.equal(config.services['git-server'].environment.AUTO_CREATE_OWNERS, 'bench');
  assert.ok(!config.services['git-server'].ports, 'the git server publishes nothing itself: the relay does');
  assert.match(fs.readFileSync(path.join(HERE, '..', 'postgres-init.sql'), 'utf8'), /adam_bench_1[\s\S]*adam_bench_2/);
});

test('compose: slots do not share a database, a volume, a socket, a network or a relay', compose, () => {
  const mounts = (n) => new Set((config.services[n].volumes ?? []).map((v) => v.source).filter((s) => s && !s.startsWith('/')));
  for (const kind of [['podman-1', 'podman-2'], ['coder-1', 'coder-2'], ['gitbox-1', 'gitbox-2']]) {
    const [a, b] = kind.map(mounts);
    assert.equal([...a].filter((x) => b.has(x)).length, 0, kind.join(' and '));
  }
  assert.notEqual(config.services['coder-1'].environment.DATABASE_URL, config.services['coder-2'].environment.DATABASE_URL);
  assert.deepEqual(Object.keys(config.services['coder-1'].networks), ['slot-1']);
  assert.deepEqual(Object.keys(config.services['coder-2'].networks), ['slot-2']);
});

test('compose: the stack has a service of each kind for each slot', compose, () => {
  for (const kind of ['podman', 'coder', 'gitbox', 'meter', 'pub-coder', 'pusher']) for (const slot of [1, 2]) assert.ok(config.services[`${kind}-${slot}`], `${kind}-${slot}`);
});

test('C: only the three relays publish ports, and only on the loopback', compose, () => {
  const published = Object.entries(config.services).filter(([, svc]) => svc.ports).map(([n]) => n).sort();
  assert.deepEqual(published, ['pub-coder-1', 'pub-coder-2', 'pub-git']);
  for (const n of published) for (const port of config.services[n].ports) assert.equal(port.host_ip, '127.0.0.1', n);
});
