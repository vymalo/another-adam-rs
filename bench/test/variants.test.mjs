import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { normalizeConfig } from '../lib/config.mjs';
import { PROFILES, sharedPrompt } from '../lib/shared-prompt.mjs';
import { AGENT_NAMES, LEAN_SECTIONS, LEAN_TOOLS, VARIANTS, leanInstructions, makeDrivers, makeLeanFolder, withoutUnusedVars } from '../lib/variants.mjs';

const FILE = `---
name: coder
description: "x"
limits:
  max_turns: 200
vars:
  # a comment
  max_check_cycles: 3
  repository_creation: >-
    a long text
    on two lines
  display_name: Adam
card:
  name: Adam
---
Your name is {{display_name}}.

# Who you are and how you talk

persona

# Tools

the tools

# How to work

checks {{max_check_cycles}} times

# A missing toolchain

toolchain

# Starting without a repository

{{repository_creation}}

# Ending your turn

end

# Rules you must not break

rules
`;

test('adam-lean: tools set, only the work sections kept in order, unused vars and their comments dropped', () => {
  const out = leanInstructions(FILE);
  assert.match(out, new RegExp(`^---\\nname: coder\\ntools: \\[${LEAN_TOOLS.join(', ')}\\]\\n`));
  assert.ok(out.includes('Your name is {{display_name}}.'), 'the opening paragraphs stay');
  const headings = [...out.matchAll(/^# (.+)$/gm)].map((m) => m[1]);
  assert.deepEqual(headings, LEAN_SECTIONS);
  assert.ok(!out.includes('repository_creation'), 'a var no kept section reads is gone, with its continuation lines');
  assert.ok(!out.includes('a long text'));
  assert.ok(!out.includes('# a comment'));
  assert.match(out, /^ {2}max_check_cycles: 3$/m);
  assert.match(out, /^ {2}display_name: Adam$/m);
  assert.match(out, /^card:\n {2}name: Adam$/m, 'what follows vars is kept');
});

test('adam-lean refuses a file it cannot reshape safely', () => {
  assert.throws(() => leanInstructions('no front matter'), /front matter/);
  assert.throws(() => leanInstructions(FILE.replace('name: coder', 'name: coder\ntools: [a]')), /already set tools/);
  assert.throws(() => leanInstructions(FILE.replace('# Ending your turn', '# Ending')), /Ending your turn/);
});

test('withoutUnusedVars keeps a front matter without vars as it is', () => {
  assert.equal(withoutUnusedVars('name: a\nlimits:\n  max_turns: 3', 'body'), 'name: a\nlimits:\n  max_turns: 3');
});

test('the lean folder loses its subagents and skills and gets the lean instructions', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'lean-'));
  fs.writeFileSync(path.join(dir, 'instructions.md'), FILE);
  for (const d of ['subagents', 'skills']) fs.mkdirSync(path.join(dir, d, 'x'), { recursive: true });
  makeLeanFolder(dir);
  assert.ok(!fs.existsSync(path.join(dir, 'subagents')) && !fs.existsSync(path.join(dir, 'skills')));
  assert.equal(fs.readFileSync(path.join(dir, 'instructions.md'), 'utf8'), leanInstructions(FILE));
  fs.rmSync(dir, { recursive: true, force: true });
});

test('makeDrivers: one driver per named variant, with its name, folder or prompt; a prompt it needs must be given', () => {
  class Adam {
    constructor(o) {
      Object.assign(this, o, { kind: 'adam' });
    }
  }
  class Oc {
    constructor(o) {
      Object.assign(this, o, { kind: 'opencode' });
    }
  }
  const d = makeDrivers({ make: { adam: Adam, opencode: Oc }, prompts: { owner: 'O', work: 'W' }, common: { home: '/h' } });
  assert.deepEqual(Object.keys(d), AGENT_NAMES);
  assert.equal(d['adam-lean'].variant, 'lean');
  assert.equal(d.adam.variant, 'full');
  assert.equal(d.opencode.sharedPrompt, 'O');
  assert.equal(d['opencode-prompt'].sharedPrompt, 'W');
  assert.equal(d['opencode-bare'].sharedPrompt, null);
  for (const [n, x] of Object.entries(d)) assert.equal(x.name, n);
  assert.throws(() => makeDrivers({ make: { adam: Adam, opencode: Oc }, prompts: { owner: 'O' }, common: {}, agents: ['opencode-prompt'] }), /work prompt is required/);
  assert.deepEqual(Object.keys(makeDrivers({ make: { adam: Adam, opencode: Oc }, prompts: {}, common: {}, agents: ['adam', 'opencode-bare'] })), ['adam', 'opencode-bare']);
});

test('the config names the variants to run: default adam and opencode, unknown or repeated names refused', () => {
  const base = { endpoint: 'https://models.example.test/v1', modelId: 'm', modelLabel: 'M', prices: { inputPerMTok: 1, cachedInputPerMTok: 0.1, cacheWritePerMTok: 1, outputPerMTok: 2 }, capUSD: 10, perRunBudgetUSD: 1, arms: { a: {} }, contextWindow: 100000 };
  assert.deepEqual(normalizeConfig(base).agents, ['adam', 'opencode']);
  assert.deepEqual(normalizeConfig({ ...base, agents: AGENT_NAMES.slice(1) }).agents, AGENT_NAMES.slice(1));
  assert.throws(() => normalizeConfig({ ...base, agents: ['adam'] }), /two or more/);
  assert.throws(() => normalizeConfig({ ...base, agents: ['adam', 'adam'] }), /two or more different/);
  assert.throws(() => normalizeConfig({ ...base, agents: ['adam', 'claude'] }), /no agent variant claude/);
  for (const v of Object.values(VARIANTS)) assert.ok(v.driver === 'adam' || v.profile === null || Object.hasOwn(PROFILES, v.profile));
});

test('the work profile adds Helpers, in OpenCode words; the owner profile is unchanged', () => {
  const file = fs.readFileSync(path.join(import.meta.dirname, '..', '..', 'bin', 'adam-coder', 'agent', 'instructions.md'), 'utf8');
  const owner = sharedPrompt(file);
  const work = sharedPrompt(file, { profile: 'work' });
  assert.ok(!owner.text.includes('# Helpers'));
  assert.ok(work.text.includes('# Helpers'));
  assert.ok(work.text.includes('`subagent`') && !work.text.includes('`explorer`'));
  assert.equal(work.text.replace(/# Helpers\n[\s\S]*?(?=# )/, ''), owner.text);
  assert.throws(() => sharedPrompt(file, { profile: 'nope' }), /no prompt profile/);
});
