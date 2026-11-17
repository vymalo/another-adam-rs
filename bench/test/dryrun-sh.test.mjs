import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { tmp } from './helpers.mjs';

const SH = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'dry-run.sh');
const run = (env, extra = {}) => spawnSync('sh', [SH], { env: { PATH: process.env.PATH, ...env }, encoding: 'utf8', ...extra });

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
