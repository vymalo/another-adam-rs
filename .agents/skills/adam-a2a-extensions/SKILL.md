---
name: adam-a2a-extensions
description: "Which A2A extensions an adam-rs agent declares and supports (A2UI v0.9.1, ui-catalog/v1, thread-tools/v1, steps/v1, text-stream/v1, mentions/v1, steer/v1), how a client activates them, and how to add or announce one. Use when writing a client or an orchestrator for adam agents, when an agent card lacks an extension, or when adding an extension to adam-rs."
---

# A2A extensions of an adam agent

An adam agent is a plain A2A 1.0 agent. On top of that it can declare seven optional extensions
on its card. A client that does not know one ignores it; every extension is removable without
breaking plain A2A. The contracts are written in `vymalo/another-agentic-system`
(its docs/api directory, the `*-v1.md` files); adam-rs holds the agent side.

Every adam-rs path below is in `vymalo/another-adam-rs` at the revision you pin (replace `main`
by that revision). The source of the list is
https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-a2a/src/extensions.rs. Contract
files: https://github.com/vymalo/another-agentic-system/blob/main/docs/api/steer-v1.md and its
siblings (`ui-catalog-v1.md`, `thread-tools-v1.md`, `steps-v1.md`, `text-stream-v1.md`,
`mentions-v1.md`; verified to exist on its main on 2026-10-03).

## When to use

* You write a client or orchestrator that talks to adam agents and must know what to activate.
* An adam agent's card does not list an extension you expect (which crate declares it?).
* You add an extension, or let an agent that is not built on `adam-ui` announce one.
* Not for the A2A protocol itself or for the agent's folder format (`adam-agent-folder`).

## Procedure

**Read the table, then act.** The constants are in `crates/adam-a2a/src/extensions.rs`:

| Extension | Constant | What it is | Declared by |
|---|---|---|---|
| A2UI v0.9.1 | `A2UI_EXTENSION_V0_9_1` | the agent sends A2UI surfaces (data parts of `application/a2ui+json`, `A2UI_MEDIA_TYPE`) and receives the renderer's capabilities and actions | `adam_ui::card_extensions()` |
| `ui-catalog/v1` | `UI_CATALOG_EXTENSION` | the agent reads the screen's own component catalog and draws with it | `adam_ui::card_extensions()` |
| `thread-tools/v1` | `THREAD_TOOLS_EXTENSION` | the agent calls the per-thread tool endpoint a message announces | `adam_ui::card_extensions()` |
| `mentions/v1` | `MENTIONS_EXTENSION` | the agent reads the agents a person mentioned and asks them | `adam_ui::card_extensions()` |
| `steer/v1` | `STEER_EXTENSION` | a message naming a `submitted` or `working` task is added to its input and read at its next step | `adam_ui::card_extensions()` |
| `steps/v1` | `STEPS_EXTENSION` | tool calls and sub-agents' work as nested steps | `adam-agent` and `adam-coder` add it (`ExtensionConfig::steps()`) |
| `text-stream/v1` | `TEXT_STREAM_EXTENSION` | the reply streamed as the model writes it, then the whole text once | `adam-agent` and `adam-coder` add it (`ExtensionConfig::text_stream()`) |

The URIs are `https://agents.vymalo.com/a2a/extensions/<name>/v1` (A2UI has its own URI);
read the exact strings in the file at your rev.

1. **Activation (client side).** Name the extension URIs you want in the `A2A-Extensions`
   request header (comma-separated) and, in a message you send, in `message.extensions`
   (`crates/adam-a2a/README.md`, "Extensions a request activates"). The handler activates only the
   URIs the card declares, exactly as written (another version, a trailing slash or another case
   is not activated), and lists what it activated in the response's `A2A-Extensions` header. Read
   the card first and send only what it declares. Every method (poll, cancel, resubscribe) carries
   its own header.
2. **What the agent does** per extension: `adam-ui` (`crates/adam-ui/README.md`) reads the
   screen's catalog and the thread tools and offers `ask_user`, `show`, `ui_catalog`;
   `adam-a2a-runtime` (`crates/adam-a2a-runtime/README.md`) reads the messages a screen sends
   (`vymalo_inbound`), reports steps and streamed text, and implements steering ("Steering a
   running task": a message to a finished task is `UnsupportedOperation`, `-32004`; to a working
   task without the activation, `InvalidParams`).
3. **Release channels** (the agent platform's extension) is not implemented by adam-rs: an
   agent can only advertise it, through `AgentCardConfig::with_extension(ExtensionConfig::new(uri))`
   (`crates/adam-a2a/src/card.rs`: "declaring one here only advertises it, the server does not
   implement any extension itself").
4. **Announce extensions on your own agent**: build the card with `AgentCardConfig`, then
   `adam_ui::with_card_extensions(card)` for the five screen extensions, plus
   `.with_extension(adam_a2a::ExtensionConfig::steps())` and `::text_stream()` when your agent
   reports steps and streams text. Wire `Agents::new(..).inbound(vymalo_inbound)` so a screen's
   messages are read (the "Wiring" section of `crates/adam-ui/README.md`). Declaring an extension
   is a promise: do not declare `steer/v1` for an agent that can lose an accepted message
   (`adam-a2a-runtime` README, "Steering a running task").
5. **Add a new extension to adam-rs** (the contract is first written in the docs/api
   directory of `vymalo/another-agentic-system`): an ADR in `docs/decisions/` (the existing ones:
   `0006` for A2UI, ui-catalog and thread-tools, `0007` for steps and text-stream, `0011` for step
   input and output, `0015` for mentions and long tool calls, `0016` for steer), a constant and an
   `ExtensionConfig` constructor in `crates/adam-a2a/src/extensions.rs`, the card entry
   (`adam_ui::card_extensions()` or the binary's `card_of`), the behaviour in `adam-a2a-runtime`
   or `adam-ui`, and tests (`crates/adam-a2a-runtime/tests/`, `bin/adam-agent/tests/agent.rs`
   asserts the card's extensions). Update the crate READMEs in the same change.

## Verify

* Fetch the agent's card and check the extension URIs it lists against the table above, at the
  rev the agent runs.
* Send a request with the header and check the response's `A2A-Extensions` header lists it.
* In adam-rs: `cargo test -p adam-a2a-runtime --test steer --test steps --test text_stream
  --test vymalo` and `cargo test -p adam-agent --test agent` (the cases that need PostgreSQL
  skip without `ADAM_TEST_POSTGRES_URL`; `ADAM_TEST_REQUIRE_DB=1` makes a skip a failure).
* `node tools/docs-check/check-docs.mjs` after editing docs.

## Pitfalls

* An extension not named by the client is off: `steps/v1` and `text-stream/v1` send plain text
  and the whole reply to a client that did not activate them.
* A URI with another case, version or trailing slash does not match: copy it from the card.
* `steer/v1` without activation on a working task is refused as plain A2A leaves it undefined.
* `AgentCardConfig.extensions` only advertises; the behaviour is in the runtime and the tools.
* The contracts live in another repository: read them at a rev that matches your adam-rs rev.

## See also

* `crates/adam-a2a/README.md`, `crates/adam-a2a-runtime/README.md`, `crates/adam-ui/README.md`,
  `docs/decisions/0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md`,
  `docs/decisions/0007-progress-as-steps-and-streamed-text.md`,
  `docs/decisions/0016-a-message-sent-to-a-working-task-is-steered-into-it.md`.
* `adam-agent-folder`, `adam-embed`.
* https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-a2a/src/extensions.rs
