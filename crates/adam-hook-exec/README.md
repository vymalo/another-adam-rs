# adam-hook-exec

The `adam-hooks/1` adapter ([ADR 0050](../../docs/decisions/0050-hooks-around-the-agent-loop-compiled-first-and-programs-second.md),
slice **P1**): a **program** on stdio, behind the `Plugin` trait. A compiled plugin is code in the
process; this crate is the other half of the owner's answer — *"both, and compiled first"* — so an
operator can add a policy without a build.

The adapter is one `Plugin`: every deciding point the program is configured at becomes one `event`
request, every event it subscribes to becomes one `observe` request, and the program answers one JSON
line per request with a verdict (`continue`, `rewrite`, `deny`, `ask`, `stop`). The verdict is the
loop's as if it were compiled: recorded in the step it affects, so **a replay asks no program
again**. The wire format is [the protocol page](../../docs/reference/hooks-protocol.md).

## The API

| Item | What it is |
|---|---|
| `HookExec::start(Config)` | starts the program in a private directory, says hello, and checks the answer: `Error::Spawn`, `Error::Hello`, `Error::Protocol`, `Error::Malformed` and `Error::Config` are the ways a start fails (a binary reports them as exit 78) |
| `Config` | what the deployment owns, passed as a value: the program and its arguments, the variables it adds to `SAFE_VARS` (a plain upper-case name, never `PATH` or `LD_*`), the agents it applies to (`applies_to`), its working directory (an empty private one by default), its `Points`, its deadline (2 s by default, at most 30 s) and its scrubber |
| `Points` | the deciding points (`DecidePoint`: `message.received`, `tool.before`, `tool.after`, `model.request`, `compaction.before`, `run.ending`) and the events it observes (an exact name, `ns.*` or `*`); a deciding point is an enum, so an observe-only event cannot be named in `decide` |
| `HookExec` | the `Plugin`: the six deciding methods, `observe` and `subscriptions`, each a request and its verdict |
| `Error` | carries an `adam-error` class: a broken start or a broken line is `Invalid`, a missed deadline or a dead program is `Transient`, an action is `Unsupported` |

## The rules

- **Fail closed.** A missed deadline, a line that is not JSON, an unreadable reply or a dead child
  is an error, never a `Continue`: the call is refused, the message is not sent.
- **Killed and restarted.** A program that closes its output or writes what it must not is killed
  at once; three deadlines in a row kill it too. Its deciding points are `Transient` until its
  backoff elapsed (1 s, doubling to 30 s), and then it is started again. One kill advances the
  backoff once, however many calls saw it.
- **No I/O under the lock.** A writer thread owns the program's stdin and takes lines from a bounded
  queue, a reader thread owns its stdout: a program that stops reading is treated as broken, never
  waited for.
- **Only what it should see.** Each payload is scrubbed whole, and a rewrite is applied only when
  the scrubbed payload equals the real one (`Invalid`, so denied): a program cannot write a
  redaction marker into a real call. A verdict's reason is scrubbed too. The child gets an empty
  environment, then `SAFE_VARS`, then the configuration's variables; stderr is logged scrubbed.
- **No run context.** A hook gets no `Ctx`, no store and no secret. A deciding request's key is
  `<plugin>/<point>/<id>`: the loop's journal key is not part of a compiled hook's inputs today, and
  a replay sends no request at all. An observer's key is the event's own, stable across replays.
- **The set's deadline is separate.** `HookLimits` defaults to 1 s per plugin call and is measured
  after it returns, so a host that wants the protocol's 2 s builds its set with
  `HookSet::with_limits`.
- **Actions are not built.** A reply that asks for one is refused with `Unsupported` rather than
  silently dropped (ADR 0050, slice H1d).

## Tests

`tests/exec.rs` runs the protocol's test program (`tests/program/main.rs`, the bin
`adam-hook-exec-test-program`): a rewrite of a call, of a message, of an answer, of a compaction's
context and of a model call; a deny, an ask and a stop with their reason; garbage refused as
`Invalid`; a dead program `Transient` and refused by the set; a dead program or another protocol
failing the start; the child's environment (only `SAFE_VARS` and the configured ones); a rewrite over
a scrubbed span refused; a missed deadline; three deadlines killing the program; a program that dies
restarted after its backoff; an observer that decides refused; and a configuration refused.

`tests/replay.rs` is the slice's end-to-end proof: a worker dies inside a tool call, a second worker
replays the journal, and the program's own log shows the recorded step asked once and the
unrecorded one twice.
