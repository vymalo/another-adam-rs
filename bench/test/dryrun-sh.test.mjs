import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { tmp } from './helpers.mjs';

const SH = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'dry-run.sh');
const run = (env, extra = {}) => spawnSync('sh', [SH], { env: { PATH: process.env.PATH, ...env }, encoding: 'utf8', ...extra });

const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};

/** The pid in `file`, or null while it is not written (or not a number). */
const pidOf = (file) => {
  try {
    const n = Number(fs.readFileSync(file, 'utf8').trim());
    return Number.isInteger(n) && n > 0 ? n : null;
  } catch {
    return null;
  }
};

/** Wait until `check()` is true, at most `ms`; false when it never is. */
const until = async (check, ms = 10_000) => {
  const deadline = Date.now() + ms;
  for (;;) {
    if (check()) return true;
    if (Date.now() > deadline) return false;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
};

/** A PATH of the real tools the script needs, and nothing else. */
function stubPath(t, extra = {}) {
  const dir = path.join(tmp(t), 'bin');
  fs.mkdirSync(dir);
  for (const tool of ['sh', 'dirname', 'cat', 'sleep', 'rm', 'chmod', 'grep', 'setsid']) {
    for (const from of ['/usr/bin', '/bin']) {
      if (fs.existsSync(path.join(from, tool))) {
        fs.symlinkSync(path.join(from, tool), path.join(dir, tool));
        break;
      }
    }
  }
  for (const [name, text] of Object.entries(extra)) fs.writeFileSync(path.join(dir, name), text, { mode: 0o755 });
  return dir;
}

test('the dry run script is gated: skipped without BENCH_TEST_DOCKER, a failure when it is required and not enabled', () => {
  const skipped = run({});
  assert.equal(skipped.status, 0);
  assert.match(skipped.stdout, /skipped \(BENCH_TEST_DOCKER is not 1\)/);
  const required = run({ BENCH_TEST_REQUIRE_DOCKER: '1' });
  assert.equal(required.status, 1);
  assert.match(required.stderr, /needs BENCH_TEST_DOCKER=1/);
});

test('enabled, it says what is missing instead of going on: no docker on the path is a failure that names it', (t) => {
  const bin = tmp(t);
  fs.symlinkSync(process.execPath, path.join(bin, 'node'));
  for (const tool of ['sh', 'dirname', 'cat', 'mktemp', 'rm', 'chmod']) {
    for (const dir of ['/usr/bin', '/bin']) {
      if (fs.existsSync(path.join(dir, tool))) {
        fs.symlinkSync(path.join(dir, tool), path.join(bin, tool));
        break;
      }
    }
  }
  const r = run({ PATH: bin, BENCH_TEST_DOCKER: '1', BENCH_HOME: tmp(t) });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /docker is not installed/);
});

test('the script is POSIX sh: no bash-only syntax', () => {
  const text = fs.readFileSync(SH, 'utf8');
  assert.match(text, /^#!\/bin\/sh\n/);
  assert.doesNotMatch(text, /\[\[|\bfunction\b|<\(|\$\{[A-Za-z_]+\/|\barray\b/);
  assert.equal(spawnSync('sh', ['-n', SH]).status, 0);
});

test('a doctor that fails stops the script before the harness starts: the 20 GB floor is enforced before a pull', (t) => {
  // `bench doctor` fails under MIN_FREE_GB (20) before anything is pulled; the script's `set -e` is what turns that
  // failure into "the harness never starts". A stub doctor exits 1 and a stub harness would leave a marker.
  const dir = tmp(t);
  const home = path.join(dir, 'home');
  fs.mkdirSync(home);
  const marker = path.join(dir, 'harness-started');
  const bin = stubPath(t, {
    node: `#!/bin/sh\ncase "\${2:-}" in\n  doctor) exit 1 ;;\n  dry-run) touch '${marker}'; exit 0 ;;\nesac\nexit 0\n`,
    docker: '#!/bin/sh\nexit 0\n',
  });
  const r = run({ PATH: bin, BENCH_TEST_DOCKER: '1', BENCH_HOME: home });
  assert.equal(r.status, 1, 'the doctor\'s failure is the script\'s');
  assert.equal(fs.existsSync(marker), false, 'the harness was not started');
});

test('a dry run that is stopped takes its whole process group: no resume child outlives the cleanup', async (t) => {
  // The defect this test was written for: a killed dry run left its `resume-child` (and the meter's relay) behind, and
  // the orphan rebuilt the self images under the next run, which then failed with `No such image`. The script starts the
  // harness in a session of its own and kills the group in its EXIT trap. Here a stub harness makes a child, the script
  // is stopped with SIGTERM (as a supervisor or a person would), and neither may be left.
  if (!['/usr/bin/setsid', '/bin/setsid'].some((p) => fs.existsSync(p))) {
    t.skip('setsid is not installed: the harness cannot be given a session of its own');
    return;
  }
  const dir = tmp(t);
  const home = path.join(dir, 'home');
  fs.mkdirSync(home);
  const nodePid = path.join(dir, 'node.pid');
  const childPid = path.join(dir, 'child.pid');
  const bin = stubPath(t, {
    node: `#!/bin/sh
case "\${2:-}" in
  doctor) exit 0 ;;
  dry-run)
    if [ "\${3:-}" = --down ]; then exit 0; fi
    echo $$ > '${nodePid}'
    sh -c 'echo $$ > "${childPid}"; exec sleep 300' &
    wait
    ;;
esac
exit 0
`,
    docker: '#!/bin/sh\nexit 0\n',
  });
  const script = spawn('sh', [SH], { env: { PATH: bin, BENCH_TEST_DOCKER: '1', BENCH_HOME: home }, stdio: 'ignore' });
  const pids = [];
  try {
    assert.ok(await until(() => pidOf(nodePid) && pidOf(childPid)), 'the stub harness and its child did not start');
    pids.push(pidOf(nodePid), pidOf(childPid));
    script.kill('SIGTERM');
    const exited = await until(() => script.exitCode !== null || script.signalCode !== null, 15_000);
    assert.ok(exited, 'the script did not stop');
    assert.ok(await until(() => pids.every((p) => !alive(p)), 10_000), `left behind: ${pids.filter(alive).join(', ')}`);
  } finally {
    for (const p of pids) if (p && alive(p)) {
      try {
        process.kill(-p, 'SIGKILL');
      } catch {
        try {
          process.kill(p, 'SIGKILL');
        } catch {
          // already gone
        }
      }
    }
    if (script.exitCode === null && script.signalCode === null) script.kill('SIGKILL');
  }
});
