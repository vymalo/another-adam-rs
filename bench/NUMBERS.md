# The three numbers the faster-coder epic rests on

The epic (sys#345, slice 1) argues that the coder is slower than OpenCode on the same tasks, and
rests on three figures. This page says what each one is, how to reproduce it from a ledger, and
which one the ledger cannot answer at all — so a reader can check the claim instead of trusting a
sentence in an epic.

```sh
node bench/bin/bench.mjs numbers --ledger /path/to/ledger.jsonl
```

The command is read-only: no configuration, no key, no network. It folds the `call` and `state`
records of the ledger (`bench/lib/meter.mjs` writes them) and prints one row per agent, then the
basis of each figure. `bench/lib/numbers.mjs` is the whole of it, and
`bench/test/numbers.test.mjs` holds the cases, including one that drops a row and watches the
figure move.

## What each figure is, and what it is computed from

| Figure | What it means | Computed from | State |
|---|---|---|---|
| **Calls per task** | how many model calls an agent spends on a task it finished, and the ratio between two agents on the same tasks | the `call` records of phase `settled` of the runs that reached `Done` (every attempt of a resumed run), over those runs, per agent | reproducible from any ledger; a ratio needs a ledger holding both agents |
| **Cache share of the fixed prompt** | how much of the fixed (first) prompt a run re-reads from the cache instead of paying for again | `usage.cached / usage.input` over the settled calls that carried the run's **first** prompt fingerprint; a child session's prompt is another fingerprint and is left out | reproducible from any ledger that records `usage` |
| **Checks step's share** | the share of the coder's work that the check tool takes | **not derivable**: the ledger records which tools an answer asked for (`toolNames`) and when a model call started and ended, never how long a tool ran | see below |

## The figure the ledger cannot answer

The epic states the checks step as **61 % of the coder's tool time**. The ledger cannot support that
figure, and not because of how this repository records it: nothing in it measures a tool's duration.
A `call` record carries `toolCalls` (how many the answer asked for), `toolNames` (which), the token
usage and the model call's `startedAt`/`endedAt` — a model call's span, not a tool's. So "share of
tool time" has no denominator in a ledger, and `bench numbers` reports what the ledger does have
instead: **the `run_checks` share of the named tool calls**, beside the basis that says so.

**The epic's 61 % is therefore published as *not measurable yet*** (the wording the coordinator
chose on ANR-Q67): not contradicted — it may come from a measurement nobody wrote down — but a
ledger cannot reproduce it, and the end-of-month rerun (ANR-424) will only change that if it records
tool durations. Two ways to make it a number, both cheap:

- **Count calls, not time**: state the `run_checks` share of named tool calls, which `bench numbers`
  prints, and drop the time claim.
- **Record tool time**: have the coder's tools write their duration into the ledger (a new field of
  a tool's record), which is what the rerun has been asked to do; then the share is computable. That
  is a change to the harness, not to this command.

## The other two

Both are computable today and neither is published here, because the numbers must come from a run,
and the **owner's answer to ANR-Q67 (option b) is that they come from the end-of-month bench rerun
(ANR-424)**, whose ledger lives outside the repository (`$BENCH_HOME/ledger.jsonl`, refused inside a
work tree). A figure is published by running the command above on that ledger and writing down, in
this table, the value with the ledger's runs and the date:

| Figure | Value | Run(s) | Date |
|---|---|---|---|
| Calls per task, and the ratio | _to be filled from the rerun's ledger (ANR-424)_ | | |
| Cache share of the fixed prompt | _to be filled from the rerun's ledger (ANR-424)_ | | |

A value written here must name the runs it came from: the command's table is per agent over every
finished run in the ledger it was given, so the same command on another ledger gives another number,
and neither is wrong.

### Where those files are

