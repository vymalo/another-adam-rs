# adam-engine

adam-rs embedded in an application ([ADR 0047](../../docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md)):
the runtime on the host's tokio runtime, with a store and a model client the host gives, and no server, no protocol and
nothing on the network of its own.

## The API

```rust
let engine = Engine::builder(store, agent, model, "my-alias")   // DynStore, AgentDef, DynModel, alias
    .secrets(my_secrets)                                         // optional: default_secrets()
    .limits(limits)                                              // optional: Limits::default()
    .start()
    .await?;
let turn = engine.send(None, Say::new("hello")).await?;          // a new conversation
engine.send(Some(&turn.conversation), Say::new("and?")).await?;  // the next message of it
engine.shutdown().await?;
```

| Call | What it does | Refused with (class) |
|---|---|---|
| `Engine::builder(store, agent, model, alias)`, then `start()` | migrates the store, releases the leases a fixed `Limits::worker_id` holds (`Store::release_worker_leases`: the runs its last process was stepping are claimed at once; nothing with the default, `None`), binds the agent with the screen's tools of an embedded agent (`adam_ui::Ui::embedded`: `ask_user`, `show`, `ui_catalog`, `plan_*`, and `Ui::screen` as its source), gives it the model, and spawns the worker on the current tokio runtime | `NoRuntime` (`Invalid`) outside a tokio runtime; `Invalid` for a zero `lease_ttl` or a `worker_id` of `Some("")`; `WorkerIdInUse` (`Rejected`) for a fixed id another live engine of this process uses on the same store; `Store` (the store's: its migration or the release); `Invalid` for an `mcp.json` or an `a2a:` subagent, the subagents' included: the engine runs neither yet, and refuses them itself, so what it accepts does not depend on the features another crate of the build turns on in `adam-assembly`; `Agent` (the definition's, `Invalid`): an empty alias, a tool nobody registered, a secret refused |
| `send(None, say)` | starts a run in a new conversation (`Delivered::Started`) | `Invalid` for no text or no id; `Stopped` (`Rejected`) after `shutdown` |
| `send(Some(&conversation), say)` | the latest run of the conversation has ended: a new run that continues it, the agent starting from its state, the transcript included (`Continued`); it is still open: the message is delivered to it, as a steer while it works (`Steered`) or as the answer while it waits (`Answered`), by the state read just before. A `Say` that already started a run starts nothing (`Repeated`) | `NotFound` for a conversation the engine has no run in |
| `steer(run, say)` | a message for a run that works: it reads it at its next step, and a run about to finish takes another step first; a waiting run takes it as its answer | `Finished` (`Rejected`) for a run that ended; `NotFound`; `Unavailable` (`Transient`) when its state moved under the call |
| `answer(run, say)` | the answer to the question a waiting run asked; it works again | `NotWaiting` (`Rejected`) for a run that works; `Finished` for one that ended; `NotFound`. A refusal names the state read after it, never one it did not read |
| `cancel(run)` | ends the run `Cancelled`; a cancelled run is answered as it is | `Finished` for a run that ended otherwise; `NotFound` |
| `list(ListQuery)` | a `Page<RunInfo>` of the engine's runs, the most recently updated first, by cursor (`page_size`, at most 100, default 50; `conversation`; `after(token)`) | `Invalid` for a token of another query |
| `get(run)` | the `RunInfo`, or `None` | |
| `events(run)` | `RunEvents`, a `futures::Stream` of `Result<EngineEvent, EngineError>`: the run's recent events, then the live ones, then `Ended` once the store says the run has ended; a run that has already ended gives `Ended` at once. See [A run's events](#a-runs-events) | `NotFound` for a run the engine does not have, refused at the call (the stream would have nothing to say) |
| `shutdown()` | waits for the messages being written, then nothing new is claimed and the steps in flight finish and commit; it returns once the worker has stopped, and so does every concurrent or later call; every clone stops; reads and `cancel` still work. Dropping a `shutdown` before it is done (a timeout around it) aborts the worker, and so does dropping the last engine, as a killed process would: the next engine started with the same fixed worker id claims its runs at once, any other once their leases run out | what the worker ended with |

The types are the engine's own: `Say { id, text }` (the id makes a send idempotent: the same `Say` again reaches
the run its first attempt started), `Turn { conversation, run, delivered, info }`, `Delivered` (`Started`, `Continued`, `Steered`, `Answered`,
`Repeated`: a closed enum, so a new way of delivering breaks a `match` on purpose), `RunInfo { run, conversation,
state, question, output, error }`, `RunState` (`Submitted`, `Working`, `Waiting`, `Done`, `Failed`, `Cancelled`),
`ConversationId`, `ListQuery`, `Page<T>`, `Limits`, `EngineEvent`, `RunEvents` and `EngineError`, which implements
`adam_error::Classify`: decide from the class. No A2A or driver type is in a signature: a live event is adam's own
`adam_runtime::RunEvent`.

