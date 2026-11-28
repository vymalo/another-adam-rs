# The A2A server: evidence to check

For people: [The A2A server](../../../docs/reference/a2a-server.md), which says what a client sees. This page holds the
evidence behind its document section and the procedure a change runs.

## The committed document

`docs/api/a2a-server.openapi.json` is what `openapi::document(&card, flags, signed)` in
`crates/adam-a2a/src/openapi.rs` builds for one fixed card — `bearer: true`, `push: false`, `extended: false`,
`signed: false` — so the file moves only when the code does, never when a deployment's card changes. It is held equal to
the code by a **unit test** in that module (`mod tests`, `the_committed_document_is_the_one_the_code_builds`), not an
integration test: `document` is `pub(crate)` and an integration test cannot call it.

Rewrite the file with the repo's golden convention (`adam-assembly`, `adam-agent-fs` and `adam-plugin` do the same):

```sh
ADAM_UPDATE_GOLDEN=1 cargo test -p adam-a2a --lib the_committed_document_is_the_one_the_code_builds
```

The drift test's failure message names that command. Keys come out sorted because `serde_json`'s `Map` is a `BTreeMap`,
so the file is byte-stable and is reviewed by its diff, not by reading it.

**Proven:** with `info.version` set to `9.9.9`, the test failed with
`docs/api/a2a-server.openapi.json no longer matches crates/adam-a2a/src/openapi.rs; regenerate it with …`; regenerated
afterwards, byte-identical to the first write (vymalo/another-adam-rs#441, squash `f9595722`).

## `start.config` of `child-sessions/v1` (ADR 0054)

For people: [the `start.config` paragraph](../../../docs/reference/a2a-server.md#child-sessions-child-sessionsv1) of the
page above. `parse_config` in `crates/adam-a2a-runtime/src/child_sessions.rs` reads the nine members (seven numbers, and
`tools` and `denyTools` as lists of non-blank strings) and calls the one `SessionBounds::admit` the model's tool calls;
`put_header` writes `vymalo.session.tools` into every round a person starts, **over** the message's own context, and the loop
reads the lists from the round's session header first (`crates/adam-llm-agent/src/session_tools.rs`).

* **What differs between roles is the tool names**, never the numbers. A control plane's bounds (`AgentDef::session_bounds`)
  carry `tools: []`, `tools_complete: false`: shape only. A worker's carry the names of the bound node and of every source
  that declares them (`ToolSource::declared`), and `tools_complete` only when every source of the node does. A declared name
  may be a **shape** (ADR 0054 amended 2026-11-28, ANR-1390): `*__*` stands for a relayed `<server>__<tool>`, and
  `SessionBounds::admit` (`crates/adam-runtime/src/session.rs`, `selects`) admits an entry that selects a concrete declared
  name with its pattern or that a declared shape matches as a string. The coder and the folder agent add the thread-tools
  source, which declares the contract's tools it knows by name and `*__*`, so **their sets are complete**: a `tools` entry
  that selects nothing is refused there, `github__*` is admitted, and a concrete `denyTools` name no tool has is still
  refused (ANR-1319, ANR-1390). A source that answers `Declared::Unknown` still leaves a set incomplete and the check
  shape-only.
* **Tests:** `child_sessions.rs` unit tests `a_start_takes_tools_and_deny_tools_as_lists_of_names`,
  `a_remote_helper_refuses_tools_and_deny_tools`, `a_round_carries_the_tools_its_header_narrows_over_what_the_message_said`;
  `adam-runtime` `session.rs` `an_incomplete_set_refuses_a_concrete_deny_entry_it_does_not_know_and_checks_no_pattern`,
  `a_complete_set_checks_patterns_and_allow_entries_too`, `a_declared_shape_admits_a_relayed_servers_tools`;
  `adam-assembly/tests/session_bounds.rs` `an_agent_with_a_source_has_an_incomplete_set_that_still_catches_a_deny_typo`,
  `a_source_that_declares_a_shape_makes_the_set_complete_and_admits_a_servers_tools`; `bin/adam-agent/tests/agent.rs`
  `a_helpers_bounds_are_complete_and_admit_a_relayed_servers_tools`; `bin/adam-coder/tests/agent_files.rs`
  `the_helpers_bounds_are_complete_and_admit_a_relayed_servers_tools`.
