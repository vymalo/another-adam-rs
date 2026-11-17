import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { OpenCodeAgent, SHARED_PROMPT_FILE, opencodeConfig } from '../lib/agents/opencode.mjs';
import { configFingerprint } from '../lib/budget.mjs';
import { normalizeConfig } from '../lib/config.mjs';
import { localGit } from '../lib/grade.mjs';
import { PINS } from '../lib/pins.mjs';
import { INSTRUCTIONS_PATH, PREAMBLE, SECTIONS, adamToolNames, frontMatterVars, namedTools, readInstructionsAt, sectionsOf, sharedPrompt } from '../lib/shared-prompt.mjs';
import { rawConfig, tmp } from './helpers.mjs';

const REPO = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const OWNER_SECTIONS = ['How to work', 'A missing toolchain', 'Ending your turn', 'Rules you must not break'];

/** adam's instructions at the pin, or null when this checkout does not have that revision (CI fetches all history; a shallow clone skips). */
async function atPin(t) {
  try {
    await localGit(REPO)(['cat-file', '-e', `${PINS.coder.revision}^{commit}`]);
  } catch {
    if (process.env.CI) assert.fail(`the pinned revision ${PINS.coder.revision} is not in this checkout: fetch all history`);
    t.skip('the pinned coder revision is not in this checkout');
    return null;
  }
  return readInstructionsAt({ repoDir: REPO, revision: PINS.coder.revision });
}

test('every section of adam\'s instructions at the pin has a decision, and exactly the four the owner named are shared', async (t) => {
  const file = await atPin(t);
  if (!file) return;
  const parsed = sharedPrompt(file); // throws on a section with no decision
  const headings = parsed.sections.map((s) => s.heading);
  assert.deepEqual([...headings].sort(), Object.keys(SECTIONS).sort(), 'the table names every section, and only those');
  assert.deepEqual(headings.filter((h) => SECTIONS[h].shared), OWNER_SECTIONS, 'in the file\'s order');
  for (const [heading, d] of Object.entries(SECTIONS)) assert.ok(d.why.length > 10, `${heading} says why`);
});

test('the shared text is a byte-for-byte subset of adam\'s file at the pin: each shared section is in it unchanged, in order', async (t) => {
  const file = await atPin(t);
  if (!file) return;
  const { template, sections } = sharedPrompt(file);
  let from = 0;
  for (const s of sections.filter((x) => SECTIONS[x.heading].shared)) {
    const at = file.indexOf(s.raw, from);
    assert.ok(at >= from, `${s.heading} is in the file, after the one before it`);
    from = at + s.raw.length;
  }
  assert.equal(template, sections.filter((s) => SECTIONS[s.heading].shared).map((s) => s.raw).join(''));
  assert.ok(template.length > 3000, 'a real prompt, not an empty one');
});

