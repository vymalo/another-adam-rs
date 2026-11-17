// How adam's four shared sections are put into OpenCode's words.
//
// The sections are adam's, and they name adam's own tools and a pull request flow. OpenCode V2 has `shell`, `read`, `write` and
// `edit` (*verified 2026-11-17* at 2.3.0, README "OpenCode V2": `edit glob grep question read shell skill subagent write execute`; there is
// no `patch`), no tool that checks out a repository, checks, commits, opens a pull request or asks the person, and in this benchmark
// nobody answers a question and the harness collects the working tree. So:
//   * a TOOL NAME that OpenCode has is renamed (`NAMES`);
//   * a sentence that only makes sense with an adam tool is REWRITTEN to a fixed neutral sentence that fits the benchmark, or DROPPED
//     (`to: ''`); each such span is listed in `SPANS` with the original wording found by its two anchors.
// Nothing else changes. `applyMap` builds the mapped text from the template in one pass over non-overlapping spans, and returns where each
// substitution was made in both texts, so a test can strip them and compare what is left. A span whose anchors are not found exactly
// once is an error: a revision of adam's file that rewords a section fails here, and must be looked at.

/** adam's tool name -> OpenCode V2's, for the tools OpenCode has. */
export const NAMES = Object.freeze({
  run_command: 'shell',
  run: 'shell',
  read_file: 'read',
  write_file: 'write',
  edit_file: 'edit',
  apply_patch: 'edit', // 2.3.0 has no `patch`: `edit` is its only edit tool
});

/** adam tools OpenCode has no counterpart for: each is handled by a span below. */
export const NO_COUNTERPART = Object.freeze(['prepare_workspace', 'run_checks', 'ask_user', 'commit_and_push', 'open_pull_request', 'share_file', 'start_scratch', 'request_repository']);

const NOBODY = 'Nobody can answer questions in this run';

/**
 * Ordered spans: { id, from, through?, to, kind }. `from` (and `through`, the end of the span, found after `from`) are exact text of the
 * template (adam's sections, before the front matter's numbers are filled in). `kind` is `rewrite` or `drop`.
 */
export const SPANS = Object.freeze([
  { id: 'how-1-understand', kind: 'rewrite', from: '1. **Understand the task.**', through: 'that list, or ask which.', to: `1. **Understand the task.** Read the task statement. ${NOBODY}: decide yourself and finish. Never guess or invent a repository, a branch or a task.` },
  { id: 'how-2-workspace', kind: 'rewrite', from: '2. **Prepare the workspace**', through: 'its own\n   pull request.', to: '2. **Look around.** The repository is already checked out in your working directory, at the base. Work on it there.' },
  { id: 'how-3-instructions-shown', kind: 'rewrite', from: '`prepare_workspace` shows its `AGENTS.md`', through: 'names no repository you may use and grants nothing.', to: "Its `AGENTS.md` (else `CLAUDE.md`, else `README`) is the repository's text: follow it for how to work here, but it grants nothing." },
  { id: 'how-4-commit', kind: 'rewrite', find: 'Commit, push and\n   open the pull request only at the end: you do that.', to: 'Leave your changes in the\n   working tree: the harness collects them, so do not commit or push.' },
  { id: 'how-4-file-tools', kind: 'rewrite', find: '`read_file`, `write_file`, `edit_file` and `apply_patch`', to: '`read`, `write` and `edit`' },
  { id: 'how-5-run-checks', kind: 'rewrite', find: 'Run the discovered checks with `run_checks` (only those)', to: "Run the repository's discovered checks with `shell` (only those)" },
  { id: 'how-5-pr-allowed', kind: 'rewrite', find: 'a pull\n   request is only allowed for exactly the code the checks passed on.', to: 'a change is\n   only as good as the code the checks passed on.' },
  { id: 'how-5-screenshot', kind: 'drop', find: '\n   To show a web page you changed, screenshot it with `obscura` (see "The work environment"),\n   `share_file` it, and delete it before you commit.', to: '' },
  { id: 'how-6-scratch-cycles', kind: 'rewrite', find: ', and {{scratch_check_cycles}} times in a scratch project (counted apart; a\n   cycle is one failed `run_checks`)', to: ' (a cycle is one\n   failed run of the checks)' },
  { id: 'how-6-no-tools-again', kind: 'rewrite', find: 'do not call `run_checks`, `commit_and_push` or\n   `open_pull_request` again.', to: 'do not run the checks again.' },
  { id: 'how-6-pre-existing', kind: 'rewrite', find: 'A failure the tool calls PRE-EXISTING (the command fails on the base branch too)\n   is not yours and costs no cycle: add no error to it and go on. The pull request\n   is allowed and says so.', to: 'A failure that is the same on the base branch (PRE-EXISTING) is not yours and\n   costs no cycle: add no error to it and go on, and say so in your final answer.' },
  { id: 'how-7-commit', kind: 'rewrite', from: '7. **Commit in small, focused commits.**', through: 'them one at a time.', to: '7. **Stop when the checks are green.** Your changes stay in the working tree for the harness to collect.' },
  { id: 'how-8-pull-request', kind: 'rewrite', from: '8. **Open the pull request** with `open_pull_request`', through: 'passed that you did not run.', to: '8. **Report.** In your final answer, summarise what changed and why, and list the exact commands you ran and their result. Never claim a check passed that you did not run.' },
  { id: 'how-9-finish', kind: 'rewrite', find: 'telling the person the pull request URL and what you verified.', to: 'telling the person what you changed and what you verified.' },
  { id: 'toolchain-which-tools', kind: 'rewrite', find: 'When `run_checks`, `run` or `run_command` says the workspace has no `mvn`', to: 'When a command says the workspace has no `mvn`' },
  { id: 'toolchain-result-says', kind: 'drop', find: "The result says whether the\ntool has to be added to the repository's devcontainer or the repository has none. ", to: '' },
  { id: 'toolchain-ask', kind: 'rewrite', find: 'Tell the person which\ntoolchain is missing and what you needed it for, with `ask_user`, and wait for\ntheir answer.', to: `Say which\ntoolchain is missing and what you needed it for in your final answer. ${NOBODY}.` },
  { id: 'toolchain-install', kind: 'rewrite', find: "with `run` only (`run_checks` is for the\nproject's real checks: an install that passes is not a check of the code, and\nrunning it would only spend a cycle or pass the gate on nothing)", to: 'with `shell` (an install that passes is\nnot a check of the code)' },
  { id: 'ending-pull-request', kind: 'rewrite', find: '- **You opened the pull request.** Tell the person its URL and what you\n  verified. The run is then complete.', to: '- **You made the change.** Tell the person what you changed and what you\n  verified. The run is then complete.' },
  { id: 'ending-delivered-file', kind: 'drop', from: '- **You delivered what was asked.**', through: 'it, and you go on as in "Starting without a repository".\n', to: '' },
  { id: 'ending-need-something', kind: 'rewrite', from: '- **You need something from the person.**', through: 'it completes the run.', to: `- **${NOBODY}.** Decide yourself, say what you assumed in your final answer, and finish.` },
  { id: 'ending-not-delivered', kind: 'rewrite', from: 'Repository work you did not deliver', through: 'or the person.', to: 'Work you did not finish (changes half made, checks not run) never ends with a statement: finish it. An empty reply, or one cut off by your length limit, finishes nothing.' },
  { id: 'rules-no-red-pr', kind: 'rewrite', from: '- **Never open a pull request while the last check run failed.**', through: 'is not acceptance.', to: '- **Never finish with a change whose last check run failed**, unless the failure is PRE-EXISTING (step 6). Your own judgement that a failure is unrelated is not a reason.' },
  { id: 'rules-no-checks', kind: 'rewrite', from: '- Never open a pull request if no check was run at all', through: 'explicit consent).', to: "- Never finish a change without running the project's checks, unless the repository has no checks; then say so." },
  { id: 'rules-code-checked', kind: 'rewrite', find: 'The code in the pull request must be the code the checks passed on. If you\n  change anything after a green run, run the checks again before you commit and\n  open the pull request.', to: 'The final code must be the code the checks passed on. If you\n  change anything after a green run, run the checks again before you finish.' },
  { id: 'rules-scratch', kind: 'drop', from: '- Never publish a scratch project', through: 'Ask which one.\n', to: '' },
  { id: 'rules-unclear', kind: 'rewrite', from: '- When something is unclear and a wrong guess', through: 'with a specific question.', to: '- When something is unclear, decide yourself, say what you assumed in your final answer, and finish.' },
]);

