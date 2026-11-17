import assert from 'node:assert/strict';
import { test } from 'node:test';
import { CALLS_PER_RUN, SELF_TASKS, identifyAgent, nextTurn, stepsDone, taskMarker } from '../lib/agent-script.mjs';
import { createScriptedModel } from '../lib/scripted-model.mjs';

const tool = (name) => ({ type: 'function', function: { name, parameters: {} } });
const ADAM = ['prepare_workspace', 'write_file', 'run_checks', 'commit_and_push', 'open_pull_request', 'read_file'].map(tool);
const OPENCODE = ['bash', 'edit', 'glob', 'grep', 'read', 'task', 'write'].map(tool);

/** Play a whole run: ask for each turn, answer each tool call, until the model answers in words. */
function play(agentTools, first, { dir = '/work/w/slot' } = {}) {
  const messages = [{ role: 'system', content: `Working directory: ${dir}\n` }, { role: 'user', content: first }];
  const calls = [];
  for (let i = 0; i < 12; i++) {
    const turn = nextTurn({ tools: agentTools, messages });
    assert.ok(turn, 'the script answers');
    if (turn.text) return { calls, text: turn.text };
    calls.push(turn.tool);
    messages.push({ role: 'assistant', content: null, tool_calls: [{ id: turn.tool.id, type: 'function', function: { name: turn.tool.name, arguments: JSON.stringify(turn.tool.args) } }] });
    messages.push({ role: 'tool', tool_call_id: turn.tool.id, content: 'ok' });
  }
  throw new Error('the script does not end');
}

test('the callers are told apart by the tools they are offered', () => {
  assert.equal(identifyAgent(ADAM), 'adam');
  assert.equal(identifyAgent(OPENCODE), 'opencode');
  assert.equal(identifyAgent([tool('bash')]), null);
  assert.equal(identifyAgent(undefined), null);
});

test('a task is named by its marker in the statement, and only the three self-tests are known', () => {
  assert.equal(taskMarker([{ role: 'user', content: 'Do it. [mock:fix-add]' }]), 'fix-add');
  assert.equal(taskMarker([{ role: 'user', content: [{ type: 'text', text: '[mock:wrong]' }] }]), 'wrong');
  assert.equal(taskMarker([{ role: 'user', content: '[mock:nothing]' }]), null);
  assert.equal(taskMarker([{ role: 'system', content: '[mock:hello]' }]), null, 'a system message names nothing');
  assert.deepEqual(Object.keys(SELF_TASKS).sort(), ['fix-add', 'hello', 'wrong']);
});

test('adam is driven through the workspace, the file, the checks, the commit and the pull request, in six calls', () => {
  const { calls, text } = play(ADAM, 'In http://git-server:8080/bench/hello-1a2b3c.git (base branch main), do this task:\n\nWrite hello. [mock:hello]');
  assert.deepEqual(calls.map((c) => c.name), ['prepare_workspace', 'write_file', 'run_checks', 'commit_and_push', 'open_pull_request']);
  assert.deepEqual(calls[0].args, { repo_url: 'http://git-server:8080/bench/hello-1a2b3c.git', base_branch: 'main' });
  assert.deepEqual(calls[1].args, { path: 'hello.txt', content: 'hello\n' });
  assert.deepEqual(calls.map((c) => c.id), ['bench-call-1', 'bench-call-2', 'bench-call-3', 'bench-call-4', 'bench-call-5']);
  assert.match(text, /pull request/);
  assert.equal(calls.length + 1, CALLS_PER_RUN.adam);
});

test('OpenCode is driven through one write in its working directory and one command, in three calls', () => {
  const { calls, text } = play(OPENCODE, 'Fix add. [mock:fix-add]', { dir: '/work/workspaces/oc-r1/slot' });
  assert.deepEqual(calls.map((c) => c.name), ['write', 'bash']);
  assert.deepEqual(calls[0].args, { filePath: '/work/workspaces/oc-r1/slot/add.mjs', content: 'export const add = (a, b) => a + b;\n' });
  assert.ok(text);
  assert.equal(calls.length + 1, CALLS_PER_RUN.opencode);
});

test('the wrong task writes the wrong answer, to the same file as hello', () => {
  const hello = play(OPENCODE, '[mock:hello]').calls[0].args;
  const wrong = play(OPENCODE, '[mock:wrong]').calls[0].args;
  assert.equal(hello.filePath, wrong.filePath);
  assert.equal(hello.content, 'hello\n');
  assert.equal(wrong.content, 'goodbye\n');
});

test('stepsDone reads the highest scripted id and ignores the ids of other callers', () => {
  assert.equal(stepsDone([{ tool_calls: [{ id: 'call_9' }] }]), 0);
  assert.equal(stepsDone([{ tool_calls: [{ id: 'bench-call-2' }] }, { tool_calls: [{ id: 'bench-call-1' }] }]), 2);
});

test('a request that is not a self-test is left to the scripted model\'s own answer', () => {
  assert.equal(nextTurn({ tools: ADAM, messages: [{ role: 'user', content: 'no marker' }] }), null);
  assert.equal(nextTurn({ tools: [], messages: [{ role: 'user', content: '[mock:hello]' }] }), null);
});

test('the scripted model serves the script, as a stream and not, with the fixed usage', async (t) => {
  const model = createScriptedModel({ script: 'agents' });
  const { url } = await model.listen();
  t.after(() => model.close());
  const ask = (stream) =>
    fetch(`${url}/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ model: 'm', stream, stream_options: { include_usage: true }, tools: OPENCODE, messages: [{ role: 'system', content: 'Working directory: /w\n' }, { role: 'user', content: '[mock:hello]' }] }),
    });
  const plain = await (await ask(false)).json();
  assert.equal(plain.choices[0].finish_reason, 'tool_calls');
  assert.equal(plain.choices[0].message.tool_calls[0].function.name, 'write');
  assert.equal(plain.usage.prompt_tokens, 1000);
  const text = await (await ask(true)).text();
  const frames = text.split('\n').filter((l) => l.startsWith('data: {')).map((l) => JSON.parse(l.slice(6)));
  assert.equal(frames[0].choices[0].delta.tool_calls[0].function.name, 'write');
  assert.equal(frames.at(-1).usage.completion_tokens, 200);
  assert.equal(frames.at(-2).choices[0].finish_reason, 'tool_calls');
});

test('without the script, the scripted model answers as it always did', async (t) => {
  const model = createScriptedModel();
  const { url } = await model.listen();
  t.after(() => model.close());
  const res = await fetch(`${url}/chat/completions`, { method: 'POST', body: JSON.stringify({ model: 'm', tools: OPENCODE, messages: [{ role: 'user', content: '[mock:hello]' }] }) });
  assert.equal((await res.json()).choices[0].message.content, 'done');
});
