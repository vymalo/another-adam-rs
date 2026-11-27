# adam-export

Turns adam-rs runs into training records: one JSON Lines record per run, in the chat shape most
trainers read, with the facets a stack filter needs, the outcome the store holds, every string
scrubbed and every id a pseudonym. The conversion is pure: no store driver and no I/O beyond the
`std::io::Write` sink it is given. The driver reads runs through a `Runtime`.

The decision is
[ADR 0085](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0085-runs-are-exported-as-training-records.md),
slice E1. The binaries' `export` subcommand (slice E2) is described in
[`adam-agent`](../../bin/adam-agent/README.md) and [`adam-coder`](../../bin/adam-coder/README.md);
the training pipeline around it is the guide
[Export runs for training](../../docs/guides/export-runs-for-training.md).

## What is in it

| Item | Purpose |
|---|---|
| `record_of(&RunMeta, &Conversation, &ExportOptions) -> Record` | One run to one record. Pure. |
| `record_with(&RunMeta, &Conversation, Option<&RequestShape>, &ExportOptions) -> Record` | The same, with the prompt and tools the run recorded (`Conversation::request`, read from the store); they win over the current ones and the record says `recorded`. |
| `RunMeta`, `RunMeta::of(&RunRecord, error)` | A run's bookkeeping without its state: id, agent, conversation id, parent, status, times, error. |
| `ExportOptions` | `new(Pseudonyms)`, then `.scrub(..)` (the deployment's secrets), `.include_reasoning(bool)`, `.current(agent, RequestShape)` (the agent's current prompt and tools). |
| `RequestShape` | A system prompt and a list of `ToolSpec`, as the export passes them. |
| `export_runs(&Runtime, agents, &Filter, &ExportOptions, &mut impl Write) -> Result<Summary, ExportError>` | The driver: lists each agent's runs, joins each one, writes the records the filter accepts. |
| `write_record(&mut impl Write, &Record)` | One record as one line of JSON. |
| `Summary` | `seen`, `written`, `filtered`, `not_a_conversation`, `unreadable`. |
| `ExportError` | `Runtime` (a listing failed; its class), `Write` (`Transient`), `Json` (`Internal`). Implements `Classify`. |
| `Filter` | Which records are written; `Filter::parse_requirement("lang=rust")`. See *Filters*. |
| `Record`, `ChatMessage`, `ChatToolCall`, `FunctionCall`, `ToolDef`, `FunctionDef`, `Outcome`, `OutcomeArtifact`, `Facets`, `ToolsSource`, `SCHEMA` | The record (below). `Serialize` and `Deserialize`. |
| `facets` (module) | `facets_of(&[Message]) -> Facets`, the kinds `LANG` and `DEP`. |
| `Scrub`, `NoSecrets`, `scrub_known_shapes` | What removes secrets. A closure `Fn(&str) -> String` is a `Scrub`. |
| `Pseudonyms`, `NoRandomness` | HMAC-SHA256 pseudonyms; `Pseudonyms::new(key)` or `Pseudonyms::random()`. |
| `IMAGE_PLACEHOLDER` | `[image]`, what an image part becomes. |

## The record: `adam.export/v1`

One line per run (wrapped here):

```json
{"schema":"adam.export/v1","id":"9b1d0c6e4f2a8b37c5d1e0f9a6b4c2d8","agent":"coder","status":"done",
 "created_at":"2026-11-27T09:12:03Z","updated_at":"2026-11-27T09:20:41Z","turns":2,"tool_calls":1,
 "usage":{"input_tokens":5120,"output_tokens":310},"models":["main"],"compacted":false,
 "facets":{"dep":["axum"],"lang":["rust"]},"outcome":{"status":"done"},"tools_source":"none",
 "messages":[{"role":"user","content":"Add a /health route."},
  {"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{\"path\":\"src/main.rs\"}"}}]},
  {"role":"tool","tool_call_id":"c1","content":"use axum::Router;\n..."},
  {"role":"assistant","content":"Done: GET /health returns 200."}]}
```

| Field | What |
|---|---|
| `schema` | always `adam.export/v1`. A new field is not a new schema; a changed meaning is |
| `id`, `parent`, `conversation` | pseudonyms of the run, the run it works for (a child) and its conversation; never the store's ids |
| `agent`, `status` | the agent's name; `runnable`, `parked`, `done` or `failed` |
| `created_at`, `updated_at` | RFC 3339 |
| `turns`, `tool_calls`, `usage` | the run's model calls, tool calls and tokens (`adam_model::Usage`) |
| `models` | the model aliases the assistant turns name, sorted, plus both sides of a fallback switch |
| `compacted` | a compaction, a shortening, omitted turns or a summary replaced history: `messages` are not what the model saw at every turn |
| `facets` | `{kind: [values]}`, derived from the transcript (*Facets*) |
| `outcome` | `status`, the scrubbed `error` of a failed run, and the run's `artifacts` by `name` and `mime_type` |
| `tools_source` | `none` (no system message, no tools), `current` (the agent's current ones, passed by the export) or `recorded` (the system prompt and tools the run's last model call was sent, `Conversation::request`, read with `RecordedRequest::load`; `export_runs` uses them when their blobs are still in the store) |
| `messages` | an optional `system`, then `user`, `assistant` (`content`, `tool_calls` as `{id, type: "function", function: {name, arguments}}` with `arguments` a JSON text, optional `reasoning_content`) and `tool` (`tool_call_id`, `content`) |
| `tools` | `{type: "function", function: {name, description, parameters}}`; left out when empty |

An assistant call whose arguments were not valid JSON keeps the model's own text as `arguments`. A
subagent's run is its own record, linked by `parent`. A chat with no tools is the same record with
no `tools`.

