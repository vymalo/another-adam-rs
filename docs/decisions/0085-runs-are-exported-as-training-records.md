# 0085. Runs are exported as training records

Status: **Accepted** (2026-11-27). The owner decided on 2026-11-27: *"Adam needs an exporter. Even for chats and all, we
need to have that. I wanna train small language models so that they'll use adam-rs very well. And at the same time,
I'll want them to understand a specific programming language x framework only, e.g rust x axum only. So e.g
qwen3.5-rust-cratestack-adam-v1-instruct a instruct model very good in rust, and cratestack, good at using adam as
harness."* Decisions 3, 5 and 6 are this ADR's choices for the owner to confirm.

*Amended 2026-11-27:* slice E2 shipped without `--current-tools`. `LlmAgent` exposes no tool schemas without a model
client and connected MCP servers, and a record marked `current` with only some tools would mislead a trainer. The
flag is a usage error; the library keeps `ExportOptions::current`, and slice E3 makes the flag unnecessary.
Slice E3 is built (2026-11-27): `Conversation::request` names the blobs of the last call's system prompt (its whole
`system`, not only the agent's instructions) and tools (RFC 8785 JSON); recording is all or nothing and fails open; the
exporter reads them with `RecordedRequest::load`.

*Amended 2026-11-27:* slice E5 is built. `--samples turn` (`Samples::Turn`, `samples_of`) writes one record per
assistant turn: the system prompt and tools recorded for that call, the history before its answer, and the answer last,
with `train_on: "last"` and `sample: {turn, index, of, exact}` (a run record says `train_on: "assistant"`); the schema
stays `adam.export/v1`. E3 now also keeps `Conversation::requests`, the run's distinct requests (one entry per change of
prompt or tools, at most 64, derived from the journaled answers so a replay writes the same list), and a sample takes
the entry at or before its turn. Limits: only the calls since the run's **last** compaction have samples (the turns
before are counted, never rebuilt), so in turn mode a compacted run is kept by default; a sample before a later
shortening, without a recorded entry, with a possible closing answer, from a run whose context names an interface
language, or from a run whose answers do not add up to its calls, is `exact: false`. `--max-tokens N` leaves out a
record over `N` tokens estimated as characters / 4, whole: a sample is never cut.

## Context

adam-rs had no exporter (*verified 2026-11-27*: no export code in `crates/`, `bin/` or `bench/`). The thread exports of
ADR 0028 are made by a client from the agent card and the A2A stream. That stream carries what the person saw. It does
not carry the messages the model was sent, the tool calls it made or the results it read, so it cannot teach a model to
drive the harness.

The store holds that material. `Runtime::state` joins a run's head and its body generation into the agent's whole
state. For an `LlmAgent` that is a `Conversation`, whose `messages` are the model's own history: user turns, assistant
turns with their `tool_calls`, and tool results. Three things are missing or unsafe for training:

* **The request's system prompt and tool schemas are not stored.** The model step journals only the answer
  (`adam-llm-agent`, the `model:<turn>` step: "only the answer is journaled, not the tools it was given"). A sample
  without its tools teaches the model to call tools it was never shown.
* **Runs hold secrets and people.** A tool result can echo a token, and a conversation's `context` and
  `prompt_statement` are about a person.
* **Nothing says what a run is about.** Training a model for one stack (Rust and axum, Rust and CrateStack) needs a
  filter on the language and the libraries a run touched.

## Decision

1. **A new crate, `adam-export`, turns a run into one record.** It is pure: no store driver, no I/O of its own beyond a
   `std::io::Write` sink. Its inputs are the run's bookkeeping (`RunRecord` without its state) and its joined
   `Conversation`. A driver over `&Runtime` lists runs (`list_runs`), joins each one (`Runtime::state`) and writes the
   records that pass the filter. A run whose agent is not an `LlmAgent` is skipped and counted.