The ledgers are **not in this repository** and never will be (they hold a run's costs and its
prompts' fingerprints): they live in **`$BENCH_HOME` on the machine that ran the bench** — today a
bench VM, one `$BENCH_HOME` per series — as **`ledger.jsonl`** and **`reviews.jsonl`**. To fill the
table above:

1. copy those two files (or run `bench numbers --ledger <path>` there) for the series whose figures
   are wanted;
2. name the series from its plan of record, which holds the pins and the spend: the pilot
   (`plans/2026-11-21-bench-pilot-8-cells.agent.md`, coder `sha-e46c8f4`, harness `ba93c9d`) or the
   night series and the rerun (`plans/2026-11-22-bench-adam-vs-opencode.agent.md`, coder
   `sha-35af9f4` and `sha-9a9e6c5`);
3. the blind second reading of all 95 runs is attached to ANR-424 as `bench-deep-analysis.html`
   (with the plan pages as its other attachments) — it is prose over the same runs, not a ledger,
   so a figure that has to be *recomputed* comes from step 1, never from it.

## ADR 0090: hidden outputs

[ADR 0090](../docs/decisions/0090-a-tool-result-is-bounded-when-it-is-made-and-a-command-that-succeeds-says-so.md)
hides a successful command's output behind one line and keeps it for `read_output`. Its decision 5
keeps that only if it is measured: on the same tasks, before and after, compare calls per task,
`read_output` calls, input tokens per task and the cache share of the prompt; **if hidden outputs make
the model call `read_output` more often than they save, the default goes back to `full` for that
tool.** It takes two ledgers of the same tasks and arm, one series with the coder pinned before slice
T1 (every output shown) and one after, each from its own `$BENCH_HOME`:

```sh
node bench/bin/bench.mjs numbers --ledger /path/to/after/ledger.jsonl --against /path/to/before/ledger.jsonl
```

`--against` is the baseline (before), `--ledger` the series measured (after). Per agent, only the tasks
**both** ledgers finished count, matched on the `task` of each run's `Planned` record; a task finished
in one ledger only, a finished run with no task id and an agent in one ledger only are left out and
listed under the tables. For each agent it prints one table, figure by figure, before, after and the
change (the difference and, when the baseline is not zero, the relative change; a share in
percentage points), then one line naming decision 5's case from which way the two figures moved:

| `read_output` calls / task | Input tokens / task | Case |
|---|---|---|
| up | down | **weigh**: the tokens saved against the calls added, the owner's call |
| up | same or up | **against `on_error`**: the default goes back to `full` |
| same or down | down | **for `on_error`** |
| same or down | same or up | **no saving** |

There is no threshold: "up" is any increase, and the weighing is left to the reader with the numbers
beside it. The command is read-only, like the single-ledger form.

| Figure | Computed from |
|---|---|
| Calls per task | as above |
| `read_output` calls per task | the `read_output` entries of the settled calls' `toolNames` (`adam_llm_agent::kept::READ_OUTPUT`), over the finished runs |
| Input tokens per task | `usage.input` (cached tokens included) of every settled call, a child session's too, over the finished runs; a settled call without usage adds nothing and is counted apart, and the line under the table says the sum is then a lower bound |
| Cache share of the prompt | the cache share of the fixed prompt, as above |

What the ledger cannot say, so the command does not either:

- **Whose output a `read_output` call read.** The meter keeps a tool call's name, never its arguments,
  so the extra calls cannot be laid at `run_checks`, `run` or `run_command`, and decision 5 is per
  tool. A case that points back to `full` does not say for which tool; telling them apart needs one
  series per tool's default, or the harness recording the output id's tool.
- **The tokens a hidden output saved, as such.** Input tokens per task is the whole prompt over the
  run; it moves with the hidden outputs and with anything else that differs between the two series
  (the coder's other changes between the pins, the model's own variance), so the two series should
  differ by as little as the pins allow.
- **More than 64 tool names in one answer**: the meter keeps the first 64 names of an answer, so a
  count past that is short.

The result, *to be filled from the owner's runs*:

| Agent | Tasks in both | Calls / task (before → after) | `read_output` / task | Input tokens / task | Cache share | Case | Ledgers (series) | Date |
|---|---|---|---|---|---|---|---|---|
| adam | _to be filled from the owner's runs_ | | | | | | | |

## Where the epics' own figures are

`docs/reference/parity.md` compares surfaces, not speed. The speed claims live in the epic (sys#345)
and in adam ADR 0048 (the bench); this page is the place their numbers are meant to be reproduced
from, so a change to a figure is a change here first.
