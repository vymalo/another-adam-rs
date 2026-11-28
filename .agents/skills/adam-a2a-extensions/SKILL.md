---
name: adam-a2a-extensions
description: "Which A2A extensions an adam-rs agent declares and supports (A2UI v1.0, ui-catalog/v2, thread-tools/v1, steps/v1, text-stream/v1, mentions/v1, steer/v1, build/v1, usage/v1, access-tier/v1, commands/v1, child-sessions/v1, locale/v1, reasoning/v1, model-pick/v1, agent-mode/v1, prompt-statement/v1, uploads/v1), how a client activates them, and how to add or announce one. Use when writing a client or an orchestrator for adam agents, when an agent card lacks an extension, or when adding an extension to adam-rs."
---

# A2A extensions of an adam agent

An adam agent is a plain A2A 1.0 agent. On top of that it can declare eighteen optional extensions (and one more, `network-tier/v1`, is decided but built nowhere)
on its card. A client that does not know one ignores it; every extension is removable without
breaking plain A2A. The contracts are written in `vymalo/another-agentic-system`
(its docs/api directory, the `*-v1.md` files); adam-rs holds the agent side.

Every adam-rs path below is in `vymalo/another-adam-rs` at the revision you pin (replace `main`
by that revision). The source of the list is
https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-a2a/src/extensions.rs. Contract
files: https://github.com/vymalo/another-agentic-system/blob/main/docs/api/steer-v1.md and its
siblings (`ui-catalog-v2.md`, `thread-tools-v1.md`, `steps-v1.md`, `text-stream-v1.md`,
`mentions-v1.md`, `usage-v1.md`, `commands-v1.md`; each also on the system's site, for example
https://vymalo.github.io/another-agentic-system/api/usage-v1 and https://vymalo.github.io/another-agentic-system/api/commands-v1; *verified* to exist on its main on 2026-11-26 for `usage-v1` and `commands-v1`).

## When to use

* You write a client or orchestrator that talks to adam agents and must know what to activate.
* An adam agent's card does not list an extension you expect (which crate declares it?).
* You add an extension, or let an agent that is not built on `adam-ui` announce one.
* Not for the A2A protocol itself or for the agent's folder format (`adam-agent-folder`).

## Procedure

**Read the table, then act.** The constants are in `crates/adam-a2a/src/extensions.rs`:

| Extension | Constant | What it is | Declared by |
|---|---|---|---|
| A2UI v1.0 | `A2UI_EXTENSION_V1_0` (and `A2UI_BASIC_CATALOG_V1_0`, `ExtensionConfig::a2ui_v1_0()`) | the agent sends A2UI surfaces (data parts of `application/a2ui+json`, `A2UI_MEDIA_TYPE`) and receives the renderer's capabilities and actions (ADR 0056, A2UI v1.0 only). **A client author: every message must carry `a2uiRendererCapabilities["1.0"]` listing the catalog** (`supportedCatalogIds` or `inlineCatalogs`, a `catalogId` of at most 256 bytes), **or adam answers that turn in words**: the offer is per turn, an offer in an earlier message does not carry over, and an action answering a form is a message too | `adam_ui::card_extensions()` |
| `ui-catalog/v2` | `UI_CATALOG_EXTENSION` | the agent reads the screen's own component catalog and draws with it | `adam_ui::card_extensions()` |
| `thread-tools/v1` | `THREAD_TOOLS_EXTENSION` | the agent calls the per-thread tool endpoint a message announces; the `caller` claim of the grant's token (`main`, or `ask:<n>` for an agent another agent asked) is read into the context key `vymalo.caller` (`adam_a2a_runtime::asked_by_an_agent`), and an asked `adam-coder` asks nobody for consent: its consent tools return an error result instead ([ADR 0036](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0036-the-coder-keeps-to-the-access-tier-the-person-picks.md#amended-2026-10-17-an-asked-agent), *Amended 2026-10-17 (an asked agent)*). Mint the claims as the contract says: a grant whose token cannot be read counts as no person. On an asked agent's message the grant also carries `person` `{said, omitted?}`, what the person wrote in the thread ([the contract's "The `person` member"](https://github.com/vymalo/another-agentic-system/blob/main/docs/api/thread-tools-v1.md#the-person-member); vymalo/another-agentic-system#271, on its main once it merges), read into the context key `vymalo.person` (`adam_a2a_runtime::person_said`, `crates/adam-a2a-runtime/src/person.rs`; `null` for a grant without a well-formed one): an asked `adam-coder` names only the repositories those words name, never the ask's text, and names nothing without them ([ADR 0036, *Built 2026-10-17*](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0036-the-coder-keeps-to-the-access-tier-the-person-picks.md#amended-2026-10-17-an-asked-agent)) | `adam_ui::card_extensions()` |
| `mentions/v1` | `MENTIONS_EXTENSION` | the agent reads the agents a person mentioned and asks them | `adam_ui::card_extensions()` |
| `steer/v1` | `STEER_EXTENSION` | a message naming a `submitted` or `working` task is added to its input and read at its next step | `adam_ui::card_extensions()` |
| `steps/v1` | `STEPS_EXTENSION` | tool calls and sub-agents' work as nested steps | `adam-agent` and `adam-coder` add it (`ExtensionConfig::steps()`) |
| `build/v1` | `BUILD_EXTENSION` | information only, nothing to activate: `params` `{revision, folderDigest}` say which build and which agent files answer (ADR 0028) | `adam-agent` (`card_of_folder`) and `adam-coder` (`agent_card_from`) add it (`ExtensionConfig::build(..)`) |
| `text-stream/v1` | `TEXT_STREAM_EXTENSION` | the reply streamed as the model writes it, then the whole text once; since 2026-10-05 also the model's **reasoning**, as chunks with `kind: "reasoning"` (`TEXT_STREAM_KIND_REASONING`), its own `artifactId`, never stated whole (ADR 0020) | `adam-agent` and `adam-coder` add it (`ExtensionConfig::text_stream()`) |
| `usage/v1` | `USAGE_EXTENSION` | the tokens of each model call, to a client that activated it: a `working` status update with **no message** and the report in the **event's** `metadata` (`call`, `stepId` for a subagent's call, `provider`, `model`, the AG-UI `TokenUsage` counts, `contextWindow`); and, for every reader, the task's `totals` in the **task's** `metadata` once it ends or waits (ADR 0032), with a `sandbox` member beside them when the run's commands ran in a run pod (`{"environment": "kubernetes", "usage": [{"cpuMillis", "memoryBytes", "commands", "milliseconds"}]}`; `{"environment": "local"}` with no `usage` means *not measured*, never zero, and a report of an older coder may say `"devcontainer"`, which reads as `local` (ADR 0052); `totals` is always present when the object is, ADR 0043), and a `shortenings` member (`{"count", "charsRemoved"}`) when the run's history was shortened to fit its budget (ADR 0090) | `adam-agent` and `adam-coder` add it (`ExtensionConfig::usage()`) |
| `access-tier/v1` | `ACCESS_TIER_EXTENSION` | the person picks, per message, how much the agent may do alone (`plan`, `edits`, `auto`, `bypass`): an activated message carries `metadata[URI] = {"tier": ...}`, a missing, unknown or not honoured tier is `InvalidParams` before anything is written, the newest message sets the tier from the agent's next step, a new task starts at the card's `default` (the coder drops the key; the backend itself carries none), the tier survives a large context, and the task says the tier it works under in its `metadata`; `params` are `{tiers, default}`, the tiers up to the deployment's `MAX_ACCESS_TIER` (`AccessTier`, `AccessTiers::up_to`, ADR 0036) | `adam-coder` (`ExtensionConfig::access_tier`); `adam-agent` does not list it |
| `agent-mode/v1` | `AGENT_MODE_EXTENSION` | the mode a run works in, per message (ADR 0039, decision 3): the card lists the folder's primary modes, `params.modes` (`{name, title, description}`, in the order declared) and `params.default`, and an activated message names one, `metadata[URI] = {"mode": "<name>"}` (`MODE_MEMBER`). A missing or unknown name is refused with a sentence whose first word is the code `mode_not_offered` (`MODE_NOT_OFFERED`; the runtime turns it into `InvalidParams` before anything is written), and a message that names none works in the folder's default. **The mode holds until a later message names another**, and it **only narrows**: it is a second `withhold` beside the access tier's, so a tool it lists is still held back by a tier that is too low and it can neither raise the tier nor grant a tool the agent does not have; a mode whose file names no `tools:` narrows nothing. Its words are added to the instructions while it is in force, and the task does **not** echo the mode back. The contract is the orchestration layer's (its docs/api/agent-mode-v1.md, https://github.com/vymalo/another-agentic-system/blob/main/docs/api/agent-mode-v1.md; ANR-979) | `adam-agent` and `adam-coder` list it when the root's folder declares `modes/` (`ExtensionConfig::agent_mode`, via `adam_service::serve`); an agent with no mode lists nothing and works as before |
| `locale/v1` | `LOCALE_EXTENSION` | the person's interface language, per message: an activated message may carry `metadata[URI] = {"locale": "<BCP 47 tag>"}`, read **only** when the request activated the extension (`Locale::of_request`), well formed per RFC 5646's `langtag` and at most 64 bytes, a malformed tag refused so the message goes on with no hint, and a message that names none carrying nothing at all; `params` are `{locale: "<tag>"}`, the agent's default language when a message names none — the deployment's own tag, never this repository's (`Locale`, `ExtensionConfig::locale`, ADR 0069). The hint is a prompt instruction for the agent's own words, never a translation layer, and it never fills a string a person must trust (a consent, a payment: reviewed static text of the deployment) | `adam-agent` and `adam-coder` list it whenever `DEFAULT_LOCALE` is set (`adam:crates/adam-service/src/serve.rs`, `ExtensionConfig::locale`); a deployment that sets none lists nothing. The system's dev stack sets it for the `chat` agent |
| `network-tier/v1` | (none yet) | the network tier a message picks, and the agent keeps (sys ADR 0062, adam [ADR 0085](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0085-an-agent-keeps-the-network-tier-a-message-names.md)): `offline` < `allow-list` < `open`, a closed list; the card lists `params {tiers, default}` and **only what the deployment enforces**; an activated message carries `metadata[URI] = {"tier": …}`, a tier the card does not list is `InvalidParams`, and the task echoes the tier the run worked under. **Decided, nothing serves it yet** (ANR-907): no card lists it, and a client that needs it reads the card, not this row | nobody lists it yet: the slices are ANR-907 |
| `reasoning/v1` | `REASONING_EXTENSION` | the reasoning effort of the model, per message (ADR 0063, decision 5): the card lists the **root** model's levels, `params` `{efforts: [<lowest first>], default?: <the resolved level>}` (`ReasoningEfforts`, `ExtensionConfig::reasoning`; a model with no profile or `reasoningEfforts: []` lists nothing), and an activated message **must** carry `metadata[URI] = {"effort": "<one of them>"}` — a missing entry, a non-object, a missing or non-string `effort`, a word outside the seven, or a level the model does not take gets the refusal sentence naming the agent and its levels (other members ignored; the runtime turns it into `InvalidParams` before anything is written). The level becomes `vymalo.reasoningEffort` (`adam_llm_agent::REASONING_EFFORT_CONTEXT_KEY`, one of `PROTECTED_CONTEXT_KEYS`, read on every turn, carried into a continuing task and a child run, a new thread starting at the resolved default), written by `RuntimeTaskBackend::with_reasoning_efforts` (`CONTEXT_REASONING_EFFORT`: the newest message wins) and checked against the agent's own levels (`ReasoningEfforts`, keyed by the registration name); the task's echo of the effort and a subagent's `subagent_start` check are **planned**. `ReasoningEffort` is `adam_model`'s, one closed list of seven words, re-exported by `adam-a2a` | `adam-coder` and `adam-agent` when the deployment profiles their model (`MODEL_ALIASES_FILE`): `ReasoningEfforts::from_models` builds the levels and the resolved default, the backend refuses one the model does not take, and `serve` adds the entry with `adam_service::card_with_reasoning` (a model with no profile, or `reasoningEfforts: []`, lists nothing; a split control plane lists none) |
| `model-pick/v1` | `MODEL_PICK_EXTENSION` | the model of the run, per message (ADR 0064, amended 2026-11-23): the card lists the aliases the deployment offers — the profiles of `MODEL_ALIASES_FILE` minus the agent's own model, in order, each once — as `params` `{choices: [{"id": "<alias>", "endpoint"?: "<label>"}, ..]}` (`ModelChoices::from_aliases`, or `from_profiles` with the profiles so each choice carries the alias's **endpoint label** from the profile's `endpoint`, ANR-623; `ExtensionConfig::model_pick`; a deployment with one model lists nothing), and an activated message **must** carry `metadata[URI] = {"model": "<one of them>"}`. A missing entry, a non-object, a missing, blank or non-string `model`, or an alias outside the choices is a sentence that begins with the code `model_not_offered` and names the choices (other members ignored; the runtime turns it into `InvalidParams` before anything is written). The alias is the run's context key `vymalo.model` (`{"model": ..}`, `CONTEXT_MODEL`), read on every model turn and honoured only for a binding the agent was given: the assembly gives a served agent its choices (`BoundDef::allow_model_choice`). **The pick is not an authorisation**: the call goes through the deployment's gateway as any other, so `model_locked` still refuses it (ADR 0067) and the fallback switch applies; caller identity is unchanged. `RuntimeTaskBackend::with_model_choices` serves it; `adam-agent` and `adam-coder` list it when the root's choices are non-empty, in `ROLE=all` (a split control plane assembles no agent and lists none yet). The consumer contract is the system's ADR 0088 (https://github.com/vymalo/another-agentic-system/blob/main/docs/api/model-pick-v1.md) | `adam-agent` and `adam-coder` (`ExtensionConfig::model_pick`, via `adam_service::serve`) when the deployment offers more than one model |
| `prompt-statement/v1` | `PROMPT_STATEMENT_EXTENSION` | the prompt statement and the person's envelope, per message (ADR 0077, https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0077-a-model-call-carries-the-prompt-statement-and-the-persons-envelope.md; the URI is proposed by the shared prompt-signature contract): an activated message may carry `metadata[URI] = {"statement"?: "<JWS>", "user"?: "<JWS>"}`, at least one, each a compact JWS of at most 8 KiB (a device sends `user` alone; `statement_of_request`; a malformed entry is `InvalidParams` naming the member, never its value). The run keeps both in its state, a later message of the task that brings new ones replaces them and one without keeps them, local subagents and session rounds start with their parent's; `MODEL_AUTH=injector`, or a turn key, sends `Prompt-Statement` and `Prompt-User-Signature` (each absent when its value is) on every model call. With a turn key (`TURN_SIGNING_KEY_FILE`, or `adam_model::SignedRequests` in a host) every model call also carries `Turn-Signature` (`adam-request/v1`, ADR 0068 amended 2026-11-25, https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md), whose `prompt_jti` is the statement's `jti` or the envelope's `nonce`, and `X-Run-Id`. No `params` | `adam-agent` and `adam-coder` (`ExtensionConfig::prompt_statement`, via `adam_service::serve`) with `MODEL_AUTH=injector` or `TURN_SIGNING_KEY_FILE`; `adam-engine` through `Say::with_prompt_statement`; a split control plane whose chart renders no `MODEL_AUTH` lists none |
| `uploads/v1` | `UPLOADS_EXTENSION` | the orchestration layer's large-file upload: an activated message carries the orchestrator's limits (`{"maxBytes", "ttlSecs"}`) and the agent may ask the thread tools for a presigned `PUT` (`request_upload`) for a file too large for a message; the card entry has **no `params`**, and the inline threshold (`UPLOADS_INLINE_MAX_BYTES`, `limits.max_inline_upload_bytes`) is the agent's own and never on the card. The contract is the system's (its docs/api/uploads-v1.md, https://github.com/vymalo/another-agentic-system/blob/main/docs/api/uploads-v1.md) | listed on every card by `adam_service::serve` (`ExtensionConfig::uploads()`), **served in part**: the coder's `share_file` sends a file over the threshold (`adam_service::inline_upload_threshold`) by reference, when a message activated the extension and the run has a thread-tools grant; a folder agent has no such tool and a `files: true` MCP result stays inline (parity gap 10) |
| `commands/v1` | `COMMANDS_EXTENSION` | the agent serves prompt templates (`commands/<name>.md` in its folder) and lists them on the card only when it has any: `params` are `{commands: [{name, description, argumentHint?}]}` sorted by name (`CommandInfo`, `ExtensionConfig::commands`). A message names one in its metadata, `{"command": "review", "arguments": "acme/app"}` (read only when the request activated the extension; an unknown name is `InvalidParams` before anything is written), or as `/review acme/app` at the start of its text (always read; a `/word` that is no command stays plain text and `//review` is a literal `/review`). The agent expands it in `submit` (`Commands`, `RuntimeTaskBackend::with_commands`) with `$ARGUMENTS`, `$1` to `$9` and `$$` (a literal `$`), once, never for a message that carries a form's answer, and stores the expansion, so a replay never expands again. Arguments at most 4 KiB, the expansion at most 32 KiB. The prompts of the agent's MCP servers are commands too, named `<server>__<prompt>` and listed on the card with the others; their text is asked of the server when the message arrives, behind one line that names the prompt and the server, and stored like a template's expansion; what is typed after the name is the prompt's arguments, the last declared one taking the rest of the line. Only a process that connects the servers lists them: the front of `topology: split` lists none and refuses `/<server>__<prompt>` with `InvalidParams`. The consumer's contract page is `commands-v1` (https://vymalo.github.io/another-agentic-system/api/commands-v1); the agent side is [`docs/reference/a2a-server.md`](https://github.com/vymalo/another-adam-rs/blob/main/docs/reference/a2a-server.md#commands-commandsv1) | `adam-coder`, `adam-agent` (from the folder's `commands/`) |
| `child-sessions/v1` | `CHILD_SESSIONS_EXTENSION` | the agent runs its helpers as sessions a client can open: the card lists `params = {agents: [{name, description}], maxOpen}` (`ChildSessionInfo`, `ExtensionConfig::child_sessions`, `None` for no helper or `maxOpen` 0; `ChildSessions::card_extension` in `crates/adam-a2a-runtime/src/child_sessions.rs` builds it from the roster). A client that activates it (header, and `message.extensions` on a message) reads each task of the parent context with `metadata[URI].sessions`, follows each round as a task of a **child context** (`<parent context, escaped>/s/<session>`, opaque to the client), starts a session with `metadata[URI].start {agent, description, mode, config?}` on a message to the parent context (no turn of the parent; `mode` is `one_shot`, the default, or `open`; `sealed` is refused for a person's start; the `keepOpen` of before is read for one release, `true` is `open`, and beside a `mode` that disagrees is `InvalidParams`; `config` is optional and **only narrows** what the helper's folder and the deployment allow: eight whole numbers, `maxTurns`, `maxToolCalls`, `maxOutputTokens`, `contextWindow`, `maxParallelToolCalls`, `timeLimit`, `maxRounds` and `tokenBudget` (1,000 to 2^53 − 1, a cap on the whole session's tokens), and two lists of tool names or `*` patterns, `tools` (allow-list) and `denyTools` (applied after it; `github__*` selects an MCP server; `read_output` stays), at most 32 entries each; an unknown member, a value above its bound, and on a remote helper anything but `timeLimit` and `maxRounds` are `InvalidParams` naming `start.config.<member>` and nothing starts. A control plane checks the numbers as a worker does but knows no tool names, so it checks the shape of the two lists only; the worker also refuses a concrete `denyTools` name no tool has, and — both shipped agents declare their whole set, the thread-tools source naming the shape `*__*` of a relayed server's tools — a `tools` entry that selects nothing (ANR-1390). Without `config` the folder's own settings apply; the session object carries the admitted `config`. the card does not yet list the options of each helper, and an agent from before `config` ignores unknown `start` members), talks to one with a message to the child context, stops a round with `CancelTask` and closes with `metadata[URI].close {cancel}`; a session that spent its `tokenBudget` refuses a message as a closed one does. **Every session object says its `mode`** (`one_shot`, `sealed` or `open`, ADR 0089 slice C2, additive; `keepOpen` beside it is what the mode means): **the person may write only to an `open` session, and to one they started**. A message to the child context or a steer of a round of an agent's `one_shot` or `sealed` session is `UnsupportedOperation` naming the mode; a close is always allowed. Read `mode` before showing a box to type in. Without activation a child context or round is `InvalidParams` and a task carries nothing of it. **A round of an `open` session may ask the person** (ADR 0045 decision 9), and only such a round: its task is `input-required` with the question, the session's round event reports `blocked`, and the person's answer is a plain message in the child context (or a follow-up to the round's task), which the parked round reads as the asking tool's result; a helper of a session the client does not show is told that nobody can answer instead of parking (`adam_llm_agent::NOT_ASKABLE`), and so is a one-shot subagent. A person's round is its own parent (`by: person`); the parent's stream says each change of a session as a status update with `metadata[URI].event` (`opened`, `round`, `reported` with `report` `done`, `failed`, `canceled` or `blocked`, `closed`, each with `mode`; `round.state` in the A2A JSON spelling), and a round's stream carries the session object on the update that ends the round and when `closing` turns true. `RuntimeTaskBackend::with_child_sessions` serves it; `adam_service::serve` lists it on the card exactly when the backend accepted the helpers (`Agents::child_sessions`, from `AgentDef::child_sessions`). The contract is https://github.com/vymalo/another-agentic-system/blob/main/docs/api/child-sessions-v1.md; the agent side is [`docs/reference/a2a-server.md`](https://github.com/vymalo/another-adam-rs/blob/main/docs/reference/a2a-server.md#child-sessions-child-sessionsv1) | `adam-agent` and `adam-coder` list it when the root has a subagent (local or remote); `maxOpen` is the effective `SESSION_MAX_OPEN` the root's file may lower |

The URIs are `https://agents.vymalo.com/a2a/extensions/<name>/v1` (`ui-catalog` is `v2`; A2UI has its own URI, `https://a2ui.org/a2a-extension/a2ui/v1.0`);
read the exact strings in the file at your rev.

**Pictures are not an extension.** A message may carry a picture as a standard A2A `raw` part (a PNG, JPEG, GIF
or WebP, at most 4 and 2 MiB a message; any other file or a `url` part is one line of the text, and a link is
never fetched). Whether the model is sent it is the deployment's word (`MODEL_INPUT_MODALITIES=text,image`, off
by default); otherwise it is a line of text. `docs/reference/a2a-server.md` ("Images in") at the pinned revision,
`https://github.com/vymalo/another-adam-rs/blob/main/docs/reference/a2a-server.md`.

1. **Activation (client side).** Name the extension URIs you want in the `A2A-Extensions`
   request header (comma-separated) and, in a message you send, in `message.extensions`
   (`crates/adam-a2a/README.md`, "Extensions a request activates"). The handler activates only the
   URIs the card declares, exactly as written (another version, a trailing slash or another case
   is not activated), and lists what it activated in the response's `A2A-Extensions` header. Read
   the card first and send only what it declares. Every method (poll, cancel, resubscribe) carries
   its own header.
2. **What the agent does** per extension: `adam-ui` (`crates/adam-ui/README.md`) reads the
   screen's catalog and the thread tools and offers `ask_user`, `show`, `ui_catalog` and the plan tools
   (`plan_work`, `plan_check`, `plan_add`; each call draws the plan as one surface `plan-<12 hex digits of a hash of the
   conversation id>`, the same for the whole conversation, decided from the catalog alone: a `Checklist` when the catalog
   validates one, else a `Column` of `Text`s, else no surface. The plan is **one artifact** (its `artifactId` is the surface
   id; each change is an artifact update with that id and no `append`, which replaces it). The client rule, written by the
   orchestration layer's UI catalog version 8 and *unverified* against the published page, 2026-11-14: a `createSurface`
   for a surface that exists starts it over, and a surface whose id starts with `plan-` (plus at least one character) is a
   plan, drawn from its newest copy (where a screen draws it is the screen's rule), which may hold no form, input or
   button). **`ask_user` has eleven kinds of question** in one argument schema that never changes with the screen: the six
   of version 5 and `confirm`, `number`, `scale`, `date` and `rank`, which need the screen's `Choices` to list them (catalog
   version 7; the agent asks the schema one kind at a time, not the version number). A screen without a kind is asked the
   nearest version 6 form (`confirm` and `scale` as radio buttons, `number`, `date` and `rank` as typed words) or words,
   and the agent checks the answer itself, by the catalog's rules, in its order (`unknown`, `unexpected`, `required`,
   `too_many`, `not_offered`, `grammar`, `below_min`, `above_max`, `off_step`, `not_a_permutation`; decimals exactly, a
   `date` is a day `YYYY-MM-DD`, nothing is trimmed). **An answer that breaks a rule is not used**: the task stays
   `input-required`, a message says which question and rule (`The answer to "budget" ... was not used (above_max): ...`),
   and the same form comes again on a new surface `ask-<call id>.r<n>` with what was right as `default`s. A client
   therefore sees a second `input-required` with a new `surfaceId` and answers on that one; it never needs to repeat
   the old answers. An answer must name the surface that is open, and a message must carry one form action: an answer for an
   earlier surface (`other_surface`) or with several form actions (`several_answers`) is refused the same way. A form is asked
   again at most 5 times; after that the agent stops asking and tells its model. A number, a day or a rank that a screen
   without the kind drew as a text box is judged as the kind (a rank is typed as the numbers of the options in order); words
   typed instead of the form reach the model unchecked. **A question may `show` what it asks about** (catalog version 8):
   `show: [{component, ...}]` in the argument schema (still one schema for every screen), 1 to 4 blocks to a question and
   8 to a form, each one of `Text`, `Image`, `Mermaid`, `Code`, `Cards`, `Callout` or `Chart` (a diff goes in `Code`). The
   agent gives the blocks the ids `q<i>-s<j>`, makes the surface a `Column` holding the form (`ask`) and the loose blocks,
   and the form's questions name them in their own `show` (ids of components of the same surface, each shown once and
   placed nowhere else). Every block is checked on every screen (its required members, types and limits as the catalog of version 9
   has them) and refused naming the member, as is a `Chart` series with other than one value per label and any control or
   format character in a text (a tab and a line break are text). A screen whose `Choices` schema has no `show` (versions 2 to 7)
   is never sent the member: the blocks it can draw go in the `Column` above the form, the rest is said in words in the
   question's text, each question's under a caption that names the question and the numbers of its blocks (the order the model
   listed them), and the model's `help` is left as written. A form asked again shows the same blocks. A client therefore sees the shown components as ordinary components of the form's surface.
   The orchestrator relays an answer and checks none of it. See `crates/adam-ui/README.md` ("The tools")
   and the contract page `ui-catalog-v2.md` in the docs/api directory of `vymalo/another-agentic-system`;
   `adam-a2a-runtime` (`crates/adam-a2a-runtime/README.md`) reads the messages a screen sends
   (`vymalo_inbound`), reports steps, streamed text and tokens ("Usage"), and implements steering ("Steering a
   running task": a message to a finished task is `UnsupportedOperation`, `-32004`; to a working
   task without the activation, `InvalidParams`).
3. **Release channels** (the agent platform's extension) is not implemented by adam-rs: an
   agent can only advertise it, through `AgentCardConfig::with_extension(ExtensionConfig::new(uri))`
   (`crates/adam-a2a/src/card.rs`: "declaring one here only advertises it, the server does not
   implement any extension itself").
4. **Announce extensions on your own agent**: build the card with `AgentCardConfig`, then
   `adam_ui::with_card_extensions(card)` for the five screen extensions, plus
   `.with_extension(adam_a2a::ExtensionConfig::steps())`, `::text_stream()` and `::usage()` when your agent
   reports steps, streams text and reports its tokens (an `LlmAgent` does all three; a hand-written agent reports tokens
   with `RunEvent::Usage` and keeps `usage_totals` in its state, the shape of `adam_runtime::UsageTotals`). Wire `Agents::new(..).inbound(vymalo_inbound)` so a screen's
   messages are read (the "Wiring" section of `crates/adam-ui/README.md`). Declaring an extension
   is a promise: do not declare `steer/v1` for an agent that can lose an accepted message
   (`adam-a2a-runtime` README, "Steering a running task").
5. **Add a new extension to adam-rs** (the contract is first written in the docs/api
   directory of `vymalo/another-agentic-system`): an ADR in `docs/decisions/` (the existing ones:
   `0006` for A2UI, ui-catalog and thread-tools (`0056` moved A2UI to v1.0 only), `0007` for steps and text-stream, `0011` for step
   input and output, `0015` for mentions and long tool calls, `0016` for steer, `0032` for usage, `0063` for reasoning), a constant and an
   `ExtensionConfig` constructor in `crates/adam-a2a/src/extensions.rs`, the card entry
   (`adam_ui::card_extensions()` or the binary's `card_of`), the behaviour in `adam-a2a-runtime`
   or `adam-ui`, and tests (`crates/adam-a2a-runtime/tests/`, `bin/adam-agent/tests/agent.rs`
   asserts the card's extensions). Update the crate READMEs in the same change.

## Verify

* Fetch the agent's card and check the extension URIs it lists against the table above, at the
  rev the agent runs.
* Send a request with the header and check the response's `A2A-Extensions` header lists it.
* In adam-rs: `cargo test -p adam-a2a-runtime --test steer --test steps --test text_stream
  --test usage --test vymalo` and `cargo test -p adam-agent --test agent` (the cases that need PostgreSQL
  skip without `ADAM_TEST_POSTGRES_URL`; `ADAM_TEST_REQUIRE_DB=1` makes a skip a failure).
* `node tools/docs-check/check-docs.mjs` after editing docs.

## Beyond extensions: what the plain A2A 1.0 server supports

Not extensions, so nothing to activate; each optional feature is **off until the deployment turns it on, and the card says
so** (the docs are the exception: on unless turned off). Read the card before you call (`capabilities.pushNotifications`, `capabilities.extendedAgentCard`, `signatures`):

* **Push notifications** (`CreateTaskPushNotificationConfig`, `Get`, `List`, `Delete`, or `configuration.taskPushNotificationConfig`
  in a send): on when the deployment sets `A2A_PUSH_ALLOWED_URLS`; your webhook must be on its list and `https`. A
  notification is a hint, `GetTask` is the truth; delivery is at least once, a state between two polls can be skipped, and
  after a bounded time of failures it stops. `token` and `credentials` are write-only. See `crates/adam-a2a/src/push/mod.rs`
  and `docs/reference/a2a-server.md`.
* **`ListTasks`**: your own tasks only, newest first, by an opaque cursor that works only for the same caller and filters;
  `totalSize` is exact without a status filter and for `completed`, an upper bound otherwise.
* **`GetExtendedAgentCard`**: authenticated callers only, when the agent folder has `card.extended`.
* **Card signatures**: a JWS (ES256 or EdDSA) over the RFC 8785 canonical card, key set at `/.well-known/jwks.json`;
  verify with `adam_a2a::VerifyingKey` (`crates/adam-a2a/src/signing.rs`). Agreement with other SDKs' canonical payloads is
  unverified.

* **Two bindings.** Every method is served over JSON-RPC (`POST /`) and over HTTP+JSON (A2A 1.0 §11:
  `POST /message:send`, `POST /message:stream`, `GET /tasks/{id}`, `GET /tasks`, `POST /tasks/{id}:cancel`,
  `POST /tasks/{id}:subscribe`, `/tasks/{id}/pushNotificationConfigs[/{configId}]`, `GET /extendedAgentCard`), by one
  handler: the caller, the extensions (the same `A2A-Extensions` header and `message.extensions`, echoed the same way), push
  configs and errors behave the same. The card lists `JSONRPC` first and `HTTP+JSON` second, at the same URL; a client that
  takes the first interface keeps JSON-RPC. REST errors are `google.rpc.Status` with an `ErrorInfo` reason
  (`TASK_NOT_FOUND`, ...). Routes: `crates/adam-a2a/src/rest.rs`
  (https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-a2a/src/rest.rs).
* **Swagger UI** at `GET /docs` and the OpenAPI 3.1 document of both bindings at `GET /openapi.json`: public, the same for
  every caller, no secret in it; the calls still need the bearer token (**Authorize** in the page). On unless the
  deployment sets `A2A_DOCS=false`. Streams cannot be watched in Swagger UI; each streaming operation gives its `curl`.
  The document: `crates/adam-a2a/src/openapi.rs`
  (https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-a2a/src/openapi.rs).

Decisions: `docs/decisions/0030-a2a-push-notifications-list-tasks-extended-card-signatures.md`,
`docs/decisions/0031-swagger-ui-and-the-a2a-rest-binding.md`
(https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0031-swagger-ui-and-the-a2a-rest-binding.md).

**The registry** is not an A2A extension either: it is a list of agent cards, `agent-registry/v1`, that a platform
serves so an orchestrator finds its agents. The platform's operator serves one for the `AgentService`s it runs
(the registry crate of vymalo/another-agentic-platform,
https://github.com/vymalo/another-agentic-platform/blob/main/crates/registry/README.md). It lists the
card URL of each agent and nothing of its extensions: read the card for those.

## Pitfalls

* An extension not named by the client is off: `steps/v1` and `text-stream/v1` send plain text
  and the whole reply to a client that did not activate them, and `usage/v1` sends no report (the task's totals are
  there either way).
* A client of `usage/v1` ignores members of the task's usage object it does not know (`sandbox` is one, ADR 0043;
  `shortenings` another, ADR 0090).
* A `usage/v1` report is a status update **without a message**: a client that shows every status's text must skip it. Its
  `call` repeats on a resubscribe (drop a call already seen). Counts arrive as whole floats (`41250.0`). A remote subagent's
  calls report nothing (the coder's own OpenCode calls were the same until ADR 0038 removed them).
* A URI with another case, version or trailing slash does not match: copy it from the card.
* A `text-stream/v1` chunk with `kind: "reasoning"` is the model's reasoning, not the reply: a reader that does not read `kind`
  shows it as a reply, so update the reader before the agents (ADR 0020).
* `steer/v1` without activation on a working task is refused as plain A2A leaves it undefined.
* `AgentCardConfig.extensions` only advertises; the behaviour is in the runtime and the tools.
* The contracts live in another repository: read them at a rev that matches your adam-rs rev.

## See also

* `crates/adam-a2a/README.md`, `crates/adam-a2a-runtime/README.md`, `crates/adam-ui/README.md`,
  `docs/decisions/0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md`,
  `docs/decisions/0056-a2ui-v1-0-only-a-hard-cutover.md`
  (https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0056-a2ui-v1-0-only-a-hard-cutover.md),
  `docs/decisions/0007-progress-as-steps-and-streamed-text.md`,
  `docs/decisions/0016-a-message-sent-to-a-working-task-is-steered-into-it.md`,
  `docs/decisions/0032-usage-per-model-call.md`
  (https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0032-usage-per-model-call.md).
* `adam-agent-folder`, `adam-embed`.
* https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-a2a/src/extensions.rs