2. **One record per run, as JSON Lines, in the chat format most trainers read.** `schema` is `adam.export/v1`.
   `messages` are `system`, `user`, `assistant` (with `tool_calls` as `{id, type: "function", function: {name,
   arguments}}`, `arguments` a JSON string) and `tool` (`tool_call_id`, `content`). `tools` are `{type: "function",
   function: {name, description, parameters}}`. That is the OpenAI chat shape that the Hugging Face chat templates of
   Qwen and the SFT tools accept (*unverified*: checked from memory, not against each trainer's current docs). A
   subagent's run is its own record, linked to its parent by `parent`. A chat with no tools is the same record with
   no `tools`. Every agent and every run is exported the same way.
3. **The record's metadata says enough to filter and to weigh.** `agent`, `status`, `created_at` and `updated_at`,
   `turns`, `tool_calls`, `usage`, `models` (the aliases its assistant turns name, and the fallback switch), `compacted`
   (a compaction replaced history, so the messages are not what the model saw at every turn), `facets` (decision 5)
   and `outcome` (decision 6). Reasoning text is kept only when the run kept it and the export asks for it
   (`reasoning_content`). Image parts become the text `[image]`: no image bytes are exported.
4. **Nothing secret or personal leaves by default.**
   * Every string goes through a `Scrub`, which the binary builds from the secrets its configuration holds (the
     redactor the tools use), so a token a tool echoed is replaced by the marker the model already knows.
   * Run, parent and conversation ids are **pseudonyms**: HMAC-SHA256 under a key the operator passes, so two exports
     with the same key link, and without the key nothing links back to the store. Without a key the export makes a
     random one.
   * A conversation's `context`, its `prompt_statement`, its session header and the A2A caller are **never** exported.
     The messages a person wrote are exported: a deployment exports only threads it may train on, and that is the
     operator's call, not the code's.
   * An export is a command someone runs. No worker, route or schedule exports anything.
5. **Facets are derived from the transcript, and marked so.** `lang`: from the file extensions in tool call arguments
   (`.rs` is `rust`). `dep`: from the dependency tables of the manifests the run read or wrote (`Cargo.toml`'s
   `[dependencies]`, `[dev-dependencies]`, `[workspace.dependencies]` and target tables; `package.json`'s
   `dependencies`), and from the crates named by `use <crate>::` in Rust text. A filter requires facets
   (`lang=rust`, `dep=axum`, `dep=cratestack`): "Rust and CrateStack" is two requirements. A facet is a heuristic, not
   a fact. A later slice may have the coder record its workspace's manifests at run time.
6. **The outcome is what the store already says.** `status` (`done` or `failed`), the error, and the run's artifacts by
   name and media type (the coder's `pull_request` artifact among them). A trainer that wants only verified pull
   requests filters on that. The self-improvement observer of ADR 0081 and a person's rating come later.
7. **The request is captured from now on (slice E3).** The model step will write the request's system prompt and tool
   list as content-addressed blobs (`Store::put_blob`, so the same prompt and tools are stored once across turns and
   runs) and name their ids in its journaled answer. The exporter then attaches the exact system prompt and tools of
   the run's last model call, `tools_source: "recorded"`. A run from before that slice has `tools_source: "none"`, or
   `"current"` when the export passes the agent's current prompt and tools, which may differ from the ones it ran with.

## Slices

| Slice | What |
|---|---|
| E0 | This ADR. |
| E1 | `adam-export`: the record, the conversion from a `Conversation`, facets, filters, `Scrub`, pseudonyms, the JSONL writer and the driver over `&Runtime`. |
| E2 | An `export` subcommand on `adam-agent` and `adam-coder`: it reads the store from the same configuration, registers the same agents, runs no server and no worker, and writes to standard output or a file. `--current-tools` passes the agent's current prompt and tools. |
| E3 | The model step records the system prompt and tools as blobs (decision 7). |
| E4 | The coder's outcome: the pull request, whether its head was verified, the checks. |
| E5 | Per-turn samples (one record per assistant turn with its exact prefix and tools), for trainers that need them. |

## Consequences

* A small model can be trained on adam's own runs of a stronger model (distillation): run `adam-coder` on Rust and
  CrateStack tasks, export the `done` runs with `lang=rust` and `dep=cratestack`, train on them, and measure the result
  with the benchmark harness (ADR 0048) by pointing `MODEL_BASE_URL` at the trained model.
* Knowing Rust and CrateStack in general needs data that is not adam's runs (code, docs). That corpus is outside this
  repository. The exporter supplies the part only adam has: how to use the harness.
* No `Store` or `Notifier` method changes, so no adapter breaks. Slice E3 adds blobs to commits a store already keeps.
* A new field of the record is not a breaking change. A changed meaning is a new `schema`.