test('what is left out is left out: none of the tool, feature or persona sections is in the shared text', async (t) => {
  const file = await atPin(t);
  if (!file) return;
  const { text, template } = sharedPrompt(file);
  for (const [heading, d] of Object.entries(SECTIONS)) {
    if (d.shared || heading === PREAMBLE) continue;
    assert.ok(!template.includes(`# ${heading}\n`), `"${heading}" is not shared`);
  }
  assert.ok(!text.includes('Your name is'), 'no persona');
  assert.ok(!/\{\{/.test(text), 'every variable is filled in');
  assert.ok(!text.startsWith('---'), 'no front matter');
});

test('a section that names an adam-only tool is never shared, except the four the owner named, and those are listed', async (t) => {
  const file = await atPin(t);
  if (!file) return;
  const tools = adamToolNames(file);
  assert.ok(tools.has('prepare_workspace') && tools.has('run_checks') && tools.has('ask_user'), 'the tool names come from the Tools section');
  const sections = sectionsOf(file);
  const offenders = sections.filter((s) => SECTIONS[s.heading].shared && !SECTIONS[s.heading].owner && namedTools(s.raw, tools).length > 0);
  assert.deepEqual(offenders.map((s) => s.heading), [], 'no other shared section names a tool');
  // The four the owner named do name tools OpenCode does not have; kept word for word as decided, and listed so the README is held to it.
  const named = new Set(sections.filter((s) => SECTIONS[s.heading].owner).flatMap((s) => namedTools(s.raw, tools)));
  assert.deepEqual([...named].sort(), ['apply_patch', 'ask_user', 'commit_and_push', 'edit_file', 'open_pull_request', 'prepare_workspace', 'read_file', 'request_repository', 'run', 'run_checks', 'run_command', 'share_file', 'start_scratch', 'write_file']);
  assert.ok(named.has('prepare_workspace') && named.has('run_checks') && named.has('open_pull_request'));
  // and the sections that were left out do name them, which is part of why
  const left = sections.filter((s) => !SECTIONS[s.heading].shared && s.heading !== PREAMBLE);
  assert.ok(left.filter((s) => namedTools(s.raw, tools).length > 0).length >= 6);
});

test('the variables are filled in from the file\'s own front matter and from nothing else', async (t) => {
  const file = await atPin(t);
  if (!file) return;
  assert.deepEqual(frontMatterVars(file), { max_check_cycles: '3', scratch_check_cycles: '5' });
  const { template, text } = sharedPrompt(file);
  assert.match(template, /\{\{max_check_cycles\}\} times/);
  assert.match(text, /at most 3 times in\s+a repository, and 5 times in a scratch project/);
  assert.equal(text, template.replace('{{max_check_cycles}}', '3').replace('{{scratch_check_cycles}}', '5'));
});

const SYNTH = (extra = '', vars = '  max_check_cycles: 3\n') => `---\nname: coder\nvars:\n${vars}---\nYour name is Adam.\n\n${Object.keys(SECTIONS).filter((h) => h !== PREAMBLE).map((h) => `# ${h}\nbody of ${h} {{max_check_cycles}}\n\n`).join('')}${extra}`;

test('a section the table does not know, or one that went away, is an error that says what to decide', () => {
  assert.doesNotThrow(() => sharedPrompt(SYNTH()));
  assert.throws(() => sharedPrompt(SYNTH('# A new section\nbody\n')), /"A new section".*no decision/);
  assert.throws(() => sharedPrompt(SYNTH().replace('# Helpers\n', '# Subagents\n')), /Subagents/);
  assert.throws(() => sharedPrompt(SYNTH().replace('# Access tiers\nbody of Access tiers {{max_check_cycles}}\n\n', '')), /no longer have the section\(s\) Access tiers/);
  assert.throws(() => sharedPrompt(SYNTH('', '  scratch_check_cycles: 5\n')), /does not give/);
});

test('OpenCode is given exactly the shared text: a file in its read-only input directory, named in the configuration', () => {
  const config = opencodeConfig({ baseUrl: 'http://meter-1:8090/r/abcdefabcdefabcdefabcdef/v1', contextWindow: 131072, instructions: [`/bench/in/${SHARED_PROMPT_FILE}`] });
  assert.deepEqual(config.instructions, ['/bench/in/shared-prompt.md']);
  assert.equal('instructions' in opencodeConfig({ baseUrl: 'http://meter-1:8090/r/abcdefabcdefabcdefabcdef/v1', contextWindow: 131072 }), false);
});

test('OpenCode\'s driver writes the shared text byte for byte beside the statement and puts the file in the run\'s configuration', async (t) => {
  const { FakeContainers } = await import('../lib/containers.mjs');
  const { loadTask } = await import('../lib/task.mjs');
  const { makeTask } = await import('./taskfix.mjs');
  const text = '# How to work\nexact text, with {braces} and a trailing blank line\n\n';
  const slotDir = path.join(tmp(t), 'slots', '1');
  const containers = new FakeContainers();
  const agent = new OpenCodeAgent({ pins: PINS, home: tmp(t), contextWindow: 131072, sharedPrompt: text, fetchBinary: async () => ({ path: process.execPath, cached: true, integrity: PINS.opencode.integrity, version: PINS.opencode.version }), pollMs: 5 });
  const stack = { slotDir: () => slotDir, workDir: async () => {}, extractTree: async () => {}, readFiles: async () => ({ files: new Map(), refused: [] }) };
  const ctx = { run: 'r1', attempt: 1, slot: 1, task: loadTask(makeTask(tmp(t), 'demo-1')), base: { dir: '/h', sha: 'a'.repeat(40), slug: 'demo-1-aaaaaa', workdir: '/app', containerRepo: '/work/bench-base/base.git' }, stack, containers, meter: { token: 'abcdefabcdefabcdefabcdef', baseUrl: 'http://meter-1:8090/r/abcdefabcdefabcdefabcdef/v1', refusal: () => null }, paths: { run: tmp(t), scratch: tmp(t) }, limits: { minutes: 1, steps: 10, cancelGraceSeconds: 1, limitMs: 60000, cancelGraceMs: 1000 }, contextWindow: 131072, log: () => {} };
  await agent.prepare(ctx);
  assert.equal(fs.readFileSync(path.join(slotDir, 'in-r1-a1', SHARED_PROMPT_FILE), 'utf8'), text);
  assert.equal(fs.readFileSync(path.join(slotDir, 'in-r1-a1', 'instruction.md'), 'utf8'), ctx.task.instruction, 'the statement is still alone in its file');
  await agent.run(ctx).catch(() => {});
  const runSpec = containers.runs.find((r) => r.script.includes(' run --standalone '));
  assert.deepEqual(JSON.parse(runSpec.input).instructions, ['/bench/in/shared-prompt.md']);
  assert.ok(runSpec.mounts.some((m) => m.target === '/bench/in' && m.readOnly), 'the input directory is read-only');
});

test('the configuration digest includes the shared prompt, so changing it voids a ledger', () => {
  const config = normalizeConfig(rawConfig());
  const fp = (sharedPrompt) => configFingerprint({ config, pins: PINS, lockTexts: ['a', 'b'], key: 'k', sharedPrompt });
  assert.equal(fp('one').digest, fp('one').digest);
  assert.notEqual(fp('one').digest, fp('two').digest);
  assert.deepEqual(Object.keys(fp('one').fields).includes('prompt'), true);
  assert.notEqual(fp('one').fields.prompt, fp('two').fields.prompt);
  assert.ok(!JSON.stringify(fp('a very specific sentence')).includes('specific'), 'the text is not in the fingerprint');
});