**`RunState` is `#[non_exhaustive]`** because slice 5's approvals may add a state (a run that waits for the person's
yes to a tool call): match it with a `_` arm.

**The engine's own API is the supported one.** The crates it re-exports (`adam_a2a_runtime` and the others) are
there for the types a host passes in (`AgentDef`, `DynModel`, `DynStore`, `SecretSource`), not as a second way in.

**What the host gives.** The store (`DynStore`: `adam_store_sqlite::SqliteStore::open(path)` for a file of the
application's own that outlives the process, or `adam_core::MemoryStore`; the engine depends on neither, so the
host brings the one it chooses), the agent's definition (`AgentDef`, from `include_agent!` or a folder),
the model (`DynModel`, with its credentials: ADR 0047 decision 4, [ADR
0058](../../docs/decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md)) and, if the
agent's files name secrets, a `SecretSource`. The engine reads no environment variable, no key and no file it was not
given. `default_secrets()` is `adam_core::NoSecrets`, which refuses every name (ADR 0047 decision 5), and the
builder's source replaces whatever source the definition had.

**Defaults** (`Limits`, `#[non_exhaustive]`: set its fields on `Limits::default()`): `concurrency` 4 runs stepped at
once, `max_parallel_tools` 10 tool calls of one turn at once (an agent's file may lower it), `lease_ttl` 30 s (zero
is refused), `event_capacity` 256 live events in the ring (1 to 65 536; the channel rounds it up to a power of two),
`worker_id` `None` (a random id at every start; `Some("")` is refused). `Limits` is `Clone`, no longer `Copy`: it holds
the id.

**A fixed worker id makes a restart claim its old runs at once; it is opt-in.** With `worker_id: None` the engine
releases nothing, so engines that share a store never take each other's leases, and a restarted engine waits up to
`lease_ttl` for the runs its last process was stepping. With `Some(id)` the host promises that **no other live engine,
in any process, uses `id` on this store**, and `start` releases every lease `id` holds (`Store::release_worker_leases`)
before the worker starts, so those runs are claimed at once (a store that does not implement the release, whose
default does nothing, still waits). Set an id only for a store no other engine shares, such as a store per install.
The SQLite store needs none: its lock allows one process per file, and opening the file clears every lease the
last process held, so a restart claims its runs at once with the default too. A
broken promise steps a run twice, its tools included: the release takes the other engine's leases, and both renew
under the same id.

Within one process, `start` refuses a second live engine with the same id on the same store (`WorkerIdInUse`,
`Rejected`: shut the other down, or give this one another id); it cannot see other processes. The id is free again once
the first engine's worker task has ended: when `shutdown` returns, or shortly after the last `Engine` is dropped (the
abort takes effect at the task's next poll, so a `start` just after can still be refused). The steps the worker spawned
are aborted with it, not waited for: a tool that ignores cancellation (blocking code, a task of its own) can still run
once the id is free, so a host that restarts in the same process calls `shutdown` first.

**Inside.** Every call is what the A2A server does for one caller: `RuntimeTaskBackend` of
[`adam-a2a-runtime`](../adam-a2a-runtime/README.md), as the subject `adam-engine` (a steer with `steer/v1`
activated), so follow-ups, steering, answers and continuations have one implementation. A conversation is a
context; a send to one names its latest run in `referenceTaskIds`, which the backend continues once it has ended.
`answer` sends the plain follow-up, which the backend takes only from a waiting run.

**A killed process resumes its run** on the SQLite store: the next process to open the file finishes it, without
running a committed step again (`tests/resume_process.rs`, below). With SQLite no fixed `worker_id` is needed: the
store's lock on the file proves the killed process is gone, and opening it clears every lease the file holds.

