import assert from 'node:assert/strict';
import fs from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { NUDGE } from '../lib/a2a.mjs';
import { PINS } from '../lib/pins.mjs';
import { MODEL } from '../lib/agents/opencode.mjs';
import { SLOTS } from '../lib/stack.mjs';
import { MIN_FREE_GB, WARN_FREE_GB } from '../lib/doctor.mjs';

const read = (rel) => fs.readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');
const README = read('../README.md');
const ADR = read('../../docs/decisions/0048-a-benchmark-harness-for-adam-coder-and-opencode.md');

test('the README names the pins that bench/pins.json holds', () => {
  assert.ok(README.includes(`${PINS.coder.image}:${PINS.coder.tag}@${PINS.coder.digest.slice(0, 11)}`));
  assert.ok(README.includes(`\`@opencode/cli\` ${PINS.opencode.version}`));
  assert.ok(README.includes(`\`${PINS.opencode.package}\``));
  assert.ok(README.includes(PINS.opencode.integrity.slice(0, 11)));
  assert.ok(README.includes(PINS.coder.revision.slice(0, 12)));
  assert.ok(!/opencode-ai|1\.19\.5/.test(README), 'no word of the old line is left as a pin');
});

test('the README and the ADR say what the agents do: the fixed reply, the limit, the deny rules, the choice about subagents', async () => {
  assert.ok(README.includes(NUDGE));
  assert.ok(README.includes(MODEL));
  assert.match(README, /every 10 s/);
  const { normalizeConfig } = await import("../lib/config.mjs");
  const { rawConfig } = await import("./helpers.mjs");
  const { dryRunConfig } = await import("../lib/dryrun.mjs");
  const limits = normalizeConfig(rawConfig()).limits;
  assert.ok(README.includes("`minutes` (the time limit of a run, " + limits.minutes + ")"), "the README names the configured default");
  assert.equal(limits.minutes, 50);
  assert.ok(normalizeConfig({ ...rawConfig(), ...dryRunConfig({ endpoint: "http://127.0.0.1:1/v1" }) }).limits.minutes < 50, "the dry run sets the same key a real run reads");
  assert.match(README, /\| allow \| deny \| deny \| deny \|/);
  assert.match(README, /`question: deny`/);
  assert.match(README, /leave them on and let `run` wait for them/);
  assert.match(README, /Unverified at 2\.3\.0/);
  assert.ok(ADR.includes('Amended 2026-11-17: the owner chose OpenCode V2 (`@opencode/cli`) for the benchmark.'));
  assert.match(ADR, /Amended 2026-11-17: slice 3 is built/);
});

test('the README numbers are the code\'s', () => {
  assert.match(README, new RegExp(`under ${MIN_FREE_GB} GB`));
  assert.match(README, new RegExp(`under ${WARN_FREE_GB} GB`));
  assert.match(README, new RegExp(`The compose file has ${['', 'one', 'two'][SLOTS]} slots`));
});

test('the README says what did not run, and no word of the market or a private value is in the slice\'s texts', () => {
  assert.match(README, /The dry run passed on this machine/);
  assert.match(README, /What the dry run did not cover/);
  const texts = [README, ADR, read('../compose.yaml'), read('../dry-run.sh')].join('\n');
  assert.doesNotMatch(texts, /\b(revenue|pricing strategy|customers?|competitors?|market share)\b/i);
});

test('the meter binds on every interface for the bench, so the relay reaches the host, in the code and in the README', () => {
  const code = read('../lib/dryrun.mjs');
  assert.match(code, /BENCH_METER_BIND \?\? '0\.0\.0\.0'/);
  assert.match(README, /\*\*`0\.0\.0\.0`\*\* for the bench/);
});
