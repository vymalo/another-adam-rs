# 0056. A2UI v1.0 only, a hard cutover

Status: **Accepted** (2026-11-19). The owner's decision (2026-11-19): *"That falls into V1, we absolutely need to
upgrade to a2ui@v1"*: adam moves from A2UI v0.9.1 to v1.0 in R1, the first release, as a hard cutover. *Amended
2026-11-19 (the owner's answers to sys ADR 0075):* the screen's catalog keeps its `catalogId` and its `version`
continues at 9 under `ui-catalog/v2` (decision 6), and a surface of a thread from before the cutover is neither drawn
nor migrated, with actions on it refused (*Consequences*); both are the owner's, marked **(owner)**. Not reading an
action's `userMessage` (decision 4) follows sys ADR 0075's proposed choice. Nothing is
built. It amends [ADR 0006](0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md) (decisions 6 to 8: what adam
writes, reads and declares for A2UI) without reversing it. The other side is the orchestration layer's ADR 0075,
[*A2UI v1.0 cutover*](https://github.com/vymalo/another-agentic-system/blob/main/docs/decisions/0075-a2ui-v1-0-cutover.md),
which also moves `ui-catalog/v1` to `ui-catalog/v2`; the epic is
[vymalo/another-agentic-system#380](https://github.com/vymalo/another-agentic-system/issues/380).

## Context

adam writes A2UI v0.9.1 and declares it on the card:

| What | Today | Where |
|---|---|---|
| Extension URI | `https://a2ui.org/a2a-extension/a2ui/v0.9.1` | `A2UI_EXTENSION_V0_9_1`, `crates/adam-a2a/src/extensions.rs` |
| Basic catalog id | `https://a2ui.org/specification/v0_9_1/catalogs/basic/catalog.json` | `A2UI_BASIC_CATALOG_V0_9_1`, same file |
| Media type | `application/a2ui+json` | `A2UI_MEDIA_TYPE`, same file |
| Envelope `version` | `"v0.9.1"` | `A2UI_VERSION`, `crates/adam-ui/src/surface.rs` |
| Renderer capabilities read | `a2uiClientCapabilities` under `v0.9.1`, then `v0.9`, then `a2uiRendererCapabilities` under `v1.0` | `inline_catalog`, `crates/adam-a2a-runtime/src/vymalo.rs` |
| Action read | `{"version": "v0.9.1", "action": {...}}` | `vymalo_inbound`, same file |
| Card entry | the URI above with `supportedCatalogIds` (the basic catalog) and `acceptsInlineCatalogs: true` | `ExtensionConfig::a2ui_v0_9_1`, `adam_ui::card_extensions` |

A2UI v1.0 is the current version, and v0.9.1 is now "previous"
(*verified 2026-11-19*, <https://a2ui.org/specification/v1.0-a2ui/>: "Version: 1.0 · Status: Current · Last Updated:
Jun 8, 2026"; and `specification/versions.json` of <https://github.com/a2ui-project/a2ui> at `b4f91c2`). v1.1 is only
collecting requirements (*verified 2026-11-19*, that repository's `README.md` at `b4f91c2`). The repository moved to
`a2ui-project/a2ui`; `google/A2UI` serves the same history (*verified 2026-11-19*: `git ls-remote` of both answered
`b4f91c26fcf62abc0720c21a65840220941d0a3f` for `HEAD`). What changes for adam (*verified 2026-11-19*,
<https://a2ui.org/specification/v1.0-evolution-guide/> and
<https://a2ui.org/specification/v1.0-a2ui-extension-specification/>, and the schemas at `b4f91c2`):

* The envelope `version` is **`"1.0"`**, not `"v1.0"`: `agent_to_renderer.json` and `renderer_to_agent.json` hold
  `"const": "1.0"`. The `v1.0` spellings adam reads today (`a2uiRendererCapabilities.v1.0`) were a guess at a draft
  and never matched the release.
* The extension URI is `https://a2ui.org/a2a-extension/a2ui/v1.0`; the card's `params` are the inner object of
  `agent_capabilities.json#/properties/1.0` (`supportedCatalogIds`, `acceptsInlineCatalogs`), never the
  `{"1.0": ...}` wrapper.
* The renderer's capabilities are `message.metadata["a2uiRendererCapabilities"]`, keyed `"1.0"`; data-model sync is
  `a2uiRendererDataModel`. **An agent MUST treat a message that carries no `a2uiRendererCapabilities` entry for its
  version as a turn in which the renderer offers no A2UI, and MUST NOT send A2UI parts in reply**; activating the
  extension alone offers no catalog.
* The basic catalog id is `https://a2ui.org/specification/v1_0/catalogs/basic/catalog.json` (the `catalogId` of
  `catalogs/basic/v1/catalog.json` at `b4f91c2`).
* **The media type does not change for adam.** The guide tells agents to move from `application/json+a2ui` to
  `application/a2ui+json`; adam and the orchestrator already write `application/a2ui+json`, which the v0.9.1
  extension page also names (*verified 2026-11-19*,
  <https://a2ui.org/specification/v0.9.1-a2ui-extension-specification/>).
* A catalog for 1.0 MUST declare `"protocolVersion": "1.0"` (it defaults to `"0.9"`), and a v0.9 catalog cannot be
  used by a 1.0 runtime. The orchestration layer's catalog therefore changes shape, and `ui-catalog/v1` becomes
  `ui-catalog/v2` (sys ADR 0075).
* `createSurface` loses `theme`, may carry `components` and `dataModel` inline, and `catalogId` is optional (then
  every component needs `@catalogId`). Bindings are `{"@path": ...}` and calls `{"@call": ...}`; ChildList templates
  keep a plain `path`; `accessibility` and `checks` become `@accessibility` and `@checks`; `@catalogId` and
  `@metadata` are new. `updateDataModel` without `value` is invalid and `null` deletes. `TextField` requires
  `value`; `openUrl` has `requiresUserActivation: true`.
* New messages: `callRendererFunction` / `rendererFunctionResponse` and `callAgentFunction` /
  `agentFunctionResponse`. The schema files are `agent_to_renderer.json` and `renderer_to_agent.json` (the
  `server_to_client` and `client_to_server` names are gone).

What adam writes today is small: `ask_user`, `show` and the plan tools write components of the screen's own catalog
(literals, never a binding or a function call: the catalog has none) under its `catalogId`, as
`[createSurface, updateComponents]`. So the cutover is mostly spellings, the capability lookup, the catalog's shape and
the card.

## Decision

1. **A2UI v1.0 only, a hard cutover.** adam writes, reads and declares v1.0 and nothing older: no dual support, no
   version negotiation, no `v0.9.1` reader kept for old messages. A build either speaks v0.9.1 (before the cutover)
   or v1.0 (after); the cutover lands on `main` as one merge (decision 9).
2. **The constants** (`adam-a2a`), each a breaking change of the public API on purpose, so that a consumer compiled
   against a v0.9.1 name fails to build rather than declare the wrong version:

   | Removed | Added |
   |---|---|
   | `A2UI_EXTENSION_V0_9_1` | `A2UI_EXTENSION_V1_0 = "https://a2ui.org/a2a-extension/a2ui/v1.0"` |
   | `A2UI_BASIC_CATALOG_V0_9_1` | `A2UI_BASIC_CATALOG_V1_0 = "https://a2ui.org/specification/v1_0/catalogs/basic/catalog.json"` |
   | `ExtensionConfig::a2ui_v0_9_1()` | `ExtensionConfig::a2ui_v1_0()`, the same `params`: `supportedCatalogIds` (the basic id) and `acceptsInlineCatalogs: true` |
   | `UI_CATALOG_EXTENSION` = `…/ui-catalog/v1` | `UI_CATALOG_EXTENSION` = `https://agents.vymalo.com/a2a/extensions/ui-catalog/v2` (sys ADR 0075) |

   `A2UI_MEDIA_TYPE` stays `application/a2ui+json`. `adam_ui::A2UI_VERSION` keeps its name and becomes `"1.0"`.
3. **What adam writes** (`adam-ui`). Every envelope says `"version": "1.0"`. A surface stays
   `[createSurface, updateComponents]` with `catalogId` set on `createSurface`: both are valid v1.0, both sides already
   test that shape, and folding the components into `createSurface` buys nothing for surfaces of a few components (a
   later change may). `createSurface` carries no `theme`, no `dataModel` and no `metadata`; components carry no `@`
   envelope key (adam's components have no accessibility or checks members today; a component that gains one writes
   the `@` spelling). adam never writes a binding, a function call, `updateDataModel`, `callRendererFunction` or
   `agentFunctionResponse` in R1.
4. **What adam reads** (`adam-a2a-runtime`, `vymalo_inbound`). The renderer's capabilities only under
   `a2uiRendererCapabilities["1.0"]`; `a2uiClientCapabilities` and every other key are ignored. An action only as
   `{"version": "1.0", "action": {...}}`; an action of another version is read as any other data part (its JSON as
   text), never as a form's answer. The action's `userMessage` and `metadata` are not read (proposed, as sys ADR 0075 decision 4): the orchestrator never
   forwards them (sys ADR 0075, decision 4), and adam does not take an agent-authored line as the person's words.
5. **A turn with no capabilities draws nothing** (A2UI's MUST). `ask_user`, `show` and the plan tools write a surface
   only when the message that started or resumed the run carried `a2uiRendererCapabilities["1.0"]` and the surface's
   `catalogId` is in its `supportedCatalogIds` or its `inlineCatalogs`; otherwise they answer in words, as they do
   today for a screen with no catalog. A catalog adam holds from an earlier message or from `get_ui_catalog` does not
   on its own allow a surface.
6. **The catalog is a `ui-catalog/v2` document** (`adam-ui`, `Catalog`, `catalog_digest`): `{catalogId,
   protocolVersion, components}` with `protocolVersion` exactly `"1.0"`, required; a document without it, with
   another value, or with any other member is refused, as `CatalogError` already refuses a member it does not know.
   **(owner)** The `catalogId` stays `https://agents.vymalo.com/a2ui/catalogs/chat` and the catalog's `version`
   continues: 9 is the first v2 catalog, so adam needs no second id. The digest is the one `docs/api/ui-catalog-v2.md` of `vymalo/another-agentic-system` defines, with its known-answer
   vector pinned in a test. The degradation paths that read a screen's `Choices` schema stay, since they read the
   schema and not a version number. The vendored v1 catalogs (`crates/adam-ui/tests/fixtures/catalog-v2.json` to
   `catalog-v8.json`) stay byte-for-byte copies: the tests that need them as v2 documents add `protocolVersion` in
   the test, and `catalog-v9.json`, the first v2 catalog, is vendored from that page with its commit in
   `crates/adam-ui/tests/fixtures/UPSTREAM`.
7. **The card** (`adam_ui::card_extensions`, `with_card_extensions`) lists `A2UI_EXTENSION_V1_0` with
   `ExtensionConfig::a2ui_v1_0()` and `ui-catalog/v2`, and never a v0.9.1 entry. `adam-agent` and `adam-coder` take it
   from there and change nothing of their own.
8. **The official schemas are the test oracle.** The v1.0 JSON schemas are vendored into `crates/adam-ui/tests/`
   from <https://github.com/a2ui-project/a2ui> at commit **`b4f91c26fcf62abc0720c21a65840220941d0a3f`**
   (2026-11-18), byte for byte with an `UPSTREAM` file naming the commit and each file's SHA-256 (at that commit,
   `agent_to_renderer.json` is `sha256:2f575f78…823761d`, `renderer_to_agent.json` `sha256:58ecde02…7215465`,
   `common_types.json` `sha256:c4e78305…f51c56`, `catalog_definition.json` `sha256:836a48b2…4d44`): every surface the
   goldens of `crates/adam-ui/tests/golden/` hold validates against `agent_to_renderer.json`, every action fixture
   against `renderer_to_agent.json`, the card's `params` against `agent_capabilities.json#/properties/1.0`, and
   `catalog-v9.json` against `catalog_definition.json`. `tools/check-vendored-catalogs.sh` compares the copies with
   the commit, as it does for the catalogs. The schemas are Apache-2.0 (the repository's `LICENSE`); the notice goes
   in `third-party-notices.md`.
9. **Two slices, one merge.** Slice A1 is `adam-ui` (decisions 3, 5, 6 and 8: the serializer, the catalog and the
   schema tests); slice A2 is the rest (decisions 2, 4 and 7, `adam-agent`, `adam-coder`, the `dev/` mocks and
   scripts that speak A2UI: `dev/agent-cards-e2e.sh`, `dev/coder-choices-e2e.sh`, `dev/coder-e2e.sh`, the docs and
   the skill). Both are pull requests into a branch `a2ui-v1`, which merges into `main` once, so `main` never has an
   agent that writes v1.0 surfaces under a v0.9.1 card. The image that merge builds is the one the orchestration
   layer pins in its last slice (sys ADR 0075).
10. **What changes in the docs and the skill** (slice A2, in the same pull request as the code, Rule 10): the crate
    READMEs of `adam-a2a`, `adam-a2a-runtime` and `adam-ui` (the constants, `A2UI_VERSION`, the capability lookup,
    the catalog's shape, the vendored schemas), `docs/reference/a2a-server.md`, `docs/guides/testing.md` (the scripts'
    A2UI lines) and the skill `adam-a2a-extensions` (its description, the table row and the URI list say A2UI v1.0
    and `ui-catalog/v2`, and it names this ADR). The consumers pick the skill up with `npx skills update`.

## Consequences

* **A mixed deployment degrades to words, never to a broken surface.** A v1.0 adam behind a v0.9.1 orchestrator gets
  no `a2uiRendererCapabilities["1.0"]` (that orchestrator writes `{"v0.9.1": ...}`, or `{"v1.0": ...}` for the
  draft URI) and no `ui-catalog/v2` metadata, so by decision 5 it answers in words. A v0.9.1 adam behind a v1.0
  orchestrator lists no URI that orchestrator detects and gets nothing A2UI. Deploy both at once all the same.
* **A run parked on a v0.9.1 form across the upgrade** is answered in words: **(owner)** a surface from before the
  cutover is not drawn or migrated and the orchestrator refuses an action on it (sys ADR 0075, decisions 4 and 6), and
  words typed instead of a form are read as they are today.
* A consumer that names `A2UI_EXTENSION_V0_9_1`, `A2UI_BASIC_CATALOG_V0_9_1` or `ExtensionConfig::a2ui_v0_9_1` stops
  compiling; `adam-upgrade` names the replacements. No `Store` or `Notifier` method changes.
* The function-call messages are not adam's in R1: nothing it writes needs one, and the screen's catalog declares no
  function. A later ADR adds them when a component needs a renderer's answer.
* Pre-1.0 spellings are gone from adam; a future A2UI version is another reviewed cutover, not a second dialect.

## Alternatives rejected

* **Speak both versions and pick by the card or the capabilities.** Twice the readers, the writers and the tests,
  for a period with no client left on v0.9.1 once the orchestration layer moves. The owner chose the cutover.
* **Keep the constant names and change their values.** A consumer would build and silently declare v1.0 under a
  name that says v0.9.1.
* **Fold every surface into one `createSurface` with inline `components`.** Valid, but a change of shape on both
  sides for no gain in R1 (decision 3).
* **Link the schemas instead of vendoring them.** A test cannot fetch over the network, and a link moves.