**Not yet**: a way to read a file artifact's bytes (`RunInfo` carries no artifacts yet, and an event carries none, below). The backend is
given the engine's ring too, and its A2A streaming path (`TaskBackend::subscribe`) builds a file part from a live
event's bytes: the engine never calls it, and a future caller that streams a task through it must read files from the
store, since the ring withholds them. Approvals are slice 5: today a run waits only on a question, and `answer`
answers it.

## A run's events

```rust
use futures::StreamExt;
use adam_engine::EngineEvent;

let mut events = engine.events(turn.run).await?;          // subscribed when this returns
while let Some(event) = events.next().await {
    match event? {
        EngineEvent::Live(event) => show(event),           // adam_runtime::RunEvent: a step, a text piece, ...
        EngineEvent::Behind { skipped } => {}             // the reader lost live events; a fresh read follows
        EngineEvent::Snapshot(info) => redraw(info),       // the run as the store has it now
        EngineEvent::Ended(info) => done(info),            // the last item: Done, Failed or Cancelled
        _ => {}                                            // EngineEvent is #[non_exhaustive]
    }
}
```

* **The store is the truth** (rule 5: a notification is never the truth). A live event is a prompt: a `Status` makes the
  stream read the run, and `Ended` is that read; with no event for 2 s it reads the run anyway, so an end nobody
  announced is not missed.
* **How it ends.** With `Ended`, the last item; or with one `Err`, then nothing: `EngineError::Stopped` (`Rejected`)
  once the worker has stopped and the run has not ended (after `shutdown`, at once rather than at a poll; when every
  `Engine` is dropped; when the worker failed), since nothing would step it; `NotFound` if the run is gone; or the
  store's error when a read fails. Call `events` again on a live engine to go on.
