# Extensions

What an adam agent's card can list, what is actually served, and where each contract is written. For agents (the test
that proves each row, the code that lists it, and the check that keeps this table whole):
[Extensions: the evidence](../../.agents/docs/reference/extensions.md).

An extension is optional. A client reads the agent's card, activates only the URIs it lists, and a client that does not
know one ignores it, so every extension is removable without breaking plain A2A. How a client activates one, and how the
rest of the server looks from outside: [The A2A server](a2a-server.md). The decision behind the family:
[ADR 0006](../decisions/0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md).

## The table

Seventeen URIs, one row each. **Listed** says when the card carries the URI, for the coder (`adam-coder`) and for a
folder agent (`adam-agent`). **Served** says whether anything in adam acts on it once a client activates it. **Contract**
is the page on [the system's site](https://vymalo.github.io/another-agentic-system/api/extensions), where the
orchestration layer writes what a client sends and reads. The system's pages were checked to exist on its `main` on
2026-11-26 (*verified*, the files of its `docs/api`).

| Extension | URI | Listed on the coder's card | Listed on a folder agent's card | Served | Contract |
|---|---|---|---|---|---|
| A2UI v1.0 | `https://a2ui.org/a2a-extension/a2ui/v1.0` | always | always | yes: surfaces are drawn, and the renderer's offer is read on every turn | A2UI's own specification (not ours); the card entry is in [`ui-catalog-v2`](https://vymalo.github.io/another-agentic-system/api/ui-catalog-v2) |
| UI catalog v2 | `https://agents.vymalo.com/a2a/extensions/ui-catalog/v2` | always | always | yes | [`ui-catalog-v2`](https://vymalo.github.io/another-agentic-system/api/ui-catalog-v2) |
| Thread tools | `https://agents.vymalo.com/a2a/extensions/thread-tools/v1` | always | always | yes, and the contract's `request_upload` tool is called by the coder's `share_file` (see `uploads/v1`) | [`thread-tools-v1`](https://vymalo.github.io/another-agentic-system/api/thread-tools-v1) |
| Mentions | `https://agents.vymalo.com/a2a/extensions/mentions/v1` | always | always | yes: the mentioned agents are the run's context, and `ask_agent` asks them | [`mentions-v1`](https://vymalo.github.io/another-agentic-system/api/mentions-v1) |
| Steer | `https://agents.vymalo.com/a2a/extensions/steer/v1` | always | always | yes | [`steer-v1`](https://vymalo.github.io/another-agentic-system/api/steer-v1) |
| Steps | `https://agents.vymalo.com/a2a/extensions/steps/v1` | always | always | yes | [`steps-v1`](https://vymalo.github.io/another-agentic-system/api/steps-v1) |
| Text stream | `https://agents.vymalo.com/a2a/extensions/text-stream/v1` | always | always | yes, the reply and the model's reasoning | [`text-stream-v1`](https://vymalo.github.io/another-agentic-system/api/text-stream-v1) |
| Usage | `https://agents.vymalo.com/a2a/extensions/usage/v1` | always | always | yes: a report per model call, and the totals on the task | [`usage-v1`](https://vymalo.github.io/another-agentic-system/api/usage-v1) |
| Build | `https://agents.vymalo.com/a2a/extensions/build/v1` | always | always | yes: information only, there is nothing to activate | no contract page yet; adam's side is [ADR 0028](../decisions/0028-the-card-says-which-build-answers.md) |
| Access tier | `https://agents.vymalo.com/a2a/extensions/access-tier/v1` | always, the tiers up to `MAX_ACCESS_TIER` | never | yes, on the coder | [`access-tier-v1`](https://vymalo.github.io/another-agentic-system/api/access-tier-v1) |
| Commands | `https://agents.vymalo.com/a2a/extensions/commands/v1` | when the agent has any command: a file in `commands/`, or a prompt of one of its MCP servers | the same | yes | [`commands-v1`](https://vymalo.github.io/another-agentic-system/api/commands-v1) |
| Child sessions | `https://agents.vymalo.com/a2a/extensions/child-sessions/v1` | when the root agent has a subagent, local or remote | the same | yes | [`child-sessions-v1`](https://vymalo.github.io/another-agentic-system/api/child-sessions-v1) |
| Locale | `https://agents.vymalo.com/a2a/extensions/locale/v1` | when `DEFAULT_LOCALE` is set | the same | yes | [`locale-v1`](https://vymalo.github.io/another-agentic-system/api/locale-v1) |
| Reasoning | `https://agents.vymalo.com/a2a/extensions/reasoning/v1` | when the deployment profiles the model (`MODEL_ALIASES_FILE`) and the model takes a level | the same | yes: a named level reaches every model call; the echo of the effort on the task is not built yet | no page of its own; the system describes how it reads it in [`model-pick-v1`](https://vymalo.github.io/another-agentic-system/api/model-pick-v1), and adam's side is [ADR 0063](../decisions/0063-a-reasoning-effort-is-one-setting.md) |
| Model pick | `https://agents.vymalo.com/a2a/extensions/model-pick/v1` | when the deployment offers a model besides the agent's own | the same | yes | [`model-pick-v1`](https://vymalo.github.io/another-agentic-system/api/model-pick-v1) |
| Prompt statement | `https://agents.vymalo.com/a2a/extensions/prompt-statement/v1` | with `MODEL_AUTH=injector` or a turn signing key | the same | yes, on adam's side: the headers go on every model call | no contract page yet; adam's side is [ADR 0077](../decisions/0077-a-model-call-carries-the-prompt-statement-and-the-persons-envelope.md) |
| Uploads | `https://agents.vymalo.com/a2a/extensions/uploads/v1` | always | always | **partly**: the coder's `share_file` sends a file over the inline threshold by reference; a folder agent has no tool that does, and `files: true` results from MCP servers still go inline | [`uploads-v1`](https://vymalo.github.io/another-agentic-system/api/uploads-v1) |

## What to read from it

* **A listed URI is not a promise, in one case, and only for part of it.** `uploads/v1` is on every card. The coder's
  `share_file` serves it: a file over the inline threshold (the folder's `limits.max_inline_upload_bytes`, else
  `UPLOADS_INLINE_MAX_BYTES`, else 10 MiB, and never more than the 4 MiB an artifact holds) goes to the bucket by
  reference, but only when a message of the conversation activated the extension and the run holds a thread-tools grant;
  otherwise it is refused, never inlined. **A folder agent (`adam-agent`) has no such tool**, and a file an MCP server
  returns with `files: true` still goes inline, so a client that sends a large file through them gets no help: the rest of
  the gap tracked as [vymalo/another-adam-rs#282](https://github.com/vymalo/another-adam-rs/issues/282) and row 10 of
  [Parity](parity.md). The orchestration layer's side is built; the end-to-end through it is not (it is proven here on a
  fake thread-tools endpoint and a mock bucket).
* **A conditional row is listed exactly when it is served.** The process adds the entry when its own configuration makes
  it true (a locale, a profiled model, a subagent, a signed model call), and a card that already lists the URI keeps its
  own entry. A deployment that sets none of them lists none of them.
* **The coder lists one URI the folder agent does not**: `access-tier/v1`, because only the coder works under a tier.
* **Two URIs have no contract page on the system's site yet**: `build/v1` and `prompt-statement/v1`. Their contracts are
  adam's own ADRs, linked in the table. The system's index of extensions says the orchestrator reads `build/v1` and does
  not yet send `prompt-statement/v1` (its own words on 2026-11-26, *unverified* here: that is its code, not ours).
* **Reasoning has no page of its own** on either side: adam's ADR 0063 is its contract and the system's `model-pick-v1`
  says how a client uses it beside the model pick.
* **A third-party fact**: A2UI v1.0 is the A2UI project's specification, not an adam contract. What adam lists and reads
  is described in [ADR 0056](../decisions/0056-a2ui-v1-0-only-a-hard-cutover.md); the specification itself is *unverified*
  here.

## This table is held whole

A check in `tools/docs-check` reads every extension URI the card builder defines in
`crates/adam-a2a/src/extensions.rs` and fails when this page lacks one, naming the URI. Adding an extension to the card
means adding its row here in the same change. How the check works:
[the evidence page](../../.agents/docs/reference/extensions.md#the-check).
