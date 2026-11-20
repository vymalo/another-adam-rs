// The agents a run can be: two drivers (adam, OpenCode), each in two variants (bench/README.md, "Agent variants", decided by the
// owner 2026-11-20).
//
//   adam              adam-coder as it ships: its whole instructions.md and every tool
//   adam-lean         the same binary and model, with an agent folder that offers only the tools a coding task needs (`tools:` in the
//                     front matter, docs/reference/agent-files.md) and keeps only the sections of the prompt about doing that task
//   opencode          OpenCode with its built-in prompt plus the `owner` profile of adam's instructions (the four sections; the default)
//   opencode-prompt   OpenCode with its built-in prompt plus the `work` profile of adam's instructions (bench/lib/shared-prompt.mjs)
//   opencode-bare     OpenCode with its built-in prompt only
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { sectionsOf } from './shared-prompt.mjs';

export const VARIANTS = Object.freeze({
  adam: { driver: 'adam', folder: 'full' },
  'adam-lean': { driver: 'adam', folder: 'lean' },
  opencode: { driver: 'opencode', profile: 'owner' },
  'opencode-prompt': { driver: 'opencode', profile: 'work' },
  'opencode-bare': { driver: 'opencode', profile: null },
});
export const AGENT_NAMES = Object.freeze(Object.keys(VARIANTS));

/** The tools adam-lean is offered: reading and changing files, commands, the checks, and delivering the change. */
export const LEAN_TOOLS = Object.freeze([
  'prepare_workspace',
  'list_files',
  'find_files',
  'search_files',
  'read_file',
  'read_files',
  'write_file',
  'edit_file',
  'apply_patch',
  'run_command',
  'run_checks',
  'commit_and_push',
  'open_pull_request',
]);

/** The sections of instructions.md adam-lean keeps (the opening paragraphs are always kept). */
export const LEAN_SECTIONS = Object.freeze(['How to work', 'A missing toolchain', 'Ending your turn', 'Rules you must not break']);

/** instructions.md for adam-lean: the front matter with `tools:` set, the opening paragraphs and the kept sections, in order. Pure. */
export function leanInstructions(text) {
  const fm = /^---\n([\s\S]*?)\n---\n/.exec(text);
  if (!fm) throw new Error('the instructions have no front matter');
  if (/^tools:/m.test(fm[1])) throw new Error('the instructions already set tools:');
  const front = fm[1].replace(/^(name:.*)$/m, `$1\ntools: [${LEAN_TOOLS.join(', ')}]`);
  if (front === fm[1]) throw new Error('the front matter has no name:');
  const sections = sectionsOf(text);
  const missing = LEAN_SECTIONS.filter((h) => !sections.some((s) => s.heading === h));
  if (missing.length > 0) throw new Error(`the instructions no longer have ${missing.join(', ')}: update LEAN_SECTIONS`);
  const body = sections.filter((s, i) => i === 0 || LEAN_SECTIONS.includes(s.heading)).map((s) => s.raw).join('');
  // adam refuses a declared variable the prompt does not use (exit 78), so the ones only the dropped sections read go too.
  return `---\n${withoutUnusedVars(front, body)}\n---\n${body}`;
}

/** The front matter without the `vars:` entries `body` never reads as `{{name}}` (and without the block's comments). Pure. */
export function withoutUnusedVars(front, body) {
  const out = [];
  let inVars = false;
  let dropping = false;
  for (const line of front.split('\n')) {
    if (/^vars:\s*$/.test(line)) {
      inVars = true;
      out.push(line);
      continue;
    }
    if (inVars && /^\S/.test(line)) inVars = false;
    if (!inVars) {
      out.push(line);
      continue;
    }
    if (/^\s*#/.test(line)) continue;
    const entry = /^ {2}([a-z_]+):/.exec(line);
    if (entry) dropping = !body.includes(`{{${entry[1]}}}`);
    if (!dropping) out.push(line);
  }
  return out.join('\n');
}

/** Turns an unpacked agent folder into adam-lean's: instructions rewritten, subagents and skills removed (they need tools it lacks). */
export function makeLeanFolder(dir) {
  const file = path.join(dir, 'instructions.md');
  fs.writeFileSync(file, leanInstructions(fs.readFileSync(file, 'utf8')));
  for (const d of ['subagents', 'skills']) fs.rmSync(path.join(dir, d), { recursive: true, force: true });
}

/**
 * One driver per variant. `prompts` maps a prompt profile to its text (`readSharedPrompt` of the pinned revision); `make` holds the two
 * driver classes, passed in so this module needs neither (and tests can pass fakes).
 */
export function makeDrivers({ make, prompts, common, agents = AGENT_NAMES }) {
  const out = {};
  for (const name of agents) {
    const v = VARIANTS[name];
    if (!v) throw new Error(`no agent variant ${name}`);
    if (v.driver === 'adam') out[name] = new make.adam({ ...common, name, variant: v.folder });
    else {
      const sharedPrompt = v.profile === null ? null : prompts[v.profile];
      if (v.profile !== null && (typeof sharedPrompt !== 'string' || sharedPrompt === '')) throw new Error(`the ${v.profile} prompt is required for ${name}`);
      out[name] = new make.opencode({ ...common, name, sharedPrompt });
    }
  }
  return out;
}

/** What identifies how adam-lean's folder is made: a folder made by another recipe is made again (its marker differs). */
export const LEAN_RECIPE = crypto.createHash('sha256').update(JSON.stringify([LEAN_TOOLS, LEAN_SECTIONS, leanInstructions.toString(), withoutUnusedVars.toString(), makeLeanFolder.toString()])).digest('hex').slice(0, 12);
