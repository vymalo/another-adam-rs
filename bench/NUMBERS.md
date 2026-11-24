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
| **Calls per task** | how many model calls an agent spends on a task it finished, and the ratio between two agents on the same tasks | the `call` records of phase `settled` of the runs that reached `Done`, over those runs, per agent | reproducible from any ledger; a ratio needs a ledger holding both agents |
| **Cache share of the fixed prompt** | how much of the fixed (first) prompt a run re-reads from the cache instead of paying for again | `usage.cached / usage.input` over the settled calls that carried the run's **first** prompt fingerprint; a child session's prompt is another fingerprint and is left out | reproducible from any ledger that records `usage` |
| **Checks step's share** | the share of the coder's work that the check tool takes | **not derivable**: the ledger records which tools an answer asked for (`toolNames`) and when a model call started and ended, never how long a tool ran | see below |

## The figure the ledger cannot answer

The epic states the checks step as **61 % of the coder's tool time**. The ledger cannot support that
figure, and not because of how this repository records it: nothing in it measures a tool's duration.
A `call` record carries `toolCalls` (how many the answer asked for), `toolNames` (which), the token
usage and the model call's `startedAt`/`endedAt` — a model call's span, not a tool's. So "share of
tool time" has no denominator in a ledger, and `bench numbers` reports what the ledger does have
instead: **the `run_checks` share of the named tool calls**, beside the basis that says so.

**The epic's 61 % is therefore marked wrong as stated** — not contradicted (it may come from a
measurement nobody wrote down), but it cannot be reproduced from a ledger, and a figure that cannot
be reproduced is not evidence. Two ways to make it one, both cheap:

- **Count calls, not time**: state the `run_checks` share of named tool calls, which `bench numbers`
  prints, and drop the time claim.
- **Record tool time**: have the coder's tools write their duration into the ledger (a new field of
  a tool's record), and then the share is computable. That is a change to the harness, not to this
  command.

## The other two

Both are computable today and neither is published here, because the numbers must come from a run,
and the bench keeps its ledger outside the repository (`$BENCH_HOME/ledger.jsonl`, refused inside a
work tree). A figure is published by running the command above on that ledger and writing down, in
this table, the value with the ledger's runs and the date:

| Figure | Value | Run(s) | Date |
|---|---|---|---|
| Calls per task, and the ratio | _to be filled from the ledger_ | | |
| Cache share of the fixed prompt | _to be filled from the ledger_ | | |

A value written here must name the runs it came from: the command's table is per agent over every
finished run in the ledger it was given, so the same command on another ledger gives another number,
and neither is wrong.

## Where the epics' own figures are

`docs/reference/parity.md` compares surfaces, not speed. The speed claims live in the epic (sys#345)
and in adam ADR 0048 (the bench); this page is the place their numbers are meant to be reproduced
from, so a change to a figure is a change here first.
