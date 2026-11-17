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

test('the README and the ADR say what the agents do: the fixed reply, the limit, the deny rules, the choice about subagents', () => {
  assert.ok(README.includes(NUDGE));
  assert.ok(README.includes(MODEL));
  assert.match(README, /every 10 s/);
  assert.match(README, /time limit \(50 minutes\)/);
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
  assert.match(README, /Nothing that needs a container service has run/);
  assert.match(README, /The dry run itself has not been run/);
  const texts = [README, ADR, read('../compose.yaml'), read('../dry-run.sh')].join('\n');
  assert.doesNotMatch(texts, /\b(revenue|pricing strategy|customers?|competitors?|market share)\b/i);
});