## Facets

A facet is a heuristic, not a fact. Both kinds are read from the assistant's tool call arguments;
`dep` also from tool results. **What a person wrote is never read for facets.**

| Kind | Derived from |
|---|---|
| `lang` | the extension of each file a tool call names (`src/main.rs` is `rust`; about forty extensions, `.toml`, `.md`, `.yaml` and `.json` included), and the build tool that starts a command or follows `&&` or `\|` (`cargo` and `rustc` are `rust`, `npm` is `javascript`, ...). A URL is not a file |
| `dep` | the dependency tables of a `Cargo.toml` (`[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, their `[workspace.*]` and `[target.*.*]` forms, and `[dependencies.<name>]`), the `dependencies`, `devDependencies`, `peerDependencies` and `optionalDependencies` of a `package.json`, and the crate of each `use <crate>::` or `extern crate` in Rust text. A file reader's line-number prefix is ignored |

Values are lower case, and a crate's `_` is written `-`, so `use tower_http::` and
`tower-http = "0.6"` are one dependency. `std`, `core`, `alloc`, `crate`, `self`, `super` and
`proc_macro` are not dependencies. Limits: a run that never names a file has no `lang`; a `use` of
one of the project's own modules counts as a `dep`; a manifest the run never read or wrote is not
seen. Facets are read before the text is scrubbed, and are names, never values.

## Filters

| Field | Default | Meaning |
|---|---|---|
| `statuses` | `[Done]` | only these statuses; empty for every one |
| `since`, `until` | none | `updated_at` at or after `since`, before `until` |
| `require` | none | facets every record must have: `("lang", "rust")`, `("dep", "cratestack")` |
| `include_compacted` | `false` | keep runs whose history was compacted |
| `min_turns` | `1` | the fewest model calls |

A record is also written only when it has at least one assistant message. `Filter::default()` is a
training export's: finished runs, not compacted, with at least one turn.
`Filter::parse_requirement` reads `kind=value`, trims both sides and lower-cases the value.

## Privacy

* **Every string passes the `Scrub`** the caller gives (the binaries build it from the redactor
  their tools use), then `scrub_known_shapes`, whatever the `Scrub` is: GitHub tokens, OpenAI- and
  Anthropic-style keys, AWS access key ids, Slack tokens, bearer headers, PEM private keys and the
  password of a URL become `adam_llm_agent::REDACTED_MARKER`, the marker the model already knows.
  Tool definitions' descriptions are cleaned too; their parameter schemas are copied as they are.
* **Ids are pseudonyms**: the first 16 bytes of `HMAC-SHA256(key, kind || 0x00 || id)`, 32 hex
  digits. The same key links two exports; without it nothing links back to the store.
  `Pseudonyms::random()` makes a key per export. `Debug` never shows the key. A tool call's id is
  the model's own and is kept.
* **Never exported**: the conversation's `context` (the caller is a key in it), its
  `prompt_statement` and its session header. Only `messages`, `turns`, `tool_calls`, `usage`,
  `artifacts` (names and media types), `model_switch` and the compaction bookkeeping are read.
* **Images** become `[image]`: a user's or assistant's image part, and one line per image of a
  tool result. No image bytes are exported.
* **Reasoning** is exported only with `include_reasoning(true)`, and only when the run kept it.

The messages a person wrote are exported: which threads a deployment may train on is the
operator's call.

## The driver and its limits

`export_runs` lists runs with `Store::list_runs` per agent, newest first, by pages of 100, with the
filter's statuses and `since` pushed into the query (`until` and the rest are checked here). Each
run is joined with `Runtime::state`, so **each agent must be registered on the runtime** (its
`join` reads a run's transcript). A state that does not decode as a `Conversation` with messages
counts as `not_a_conversation` (another kind of agent, or a run that never started). A run whose
state cannot be read is logged with its id and counted as `unreadable`; the export goes on. A
failed listing or write stops it.

**Only runs that have a conversation id are listed.** The query's scope is
`ConversationScope::Prefix("")`, and `ConversationScope::contains` is `false` for a run with no
conversation id. A child started with `Runtime::start_child` has none ("an ordinary run: it has no
conversation"), so **a subagent's run without a conversation is not reached**; child sessions and
a session's timer runs, which have one, are.

## Environment

None: the crate reads no variable. The binaries read `EXPORT_PSEUDONYM_KEY` for the key of
`Pseudonyms::new` ([environment variables](../../docs/reference/environment.md)).

## Tests

`cargo test -p adam-export` runs the unit tests:

* facets: `a_path_and_a_command_say_the_language`, `a_cargo_manifest_names_its_dependencies`,
  `a_use_line_names_its_crate`, `a_package_json_names_its_dependencies`,
  `what_a_person_wrote_is_not_read`;
* filter: `a_requirement_is_kind_equals_value`, `the_default_keeps_finished_runs_only`;
* pseudonyms: `the_mac_is_hmac_sha256` and `a_long_key_is_hashed_first` (RFC 4231 cases 2 and 6),
  `a_pseudonym_depends_on_the_key_and_the_kind`;
* scrub: `every_known_shape_is_removed`, `a_url_keeps_everything_but_its_password`,
  `ordinary_text_is_borrowed`, `a_closure_is_a_scrub`.

The integration tests in `tests/` cover the conversion and the driver end to end. No test needs a
database.

## See also

* [ADR 0085](../../docs/decisions/0085-runs-are-exported-as-training-records.md): the decision and
  its slices.
* [Export runs for training](../../docs/guides/export-runs-for-training.md): the pipeline, from
  collecting runs to evaluating a tuned model.
