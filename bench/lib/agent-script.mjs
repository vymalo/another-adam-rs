// What the scripted model says to each agent, for the three self-test tasks (bench/tasks/self). A pure function of
// the request, stateless like the dev stack's scripts: the caller is told by the tools it is offered, the task by a
// `[mock:<name>]` marker in the statement, and the step by the tool-call ids already in the history.
//
//   adam      prepare_workspace, write_file, run_checks, commit_and_push, open_pull_request, then an answer
//   OpenCode  write, bash, then an answer
//
// The ids are `bench-call-<n>`: the next step is the highest n in the history, plus one.

export const SELF_TASKS = Object.freeze({
  hello: { file: 'hello.txt', content: 'hello\n' },
  wrong: { file: 'hello.txt', content: 'goodbye\n' },
  'fix-add': { file: 'add.mjs', content: 'export const add = (a, b) => a + b;\n' },
});

/** The calls each agent makes for one request of a task, by step. Calls per run: adam 6, OpenCode 3. */
export const CALLS_PER_RUN = Object.freeze({ adam: 6, opencode: 3 });

const names = (tools) => new Set((tools ?? []).map((t) => t?.function?.name).filter(Boolean));

/** 'adam' when it is offered `prepare_workspace`, 'opencode' when it is offered `bash` and `write`, else null. */
export function identifyAgent(tools) {
  const n = names(tools);
  if (n.has('prepare_workspace')) return 'adam';
  if (n.has('bash') && n.has('write')) return 'opencode';
  return null;
}

const textOf = (content) => (typeof content === 'string' ? content : Array.isArray(content) ? content.map((p) => p?.text ?? '').join('') : '');

/** The task named by `[mock:<name>]` in a user message, or null. */
export function taskMarker(messages) {
  for (const m of messages ?? []) {
    if (m?.role !== 'user') continue;
    const hit = /\[mock:([a-z-]+)\]/.exec(textOf(m.content));
    if (hit && Object.hasOwn(SELF_TASKS, hit[1])) return hit[1];
  }
  return null;
}

/** The step number of the highest scripted call in the history (0: nothing yet). */
export function stepsDone(messages) {
  let max = 0;
  for (const m of messages ?? []) {
    for (const c of m?.tool_calls ?? []) {
      const hit = /^bench-call-(\d+)$/.exec(c?.id ?? '');
      if (hit) max = Math.max(max, Number(hit[1]));
    }
  }
  return max;
}

/** The directory OpenCode says it works in, from its system prompt. */
export function workingDirectory(messages) {
  for (const m of messages ?? []) {
    const hit = /Working directory: (\S+)/.exec(textOf(m?.content));
    if (hit) return hit[1];
  }
  return null;
}

const repoUrl = (messages) => {
  for (const m of messages ?? []) {
    if (m?.role !== 'user') continue;
    const hit = /https?:\/\/[^\s)]+?\.git/.exec(textOf(m.content));
    if (hit) return hit[0];
  }
  return null;
};

/**
 * The next turn for a request: { tool: { id, name, args } } or { text }, or null when the caller is not one of
 * the two agents or the task is not a self-test (the scripted model then answers as it always did).
 */
export function nextTurn(body) {
  const agent = identifyAgent(body?.tools);
  const task = taskMarker(body?.messages);
  if (!agent || !task) return null;
  const spec = SELF_TASKS[task];
  const n = stepsDone(body.messages) + 1;
  const call = (name, args) => ({ tool: { id: `bench-call-${n}`, name, args } });
  if (agent === 'opencode') {
    const dir = workingDirectory(body.messages);
    if (n === 1) return call('write', { filePath: `${dir ?? '.'}/${spec.file}`, content: spec.content });
    if (n === 2) return call('bash', { command: 'true', description: 'a smoke command' });
    return { text: 'Done: the file is written.' };
  }
  if (n === 1) return call('prepare_workspace', { repo_url: repoUrl(body.messages), base_branch: 'main' });
  if (n === 2) return call('write_file', { path: spec.file, content: spec.content });
  if (n === 3) return call('run_checks', { command: 'true' });
  if (n === 4) return call('commit_and_push', { message: 'feat: bench self test' });
  if (n === 5) return call('open_pull_request', { title: 'feat: bench self test', body: 'Written by the scripted model of the benchmark harness.' });
  return { text: 'Opened the pull request: the file is written and the checks passed.' };
}
