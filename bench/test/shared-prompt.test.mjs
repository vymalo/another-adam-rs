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
import { NAMES, NO_COUNTERPART, SPANS, applyMap, withoutSubstitutions } from '../lib/prompt-map.mjs';
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
  const { template, mapped, text } = sharedPrompt(file);
  assert.match(template, /\{\{max_check_cycles\}\} times/);
  assert.match(text, /at most 3 times in\s+a repository \(a cycle is/);
  assert.equal(text, mapped.replace('{{max_check_cycles}}', '3'));
  assert.ok(!/\{\{/.test(mapped.replace('{{max_check_cycles}}', '')), 'no other variable is left');
});

const SYNTH = (extra = '', vars = '  max_check_cycles: 3\n') => `---\nname: coder\nvars:\n${vars}---\nYour name is Adam.\n\n${Object.keys(SECTIONS).filter((h) => h !== PREAMBLE).map((h) => `# ${h}\nbody of ${h} {{max_check_cycles}}\n\n`).join('')}${extra}`;

test('a section the table does not know, or one that went away, is an error that says what to decide', async (t) => {
  assert.throws(() => sharedPrompt(SYNTH()), /does not find its text exactly once/, 'every section is known, but the synthetic text is not adam\'s, so the mapping refuses it');
  assert.throws(() => sharedPrompt(SYNTH('# A new section\nbody\n')), /"A new section".*no decision/);
  assert.throws(() => sharedPrompt(SYNTH().replace('# Helpers\n', '# Subagents\n')), /Subagents/);
  assert.throws(() => sharedPrompt(SYNTH().replace('# Access tiers\nbody of Access tiers {{max_check_cycles}}\n\n', '')), /no longer have the section\(s\) Access tiers/);
  const file = await atPin(t);
  if (file) assert.throws(() => sharedPrompt(file.replace('  max_check_cycles: 3\n', '')), /does not give/);
});

test('a new adam tool named in a shared section, with no entry in the mapping, is an error and never reaches OpenCode', async (t) => {
  const file = await atPin(t);
  if (!file) return;
  const tools = file.indexOf('\n# Tools\n');
  const work = file.indexOf('\n# How to work\n');
  assert.ok(tools >= 0 && work >= 0, 'the file has both sections');
  const withTool = file
    .replace('\n# How to work\n', '\n# How to work\nCall `brand_new_tool` before anything else.\n')
    .replace('\n# Tools\n', '\n# Tools\n- `brand_new_tool`: a tool adam gained after the mapping was written.\n');
  assert.throws(() => sharedPrompt(withTool), /still names adam's tool\(s\) brand_new_tool/);
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

test('OpenCode\'s shared text names no adam-only tool, and none of adam\'s product features', async (t) => {
  const file = await atPin(t);
  if (!file) return;
  const { text } = sharedPrompt(file);
  assert.deepEqual(namedTools(text, adamToolNames(file)), []);
  for (const word of ['prepare_workspace', 'run_checks', 'ask_user', 'commit_and_push', 'open_pull_request', 'share_file', 'start_scratch', 'request_repository', 'apply_patch', 'run_command', 'read_file', 'read_files', 'write_file', 'edit_file', 'accept_red_checks', 'obscura', 'scratch']) {
    assert.ok(!text.includes(word), `${word} is not in OpenCode's text`);
  }
  for (const own of ['`shell`', '`read`', '`write`', '`edit`']) assert.ok(text.includes(own), `${own} is named`);
  assert.ok(!text.includes('`patch`'), 'OpenCode 2.3.0 has no patch tool');
});

test('every adam tool named in the four sections is in the mapping table: a renamed one, or one a sentence was rewritten or dropped for', async (t) => {
  const file = await atPin(t);
  if (!file) return;
  const { template, applied } = sharedPrompt(file);
  const named = namedTools(template, adamToolNames(file));
  const handled = new Set([...Object.keys(NAMES), ...NO_COUNTERPART]);
  assert.deepEqual(named.filter((n) => !handled.has(n)), [], 'no tool is unmapped');
  // each tool with no counterpart is inside a span the table rewrites or drops; each renamed tool has a rename or sits in a span
  for (const tool of NO_COUNTERPART) {
    const inSpan = applied.filter((a) => a.kind !== 'name' && a.original.includes(`\`${tool}`));
    assert.ok(inSpan.length > 0, `${tool} is handled by a span`);
  }
  for (const [from, to] of Object.entries(NAMES)) {
    const renamed = applied.some((a) => a.id === `name:${from}`) || applied.some((a) => a.kind !== 'name' && a.original.includes(`\`${from}\``));
    assert.ok(renamed, `${from} is mapped to ${to}`);
  }
  assert.deepEqual(NAMES, { run_command: 'shell', run: 'shell', read_file: 'read', write_file: 'write', edit_file: 'edit', apply_patch: 'edit' });
});

test('the mapped text is the original with only the table\'s substitutions applied, segment by segment', async (t) => {
  const file = await atPin(t);
  if (!file) return;
  const { template, mapped, applied } = sharedPrompt(file);
  assert.notEqual(mapped, template);
  // cut every substitution out of both texts: what is left must be the same
  assert.equal(withoutSubstitutions(template, applied, 'template'), withoutSubstitutions(mapped, applied, 'mapped'));
  for (const a of applied) {
    assert.equal(template.slice(a.at, a.at + a.length), a.original, `${a.id}: the original is where it says`);
    assert.equal(mapped.slice(a.mappedAt, a.mappedAt + a.mappedLength), a.to, `${a.id}: the replacement is where it says`);
  }
  // and every span of the table was used once, in the template's order
  assert.deepEqual(SPANS.map((s) => s.id).sort(), applied.filter((a) => a.kind !== 'name').map((a) => a.id).sort());
  assert.deepEqual(applied.filter((a) => a.kind === 'drop').map((a) => a.id), ['how-5-screenshot', 'toolchain-result-says', 'ending-delivered-file', 'rules-scratch'].sort((x, y) => applied.findIndex((a) => a.id === x) - applied.findIndex((a) => a.id === y)));
});

test('a revision of adam\'s file that rewords a mapped sentence, or adds an unmapped tool to a shared section, is an error', () => {
  assert.throws(() => applyMap('# How to work\nnothing the table knows\n'), /does not find its text exactly once/);
  const file = (extra) => `---\nname: coder\nvars:\n  max_check_cycles: 3\n---\n# Tools\n\n- \`new_tool { x }\`: does something.\n- \`ask_user { q }\`: asks.\n\n${Object.keys(SECTIONS).filter((h) => h !== PREAMBLE && h !== 'Tools').map((h) => `# ${h}\n${h === 'How to work' ? extra : ''}\n`).join('')}`;
  assert.throws(() => sharedPrompt(file('call `new_tool` now')), /does not find its text|still names/);
});

test('the fingerprint covers the mapped text: the digest is of what OpenCode receives', async (t) => {
  const file = await atPin(t);
  if (!file) return;
  const { text } = sharedPrompt(file);
  const config = normalizeConfig(rawConfig());
  const fp = (sharedPromptText) => configFingerprint({ config, pins: PINS, lockTexts: ['a', 'b'], key: 'k', sharedPrompt: sharedPromptText });
  assert.equal(fp(text).digest, fp(sharedPrompt(file).text).digest);
  assert.notEqual(fp(text).digest, fp(text.replace('`shell`', '`bash`')).digest, 'a changed substitution voids a ledger');
  assert.notEqual(fp(text).digest, fp(sharedPrompt(file).template).digest, 'adam\'s own words are another text');
});

test('the README and ADR 0048 show every replaced or dropped phrase, so the owner sees how OpenCode\'s text differs', async (t) => {
  const file = await atPin(t);
  if (!file) return;
  const { applied } = sharedPrompt(file);
  const norm = (x) => x.replace(/\s*\n\s*/g, ' ').replace(/\|/g, '\\|').trim();
  const docs = [path.join(REPO, 'bench', 'README.md'), path.join(REPO, 'docs', 'decisions', '0048-a-benchmark-harness-for-adam-coder-and-opencode.md')].map((f) => fs.readFileSync(f, 'utf8'));
  for (const doc of docs) {
    for (const a of applied.filter((x) => x.kind !== 'name')) {
      assert.ok(doc.includes(norm(a.original)), `${a.id}: adam's words are shown`);
      if (a.to !== '') assert.ok(doc.includes(norm(a.to)), `${a.id}: OpenCode's words are shown`);
    }
    for (const [from, to] of Object.entries(NAMES)) assert.ok(doc.includes(`| \`${from}\` | \`${to}\` |`) || doc.includes(`\`${from}\``), `${from} is shown`);
  }
});

test("the shared prompt builds from adam's instructions as they are in this checkout, so a change to them fails here before the pin moves", () => {
  const instructions = fs.readFileSync(path.join(REPO, INSTRUCTIONS_PATH), 'utf8');
  const shared = sharedPrompt(instructions);
  assert.ok(shared.text.length > 0);
});
