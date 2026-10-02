# 0014. A `turn_output` answer is the run's answer

Status: **Accepted** (2026-10-02), with plan 10 of 2026-10-02 (A3 of its second wave). The other side of the contract is the
orchestration layer's: the `turn_output` section of `docs/api/thread-tools-v1.md`, the amendment of 2026-10-02 to its
ADR 0031 ("working text and the turn's answer") and "The agent's words" of `docs/api/agui.md`, all in
`vymalo/another-agentic-system`. **Extends [ADR 0006](0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md)** (a tool can come from
the thread-tools endpoint, read at every model turn) **and [ADR 0011](0011-a-tool-calls-step-carries-its-input-and-output.md)**
(the call's step carries its input and output, as for every tool). **Built:** `ToolOutput::announcing`,
`Conversation::announced`, the run's output, `adam-ui`'s `turn_output`, the instructions and the tests.

## Context

The owner asked the chat to show one answer per turn and everything else the agent says as working text. The orchestration
layer does it by structure first (the words that end the turn are the answer) and, for the cases the structure misses, with a
thread tool: `turn_output { text }`, built into its thread-tools endpoint. An agent that has shown its answer and **keeps
working** (it commits, it cleans up), and one whose last words are not its answer (the answer, a surface drawn, then "there it
is"), call it, and the orchestrator records `text` as the turn's answer and files everything else of the turn as working text.

That serves the orchestrator's reader. A client that reads **only A2A** reads the task: its `completed` status message is the
run's output `text`, which is what the model said last. For an agent that announced its answer and then closed with "Done.",
that is "Done.", while the chat shows the announced Markdown: two readers of one task read two answers.

*Verified 2026-10-02 by reading the code at commit `5e38a0a`:* a model turn with no tool call ends the run with
`{"text": <that turn's text>, "artifacts": [..], "stream": <its stream>}` (`LlmAgent::model_turn`), and `adam-a2a-runtime` builds
the `completed` status message from `output.text` and marks it with `output.stream` (`convert.rs`, `text_stream.rs`); a
thread tool is called through `ThreadTools::call`, which returned the endpoint's text (`{"delivered": true}`) as the result;
nothing in the loop let a tool set the run's output.

## Decision

1. **A tool can announce the run's answer: a generic seam in the agent loop.** `ToolOutput` has a member
   `answer: Option<String>` (set with `ToolOutput::announcing(text)`; serde default, not written while `None`). The loop keeps
   the last announcement of a successful result in `Conversation::announced` (also a serde default, not written while `None`),
   and **when the run ends, the output's `text` is the announcement**, not the model's closing words. A result that is an
   error announces nothing, whatever its `answer` says. The state is derived from the journaled results of the tool steps, so
   a replay announces the same words and does not call the tool again; the output is derived from the state, so a worker that
   takes the run over finishes with the same answer.
2. **`adam-ui` uses it for `turn_output`.** `ThreadTools::call` recognises the name. When the endpoint accepts the call (not an
   error result) and the `text` argument is not blank, the call's result for the model is
   *Delivered to the person as your answer. Finish now with one short line, and do not repeat the answer.*
   (`TURN_OUTPUT_DELIVERED`, in place of the endpoint's `{"delivered": true}`, which tells a model nothing about what to do
   next) and the text is announced. A refusal (the turn is over, blank or oversize text, an endpoint that is down or no longer
   accepts the grant) is what it was: an error result the model reads, and nothing is announced. Nothing else of the tool is
   special: it is listed under its name at every model turn like every tool of the endpoint, and an endpoint that does not
   list it (an orchestrator without it) leaves everything as it was.
3. **The last announcement wins**, as in the orchestrator's log. The endpoint accepts every call while the turn is open; the
   run's answer is the text of the last call that succeeded.
4. **The closing words are still said.** When a turn that closes the run follows an announcement, its text is sent as
   `agent_text` with the `stream` it was sent as, as the words before a tool call are, and the output names **no stream** (the
   output's text is not the streamed words, so the `completed` status carries no marker). The orchestrator files the closing
   words as working text; the `completed` status words repeat the announcement, and the orchestrator drops words that repeat
   the last announced words.
5. **A message that reaches the run ends the turn, and clears the announcement.** The person's answer to a question the run was
   parked on, or any later message, starts a new turn in the orchestrator ("a new message, a card's action or a rework starts a
   new turn"). What was announced for the turn before is not the answer of this one. A run that continues another starts with
   none.
6. **The instructions say it in a sentence that holds without the tool.** The prompt system has no conditional on the tools the
   model is offered (the tools of a thread-tools endpoint are read at each model turn, after the prompt is assembled), so the
   coder's "What the person sees" says "**If you have a `turn_output` tool**, call it with your complete answer once it is
   ready, then end your turn with one short line, and do not repeat the answer; if it fails, or you have no such tool, your
   last words are your answer". The two example agents under `dev/agents/` carry a shorter version of it.

## Consequences

* **A plain A2A client reads the same answer the orchestrator shows**: the `completed` status message is the announced Markdown.
  The closing line is in the log of events and in the history, not in the output.
* **The coder's stop with no pull request.** `CoderAgent` turns a stop with no pull request (and no shared scratch result) into
  a question to the person, with the output's text as the question. After an announcement that text is the announced answer,
  so the `input-required` status repeats the announced words (the orchestrator says them once) and not "Done.". The model's
  history keeps its own closing line.
* **The announced text passes through the coder's redactor** with the rest of the output; the arguments of the call are in the
  step's `input` and the result text in its `output`, redacted and cut as for every tool (ADR 0011). The announced text is
  stored in the run's state and journal, as the model's words are.
* **`ToolOutput` gains a public field**, so a struct literal of it (nothing in this repository builds one) needs `..Default::default()`;
  the constructors, `Default` and serde are unchanged for older journals.
* **A tool other than `turn_output` can announce.** Nothing is special-cased in the loop; a deployment's own tool that hands
  over the answer by another route can use it.
* **The run's `truncated` flag is not set** for an announced answer: it describes the closing words, which are not the output.

## Alternatives considered

* **Announce through the run's output in the tool's own state** (a side channel in `ToolCtx` that the loop reads at the end).
  Rejected: the announcement would not be in the journaled result, so a worker that replays a recorded step would not know it.
  A member of the result is journaled for free.
* **Set the output from `adam-ui` after the run** (a decorator of the agent). Rejected: `adam-ui` is a source of tools, not an
  agent; the loop is where the final output is made, and any other tool source gets the same seam.
* **Keep the closing line as the output and send the announcement beside it** (an artifact or metadata). Rejected: a plain A2A
  reader reads the status message, and the point is that it reads the answer.
* **Tell the model the endpoint's `{"delivered": true}`.** Rejected: the owner's chats showed a model that repeated its answer
  after a tool said nothing about what to do next; the result says to finish with one line.
* **A conditional in the prompt on the offered tools.** Not built: the prompt is assembled once and the tools are read at every
  model turn; the sentence that reads the same either way costs nothing.

## Verified and unverified

* *Verified 2026-10-02*, by tests in this repository: an announced answer is the run's output and the closing line is not, the
  last of several wins, in one message and across turns, a refused call changes nothing and one after an announcement keeps
  it, with no announcement the closing words are the answer with their stream as before, a replay after a crash inside the
  next tool keeps the announced answer without calling the tool again, a message that reaches the run clears it, and the
  serde shapes of older journals (`crates/adam-llm-agent/tests/announced_answer.rs`); a whole agent behind A2A against the
  fake endpoint: the announced text is the `completed` status message and the output, the model is told the result above,
  the last of two wins, the endpoint's refusals (the turn is over, oversize) leave the closing words as the answer and the
  model reads the error, and an endpoint without the tool changes nothing (`crates/adam-ui/tests/turn_output.rs`, and the unit
  tests of `crates/adam-ui/src/thread_tools.rs`); the coder's prompt tells it so and still reads without the tool
  (`bin/adam-coder/tests/agent_files.rs`).
* *Unverified:* that the orchestration layer drops the `completed` words that repeat an announcement as its S7 describes
  (read from its documents in a local checkout, not run against this agent), and what a real model does with the result text
  (the coder's end-to-end scripts use a scripted model).
