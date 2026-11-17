import assert from 'node:assert/strict';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { fakeRepo, tmp } from './helpers.mjs';

const BENCH = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'bin', 'bench.mjs');
const run = (args, env = {}) => spawnSync(process.execPath, [BENCH, ...args], { env: { ...process.env, ...env }, encoding: 'utf8' });

test('pilot, confirm, run and regrade are commands: with no configuration they exit 78, and an unknown option is a usage error', (t) => {
  const home = tmp(t);
  const env = { HOME: home, BENCH_HOME: path.join(home, 'results'), BENCH_CONFIG: path.join(home, 'none.json') };
  for (const cmd of ['pilot', 'confirm', 'run', 'regrade']) {
    const r = run([cmd], env);
    assert.equal(r.status, 78, `${cmd}: ${r.stderr}`);
    assert.doesNotMatch(r.stderr, /not built yet/);
  }
  assert.match(run(['--help']).stdout, /confirm \[--over-cap\]/);
});

test('no command and an unknown command are usage errors', () => {
  assert.equal(run([]).status, 64);
  const r = run(['frobnicate']);
  assert.equal(r.status, 64);
  assert.match(r.stderr, /unknown command/);
  assert.equal(run(['--help']).status, 0);
  assert.equal(run(['report', '--nope']).status, 64);
});

test('a results directory inside the repository is refused with 78', (t) => {
  const home = tmp(t);
  const r = run(['report'], { HOME: home, BENCH_HOME: path.join(path.dirname(fileURLToPath(import.meta.url)), 'results') });
  assert.equal(r.status, 78);
  assert.match(r.stderr, /BENCH_HOME/);
  const elsewhere = fakeRepo(t);
  const inCwd = spawnSync(process.execPath, [BENCH, 'report'], { cwd: elsewhere, env: { ...process.env, HOME: home, BENCH_HOME: path.join(elsewhere, 'out') }, encoding: 'utf8' });
  assert.equal(inCwd.status, 78, 'the repository of the working directory counts too');
});

test('tasks without a known subcommand is a usage error, and with --help it prints the usage', () => {
  assert.equal(run(['tasks']).status, 64);
  assert.equal(run(['tasks', 'frobnicate']).status, 64);
  const r = run(['tasks', '--help']);
  assert.equal(r.status, 0);
  assert.match(r.stderr, /bench tasks <command>/);
});
