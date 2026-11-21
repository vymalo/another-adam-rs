// The system prompt both agents share.
//
// adam's `bin/adam-coder/agent/instructions.md` at the pinned coder revision is its whole system prompt. OpenCode has its own
// built-in prompt and tools, so it is given the part of adam's that is about doing the task well, and not the part that is about
// adam's own tools and features. The shared text is taken word for word from the file: whole sections, never edited, joined in
// their order. Only the two numbers a section reads from the file's own front matter (`{{max_check_cycles}}` and
// `{{scratch_check_cycles}}`) are filled in, with the defaults the file itself gives, as adam does.
//
// Which sections are shared is a table (`SECTIONS`), decided by a person and recorded in bench/README.md and ADR 0048. A section the
// table does not know is an error: a new revision of the pin that adds or renames a section must be decided before it is run.
import { localGit } from './grade.mjs';
import { applyMap } from './prompt-map.mjs';

export const INSTRUCTIONS_PATH = 'bin/adam-coder/agent/instructions.md';
export const PREAMBLE = '(the opening paragraphs, before the first heading)';

/**
 * Every section of the file at the pinned revision: shared or left out, and why. `owner` marks the four the owner named; they are
 * shared although they name adam's own tools (`prepare_workspace`, `run_checks`, `ask_user`, `commit_and_push`, `open_pull_request`,
 * `share_file`, `start_scratch`, `edit_file`, `request_repository`), which OpenCode does not have: kept as decided, word for word.
 */
export const SECTIONS = Object.freeze({
  [PREAMBLE]: { shared: false, why: "adam's persona (its name and role) and its file tools by name" },
  'Who you are and how you talk': { shared: false, why: "adam's name and persona; says what adam can do and how its scratch projects and repository creation work" },
  'Requests that are not a code change': { shared: false, why: 'names prepare_workspace, start_scratch, share_file and the scratch project: adam-only tools and features' },
  'What the person sees': { shared: false, why: "adam's working notes, turn_output, share_file and the steps panel: a product feature OpenCode does not have" },
  Tools: { shared: false, why: "the guide to adam's own tools, by name" },
  Helpers: { shared: false, why: "adam's explorer and reviewer subagents" },
  'Access tiers': { shared: false, why: "adam's access tiers (plan, edits, auto, bypass), which the benchmark does not set" },
  'How to work': { shared: true, owner: true, why: 'how to do the task: discover the checks, small steps, verify, fix, commit and open the pull request in one call' },
  'Starting without a repository': { shared: false, why: "adam's scratch projects, publish_scratch and repository creation: the benchmark always names a repository" },
  'The work environment': { shared: false, why: "adam's run pod or its own container, use_repository_environment and the obscura browser; OpenCode runs in the task's own container" },
  'A missing toolchain': { shared: true, owner: true, why: 'what to do when the environment lacks a toolchain, and when a project tool only needs its dependencies installed' },
  'Asking the person': { shared: false, why: 'ask_user and its forms; OpenCode has a different question tool, which the benchmark denies' },
  'Questions and conversation': { shared: false, why: 'names run_command and refers to the "Who you are" section, which is left out' },
  'Ending your turn': { shared: true, owner: true, why: 'how a turn ends: the pull request, an answer, a delivered file, a question' },
  'Rules you must not break': { shared: true, owner: true, why: 'never open a pull request on red checks, run the checks again after a change, stay within the task, no secrets' },
});