* **Bounded whatever the reader does.** The runtime emits into one ring (`tokio::sync::broadcast`) of `event_capacity`
  events, every run's, one copy shared by every reader. A reader that does not keep up holds nothing more: the ring
  overwrites its oldest events, and the reader learns it on its next read, `Behind { skipped }` (how many events of the
  engine it missed, **every run's**: a quiet run beside busy ones can get `Behind` with none of its own events lost), then
  at once `Snapshot(RunInfo)` read from the
  store, or `Ended` if the run is over. The live events after a `Snapshot` may have been emitted before it: take a text
  piece by its `offset` and a step by its `id`. A stream holds besides the run's recent events it started with (the
  runtime's replay: at most `REPLAY_EVENTS_PER_RUN`, 64, of the run, within its 16 MiB for every run, see
  [`adam-runtime`](../adam-runtime/README.md#a-late-subscriber)) and at most two items. So a reader that reads only
  after its run is over gets at most `REPLAY_EVENTS_PER_RUN + event_capacity` live events (80 with a ring of 16), then
  `Behind`, then `Ended` with the whole output.
* **What an event weighs.** A text piece is at most 1 KiB (`MAX_TEXT_DELTA_BYTES`), a step's end about 12 KiB of input and
  output. **A file artifact's bytes are taken out** before the event enters the ring (the engine's sink; a file can be
  4 MiB): its `name`, `mime_type` and `filename` are kept, `bytes` is empty, and `data` (`null` for a file in the
  runtime's own events) says what was withheld, `{"withheld": {"bytes": <len>, "sha256": "<hex>"}}`, which
  `WithheldFile::of(&event)` reads (`len`, and the `sha256` that `Artifact::file_sha256` gives). Every file of an
  engine's events says it, an empty one too (`len` 0), so a stripped file is never taken for an empty one. The file is
  durable with the run.
  Not bounded by the engine: a `Custom` event's payload and a JSON artifact's `data`, which are what the agent's tools
  make.
* **Unknown and ended runs.** `events` of a run the engine does not have is `NotFound`, at the call. A run that has
  already ended gives `Ended` at once and nothing else.
* **It does not keep the engine alive.** Drop every `Engine` and the worker stops as a killed process would; an open
  stream ends then, with `Stopped`.

## The example

`examples/chat` is a terminal chat, the smallest application that embeds the engine (the guide:
[Embed adam in an application](../../docs/guides/embed-in-an-app.md)):

```sh
MODEL_BASE_URL=https://gateway.example.com/v1 MODEL_API_KEY=... MODEL=my-model cargo run -p adam-engine --example chat
```

* `main.rs` builds what the host gives: the model with `adam-model-openai` from `MODEL_BASE_URL`, `MODEL_API_KEY` (unset
  or empty sends no header; not UTF-8 is an error that names the variable) and `MODEL`, the names `adam-service` uses; a
  `MemoryStore`; and the agent, `examples/chat/agent/`, read at startup with `AgentFolder::load` from
  `CARGO_MANIFEST_DIR`, the build machine's path: the development path. An application ships its agent with
  `include_agent!` instead (a comment shows how).
* `app.rs` is the loop, `run(engine, input, out)`: a plain line is a `send` to the conversation; `/steer <text>`,
  `/answer <text>`, `/cancel`, `/list` and `/quit` (which says `[stopping: the current step finishes first]` while a
  step is in flight). At the end of the input it follows the run to its end or its question, then shuts the engine
  down, so `printf 'hi\n' | ...` prints the answer. A send that starts a new run first reads the last one's end. It prints
  the followed run's text as it streams (a piece by its `offset`, so a repeat is not printed twice), its question when
  it waits (read with `get` on a `Status`), `[behind: N events skipped]`, and how it ended; a refusal is printed with
  its class.
* `adam-model-openai`, `secrecy` and tokio's `io-std` and `io-util` are **dev-dependencies**: the embedded check reads
  normal dependencies only, so the engine's tree still holds no HTTP client and no TLS stack.

## The crates it is built from

The crate's dependencies are the crates an embedded agent needs, each with its server parts turned off, and it
re-exports them:

| Crate | Built without |
|---|---|
| [`adam-a2a-runtime`](../adam-a2a-runtime/README.md), [`adam-a2a`](../adam-a2a/README.md) | `push`, and `server`: the backend port and the A2A types are data here |
| [`adam-ui`](../adam-ui/README.md) | `thread-tools`: `Ui::embedded` and `Ui::screen` |
| [`adam-assembly`](../adam-assembly/README.md) | `remote`: no remote (`a2a:`) subagents, so no A2A client, HTTP client or TLS; `bind` refuses one with `Error::RemoteNotEnabled` ([Without remote subagents](../adam-assembly/README.md#without-remote-subagents-feature-remote)) |
| [`adam-runtime`](../adam-runtime/README.md), [`adam-llm-agent`](../adam-llm-agent/README.md), [`adam-core`](../adam-core/README.md), [`adam-agent-fs`](../adam-agent-fs/README.md), [`adam-model`](../adam-model/README.md), [`adam-error`](../adam-error/README.md) | nothing: they have no server parts |

It also depends on `a2a-lf` (the A2A types, data only: ADR 0047's confirmed decision 1), `tokio` (`rt`, `sync`,
`time`), `futures` (the `Stream` of a run's events), `async-trait` (the engine's event sink), `serde_json` (what a
withheld file says), `thiserror` and `uuid`.

## The dependency check

`tools/embedded-deps/check.sh` (CI job `embedded`, part of the required `lint`):

* builds `adam-a2a`, `adam-a2a-runtime`, `adam-ui` and `adam-assembly` with their defaults off and with each feature
  alone, tests included, and this crate, under `clippy --all-targets -D warnings`;
* fails when a dependency turns a gate back on in its crate's own build without defaults (`cargo tree -e features -i`;
  `adam-assembly`'s tests depend on `adam-agent-fixture`, which depends on `adam`, which depends on `adam-assembly`);
* fails when this crate's tree (`cargo tree -e normal --target all`, so a Windows- or macOS-only dependency is seen
  from Linux too) holds any of: `axum`, `hyper`, `h2`, `reqwest`, `ureq`, `curl`, `curl-sys`, `tower-http`, `warp`,
  `actix-web`, `tonic`, `tokio-tungstenite`, `a2a-server-lf`, `a2a-client-lf`, `utoipa-swagger-ui`, `sqlx`,
  `sqlx-core`, `tokio-postgres`, `postgres`, `mongodb`, `rusqlite`, `libsqlite3-sys`, `diesel`, `redis`, `kube`,
  `k8s-openapi`, `rustls`, `aws-lc-rs`, `aws-lc-sys`, `ring`, `openssl`, `openssl-sys`, `native-tls`, `rmcp`. Not
  `mio`: tokio's process support needs it, and the tools (slice 6) run commands;
* runs the split crates' unit and integration tests with their defaults off, and their docs under `-D warnings`;
* `self-test` proves it: it plants `axum`, then `rustls`, in this crate's manifest (in the working tree, put back after),
  and turns `adam-ui/thread-tools`, `adam-a2a-runtime/push` and `adam-assembly/remote` back on; each must fail the
  check and be named (`rmcp`, `axum`, `reqwest`). The
  files are put back on any exit, a failure or an interrupt included.

## Features and environment

No Cargo features, no environment variables. The example reads `MODEL_BASE_URL`, `MODEL_API_KEY` and `MODEL` (its
`main.rs`, not the engine), and `tests/resume_process.rs` reads `ADAM_ENGINE_TEST_RESUME_CHILD` and `ADAM_ENGINE_TEST_RESUME_FIXED_ID` in the child it starts.

## Tests

`tests/engine.rs`, over `MemoryStore` and `adam_model::MockModel` (wrapped so that one model call waits at a gate);
every wait is bounded by `tokio::time::timeout`:

* `send_starts_then_continues`: the second send is a new run in the same conversation (`Continued`), and the
  model's second request holds the first exchange; a conversation the engine never started is `NotFound`;
* `a_send_to_a_working_run_steers_it` (`Steered`, the same `Say` twice read once),
  `a_send_to_a_waiting_run_answers_it` (`Answered`, then `Done`), `a_repeated_say_starts_nothing` (`Repeated`);
* `a_steer_to_an_ended_run_is_refused` (`Finished`), and an unknown run is `None`;
* `steer_reaches_a_working_run`: a message sent while the model writes the answer is read before the run ends;
* `answer_resumes_a_question`: `ask_user` parks the run with its question, `answer` resumes it to `Done`, and an
  answer to the working run is `NotWaiting` (`Rejected`) and reaches nothing;
* `cancel_ends_a_waiting_run`: idempotent, and the cancelled run takes no answer;
* `list_pages_newest_first`: pages, a conversation's runs alone, a token of another query refused;
* `shutdown_refuses_new_work_and_reads_go_on` (a send, a steer and an answer are `Stopped`);
* `a_dropped_shutdown_stops_the_worker`: a timed-out `shutdown` aborts the worker, so the run's lease runs out;
  `concurrent_shutdowns_both_wait_for_the_worker`;
* `events_follow_a_run_to_its_end`: a reader that keeps up hears the tool's step and the text live, then `Ended(Done)`
  with the output, once, and nothing after it;
* `a_slow_reader_stays_bounded`: a ring of 16 and a reply of 256 KiB (256 pieces); the reader reads only after the run
  is done, and gets at most `REPLAY_EVENTS_PER_RUN + 16` live events, `Behind { skipped > 0 }`, then `Ended` with the
  whole output read from the store;
* `a_reader_behind_a_running_run_gets_a_snapshot`: the same ring, a reply of many pieces and then a question; the
  reader reads once the run waits, gets `Behind`, then a `Snapshot` (`Waiting`, the question), and after the answer
  follows the run live to `Ended(Done)`;
* `events_of_an_unknown_run_and_of_a_finished_one`: `NotFound` at the call, and `Ended` alone;
* `a_stream_ends_stopped_after_shutdown`: a stream on a waiting run, and one opened after `shutdown`, end with
  `Stopped`, under a second, not at a poll;
* `a_dropped_engine_resumes_its_run`, the in-memory stand-in for a killed process: two engines over one `MemoryStore`
  (`Arc`) on the lease-expiry path, a random worker id each (the default) and a lease of 300 ms: engine A dropped while
  its run's second model call is in flight (its worker aborted, its open stream ended with `Stopped`), and engine B,
  its script written before its worker starts, finishing the run once A's lease runs out; B's model is called once, its request holds the tool's result once and
  ends with it, and B's
  stream, opened while B's first model call waits at a gate (so it starts with all B did since it claimed the run),
  shows the tool's step never run again: the committed step is kept. `tests/resume_process.rs`, below, is the real
  proof, a process killed with `SIGKILL`, on the SQLite store;
* `a_restarted_engine_claims_its_old_runs_at_once`: A, with the fixed id `restarted` and a lease of an hour, is dropped
  mid-step, and B, the same id on the same store (started once A's worker has ended and freed the id), finishes the
  run within 5 s, which it cannot do without the release; `an_empty_worker_id_is_refused` (`Some("")`, `Invalid`);
* `two_default_engines_on_one_store_do_not_take_each_others_leases` (`None` twice) and
  `two_engines_with_their_own_ids_keep_their_own_leases` (`engine-a`, `engine-b`): two live engines on one store, A's
  run in flight under a lease of an hour; after B's start it is still leased and `Working`, B's model sees only B's
  run, and each engine steps its own run once;
* `a_second_live_engine_with_the_same_id_is_refused`: `WorkerIdInUse` (`Rejected`) on the same store; the same id on
  another store starts, and on the first once A has shut down;
* `start_refuses_what_it_cannot_build` (a zero lease and a ring of 0 or over 65 536 too; it passes alone and with
  `--features adam-assembly/mcp,adam-assembly/remote`, the set CI's workspace build unifies) and `start_outside_a_tokio_runtime_is_refused`: fail
  closed.

`tests/example.rs` drives the example's `app::run` (`#[path]`) with lines typed one at a time, each once the transcript
shows what the last did, over `MemoryStore`, the example's own agent folder and a scripted model:

* `the_example_chats_streams_and_answers_a_question`: the whole transcript, line for line: a send whose reply is on
  the screen while the model has not written its second half, `[done]`, a question answered with `/answer`, `/list`
  (two runs, the latest first) and `/quit` (`bye`);
* `the_example_steers_refuses_an_early_answer_and_cancels`: `/answer` to a run that works is refused (`Rejected`) and
  reaches no model request; `/steer` does, and the run takes another step to read it; `/cancel` with no open run is
  refused by the example, and ends a waiting run `[cancelled]`;
* `the_end_of_the_input_waits_for_the_run`: an input that ends while the model has not answered still gets the whole
  reply, `[done]`, then `bye`; one that ends after a question gets the question, then `bye`;
* `quit_during_a_step_says_it_waits_for_it`: `/quit` while the model works prints the stopping line, then `bye`.

`tests/resume_process.rs` (Unix), `a_killed_process_resumes_its_run`, on a SQLite file in the test's own directory
(`adam-store-sqlite`, a dev-dependency only, so the embedded tree is unchanged): the test runs its own binary again as
a child (`ADAM_ENGINE_TEST_RESUME_CHILD`, read by this test only, names the store's directory), which sends a message
whose model blocks once the run's first step (a `plan_work` call) is committed and says so in a file; the parent kills
it with `SIGKILL`, opens the same store, and its engine finishes the run. The child's lease is an hour, so the parent
finishes in time only because the lease is let go when it opens the store. It runs twice: with the engine's default,
a random worker id, where the store's `migrate` clears the dead process's leases (its file lock proves the process is
gone); and with the fixed id `resume-test` in both halves (`ADAM_ENGINE_TEST_RESUME_FIXED_ID`), where `start` releases
that id's leases too. Each time the parent's model, scripted for the next step only, is called once, and its request
holds the tool's result once and ends with it, which fails if the committed step was lost.

In `src/`: `a_name_the_environment_has_is_refused_by_the_engines_default` (slice 2's proof),
`the_sink_keeps_file_bytes_out_of_the_ring` and `a_withheld_file_says_its_length_and_digest` (a file's event reaches a
reader with its name, its length and SHA-256 and no bytes, an empty file says it is one, every other event is as it
was), `the_engines_runtime_withholds_file_bytes_from_the_ring_only` (the chat agent's tools make no file, so a stub agent
makes one on the runtime `start` builds, `runtime_with_ring`: the ring has the bytes withheld, the committed record has
them all), and
`every_backend_refusal_keeps_its_class` and `a_refusal_for_the_state_says_the_state_it_read` (the races a test cannot
make happen on cue).

## See also

[ADR 0047](../../docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md),
[ADR 0058](../../docs/decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md),
[`docs/guides/embed-in-an-app.md`](../../docs/guides/embed-in-an-app.md),
[`docs/guides/embed-adam.md`](../../docs/guides/embed-adam.md).
