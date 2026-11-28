# 0088. A tool result is bounded when it is made, and a command that succeeds can say only that

Status: **Accepted** (2026-11-27). The owner decided on 2026-11-27: *"Yes do the ADR on capping tool. One idea too. For
bash scripts, we hide the response by default if no error and add an option to view the responses even if successful."*
Decision 3's per-tool defaults (`run_command` keeps showing its output) and decision 2's bound for MCP results are this
ADR's choices for the owner to confirm. Builds on
[ADR 0044](0044-paged-tools-a-stable-prompt-prefix-a-plan-skills-commands-and-project-context.md) (paged tools, a stable
prompt prefix) and [ADR 0086](0086-the-context-window-follows-the-model-and-the-history-budget-is-200k.md) (the history
budget).

*Amended 2026-11-28:* slice T1 is built. `run_command`, `run` and `run_checks` take `output` (`adam_code_tools::OutputMode`),
with decision 3's defaults, each a constant beside its tool. A command that succeeds with `on_error` is shown as its `$`
line, its exit line as before (`exit code 0`, or `exit code 0: passed` for a check) and one line:
`[output: N bytes, not shown because the command succeeded; kept as o-…; read_output {"id":"o-…"} reads it]`. Two
deviations from the table: the exit code stays on its own line, and the size and id go on a line in brackets, so that a
shortened history recognises it as a kept-output pointer; and an output that printed nothing says `(no output)` and keeps
nothing. An output the keeper refused is shown as with `full`. The evidence:
[0088: the evidence and the slice proofs](../../.agents/docs/decisions/0088-a-tool-result-is-bounded-when-it-is-made-and-a-command-that-succeeds-says-so.md).

## Context

A provider caches a request's prefix: tools, then the system prompt, then the messages. A tool result appended at the end
is read from the cache on every later call, **as long as nothing before it changes**. Two things change the past: a
compaction (by design) and the shortening of old tool results. Every result also costs its tokens on every later call,
cached or not.

What adam does today (*verified 2026-11-27*):

* **Commands are bounded when they run.** `run_command`, `run` and `run_checks` show an output that fits
  `CHECK_OUTPUT_TAIL_BYTES` (16 KiB) whole. A longer one is shown as head and tail and kept whole under an output id that
  `read_output` pages (`crates/adam-code-tools/src/output.rs`, `crates/adam-llm-agent/src/kept.rs`).
* **An MCP result is bounded at 64 KiB**, four times the command bound, then kept and paged (`MAX_RESULT_BYTES`,
  `crates/adam-mcp/src/tool.rs`).
* **Old results are shortened only past the history budget**, in place, oldest first, down to three quarters of the
  budget, so it happens rarely and the cached prefix moves once per shortening, not per call
  (`record_shortenings_from`, `crates/adam-llm-agent/src/history.rs`).
* **A command that succeeds shows its output like one that fails.** A clean `cargo build` or `npm install` puts up to
  16 KiB of progress lines in the history, where they cost tokens on every later call and push the run toward a
  shortening, for nothing the model needs.

## Decision

1. **Every tool result is bounded when it is made, never later by design.** A result the model reads in full is at most
   the deployment's tool-result bound. The bound is today's `CHECK_OUTPUT_TAIL_BYTES` for commands. Anything longer is
   kept and paged with `read_output`, as commands do now. Shortening of old results stays, but only as the last resort past
   the history budget. Each shortening is counted in the run's usage, so a deployment sees it.
2. **An MCP result takes the same bound as a command** (16 KiB by default instead of 64 KiB): one rule for every tool, so
   the cost of a call does not depend on which kind of tool made it.
3. **A command takes `output`, a closed enum (Rule 3), chosen per call:**

   | Value | On success (exit 0) | On failure |
   |---|---|---|
   | `on_error` | one line: `exit code 0`, the size, and the output id; nothing else | the output as today (head and tail, kept) |
   | `full` | the output as today | the output as today |

   * The output is **always kept**, whatever `output` says, so `read_output` can show a successful command's output
     later. Hiding is about what enters the history, never about losing data.
   * Defaults per tool, because the tools have different jobs:

     | Tool | Default | Why |
     |---|---|---|
     | `run_checks` | `on_error` | a check that passes needs no output |
     | `run` (builds, installs, generators) | `on_error` | its result is the files it made |
     | `run_command` | `full` | it exists to look around (`ls`, `cat`, `git log`): its output is the answer |

   * A folder's `limits` may set each tool's default; a call's `output` wins.
4. **The tools' descriptions say it**, in one sentence each: what `on_error` shows, and that `read_output` reads a hidden
   output. The prompt prefix changes once, when this ships.
5. **It is measured before it is kept.** The bench (ADR 0048) compares, on the same tasks, calls per task, `read_output`
   calls, input tokens per task and the cache share of the prompt, before and after. If hidden outputs make the model call
   `read_output` more often than they save, the default goes back to `full` for that tool.

## Slices

| Slice | What |
|---|---|
| T1 | `output` on `run_checks`, `run` and `run_command`, with the defaults above; the success line; the output always kept; tests for each value and tool. |
| T2 | The MCP bound follows the tool-result bound; shortenings counted in the run's usage. |
| T3 | A folder's `limits` sets the defaults; `docs/reference/agent-files.md`, `docs/reference/coder-agent.md`, `docs/reference/environment.md`. |
| T4 | The bench comparison of decision 5, and the result written in `bench/NUMBERS.md`. |

## Consequences

* A typical run (several builds and test passes that succeed) keeps far fewer tokens in its history, so each later call is
  cheaper and a shortening, which moves the cached prefix, is rarer.
* The model loses nothing: every output is kept and one `read_output` call away.
* The exporter's samples (ADR 0085) show the success line, which is what the model saw.
* No `Store`, `Notifier` or `ModelClient` method changes.

*Amended 2026-11-28:* slice T2 is built. The MCP bound is the deployment's, `McpPolicy::result_bytes`: 16 KiB by default
(`DEFAULT_RESULT_BYTES`, so a folder agent served by `adam-agent` has it too), and the coder sets it to its
`CHECK_OUTPUT_TAIL_BYTES`. A successful text over it is kept, and what the model is shown (the line that names it, a first
page and the page's last line) fits the bound: the first page holds as many lines as fit, and always one. An error result
is cut at the bound. `MAX_RESULT_BYTES` stays as a deprecated name of the default. Each shortening is counted in the run's
state (`shortening_usage`: the count and the characters it took out) and written on the task as a `shortenings` member of
the `usage/v1` object, `{"count": 2, "charsRemoved": 412000}`, with an `info` log line. Two choices the decision left open:
the count is the run's own history, not its children's, and a task whose history was never shortened carries the object it
always did. The evidence:
[0088: the evidence and the slice proofs](../../.agents/docs/decisions/0088-a-tool-result-is-bounded-when-it-is-made-and-a-command-that-succeeds-says-so.md#t2-built-2026-11-28).
