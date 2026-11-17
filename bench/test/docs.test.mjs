import assert from 'node:assert/strict';
import fs from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { LIMITS } from '../lib/meter.mjs';

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