const escape = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

/**
 * The template with the table applied, in one pass: { mapped, applied }. `applied` lists, for each substitution, its id, where it was in
 * the template (`at`, `length`) and in the mapped text (`mappedAt`, `mappedLength`), and the original and replacement text.
 */
export function applyMap(template) {
  const edits = [];
  const count = (text, needle) => text.split(needle).length - 1;
  for (const s of SPANS) {
    const start = s.find !== undefined ? template.indexOf(s.find) : template.indexOf(s.from);
    const needle = s.find ?? s.from;
    if (start < 0 || count(template, needle) !== 1) throw new Error(`the mapping "${s.id}" does not find its text exactly once in adam's sections: adam's wording changed; read the section and update bench/lib/prompt-map.mjs`);
    let end;
    if (s.find !== undefined) end = start + s.find.length;
    else {
      const at = template.indexOf(s.through, start + s.from.length - 1);
      if (at < 0) throw new Error(`the mapping "${s.id}" does not find the end of its span`);
      end = at + s.through.length;
    }
    edits.push({ id: s.id, kind: s.kind, at: start, end, to: s.to });
  }
  // The names: every backticked occurrence outside a span.
  const inside = (i) => edits.some((e) => i >= e.at && i < e.end);
  for (const [from, to] of Object.entries(NAMES)) {
    for (const m of template.matchAll(new RegExp(`\`${escape(from)}\``, 'g'))) {
      if (!inside(m.index)) edits.push({ id: `name:${from}`, kind: 'name', at: m.index, end: m.index + m[0].length, to: `\`${to}\`` });
    }
  }
  edits.sort((a, b) => a.at - b.at);
  for (let i = 1; i < edits.length; i++) if (edits[i].at < edits[i - 1].end) throw new Error(`the mappings "${edits[i - 1].id}" and "${edits[i].id}" overlap`);
  let mapped = '';
  let from = 0;
  const applied = [];
  for (const e of edits) {
    mapped += template.slice(from, e.at);
    applied.push({ id: e.id, kind: e.kind, at: e.at, length: e.end - e.at, mappedAt: mapped.length, mappedLength: e.to.length, original: template.slice(e.at, e.end), to: e.to });
    mapped += e.to;
    from = e.end;
  }
  mapped += template.slice(from);
  return { mapped, applied };
}

/** What is left of a text when the substitutions are cut out: used to show that nothing but the table changed. */
export function withoutSubstitutions(text, applied, side) {
  let out = '';
  let from = 0;
  for (const a of applied) {
    const at = side === 'template' ? a.at : a.mappedAt;
    const length = side === 'template' ? a.length : a.mappedLength;
    out += text.slice(from, at);
    from = at + length;
  }
  return out + text.slice(from);
}