/** The body of the file (after the front matter) cut into its sections: [{ heading, raw }]. `raw` starts at the heading line. */
export function sectionsOf(text) {
  const body = text.replace(/^---\n[\s\S]*?\n---\n/, '');
  const starts = [...body.matchAll(/^# (.+)$/gm)].map((m) => ({ heading: m[1].trim(), at: m.index }));
  const out = [{ heading: PREAMBLE, raw: body.slice(0, starts[0]?.at ?? body.length) }];
  starts.forEach((s, i) => out.push({ heading: s.heading, raw: body.slice(s.at, starts[i + 1]?.at ?? body.length) }));
  return out;
}

/** The numeric variables of the front matter's `vars:` block, as adam reads them. */
export function frontMatterVars(text) {
  const block = /^---\n([\s\S]*?)\n---\n/.exec(text)?.[1] ?? '';
  const vars = {};
  let inVars = false;
  for (const line of block.split('\n')) {
    if (/^vars:\s*$/.test(line)) inVars = true;
    else if (/^\S/.test(line)) inVars = false;
    else if (inVars) {
      const m = /^ {2}([a-z_]+):\s*(\d+)\s*$/.exec(line);
      if (m) vars[m[1]] = m[2];
    }
  }
  return vars;
}

/** Names of adam's own tools: the bullets of its "Tools" section (`- \`name ...\``). Argument names are not tools. */
export function adamToolNames(text) {
  const tools = sectionsOf(text).find((s) => s.heading === 'Tools');
  const names = new Set([...(tools?.raw.matchAll(/^- `([a-z_]+)/gm) ?? [])].map((m) => m[1]));
  return names;
}

/** The adam-only tool names a piece of text mentions in backticks. */
export function namedTools(raw, tools) {
  return [...new Set([...raw.matchAll(/`([a-z_]+)[` ]/g)].map((m) => m[1]).filter((n) => tools.has(n)))].sort();
}

/**
 * The shared prompt of an `instructions.md`: { template, mapped, applied, text, sections }. `template` is the kept sections as they are in the
 * file (adam's words); `mapped` is the template in OpenCode's words (bench/lib/prompt-map.mjs); `text` is `mapped` with the numbers the file's
 * front matter gives filled in, and is what OpenCode receives. Throws when a section is not in the table, or a kept section reads a variable the
 * file does not give.
 */
export function sharedPrompt(instructions) {
  const sections = sectionsOf(instructions);
  for (const s of sections) if (!Object.hasOwn(SECTIONS, s.heading)) throw new Error(`the section "${s.heading}" of adam's instructions has no decision: add it to SECTIONS in bench/lib/shared-prompt.mjs (shared or left out, and why)`);
  const kept = sections.filter((s) => SECTIONS[s.heading].shared);
  const missing = Object.keys(SECTIONS).filter((h) => !sections.some((s) => s.heading === h));
  if (missing.length > 0) throw new Error(`adam's instructions no longer have the section(s) ${missing.join(', ')}: update SECTIONS`);
  const template = kept.map((s) => s.raw).join('');
  const { mapped, applied } = applyMap(template);
  const left = namedTools(mapped, adamToolNames(instructions));
  if (left.length > 0) throw new Error(`the mapped text still names adam's tool(s) ${left.join(', ')}: add them to bench/lib/prompt-map.mjs (a name OpenCode has, or a span that rewrites or drops the sentence)`);
  const vars = frontMatterVars(instructions);
  const text = mapped.replace(/\{\{([a-z_]+)\}\}/g, (all, name) => {
    if (!Object.hasOwn(vars, name)) throw new Error(`a shared section reads {{${name}}}, which the file's front matter does not give`);
    return vars[name];
  });
  return { template, mapped, applied, text, sections };
}

/** adam's instructions at a revision of this repository (the harness's own checkout, as `ensureAgentFolder` reads it). */
export async function readInstructionsAt({ repoDir, revision }) {
  if (!/^[0-9a-f]{40}$/.test(revision)) throw new Error('the revision must be a full commit id');
  const git = localGit(repoDir);
  try {
    return (await git(['show', `${revision}:${INSTRUCTIONS_PATH}`])).toString('utf8');
  } catch {
    throw new Error(`adam's instructions at ${revision.slice(0, 7)} are not in this checkout; fetch it (git fetch origin ${revision})`);
  }
}

/** The shared prompt text of the pinned revision. */
export async function readSharedPrompt({ repoDir, revision }) {
  return sharedPrompt(await readInstructionsAt({ repoDir, revision })).text;
}
