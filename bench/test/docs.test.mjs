import assert from 'node:assert/strict';
import fs from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { LIMITS } from '../lib/meter.mjs';
import { MAX_PATCH_BYTES } from '../lib/grade.mjs';
import { MAX_NON_TEST_LINES } from '../lib/mine.mjs';
import { OWN_PER_REPO, OWN_TOTAL, PUBLIC_PER_REPO, PUBLIC_TOTAL } from '../lib/tasks-cli.mjs';

const read = (rel) => fs.readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');
const README = read('../README.md');
const ADR = read('../../docs/decisions/0048-a-benchmark-harness-for-adam-coder-and-opencode.md');
const MiB = (n) => `${n / 1024 / 1024} MiB`;

test('the README names the meter limits with the numbers the code has', () => {
  assert.ok(README.includes(`\`maxRequestBytes\` ${MiB(LIMITS.maxRequestBytes)}`), 'request limit');
  assert.ok(README.includes(`\`maxResponseBytes\` ${MiB(LIMITS.maxResponseBytes)}`), 'response limit');
  assert.ok(README.includes(`\`maxCallMs\` ${LIMITS.maxCallMs / 60000} minutes`), 'call time limit');
});

test('the README and the ADR say the cap is soft by one call per running slot', () => {
  for (const [name, text] of [['README', README], ['ADR', ADR]]) {
    assert.match(text, /soft by (at most )?one call per (running )?slot/i, name);
    assert.match(text, /excess/i, name);
  }
});

test('the README says what the lock, the guard, the error body, n and the interruptions are', () => {
  assert.match(README, /link/i);
  assert.match(README, /`\.git`/);
  assert.match(README, /"type":"upstream_error"/);
  assert.match(README, /`n`[^\n]*(absent or exactly 1|absent or 1)/);
  assert.match(README, /at most two interruptions/i);
  assert.match(README, /loopback/i);
});

test('the README names the task limits with the numbers the code has', () => {
  assert.ok(README.includes(`at most ${MAX_NON_TEST_LINES} non-test lines`));
  assert.ok(README.includes(`more than ${MAX_PATCH_BYTES / 1024 / 1024} MB`));
  assert.ok(README.includes(`public: ${PUBLIC_TOTAL}, at most ${PUBLIC_PER_REPO} per repository`));
  assert.ok(README.includes(`own: ${OWN_TOTAL}, at most ${OWN_PER_REPO} per repository`));
});

test('the README says what is never in the repository and what stays unverified about the tasks', () => {
  assert.match(README, /No task content, ever/);
  assert.match(README, /there is no default/);
  assert.match(README, /read-only in that container only/);
  assert.match(README, /\*Unverified:\* where under `v2\/`/);
  const own = fs.readFileSync(fileURLToPath(new URL('../tasks/own/README.md', import.meta.url)), 'utf8');
  assert.match(own, /never from its body/);
  assert.match(own, /## Interface/);
});
