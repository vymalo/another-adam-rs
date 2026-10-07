# 0027. Every tool has a title for its step

Status: **Accepted** (2026-10-07)
Builds on [ADR 0007](0007-progress-as-steps-and-streamed-text.md) (steps and their labels) and
[ADR 0011](0011-a-tool-calls-step-carries-its-input-and-output.md) (an MCP tool's `title` is its step's label).
Amends [ADR 0021](0021-the-coder-is-adam-a-general-agent-that-can-code.md): the label of the OpenCode step.

## Context

The owner exported production threads of Adam (the `adam-coder` binary, image `sha-6478fbc`). The activity panel of the
chat listed its steps as `edit_file`, `run_command`, `read_file`, `write_file`, `start_scratch`, `ask_user`, `run_checks`,
`prepare_workspace`: the names the *model* calls the tools by. ADR 0011 had already fixed this for MCP tools (a server's
`title` is the label of the step: `Web search`, not `search__web_search`), but the default of `Tool::step_style` is the
tool's name, and nothing in the coder, the screen's tools or the assembly's own tools said anything else. The person reads
the panel; the name is an identifier.

## Decision

1. **A tool that a person can see in a step has a title.** Every tool of the coder is `#[tool(label = "...")]`
   (`Prepare the workspace`, `Run a command`, `Read a file`, `Write a file`, `Edit a file`, `Apply a patch`, `Make files with
   a command`, `Run the checks`, `Rebuild the environment`, `Commit and push`, `Open a pull request`, `Start a scratch project`,
   `Publish a scratch project`, `Ask to use a repository`, `Create a repository`, `Share a file`, `Hand to OpenCode`). The
   tools of `adam-ui` (`ask_user` is `Ask you`, `show` is `Show on your screen`, `ui_catalog` is `List what the screen can
   draw`) and `adam-assembly` (`load_skill` is `Load a skill`, `read_skill_file` is `Read a skill file`) say theirs in
   `Tool::step_style`. Titles are short verb phrases in the second person where the person is addressed; the table is in
   `bin/adam-coder/README.md`.
2. **The tool's name does not change.** The model calls tools by name, journals replay by `tool:<call id>`, and the specs
   are pinned by golden files; only the label of the step changes. The plain line a client without `steps/v1` reads
   (`<label>: done`) carries the title too, which is the intent.
3. **A subagent's title is its tool name capitalised**, with `_` and `-` as spaces (`explorer` is `Explorer`,
   `code_reviewer` is `Code reviewer`). The alternative, the first words of its `description`, was rejected: a description is
   free text written for the model ("Reads a repository and reports where things are..."), and its first words are
   rarely a name. The tool name is already the author's short name for the agent, and is unique among the parent's tools.
4. **A source's tool can carry a title too.** `ToolNote` (what a `ToolSource` says about a tool it lists) gains
   `label: Option<String>`, kept in the run's state like the rest of the note (it is absent from a state written before, and
   not written while `None`). `adam_mcp::RemoteTool` gains the endpoint's `title`. `ThreadTools` labels each tool with the
   endpoint's title and, for `turn_output`, which the orchestrator lists without one, with `Send the answer`. The agent uses
   the label for every report of the call, from the first to the last. The tools the orchestrator reports itself (`reportsStep`)
   are unchanged: the agent reports none.
5. **A new tool without a title fails a test.** `every_tool_of_the_coder_has_a_title_for_its_step`
   (`bin/adam-coder/tests/agent_files.rs`) requires a label that is not the name, has no underscore and is unique.
6. **The OpenCode step is `Hand to OpenCode`** (it was `OpenCode`, ADR 0021): the title says what the call does, like the
   others; its kind (`subagent`) and icon (`opencode`) are unchanged.

## Consequences

* Additive API: `ToolNote::label` and `ToolNote::with_label`, `RemoteTool::title`, `ToolNote::is_meaningful` is public,
  `TURN_OUTPUT_TITLE`. `ToolNote` and `RemoteTool` have public fields and no `#[non_exhaustive]`: code that builds either by
  a struct literal outside this repository stops compiling and adds the new field (there is none in-tree: both have
  constructors). No required trait method is added.
* The orchestration layer and the chat read `label` as they did; a label is "a one-line string", so nothing there changes.
  The dev fixtures and e2e scripts that matched the plain line of the OpenCode step read `Hand to OpenCode: done`.
* The chat's step list needs no knowledge of adam's tool names, and a tool added later shows its title or fails a test.

## Alternatives rejected

* **Map names to titles in the orchestration layer.** It would have to know every agent's tools.
* **Derive a title from the name** (`edit_file` to `Edit file`). Cheap, but `run` becomes `Run`, and a title is a
  sentence a person reads, which an author writes.
* **Use the first sentence of the description.** Written for the model, too long for a row.
