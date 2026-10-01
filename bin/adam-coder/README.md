# adam-coder

The coder agent: a coding task in, a verified pull request out, over A2A.

Given "in repo X, do Y" it

1. prepares a git worktree of X in the run's workspace (`adam-workspace`; a workspace holds several repositories, and
   a task that names none starts in a **scratch project** that is published to the repository the person names later),
2. makes the change: small, well-located edits itself (`read_file`, `write_file`, `apply_patch`), broad ones
   through OpenCode over ACP (`adam-acp`),
3. runs the project's own checks, at most `MAX_CHECK_CYCLES` failing cycles,
4. commits, pushes and opens a pull request, and
5. streams progress throughout and reports the check results, the branch and the pull request as artifacts.

It is durable (every model and tool step is journaled by `adam-runtime`, so a
restarted worker replays instead of repeating a side effect) and addressable
(an A2A 1.0 server from `adam-a2a`, backed by `adam-a2a-runtime`). By default one
process serves A2A and runs the workers; replicas share one Postgres. `ROLE` splits
the two halves into separate processes (see [Roles](#roles)); a control plane
needs no model or GitHub configuration.

```mermaid
sequenceDiagram
  participant O as Orchestrator (A2A client)
  participant S as adam-coder (A2A server)
  participant R as Runtime + workers
  participant M as Model (OpenAI-compatible)
  participant W as Worktree / OpenCode (ACP)
  participant G as Git remote + GitHub
  O->>S: SendStreamingMessage "in repo X, do Y"
  S->>R: start run (task id = run id)
  loop until the model stops
    R->>M: next turn
    M-->>R: tool calls
    R->>W: prepare_workspace / read_file, write_file, apply_patch / delegate_to_opencode / run_checks
    R-->>O: progress (status updates)
  end
  R->>G: commit_and_push, open_pull_request
  R-->>O: artifacts (checks, branch, pull_request) then completed
```

## Tools

| Tool | Does |
|---|---|
| `prepare_workspace { repo_url, base_branch?, branch? }` | `RunWorkspace::add_repository` for the run: **a slot of the run's workspace** (a worktree named after the repository: `slot: <dir>` in the result), idempotent per repository, so a restart or a repeated call reuses it and a second repository is added next to the first ([below](#the-workspace-of-a-run)). **Only for a repository the person named** in their own messages of the run (see [the rules](#the-rules-in-code)); any other is a tool error that sends the model to `ask_user`. Without `base_branch` the worktree starts from the repository's default branch (`Workspaces::default_branch`, what the remote's `HEAD` names; a repeated call in a prepared workspace reuses its base without asking the remote). A `base_branch` the remote does not have is a tool error that lists the remote's branches (the first 30) so the model can pick one or ask. With `branch` (a branch an earlier `commit_and_push` of the conversation reported for this repository), `Workspaces::prepare_continuing`: the worktree starts from that branch, and `open_pull_request` later adds the run's commits to it and so updates its pull request (see [A task that continues a task](#a-task-that-continues-a-task)) |
| `start_scratch { name? }` | `RunWorkspace::add_scratch` for the run (`scratch` unless named; `^[a-z0-9][a-z0-9._-]{0,63}$`, not ending `.git`): **a scratch slot**, a local git repository with an empty root commit, to build and test something in before any repository is named. Idempotent per name; a name a repository's slot has, or a bad one, is a result that says so. The result says the project is **temporary** (it exists only while the task is open, nothing is kept unless it is published) and that the model must tell the person ([below](#scratch-projects)). Every tool that works in a slot works in it: the file tools, `run_command`, `run_checks`, `delegate_to_opencode` |
| `publish_scratch { repo_url, scratch?, base_branch?, path?, overwrite? }` | puts the files of a scratch project into a repository **the person named** (the same rule and the same refusal as `prepare_workspace`, checked before anything talks to a remote): the repository's slot is found or made, an **empty** remote (no ref at all) first gets an empty-tree `Initial commit` on its base branch (`Workspaces::initialize_empty`: the only push outside `agent/*`, never forced; `main` unless `base_branch`), a repository that **already has files** needs `path` (a directory of it) or `overwrite: true` (the person's to decide: the result tells the model to ask), then `copy_into` (all or nothing, collisions listed). The project remembers where it went (`published_to`). The result says which slot to use next and whether the checks that ran on the project hold for this code ([below](#scratch-projects)) |
| `run_command { command, cwd?, repo? }` | **looking around**: `git branch -r`, `ls`, `cat README.md`, `git log`. Run in the run's environment ([below](#where-the-processes-of-a-run-run)) with the same shell, `cwd` rule, timeout and output cap as `run_checks`, but it emits **no** `checks` artifact, uses **no** check cycle, and a non-zero exit is a plain answer, not a failure. It is not an editing path: `HEAD`, the branch, the tree of the worktree (what `commit_and_push` would commit), the refs and the git configuration (see [below](#looking-around-and-what-it-may-not-do)) are recorded before the command, and a command after which any of them differs is **undone** (`git reset --hard`, `clean`, `read-tree`: uncommitted work of the run comes back exactly) and refused, with a message that changes go through `delegate_to_opencode`. Writes to ignored paths (build output) are not changes |
| `read_file { path, start_line?, end_line?, repo? }` | a text file of the worktree, **confined to it** ([below](#reading-and-changing-files-itself)): the whole file (cut at 256 KiB, the cut marked) or the lines `start_line..=end_line` each behind its number; a binary file (a NUL byte) is "binary file, N bytes, not shown". Progress line: `read <path> (<slot>)` |
| `write_file { path, content, repo? }` | creates or replaces a file with exactly `content` (at most 1 MiB), creating its parents; written next to its target and renamed over it, so an interrupted write never leaves half a file, and the mode of a replaced file is kept. Refuses a path through a symlink and anything inside `.git`. Progress line: `wrote <path> (<slot>)` |
| `apply_patch { patch, repo? }` | a unified diff (at most 1 MiB) with `a/` and `b/` before the paths, for one or several files, checked before it is applied and then applied by `git apply`, all or nothing; its result lists the files changed. Progress line: `patched <files> (<slot>)` |
| `delegate_to_opencode { instructions, repo? }` | spawns the ACP agent in the worktree of the slot, as the run's environment prepared the command ([below](#where-the-processes-of-a-run-run); `ClientPolicy { fs_root: worktree }`), reports what OpenCode does as steps (see [Steps](#steps-what-the-person-sees-of-the-work)), returns its summary and the changed files |
| `run_checks { command, cwd?, repo? }` | **the project's real checks only** (what its CI, README or Makefile run). `bash -lc <command>` in the worktree (`sh -lc` where the image has no bash; a login shell keeps the toolchain `PATH` from `/etc/profile.d`, and bash-isms such as `${PIPESTATUS[0]}` work), a `cwd` must stay inside it, timeout kills the process group (and tells the run's environment), output tail capped, secrets hidden from the child; artifact `checks` (see [Artifacts](#artifacts)). A command the shell cannot find is a **missing toolchain** (below), not a failed check |
| `commit_and_push { message, repo? }` | in a **scratch project**: a commit, locally, and nothing else (no push, no `branch`, no bound `checks`; it says nothing is published until the person names a repository). In a repository: `commit_all` + `push` to **the run's own branch** `agent/<run>` (also for a run that continues a branch, which this tool never touches); artifacts `checks` (bound to the pushed commit, see [Artifacts](#artifacts)) then `branch`. It records the line of work in the run notes itself (`RunNotes::pushed_branches`), and its text ends with `repository: <url>` and `branch: <name>` lines (the last two lines: the fallback by which a later task learns which branches exist when the notes are not at hand) |
| `open_pull_request { title, body, accept_red_checks?, repo? }` | a scratch project has no remote: it is a result that sends the model to `publish_scratch`. In a repository, after the gate (below), moves the branch the run continues to the pushed commit (`Worktree::publish`: `git push origin <own>:<continued>`, never forced), then reports the pull request already open for the branch ("was already open", title and description unchanged) or opens one with `CodeHost::open_pull_request`; on an already open pull request with accepted red checks it adds a comment with the note; artifact `pull_request`: a data part (`url`, `number` as a string, `branch`, `repository`) followed by an A2A `url` part with the pull request's URL (`Part.url`, so a chat UI shows a link) |
| `ask_user { question, choices? }` | `ToolError::NeedsInput`: the run parks, A2A reports `input-required` with the question. With `choices` (up to 8 questions of 2 to 8 options, as one form) and a screen that can draw it, the question carries an A2UI surface and the person's answers come back as the result; see [Asking with choices](#asking-with-choices). It is [`adam-ui`](../../crates/adam-ui/README.md)'s tool under the coder's own words about when to ask, and `asks_user()` is `true`, so `adam-assembly` refuses to give it to a subagent |
| `show { blocks, title? }`, `ui_catalog {}` | the screen's components as tools ([`adam-ui`](../../crates/adam-ui/README.md)): `ui_catalog` lists what the person's screen can draw, `show` draws blocks of it beside the text answer. A coding task does not need them; they answer "answer in text" when the screen sent no catalog |

A folder's `mcp.json` adds the tools of its MCP servers to these, named `<server>__<tool>` (see [MCP tools from the
folder](#mcp-tools-from-the-folder)); they are not part of the fourteen. The shipped folder's own `mcp.json` adds
twelve **read-only** tools of GitHub, `github__get_me`, `github__get_file_contents`, `github__list_branches` and the
rest (see [GitHub over MCP](#github-over-mcp-read-only)). The tools the conversation's endpoint lists
(`thread-tools/v1`) are offered too, at every model turn, under their listed names: a `ToolSource`, not a tool of
this crate (see [Asking with choices](#asking-with-choices)).

Eleven tools are written in this crate; the other three are `adam-ui`'s, built from `ToolEnv::ui`. Each of the eleven is an `async fn` under `#[tool]` (`adam::tool`, see the [`adam` README](../../crates/adam/README.md#tool)) in
`src/tools/`: the function's doc comment is the description the model reads, the parameter docs are the
argument descriptions, and `State<ToolEnv>` is the shared environment. `coder_tools(&env)` is
`tools![..]` wrapped so that everything a tool returns or fails with passes through the `Redactor`, and
`CoderAgent` gives the agent the `ToolEnv` as state (`LlmAgentBuilder::state`), which is where the tools read it.
The specs the model sees are pinned by `tests/fixtures/tool-specs/*.json` (see [Tests](#tests)); tool names and the
journal's `tool:<call id>` step names are unchanged, so a run started before the port replays.

### Steps: what the person sees of the work

The card lists `steps/v1` ([ADR 0007](../../docs/decisions/0007-progress-as-steps-and-streamed-text.md); the contract is
the orchestration layer's `docs/api/steps-v1.md`), and every tool call is a step to a client that activates it. Most are
plain steps labelled with the tool's name (`prepare_workspace`, `run_checks`, ...), whose progress lines
(`running checks: ...`) are updates of their own step. **`delegate_to_opencode` is a `subagent` step labelled
OpenCode** (`#[tool(step = "subagent", label = "OpenCode", icon = "agent")]`), and what OpenCode reports over ACP is the
tree under it:

| OpenCode reports | The step |
|---|---|
| a tool call (`ToolCall`) | a child `acp:<call id>:<ACP id>` that runs under OpenCode's step: kind `command` for ACP's `execute`, `tool` for the others; the icon is ACP's kind when the contract has one (`read`, `edit`, `delete`, `move`, `search`, `think`, `fetch`, `execute`); the label is its title |
| an update of that call (`ToolCallUpdate`) | `completed` or `failed` ends the child, with its output as the detail (cut to 300 characters); output while it runs is an update; `in_progress` with nothing new is not reported |
| its plan (`Plan`) | an update of OpenCode's own step: `plan: 2 of 5 done` |
| a line of its reply (`AgentText`) | an update of OpenCode's own step, the line as the detail |
| its reply, at the end | one `message` child, `OpenCode's summary`, the last 1000 characters as the detail |
| a tool call that never said it ended | ended `canceled` with the turn; `failed` when the turn failed, `canceled` when the run was cancelled |

Every label and detail comes from OpenCode, so it is **scrubbed of the secrets the process holds and cut** before it is
reported, like every other line. A client that did not activate steps reads the same work as lines of text: the title of
a tool call when it starts, `<title>: done` or `<title>: failed: <output>` when it ends, the lines of OpenCode's reply
and its plan as they are, and `OpenCode: done` when the call ends (`tests/e2e.rs`, `tests/tools.rs`).

### Streamed answers: the words as the model writes them

The coder **streams its model calls** (`LlmAgentBuilder::stream_text`, on by default), and its card lists `text-stream/v1`
([ADR 0007](../../docs/decisions/0007-progress-as-steps-and-streamed-text.md); the contract is the orchestration layer's
`docs/api/text-stream-v1.md`). A client that activates it reads each answer as chunks while the model writes (artifact updates
named `reply`, each with where it begins in UTF-8 bytes, the last marked `lastChunk`), then the status that ends the turn (`completed`, or `input-required` for a reply that delivers nothing, which the coder
turns into a question: `PendingQuestion::stream` says which stream the question is), whose message is the whole text
and names the stream (`metadata["https://agents.vymalo.com/a2a/extensions/text-stream/v1"] =
{"streamId": ..}`); the words the model writes before a tool call are stated by a `working` status of their own, the same
way. A client that does not activate it reads the whole reply with the turn, as before, and a blocking `message/send` is
unchanged. The chunks are live and not stored, like every event: what the run records is the final text. The scripted
models of `dev/wiremock/mock-openai` answer a stream as they answer a completion, and `dev/coder-e2e.sh` and
`dev/greeting-e2e.sh` check that the answer arrives as chunks that add up to it.

### Asking with choices

The person's screen (the orchestration layer's chat) can draw a form. The coder announces that on its card
(`adam_ui::with_card_extensions`: A2UI v0.9.1 with `acceptsInlineCatalogs: true`, `ui-catalog/v1`,
`thread-tools/v1`; `agent_card_from` adds them, with `steps/v1` and `text-stream/v1` below, and `tests/fixtures/agent/card.json` pins them all), reads A2A messages as one from
a screen (`vymalo_inbound`, set by `Coder::new_with` and `serve`), and gives the model `ask_user { question, choices? }`:
three questions at once (a database, a login, where it runs) become **one Choices surface** beside the question, and the
person's answers come back as the tool result, `- db: pg` per question, which the model quotes in its next words.

```mermaid
sequenceDiagram
  participant S as Screen (orchestrator)
  participant C as Coder (A2A server and worker)
  participant M as Model
  participant E as Thread tools endpoint
  S->>C: message: text, ui-catalog/v1 {version, digest}, the catalog inline or only referenced, thread-tools/v1 {url, token}
  C->>E: tools/list (at every model turn, with the grant)
  E-->>C: get_ui_catalog and the tools attached since
  C->>M: the coder's tools, ask_user, show, ui_catalog, then the listed ones
  M->>C: ask_user {question, choices: [db, auth, deploy]}
  C->>C: the catalog: inline, held by digest, or get_ui_catalog once (digest checked)
  C-->>S: input-required: the question and an application/a2ui+json surface (one Choices)
  S->>C: an A2UI action: answers db=pg, auth=keycloak, deploy=compose
  C->>M: the tool result "The person answered through the interface: - db: pg ..."
  M-->>C: "Going with Postgres, Keycloak and Compose."
```

```mermaid
stateDiagram-v2
  [*] --> Asking: the model calls ask_user with choices
  Asking --> Form: the screen's catalog is read and has Choices
  Asking --> TextQuestion: no catalog, none with Choices, or it cannot be read now
  Form --> Waiting: input-required with the surface
  TextQuestion --> Waiting: input-required, the options listed in the text
  Waiting --> Answered: the A2UI action (or the person's words)
  Answered --> [*]: the answers are the tool result
```

What the deployment decides: the URL a message announces for the conversation's tools is an MCP server's, so
**`MCP_ALLOW_INSECURE=true` is needed when the orchestrator is reached over plain `http` on another host** (a compose
stack: `http://orchestrator:8080`); https and loopback need nothing. Without a usable grant (none, expired, refused) the
coder offers no thread tools and asks in text; none of it fails a run. The mock model asks the three questions for a
task that carries `[mock:choices]` (`dev/wiremock/mock-openai/mappings/coder-choices.json`), and
`dev/coder-choices-e2e.sh` runs the chain through the compose stack. How a *live* model uses `choices` is *unverified*.

### Artifacts

A run reports its work as A2A artifacts (`adam_a2a_runtime::artifact_of`): one data part of media type
`application/json`, with an id derived from the content, so a replayed step's artifact carries the same id and a
subscriber sees it once.

| Name | From | Data |
|---|---|---|
| `checks` | every `run_checks` call that ran its command, and `commit_and_push` (bound, below) | `passed`, `commit`, `tree?`, `summary?`, `findings?` (below) |
| `branch` | `commit_and_push`, after its bound `checks` | `repository`, `branch` (the branch the commit was pushed to: the run's own), `base_branch`, `commit`, and `continues` when the run continues another branch that the commit has not been published to yet (it is, by `open_pull_request`, after the gate) |
| `pull_request` | `open_pull_request` | `url`, `number` (a string), `branch`, `repository`, then an A2A `url` part |

**`checks`** is what an orchestrator gates on. Its data part:

| Field | Type | |
|---|---|---|
| `passed` | bool | the command exited 0 in time **and** `commit` was determined |
| `commit` | string | the 40-hex SHA of a commit: for `run_checks`, the `HEAD` of the run's worktree when the command ran (`""` only when it could not be read); for `commit_and_push`, the pushed commit |
| `repository` | string, optional | the URL of the repository of the slot the command ran in; for the verdict `commit_and_push` binds to a pushed commit, the repository it was pushed to. Absent for a check that ran in a scratch project (it has none), and from a report of an older coder |
| `tree` | string, optional | the 40-hex git tree id of the code that was checked: the worktree as `commit_and_push` would commit it (`git add -A`: tracked changes and untracked files, minus what `.gitignore` excludes), computed in a temporary index. Absent when it could not be computed |
| `summary` | string, optional | one line: `` `cmd` passed ``, or `` `cmd` failed: exit code 2 `` / `timed out after 900s` / `killed by a signal`. Says so when the worktree had uncommitted changes on top of `commit`, or that the tree was checked before it was committed |
| `findings` | `[{check, message}]`, optional | one entry per failing check: `check` is the command, `message` is how it ended, then the tail of its output |

* **From `run_checks`: one artifact per call that ran**, reflecting that run and bound to `HEAD`, with `tree`.
  The agent runs its checks before `commit_and_push`, so this is usually the commit *below* the one that gets
  pushed, with the change still uncommitted (the summary says so). Calls the tool refuses before running anything
  (a bad `cwd`, no workspace, the exhausted cycle budget) emit none.
* **From `commit_and_push`: the verdict on the pushed commit,** emitted before `branch`, so a consumer that has
  seen `branch` already has it. The tool compares the tree of the pushed commit with the `tree` of the run's last
  `run_checks`:
  * **Same tree:** the same report bound to the pushed commit: `commit` is the pushed SHA, `tree` its tree,
    `passed` and `findings` are the last run's, and the summary notes it was checked on the identical tree
    before it was committed. This is also what a `commit_and_push` with nothing new to commit reports, when the
    checks ran on the tree that is already pushed.
  * **Different tree, or no `run_checks` in the run:** `passed: false`, `commit` the pushed SHA, `tree` its tree,
    and one finding `{check: "checks", message: "the pushed tree was not checked: the last checks ran on <tree10>
    (or: no check ran in this run), the commit has <tree10>"}`. Edits made after the checks are unverified.
* **The last one for a commit wins.** A run reports several `checks` (red, fix, green, then the bound one). A
  consumer that gates on "the checks for the pushed SHA" takes the last `checks` whose `commit` is that SHA; a
  green one there means the code in that commit was checked. Each is a separate artifact (its id is derived from
  its content); two with identical content share an id and arrive once.
* **No commit is a failure, not a gap.** If `HEAD` cannot be read (no repository, no commit) `run_checks` still
  emits the artifact with `passed: false`, `commit: ""` and a finding named `commit` that says so.
* **Caps.** At most 20 findings and 16 KiB in total (the names and the messages). A message that does not fit keeps its
  end behind a `[cut: the last N of M bytes]` line; findings that do not fit are replaced by a last one named `findings`
  (`[cut: N more findings left out ...]`). A check name is cut at 256 bytes. A run of one command yields one
  finding; the caps guard the other cases.
* **Redacted.** The output is scrubbed with the run's `Redactor` (see [Secrets in output](#secrets-in-output)) before it
  is cut, and the finished artifact is scrubbed again on its way out, like every tool result.
* **Replay-safe.** Both artifacts are part of their tool's journaled result and hold nothing that varies between
  executions (no timestamps, no ids of their own), so a replayed step re-emits identical artifacts. The last
  report and its tree, and the 32 before it, are kept in the run's notes (the file next to the workspace that also holds the cycle count),
  written before the tool's result is journaled; the bound report is a pure function of the notes, the pushed SHA
  and its tree. A `commit_and_push` that ran twice (a crash before its result was journaled) commits nothing the
  second time, sees the same `HEAD` and tree, and emits the same `checks` once.

Arguments the schema does not allow (a missing `command`, a number where a string belongs) come back to the model
as a tool result, `invalid arguments for `run_checks`: ...`; an empty or blank required value still says
`<argument> is required`.

### The rules, in code

The system prompt (`agent/instructions.md`, with `{{max_check_cycles}}`; see
[Where the prompt and the card live](#where-the-prompt-and-the-card-live)) tells the model the rules. The tools
make them hold:

* **Cycle limit.** Every failed `run_checks` costs a cycle, counted per tool call
  id (a replay never counts twice). At `MAX_CHECK_CYCLES` the tool tells the
  model to stop and refuses to run anything; `commit_and_push` and
  `open_pull_request` refuse too. The run then ends `failed` with the findings
  (the output of the last failing check) and no pull request.
* **No PR on red checks.** `open_pull_request` opens a pull request only if the
  most recent check run **on exactly the code the pull request contains** (the
  tree of the pushed `HEAD`, compared with the tree the check ran on, in whichever slot it ran) passed, and the
  branch is pushed. The single override is `accept_red_checks: true`, which the
  prompt reserves for explicit user consent obtained with `ask_user`; a pull
  request opened that way says so in its body, and one that was already open says
  so in a comment (its body is not ours to rewrite). It never overrides an exhausted
  cycle budget (a deliberate hardening: after the limit the run must stop).
* **A continued branch is reached only through the gate.** A run that continues a branch
  (`prepare_workspace`'s `branch`) pushes its commits to the run's own `agent/<run>`, never to
  that branch: `commit_and_push` cannot move it. Only `open_pull_request`, after the check
  above has passed (or been accepted), moves it to the pushed commit, as a fast-forward
  that is never forced. So a pull request that is open for the branch never carries commits that
  nobody verified, and a rework whose checks stay red leaves the branch and its pull request
  exactly as the last verified task left them. The pull request of a continued branch is found by its head
  alone (`find_pull_request_on_head`), and the base branch is recorded with the branch (`PushedBranch::base`,
  inherited with the notes), so a continuing run works against the same base and never opens a second pull
  request. A comment that cannot be posted (accepted red checks on an open pull request) does not undo the
  update: the tool reports the pull request with a warning, records it (`RunNotes::published`, so the verdict
  of a run that ends anyway does not say the pull request was not updated), and posts the note once per
  pushed commit. If the branch moved on the remote since the task
  started (someone pushed to it), it is not overwritten: the tool says so, names the run's own
  branch where the commits are, and tells the model to ask the person.
* **Completion policy.** When the model stops (a turn without tool calls) the
  run completes only if it opened a pull request (or updated the one of the branch it
  continues). A run that ends with red
  checks, the check-cycle budget used up and no pull request fails instead of
  completing, whatever the model says. So does a run that ends without a pull
  request because GitHub or git rejected the credentials (the model cannot fix a
  bad token, or an App that is not installed there): the error names `GITHUB_TOKEN`, or the `GITHUB_APP_*`
  variables in App mode. Anything else is a **question,
  not a completion** (red checks with cycles left included: fixing or asking is
  the model's call): the model
  that answers "Hi! I need a repository and a task" in plain text asked
  something, and the run parks exactly as if it had called `ask_user`: A2A
  reports `input-required` with the model's text (trimmed; a fixed sentence when
  there is none) as the question, the run waits with no timer, and the next
  delivered message resumes it. The conversation is made to say what happened:
  the model's last message gets an `ask_user` call carrying that text, and the
  person's answer is that call's result, as for a real `ask_user`, so the
  history stays valid for every provider; its id is `stop` and the turn as five
  digits, nine alphanumeric characters, which the strictest providers accept. The
  turn and check-cycle limits and CancelTask apply to a parked run as to any other.
  **A run with nothing to deliver never completes on its own**, and there is no
  limit on how often it asks: it is a chat, and the person can answer, say
  something else or stop it. The exits are a pull request, a failure (the rules
  above, `max_turns`, `max_tool_calls`, a model or tool error that is not retried)
  and CancelTask (a chat's Stop).
* **A scratch project is temporary, and what is published is what was checked.** It lives as long as the run (the
  janitor deletes it with the rest of the workspace; the instructions and the tool's result tell the model to say so),
  `commit_and_push` there is a local commit and `open_pull_request` is refused, so nothing leaves the process until
  `publish_scratch` copies the files into a repository the person named. The gate does not change: the checks that ran
  on the project are bound to the pushed commit only when its tree is the same ([below](#scratch-projects)).
* **The file tools stay in the worktree.** `read_file`, `write_file` and `apply_patch` check every path with `confine`
  (no `..`, no absolute path, nothing inside `.git`, no write through a symlink, no read that leaves the worktree) and
  `apply_patch` checks the paths git itself reads from the patch, so a hostile path is a refusal the model is told, never a
  file outside the worktree (see [above](#reading-and-changing-files-itself)). `run_command` stays read-only beside them.
* **Looking around is not checking.** `run_command` exists so that exploring a repository does not look like
  verifying it: in the owner's live thread the model explored with `run_checks` (`ls`, `cat README.md
  CLAUDE.md` with exit 1 because `CLAUDE.md` was missing, `mvn package`, `ls /usr/lib/jvm`) and every
  exploration was a check cycle, so three "failures" failed a run that had checked nothing. Now exploration
  has no artifact and no cycle, has its changes to `HEAD`, the branch and the working tree undone, and the prompt says to use `run_checks` only for the
  project's own checks.
* **A missing toolchain is reported, and the coder waits.** When a command **exits 127 and says so as a
  shell does** (`sh: 1: mvn: not found`, `bash: line 1: mvn: command not found`, printed by the shell that ran it
  or by a script the command runs; `missing_tool`; both are required, so a nested `sh: 1: gti: not found` in the
  output of a run that exits 101, a bare 127, a missing file or a test that prints "not found" is not one), both
  tools answer that the workspace has no `mvn`: **no check cycle is used, no `checks` artifact is emitted,
  nothing is recorded for the gate**, and the model is told not to retry variants, not to search the filesystem
  and not to install a system toolchain, but to tell the person which toolchain is missing with `ask_user` and
  wait (the owner's decision: the workspace image carries the toolchains it carries, no Java for now). A tool
  that **the project brings itself** (`jest`, `vitest`, `tsc` with a `package.json` that declares it or is
  one of the well-known node tools, `pytest` with a Python project file; `project_dependency_hint`) is a
  dependency that is not installed yet, and the answer says to install the project's dependencies with its own
  command (`pnpm install`, `npm ci`, `pip install -r requirements.txt`, picked from the lock files) through
  `delegate_to_opencode` **only** (an install that passes under `run_checks` would be a green check on code
  nobody tested); it also costs no cycle. The second time the same tool is reported missing in a run
  (`RunNotes::missing_tools`, one entry per call id so a replay counts once) the answer is to ask the person,
  whatever the project says.
* **A question is answered, not worked on.** The prompt says that a greeting or a question about the
  repository ("List all branches") gets a direct answer (after `prepare_workspace`, with `run_command`) and ends
  the turn; the run then parks as a question like any stop without a pull request, and the chat goes on.
* **Only a repository the person named.** `prepare_workspace` and `publish_scratch` refuse a
  repository that is not named in the person's own messages of the run (a scratch project is published
  only to one the person named: nothing is copied, pushed or even asked of a remote otherwise): the
  task, and every answer delivered to it (user messages and the results of
  `ask_user`, paired with the question by position in the history, because
  providers that send no call ids get `call_0`, `call_1` again in every turn; what
  the model or a tool wrote never counts, and neither does text quoted in a
  fence labelled `untrusted`, which is how the orchestrator's message that sends
  a job back quotes findings of checks and reviewers: its `request` fence, the
  person's own words, does count). Before every step
  `CoderAgent` reads those messages (and the inbox, without consuming it:
  `Ctx::peek_inbox`) and records the repositories they name in the run notes
  (`RunNotes::named_repos`); the tool compares the argument with them and never
  trusts the model alone. Repositories are compared as normalised
  `host/owner/name` ([`tools::named`](src/tools/named.rs)): case-insensitive,
  without scheme, credentials, `.git` or a trailing slash; `https://host/owner/name(.git)`,
  `host/owner/name`, `git@host:owner/name`, `owner/name` (the host is the first
  of `ALLOWED_REPO_HOSTS`, `CoderSettings::default_repo_host`) and, for local
  remotes, the absolute path or `file://` URL all name the same repository, a
  port stays part of the host (`http://git-server:8080/local/sandbox.git`) unless
  it is the scheme's default, and `www.github.com` is `github.com`. The
  refusal is a tool result that says which repositories were named (only what
  the person wrote, and not words that are files such as `src/main.rs`) and tells
  the model to ask the person with `ask_user`; it is not a run failure. The tools
  read what `CoderAgent` records, so `coder_tools` under another agent refuses
  every repository.
  A message that continues a parked run (same `contextId`, no `taskId`, while the
  task is `input-required`) is delivered to that run, so a repository named in the
  original request still counts on it. A message after the task ended starts a new
  task in the context; it knows only its own messages unless it references the earlier task
  (`referenceTaskIds`), see [A task that continues a task](#a-task-that-continues-a-task).

```mermaid
stateDiagram-v2
  [*] --> Stepping
  Stepping --> Stepping: tool calls
  Stepping --> Stopped: the model stops (a turn without tool calls)
  Stopped --> Completed: a pull request was opened
  Stopped --> Failed: red checks with no cycles left, or the credentials were rejected
  Stopped --> InputRequired: anything else, the model's text is the question
  Stepping --> InputRequired: ask_user
  InputRequired --> Stepping: the person answers
  InputRequired --> Canceled: CancelTask
  Completed --> [*]
  Failed --> [*]
  Canceled --> [*]
```

Per-run bookkeeping (cycles, last check, pushed sha, pull request, repositories named, branches the
conversation pushed) lives in
`<WORKSPACE_ROOT>/coder/<run>.json` next to the worktree, written atomically.

### Reading and changing files itself

The model used to change a worktree only by delegating to OpenCode: a process, a model and a conversation for a one-line
fix. `read_file`, `write_file` and `apply_patch` (`src/tools/files.rs`) read and change files of the worktree in this
process, and the instructions say when to use which: small, well-located changes yourself, broad multi-file changes
through `delegate_to_opencode`. The model chooses every path, so the paths are the point.

**The confinement rule** (`confine(slot_root, rel, Access)`, public):

* an empty path, an absolute path, any `..` component and any component equal to `.git` (compared without regard to case)
  are refused;
* to **read**, the path is canonicalised (symlinks followed) and the result must be inside the canonical root, and not
  inside a `.git`: a symlink that stays inside the worktree is read, one that leads out is refused;
* to **write**, the deepest part of the path that exists is looked at one component at a time: **any symlink on the way,
  the file itself included, is refused** (also one that stays inside: a write through a link is a write to a place the
  model did not name), and what exists must be inside the canonical root.

Every refusal is a tool **result** that names its reason, not a failed run. `apply_patch` does not trust its own reading
of the patch: it asks `git apply --numstat -z` which paths git would touch (a rename names both ends, however the patch
spells it), checks each with the rule for writing, refuses a mode of a symlink or a submodule (`120000`, `160000`) and a
binary patch, then runs `git apply --check` and `git apply` (hooks and the file system monitor off, no user or system
configuration, never `--unsafe-paths`). **A hunk that does not match changes nothing** and git's message goes to the
model. The line counts of a hunk header are written by a model, which counts badly, so a patch git cannot read or apply as
written gets a second reading with `--recount` (not the first: `--recount` takes the `--- ` line of the next file for a
removed line when no `diff --git` line separates them); the policy refusals are the same under both. The three tools change files and never touch git: `commit_and_push`
commits what the files say, a change made by these tools changes the worktree's tree like OpenCode's, so the commit
that follows is bound to a check only after a new `run_checks` (the gate is unchanged).

```mermaid
sequenceDiagram
  participant M as Model
  participant T as apply_patch
  participant G as git apply
  participant W as Worktree
  M->>T: patch (unified diff)
  T->>G: --numstat -z (which paths, never trusting the patch's own spelling)
  G-->>T: the paths
  T->>T: confine each path for writing, refuse symlink and submodule modes
  T->>G: --check
  alt it applies
    T->>G: apply
    G->>W: the files change
    T-->>M: the files changed
  else a hunk does not match
    T-->>M: nothing changed, git's message
  end
```

```mermaid
stateDiagram-v2
  [*] --> Read: the patch, at most 1 MiB, no NUL
  Read --> Refused: a path leaves the worktree, touches .git or a symlink, a symlink or submodule mode, a binary part
  Read --> Checked: git can read it and every path is confined
  Read --> Recounted: git cannot read it or it does not apply as written
  Recounted --> Checked: it does with --recount
  Recounted --> Refused: it does not
  Checked --> Applied: git apply
  Applied --> [*]
  Refused --> [*]: the reason, nothing changed
```

Limits: 256 KiB of a file read, 1 MiB of content or patch. The checks and the write are not one atomic step; the tools of
a run are called one at a time and what a command leaves running is killed with it, and a write goes to a new file that is
renamed over its target, which replaces a symlink that appeared meanwhile instead of following it.

### The workspace of a run

A run's files are a **workspace**: a directory of slots, `<WORKSPACE_ROOT>/workspaces/<run>/<dir>/`
([ADR 0008](../../docs/decisions/0008-a-workspace-holds-several-repositories.md), built in
[`adam-workspace`](../../crates/adam-workspace/README.md#a-runs-workspace-slots-scratch-projects-and-the-copy-between-them)).
Each repository the person names is a **slot**, a worktree on the run's own branch, called after the repository
(`sandbox`; `<name>-<owner>` if two repositories of the run share a name). `prepare_workspace` adds one (a repository
that is already there returns its slot), and every tool that works in a repository takes `repo`: the slot's name or
the repository's address (`https://github.com/acme/lib`, `acme/lib`, a path). **With one slot `repo` may be left out;
with several, leaving it out is an error result that lists the slots** (and a `repo` that matches none lists them too).
A run that began before slots existed has its one worktree in the old layout (`<root>/worktrees/<run>`): it is read as a
slot, and may be joined by others.

* **The gate binds by tree, across slots.** `run_checks` records its result with the slot it ran in, in the notes'
  history of the last 32 runs (`RunNotes::checks.history`; `last` stays beside it). For a commit with tree *T*, in any
  slot, the deciding check is **the most recent run, from any slot, whose tree is *T*** (`RunNotes::checked`): a tree id
  is a content address, so the same tree is the same code, and the most recent check of that code wins (a green then a red
  on it reads as red). No such run means "unchecked". `commit_and_push` binds that run's report to the pushed commit
  (`commit` the pushed SHA, `repository` the slot's repository) and `open_pull_request` refuses unless that check passed.
  Pushing to two repositories in one job leaves the orchestration layer's gate judging only the last `branch`
  (its open question 40). The cycle budget stays per run.
* **The janitor** (`src/janitor.rs`, a worker component of the process: roles `all` and `worker`) sweeps once at startup
  and then every `WORKSPACE_SWEEP_SECS` (300; `0` turns it off): for each run that has a workspace on this worker's volume
  (`Workspaces::runs`), a run the store reports `done` or `failed` (a cancel is `failed`), or does not know, has what its
  environment holds released (`Environment::release`; nothing for `Local`; a release that fails leaves the workspace for
  the next sweep) and **then** loses its
  workspace (`RunWorkspace::remove`: every slot, the legacy worktree, the metadata); the sweep then asks the environment what
  it still holds (`held_runs`) and releases what belongs to runs that are over or unknown even though they have no workspace
  here (`Sweep::orphans`); a **`runnable` or `parked` run is
  never swept**, so a run waiting a week for an answer keeps its files; a directory that is not a run id is left alone.
  The run's notes (`<root>/coder/<run>.json`) and its `agent/*` branches in the mirrors stay: the branch is the work.
  Errors are logged and never stop the process.

```mermaid
sequenceDiagram
  participant M as Model
  participant T as Tools
  participant W as RunWorkspace
  participant N as Run notes
  participant J as Janitor
  participant S as Store
  M->>T: prepare_workspace(lib), prepare_workspace(app)
  T->>W: add_repository, add_repository
  T-->>M: slot: lib, slot: app
  M->>T: write_file(repo: lib) and run_checks(repo: lib)
  T->>W: the slot's worktree
  T->>N: the check, with its slot and its tree
  M->>T: commit_and_push(repo: app)
  T->>N: the most recent check on the pushed tree, any slot
  N-->>T: bound to the commit, or unchecked
  T-->>M: checks and branch artifacts, with the repository
  loop every WORKSPACE_SWEEP_SECS
    J->>S: load_run for each run with a workspace
    S-->>J: open, finished or unknown
    J->>W: remove, for a finished or unknown run
  end
```

```mermaid
stateDiagram-v2
  [*] --> NoWorkspace: the run starts
  NoWorkspace --> OneSlot: prepare_workspace on a repository the person named
  OneSlot --> SeveralSlots: prepare_workspace on another repository the person named
  SeveralSlots --> SeveralSlots: the tools take repo
  OneSlot --> Swept: the run is done or failed, the janitor sweeps
  SeveralSlots --> Swept: the run is done or failed, the janitor sweeps
  NoWorkspace --> Swept: the store does not know the run
  Swept --> [*]: the notes and the branches stay
```

### Scratch projects

A task that names no repository ("write a script that prints the first seven Fibonacci numbers; I'll give you the
repository later") used to end in a question. `start_scratch` makes a **scratch slot**: a local git repository (branch
`main`, an empty root commit) in the run's workspace, where the model writes the files, runs the checks it writes for
them and commits as it goes, with the tools it has in a repository. Nothing leaves the process: `commit_and_push` there
is a local commit and `open_pull_request` is refused. When the person names a repository, `publish_scratch` puts the
project into it:

1. **The grant.** The repository must be one the person named, as for `prepare_workspace`.
2. **The slot.** The repository's slot is found, or made: a remote with **no ref at all** (a repository that was just
   created) is first given an empty-tree `Initial commit` on its base branch, so that the pull request has a base; then
   `add_repository` makes its worktree. A repository that **already has files** (the tree of its base is not empty) is
   refused unless the call has `path` or `overwrite`, which are the person's to give: the result tells the model to ask.
3. **The copy.** `copy_into(project, worktree, path, overwrite)`: all or nothing, with every collision listed.
4. **The next step.** The result says which slot to use and whether the checks that ran on the project still hold:
   the **tree** of the worktree after the copy is looked up in the run's check history (`RunNotes::checked`). The same
   tree (the files landed unchanged in an empty repository) means the check that passed on the project is the check of
   the code that will be pushed, so `commit_and_push` binds it to the pushed commit and the pull request is allowed;
   any other tree is *not checked* until `run_checks` runs in the new slot, and the result says so.

The project is left as it is and remembers where it went (`published_to`); the tools that change it afterwards
(`write_file`, `apply_patch`, `commit_and_push`, `start_scratch` again) say that the change does not reach the repository,
and a second `publish_scratch` copies it again (a file that differs needs `overwrite`). The scratch history is **not**
carried: the pull request holds the commits made in the repository's worktree (ADR 0008, decision 5).

```mermaid
sequenceDiagram
  participant P as Person
  participant M as Model
  participant T as Tools
  participant W as RunWorkspace
  participant G as Git remote
  P->>M: a task, no repository
  M->>T: start_scratch(fib)
  T->>W: add_scratch
  M->>T: write_file, run_checks(repo: fib)
  T->>W: the check and its tree, recorded in the notes
  M->>P: asks which repository to publish it to (the run parks)
  P->>M: "Publish it to <repository>"
  M->>T: publish_scratch(repository)
  T->>T: the repository is named by the person
  T->>G: ls-remote: no ref at all?
  T->>G: push an empty first commit as main
  T->>W: add_repository, copy_into
  T-->>M: slot, copied files, did the checks run on this code
  M->>T: commit_and_push, open_pull_request (repo: the new slot)
  T->>G: push agent/run, pull request
```

```mermaid
stateDiagram-v2
  [*] --> Empty: the run starts
  Empty --> Scratch: start_scratch
  Scratch --> Scratch: write_file, run_checks, commit_and_push (local)
  Scratch --> Refused: publish_scratch to a repository nobody named
  Refused --> Scratch: nothing was touched
  Scratch --> Published: publish_scratch (named; empty, or path / overwrite)
  Published --> Pushed: run_checks if the tree changed, commit_and_push, open_pull_request in the new slot
  Scratch --> Deleted: the run ends, nothing published (the janitor)
  Published --> Released: the run ends (the janitor), the pushed branch stays
  Deleted --> [*]
  Pushed --> Released
  Released --> [*]
```

### Where the processes of a run run

`run_command`, `run_checks` and `delegate_to_opencode` do not spawn a process themselves: each asks the run's
**environment** ([`adam_workspace::Environment`](../../crates/adam-workspace/README.md#where-a-runs-processes-run-the-environment-port),
`ToolEnv::environment`) for the run's session, has the session prepare the command from an `ExecSpec`, and spawns what
comes back in a process group of its own. The default is `Local`, this container, and it behaves exactly as the tools always
did (a login shell, the same `cwd`, the same hidden secrets); a deployment composing the coder with another `Environment`
(`ToolEnv::with_environment`, and the same value for `Janitor::with_environment`) runs the processes of a run somewhere
else without the tools changing. There is no setting for it yet: `serve` builds `Local`.

```mermaid
sequenceDiagram
  participant T as run_command, run_checks, delegate_to_opencode
  participant E as Environment
  participant S as EnvSession
  participant P as the process
  T->>E: ensure(workspace of the run, progress)
  E-->>T: its steps are shown as steps of the tool call (env:run:step)
  E-->>T: the run's session
  T->>S: prepare(ExecSpec: the program or shell command, cwd, env, hide)
  S-->>T: PreparedCommand
  T->>P: spawn, in a process group of its own
  alt a timeout or a cancel
    T->>P: kill the process group
    T->>S: kill(exec id)
  end
  Note over T,E: the janitor releases the run's environment before it removes the workspace
```

```mermaid
stateDiagram-v2
  [*] --> NoSession: the tool is called
  NoSession --> Session: ensure
  NoSession --> Failed: ensure fails
  Failed --> [*]: the model gets the reason, no check cycle is used, nothing is recorded for the gate
  Session --> Running: prepare, spawn
  Running --> Finished: the process ends
  Running --> Killed: timeout or cancel, the group and the session's kill
  Finished --> [*]
  Killed --> [*]
```

* **Paths are the same everywhere.** The `cwd` of a spec is the slot's own path, so the file tools, `resolve_cwd`, OpenCode's
  `fs_root` and the git snapshots of `run_command` need no mapping. The file tools and all git work stay in this process.
* **No secret in a spec.** `ExecSpec.env` carries the OpenCode configuration (which names the key as `{env:MODEL_API_KEY}`) and
  never a value of a secret; what the process must not see of this process's own environment is `ExecSpec.hide`:
  `GITHUB_TOKEN`, `GITHUB_APP_PRIVATE_KEY`, `DATABASE_URL`, `A2A_BEARER_TOKENS` and `MODEL_API_KEY` for the project's commands,
  the first four for OpenCode (which reads the model key from its environment). The App's key as a *file*
  (`GITHUB_APP_PRIVATE_KEY_PATH`) is a path, not a secret, and the file is readable by the user the checks run as. A name in `hide` stays hidden even when a spec sets it.
* **A failure to make the environment is a result for the model**, worded `the work environment: <reason>` (the end of a
  failed build's output follows, scrubbed): permanent for a configuration or a build that is wrong, transient for a runtime
  that is down or too slow. Nothing ran, no check cycle was used, no `checks` artifact was emitted. A cancel of the run ends
  the wait for `ensure`.
* **A timeout or a cancel** kills the process group here and then calls `EnvSession::kill` with the id of the command, for what
  lives where this process cannot reach. For OpenCode the same call follows the stop (`session/cancel`, then the kill).
* **The ACP client cannot yet start a process with an empty environment**: a session whose `PreparedCommand` has `env_clear`
  is refused for OpenCode with `the ACP client cannot start a process with an empty environment`. `Local` never asks for it.

### Looking around, and what it may not do

`run_command` is the only tool that runs a model's command outside the check gate, so changes it makes
to `HEAD`, the branch and the working tree are undone: the worktree is recorded, the command runs, and the
worktree is compared with what was recorded. The record also covers what a command could change without
touching a file and that later git calls of the coder would act on: the refs outside the agent's own
namespaces (`refs/heads/agent/*` and `refs/remotes/*` are other runs' and fetches', and move while a
command runs) and the local git configuration (without `branch.*`, which `prepare` writes), so a
`core.fsmonitor`, a `remote.origin.pushurl`, a `url.*.insteadOf`, an alias, a new branch or tag is undone too.
**This is a guard against accidents, not isolation**, and not what the credentials rely on (the workspace
cleans the mirror's configuration itself, under its lock, before every credentialed command). A command can
still read everything the process can, use the network and write outside the worktree. The refs and the
configuration are shared by every run on the mirror and other runs change them while a command runs, so
these are **not** compared, and a command can change them unseen: `refs/heads/agent/*` (another run's
branch), `refs/remotes/*`, `refs/tags/*` (a fetch follows tags) and `refs/stash` (a `git stash` elsewhere),
and the configuration keys `branch.agent/*.adam-run`, `.remote` and `.merge`. `info/exclude` is compared.
The restore holds the mirror lock, refuses to touch anything if the worktree's `.git` no longer points at this
run's git directory (and puts that file back first), and deletes and sets refs with `--no-deref`. Where the tree cannot be computed (an embedded repository without a commit), the record falls back to
`git status`: a change is still seen and refused, but only `HEAD` and the branch are put back, and the model
is told the files may differ.

```mermaid
sequenceDiagram
  participant M as Model
  participant T as run_command
  participant W as Worktree
  participant S as Shell (bash -lc)
  M->>T: command
  T->>W: record HEAD, branch and tree of the files
  T->>S: run (timeout, output cap, no secrets)
  S-->>T: exit code and output tail
  T->>W: read HEAD, branch and tree again
  alt nothing differs
    alt the shell could not find a command
      T-->>M: the workspace lacks it: no cycle, no checks
    else it ran
      T-->>M: exit code and output, no artifact, no cycle
    end
  else something differs
    T->>W: reset --hard, clean, read-tree: back to the record
    T-->>M: refused and undone, changes go through delegate_to_opencode
  end
```

```mermaid
stateDiagram-v2
  [*] --> Recorded: HEAD, branch, tree
  Recorded --> Ran: the command
  Ran --> Unchanged: same HEAD, branch and tree
  Ran --> Changed: any of them differs
  Changed --> Undone: restored to the record
  Unchanged --> MissingTool: the shell could not find a command
  Unchanged --> Answered: exit code and output
  Undone --> [*]: refused
  MissingTool --> [*]: reported, nothing counted
  Answered --> [*]
```

### A task that continues a task

A rework or a follow-up is a new A2A task in the same `contextId` that names the task it builds on in
`referenceTaskIds`. `CoderStarter` and `CoderAgent` forward `init_continuing` to `LlmStarter`, so the new
run starts from the conversation of the referenced run (see
[ADR 0003](../../docs/decisions/0003-a-new-task-continues-the-task-it-references.md)). Three things follow:

* **The person's words are all of the conversation's.** `person_texts` reads the user messages of the
  whole carried conversation, so a repository named in the first task is named in the second. A continued
  user message can have several text parts (the task, the marker that says older turns were left out, the
  next message), so they are read **part by part**, skipping the marker (`Conversation::is_omission_marker`,
  which only ever matches the framework's own text, never a message that merely starts like it) and never
  letting a block one part leaves open swallow the next. Everything else is as above: assistant text and
  tool results never name a repository, nor does text in an `untrusted` fence.
* **The branch can be carried on.** `prepare_workspace`'s `branch` starts the worktree from the branch an
  earlier task pushed, and `open_pull_request` later moves that branch to the run's commits (after its
  gate) and reports the pull request that is already open for it ("was already open") instead of failing or
  opening another. The branch is **not taken on the model's word**, and **not parsed out of text** when it
  can be avoided: `commit_and_push` records the line of work itself in the notes of its run
  (`RunNotes::pushed_branches`: the branch it continued, or the run's own), and before every step the agent
  adds the `pushed_branches` of the notes of the run it continues (`Conversation::continued_from`; it holds
  what that run inherited too, so one hop is enough). Only when those notes are not there (the earlier run's
  volume is not this worker's, as with the `isolated` placement) does it read the `commit_and_push` results
  of the carried history, paired with their call by position, and then only results that end in the two
  lines `repository: <url>` and `branch: <name>` and that history truncation did not cut (the cut is exactly
  where the lines are, so a shortened result is no evidence). The tool accepts only a branch recorded for the
  repository it is asked about; a name the model found in the repository (another person's branch, say) is
  refused and the model is told to start a new branch. The workspace adds its own limits
  (`Workspaces::prepare_continuing`): an `agent/*` branch that exists on the remote, never forced. The
  worktree is still the run's own (`agent/<run>` is what is checked out and what `commit_and_push` pushes);
  `Worktree::branch` names the continued branch, `Worktree::local_branch` the run's own.
* **A new job stays a new job.** Without `branch` the worktree starts from the base branch on a branch of
  its own, as before, and the prompt says when to use which.

```mermaid
sequenceDiagram
  autonumber
  participant M as Model
  participant T as Coder tools
  participant W as Worktree (agent/run2)
  participant R as Remote
  participant H as Code host (PR 7 on agent/abc)
  M->>T: prepare_workspace(branch agent/abc)
  T->>W: start from origin/agent/abc
  M->>T: run_checks, commit_and_push
  T->>R: push agent/run2 (agent/abc untouched)
  M->>T: open_pull_request
  alt gate passed, or red checks accepted
    T->>R: push run2:agent/abc (fast-forward, never forced)
    T->>H: find the pull request of agent/abc (head and base)
    H-->>T: PR 7
    opt red checks accepted
      T->>H: comment: the update was not verified
    end
    T-->>M: PR 7 was already open and carries the commits
  else red or unchecked, or budget spent
    T-->>M: refused: nothing touched agent/abc or PR 7
  else agent/abc moved on the remote
    T-->>M: refused, ask the person (commits are on agent/run2)
  end
```

```mermaid
stateDiagram-v2
  [*] --> Prepared: prepare_workspace(branch)
  Prepared --> PushedOwn: commit_and_push (agent/run2)
  PushedOwn --> PushedOwn: more work, commit_and_push
  PushedOwn --> Gated: open_pull_request
  Gated --> Refused: checks red or missing, unless accepted
  Refused --> PushedOwn: fix, re-run the checks
  Gated --> Moved: agent/abc fast-forwarded to the pushed commit
  Gated --> Stale: agent/abc moved on the remote
  Stale --> [*]: ask the person; nothing overwritten
  Moved --> Reported: the open pull request is reported
  Reported --> [*]: completed
  Refused --> [*]: check budget spent, the run fails: the pull request was not updated
```

The old run's workspace is not removed by this task: the janitor removes it when that run is over ([below](#the-workspace-of-a-run)); the branch
that was pushed is what carries the work, so the new worktree does not depend on it.

### Who it is

The coder has a name and talks like a colleague, not like its tool schemas (adam-rs#55).

* **Its name** is `Coder`: `vars.display_name` in `agent/instructions.md`, said by the first line of the prompt,
  `Your name is {{display_name}}.`, and the `card.name` the A2A card advertises. "What is your name?" is answered
  with it, never with "I don't have a name". A deployment that mounts its own folder
  ([`ADAM_AGENT_DIR`](#a-folder-at-run-time-adam_agent_dir)) changes the name by changing the var (and `card.name`).
* **A greeting gets a greeting**: "hi" is answered with a short greeting that says the name and what the agent does
  in one sentence (the second persona line, `In one sentence: <summary>.`) and asks one question, which repository
  and what to change. It is not a task with something missing, so no tool is called and nothing is asked for "the
  task". The run waits for the answer (A2A `input-required`, as for any plain-text stop that delivers nothing).
* **"What can you do?" and "list your tools"** are answered in plain words first: look around a repository the
  person names, have a change made, run its checks, push a branch and open a pull request, ask when unsure. Then what
  it cannot do, and why: it works only on a repository the person names (it cannot start without one or create one),
  and it makes the change inside a private worktree of the repository, itself (`read_file`, `write_file`,
  `apply_patch`: adam-rs#53) or with OpenCode. Tool names and arguments appear only if the person asks for the
  detail. The "what I can't do" sentence stays true until the scratch workspace and repository creation of
  adam-rs#52 and #54 land, and then it must change with them.
* **The two persona lines are a convention**: the body of the instructions starts with exactly
  `Your name is {{display_name}}.` and then `In one sentence: <summary>.` (the summary ends at its first period and has
  no `"` or `\`). The mocks of the model build their greeting from those two lines (`mock-coder` in
  `dev/wiremock/mock-openai`), so editing them changes the mocked answer, and a folder for another agent that follows
  the convention gets a greeting from the same mock.

Which words a live model chooses is *unverified*. What is tested: the model is sent the persona lines, the rules
above and the rest of the prompt (the instruction snapshot, `tests/fixtures/agent/prompt.txt`), a scripted model that
greets from the persona lines gets the run to wait without any tool, editing the folder changes the greeting
(`tests/agent_files.rs`), and the whole chain runs through the compose stack (`dev/greeting-e2e.sh`).

### Where the prompt and the card live

One file, [`agent/instructions.md`](agent/instructions.md), holds what describes the agent, in the format of
[`docs/authoring.md`](../../docs/authoring.md): the frontmatter has `name` (`coder`, which must equal
`AGENT_NAME`), `description`, `limits` (200 turns, 400 tool calls, 8192 output tokens, 100000 tokens of history),
`vars.max_check_cycles` (the default, 3), `vars.display_name` (`Coder`: the name the agent says) and `card:` (the A2A
card: name `Coder`, the `coding-task` skill with its tags and example); the body is the system prompt.

```mermaid
sequenceDiagram
  participant B as build.rs
  participant C as adam-coder (lib)
  participant D as AgentDef
  participant A as CoderAgent
  B->>C: adam_agent_fs::build("agent").emit(): validates the file, writes OUT_DIR/adam_agent.rs
  C->>D: from_manifest(AGENT) (adam::include_agent!)
  A->>D: var("max_check_cycles", settings), bind(coder_tools), state(ToolEnv), model(client, alias)
  D-->>A: Assembly: the root LlmAgent, its prompt, limits, tools
  Note over A: the LlmAgent steps the run, CoderAgent adds the completion policy
  C->>D: card(public_url, CARGO_PKG_VERSION) for the A2A router
```

To change what the model is told or what the card advertises, edit that file and run the tests: the
build fails with the file and line if the frontmatter is wrong, and binding fails at startup (not in the
middle of a run) for a `{{placeholder}}` the frontmatter does not declare, a var it declares and the body never
uses, or a `tools:` name the coder does not register. Then update `tests/fixtures/agent/prompt.txt` (the
**instruction snapshot**: the body of `instructions.md`, placeholders as written) and `card.json` in the same commit:
a change of what the model is told is a reviewed diff of those files, a change of behaviour to decide on and not
a refactor (see [Tests](#tests)). The limit in the prompt follows
`MAX_CHECK_CYCLES`: the process passes `CoderSettings::max_check_cycles` as the var, so the file's default only
applies to a caller that does not.

What stays in Rust is what a file cannot say: the tools, the completion policy (`CoderAgent`
wraps the assembled `LlmAgent`, fails a run that ends on red checks with no cycles left and no pull request and turns any
other stop without one into a question), the record of the repositories the person named, and the
redaction. `CoderAgent::new` and `with_tools` panic if the agent cannot be assembled, which only a model alias that
is empty or has whitespace can cause; `try_new` and `try_with_tools` return the error, and the binary uses those,
so a bad `MODEL` is a startup error. A control plane has no model, so it takes the card from the file with
`AgentDef::card` (`agent_card(url)`), and `CoderAgent::assembly().card(url, version)` gives the same card.

The Docker build context must contain `agent/`: `docker/coder/Dockerfile.dockerignore` excludes `**/*.md` and
re-includes `bin/adam-coder/agent/**`, and `build.rs` fails the build without the file.

#### A folder at run time (`ADAM_AGENT_DIR`)

The same files can be read when the process starts instead of when it was built
([ADR 0004](../../docs/decisions/0004-agent-folders-at-run-time.md)). Set `ADAM_AGENT_DIR` to the directory that holds
`agent/` (or to `agent/` itself) and **every role** reads it once, at startup: the control plane serves the card
of the folder, the workers assemble the agent from its prompt, limits, vars, skills and subagents. Unset or blank,
the copy `build.rs` embedded is used, as before. There is no watcher: an edit applies at the next start
(`docker compose --profile app up -d coder`, a rollout), so a restart is a deploy, and a change to which tools exist
(`tools:`, a subagent) can fail the replay of a run that is mid-turn, like any deploy of new code.

```mermaid
sequenceDiagram
  participant S as serve
  participant F as AgentFiles
  participant A as AgentFolder (adam-assembly)
  participant C as CoderAgent
  S->>F: AgentFiles::load(ADAM_AGENT_DIR)
  alt unset or blank
    F-->>S: Embedded
  else a folder
    F->>A: AgentFolder::load(path)
    A-->>F: def, warnings, digest (or the diagnostics)
    F->>F: name must be `coder`
    F-->>S: Folder
  end
  S->>S: log `agent files` (source, path, digest, agent, warnings) and each warning
  S->>C: CoderAgent::try_from_files(files, ...) (workers), agent_card_from(files, url) (control plane)
```

The contract of a folder:

| The folder | Rule |
|---|---|
| `name` | must be `coder` (`AGENT_NAME`): the runs are stored under it. Another name exits 78 naming the field |
| `vars` | must declare `max_check_cycles`: the process supplies the value (`MAX_CHECK_CYCLES`), and a folder without the var fails at assembly naming it. Declare `display_name` too if the prompt uses `{{display_name}}` (the shipped one does); the file's value is the name the agent says |
| `description` or `card.description` | one of them: the card needs it (exit 78 for a control plane otherwise) |
| `tools:` | optional; may narrow the coder's own tools, and a name that is not one is refused with a suggestion. Without it the agent gets all of them |
| `subagents/` | assembled and **registered beside the coder** (`coder/<name>`, `CoderAgent::subagents`). A subagent runs as a child run with its own run id, so the tools that work on the worktree of the run that calls them find none in it: give it tools that need no worktree |
| `mcp.json` | optional: the MCP servers whose tools the agent gets, named `<server>__<tool>` after its own (see [MCP tools](#mcp-tools-from-the-folder)). Connected by the **workers** at startup |
| `schedules/` | read, not run: a warning says so |
| the rest | skills, `limits`, `model:` and the card follow the [authoring layer](../../docs/authoring.md) |

What a folder cannot change is what the tools do (the policy, the checks, the redaction): it changes what the agent
says and offers. Any mistake in the files (not found, a YAML error, an unknown tool, another agent's name, several
agents under `agents/`) stops the process before it connects, exit code 78, with every finding as
`path:line: error: ...` in the one `adam-coder failed` line. A warning does not stop it and is logged as
`path:line: warning: ...`. The `agent files` line says what runs: `source` (`folder` or `embedded`), `path` (the directory that holds `agent/`:
`/etc/adam` for `ADAM_AGENT_DIR=/etc/adam/agent`), `digest`
(`sha256:...`; the shipped `agent/` read from disk has the digest of the embedded copy), `agent` and `warnings`. The
folder must be readable by the runtime user (uid 10001 in the image).

#### MCP tools from the folder

An `agent/mcp.json` in the folder (and one next to each subagent's file) names MCP servers; every worker
connects them once at startup, before it serves, and gives the agent their tools beside its own, named
`<server>__<tool>` (`tools:` in the frontmatter selects among all of them: `linear__*` takes a server's tools).
Servers are streamable HTTP (`type: http`) or local processes (`command`); `type: sse` is refused. The format and the
rules are those of [`adam-mcp`](../../crates/adam-mcp/README.md) and
[MCP tools at run time](../../docs/authoring.md#mcp-tools-at-run-time-built-feature-mcp).

```json
{
  "mcpServers": {
    "search": {
      "type": "http",
      "url": "https://search.example.com/mcp",
      "headers": { "Authorization": "Bearer ${SEARCH_TOKEN}" },
      "tools": ["web_search"]
    }
  }
}
```

* **`${VAR}` and `${VAR:-default}`** in `headers`, `args` and `env` read the process environment (`SEARCH_TOKEN`
  above): put credentials there. A variable that is unset (and has no default) stops the process with exit 78
  naming the variable, never its value.
* **`${VAR}` in a `url`** is refused (exit 78 naming the variable) unless the deployment sets
  `MCP_ALLOW_URL_VARS=true`: the MCP client library logs the URL it dials, so a secret there would reach the logs.
  A URL that is not a secret (`"url": "${SEARCH_URL}"`, so that one folder serves a stack and a cluster) is what the
  flag is for.
* **Which kinds are allowed** is the deployment's: a local process needs `MCP_ALLOW_STDIO=true`, plain `http` to
  another machine needs `MCP_ALLOW_INSECURE=true` (development only); `https` and loopback need nothing.
* **A server that is down** at startup stops the process with exit 69, so a supervisor restarts it until the server
  is up; a mistake in the files or the policy is 78. A tool call that fails is an error result the model reads.
* A folder without an `mcp.json` connects nothing. **The embedded copy has one**: it names the GitHub MCP server, a
  local process (see [GitHub over MCP](#github-over-mcp-read-only)), so a coder on the embedded files needs
  `MCP_ALLOW_STDIO=true` and `github-mcp-server` on its `PATH`, which the image has. A **control plane** serves the
  card and starts runs, which needs no tools, so it connects no server: only `all` and `worker` do.

### Retry safety

Each tool's side effect runs inside `LlmAgent`'s journaled `tool:<call id>`
step, and each is also idempotent by construction, so a call that dies before
its result is journaled (or a transient retry, which starts at a fresh journal
position) does not duplicate anything: `commit_all` is a no-op without changes,
pushing a commit the remote already has is a no-op (so is moving a continued
branch to a commit it already has), and
`CodeHost::open_pull_request` returns the open pull request of the same head.
The one exception is the comment that a red-accepted update of an open pull request adds: a
crash after it was posted and before the call was journaled posts it again when the call repeats.

### Cancel and rate limits

* **CancelTask** fails the run as `cancelled: ...` (A2A `canceled`) at once and
  fires the step's `CancelToken`. `delegate_to_opencode` then sends ACP
  `session/cancel`, gives OpenCode two seconds to end its turn, kills it **and
  its process group** (the commands it started) and waits until it is reaped
  before returning, so nothing outlives the tool. `commit_and_push` and
  `open_pull_request` refuse to act once the token has fired, so a later call of
  the same model turn cannot publish a cancelled run. The child is started in
  its own process group, so a terminal Ctrl-C does not reach it; the coder's
  own shutdown and cancel paths do.
* **429 with `Retry-After`** from the model gateway is carried to the runtime
  (`ModelError::RateLimited` -> `AgentError::Transient` with a `retry_after`):
  the retry waits at least that long, whatever the backoff says.

## Configuration

Environment variables (`src/config.rs` is the reference for the coder's own, and
[`adam-service`](../../crates/adam-service/README.md#environment) for `ROLE`, `DATABASE_URL`, `A2A_BEARER_TOKENS`,
`PUBLIC_URL`, `LISTEN_ADDR`, `WORKERS`, `WORKER_ID`, `MODEL_*` and `MCP_ALLOW_*`, which every agent binary reads the same
way; every problem is reported at once at startup):

| Variable | Meaning | Default |
|---|---|---|
| `ROLE` | what this process runs: `all`, `control-plane` or `worker` (see [Roles](#roles)) | `all` |
| `DATABASE_URL` | Postgres for the run store | required |
| `A2A_BEARER_TOKENS` | comma-separated accepted tokens (fail closed: none = no server) | required by `all` and `control-plane` |
| `PUBLIC_URL` | where clients reach the JSON-RPC endpoint (agent card) | required by `all` and `control-plane` |
| `LISTEN_ADDR` | bind address: the A2A server, or a worker's `/healthz` listener | `0.0.0.0:8080` |
| `MODEL_BASE_URL`, `MODEL_API_KEY` | OpenAI-compatible gateway (with `/v1`) and its key | required by `all` and `worker` (key may be empty) |
| `MODEL` | model alias of the agent | required by `all` and `worker` |
| `OPENCODE_MODEL` | model alias OpenCode uses through the same gateway | `MODEL` |
| `GITHUB_TOKEN` | push and pull request token (a personal access token); only ever sent to the `ALLOWED_REPO_HOSTS`. Must be unset or empty in App mode | one of this or the App's variables, for `all` and `worker` |
| `GITHUB_APP_ID`, `GITHUB_APP_INSTALLATION_ID` | GitHub App mode: the App's application ID or client ID (the JWT's `iss`), and the installation's ID (a positive integer). See [GitHub credentials](#github-credentials-a-token-or-an-app-installation) | both required in App mode, unset in token mode |
| `GITHUB_APP_PRIVATE_KEY_PATH`, `GITHUB_APP_PRIVATE_KEY` | the App's private key, a PEM (PKCS#1 as GitHub gives it, or PKCS#8): a file, or inline (`\n` escapes accepted). Exactly one. Parsed at startup | one required in App mode |
| `ALLOWED_REPO_HOSTS` | comma-separated hosts (`name` for any port, or `name:port`) repositories may live on; the token is scoped to them. The first is also the host `owner/name` stands for when the person writes a repository that way | `github.com` |
| `GITHUB_API_URL` | GitHub REST API root (GitHub Enterprise: `https://<host>/api/v3`; tests and `compose.yaml`: `mock-github`) | `https://api.github.com` |
| `ALLOW_LOCAL_REPOS` | also accept local paths, `file://` and plain `http://` repositories. **Development and tests only** | `false` |
| `WORKSPACE_ROOT` | mirrors, the workspaces of runs, run notes | `/work` |
| `WORKSPACE_SWEEP_SECS` | how often the janitor removes the workspaces of finished runs ([below](#the-workspace-of-a-run)); `0` turns it off | `300` |
| `WORKSPACE_PLACEMENT` | where the files of a run live: `shared`, `affinity` or `isolated` (`a2a-only` is refused; see [Workspace placement](#workspace-placement)) | `shared` |
| `WORKER_ID` | stable identity of this worker (lease identity, and run owner when pinned): 1 to 128 of letters, digits, `.`, `_`, `-`, not starting with `.` | random per process; **required** by `affinity` and `isolated` |
| `WORKERS` | runs advanced concurrently | `4` |
| `MAX_CHECK_CYCLES` | failed `run_checks` before the agent must stop | `3` |
| `CHECK_TIMEOUT_SECS`, `CHECK_OUTPUT_TAIL_BYTES` | limits of one `run_checks` (and of one `run_command`) | `900`, `16384` |
| `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL` | identity of the commits | `adam-coder`, `adam-coder@users.noreply.github.com` |
| `PR_DRAFT` | open pull requests as drafts | `false` |
| `OPENCODE_COMMAND` | the ACP program and arguments | `opencode acp` |
| `MCP_ALLOW_STDIO` | let the folder's `mcp.json` start local processes (`command` servers). The file would decide what this process runs, with its rights: leave it off unless the image ships the server. **The coder image sets it** (it ships `github-mcp-server`, which the shipped `mcp.json` starts), so an `adam-agent` run from that image allows it too: set `MCP_ALLOW_STDIO=false` there to refuse local processes | `false` (the image: `true`) |
| `MCP_ALLOW_INSECURE` | let it reach plain-`http` MCP servers on other machines (`localhost` and loopback never need it). **Development only**: requests and headers cross the network in the clear | `false` |
| `MCP_ALLOW_URL_VARS` | let it write `${VAR}` in a server's `url`. Off because the MCP client library logs the URL it dials (credentials belong in `headers`, where `${VAR}` always works); turn it on only if that log is filtered | `false` |
| `ADAM_AGENT_DIR` | the folder that holds `agent/` (or `agent/` itself): the prompt, card, skills, subagents and `mcp.json`, **read once at startup by every role**; it must be an existing directory (exit 78 naming the variable otherwise). See [A folder at run time](#a-folder-at-run-time-adam_agent_dir) | unset: the copy embedded in the binary |

Everything from `MODEL_BASE_URL` down, except `ADAM_AGENT_DIR` (every role reads that one), is read by the roles that run workers (`all`, `worker`)
only (the `MCP_ALLOW_*` flags too: a control plane connects no MCP server), and arrives in `Config::worker`, a `WorkerConfig` that is `Some` exactly for those roles.
A control plane neither needs nor validates any of it (see [Roles](#roles)).

OpenCode's configuration is generated at startup into
`OPENCODE_CONFIG_CONTENT` (custom `@ai-sdk/openai-compatible` provider at
`MODEL_BASE_URL`, model `OPENCODE_MODEL`, key by reference `{env:MODEL_API_KEY}`,
never inlined) together with `OPENCODE_DISABLE_AUTOUPDATE=1`; see
`src/opencode.rs` for what was verified against the OpenCode sources. The
OpenCode child does not see `GITHUB_TOKEN`, `GITHUB_APP_PRIVATE_KEY`, `DATABASE_URL` or
`A2A_BEARER_TOKENS`, and the checks do not see those or `MODEL_API_KEY`.

### Workspace placement

`WORKSPACE_PLACEMENT` is parsed with `adam_host::Placement` (case-insensitive; unset or blank means
`shared`) by the roles that run workers, together with `WORKER_ID`. It decides where a worker keeps
its files and whether a run stays on one worker
([ADR 0002](../../docs/decisions/0002-workspace-placement.md)):

| `WORKSPACE_PLACEMENT` | Worker root | Runs | `WORKER_ID` |
|---|---|---|---|
| `shared` (default) | `WORKSPACE_ROOT`, one volume mounted by every worker (RWX); guarded by the mirror lock of [`adam-workspace`](../../crates/adam-workspace/README.md) | any worker steps any run | optional |
| `affinity` | `WORKSPACE_ROOT/<WORKER_ID>` | pinned to the worker that first claimed them | required |
| `isolated` | `WORKSPACE_ROOT`, a volume of this worker only (a PVC per worker) | pinned | required |
| `a2a-only` | | refused: every tool of the coder needs a workspace | |

* A pinning placement (`affinity`, `isolated`) makes `serve` build the runtime with
  `ClaimScope::Pinned` (`adam_service::claim_scope_for`) and `worker_id = WORKER_ID` (`RuntimeOptions::claim_scope`,
  `RuntimeOptions::worker_id`); a worker logs its `placement`, `worker_id` and `root` (`workspace placement`). The id must survive restarts (a StatefulSet pod name): a run stays
  with the worker of that name. Without `WORKER_ID` the process exits 78, naming the variable.
* `a2a-only` exits 78 for `all` and `worker`: a host whose agents only call remote agents (the
  orchestrator) can use the placement, the coder cannot. A `control-plane` reads neither variable.
* **Known limitation: a pinned run whose worker never returns is stranded.** No other worker
  claims it and nothing reports it. Adoption is future work. Keep worker ids stable, and do not
  scale a pinned deployment in.
* Without a placement that fits, two workers on two disks fork a run silently: a second clone, a
  second branch and a second pull request (the ADR has the chain).

### Roles

`ROLE` is parsed with `adam_host::Role` (`all`, `control-plane`, `worker`; case-insensitive;
unset or blank means `all`). Anything else is a configuration error (exit 78) that names
`ROLE` and the accepted values. The process registers its parts as components of an
`adam_host::Host`, which starts only those the role runs. The components, the store, the
notifications and the drain are [`adam-service`](../../crates/adam-service/README.md#the-process)'s
`serve`, shared with every agent binary; `adam-coder` reads its files, builds its agent and hands it over
(`src/serve.rs`).

| Role | Starts | Listener | Workspace root |
|---|---|---|---|
| `all` (default) | the A2A server and the workers, in one process: today's behaviour | A2A and `/healthz` on `LISTEN_ADDR` | created |
| `control-plane` | the A2A server over `Coder::control_plane_with`: a `Runtime` with the agent's `CoderStarter` only, used to start, deliver to, cancel and view runs; `run_worker` is never called | A2A and `/healthz` on `LISTEN_ADDR` | **not** created |
| `worker` | `Runtime::run_worker`, and a listener that answers `GET /healthz` (`200 ok`, the route the A2A router serves) and nothing else | `/healthz` only on `LISTEN_ADDR` | created |

Every role also runs the component `notify` (below). The roles meet in the Postgres store (the
run record's version compare-and-swap, and leases), so any number of each can share one
database; the store is what is correct, and `notify` only makes it fast.

**Live events and wake-up across processes.** Every process builds one
[`PgNotify`](../../crates/adam-notify-postgres/README.md) over the store's own connection pool and runs
its listener as the host component `notify` (a worker component in `all` and `worker`, which
stops only after the `worker` component has finished, so the last step's events and signals
are still sent; a control-plane component in `control-plane`). It logs
`listening for notifications` once `LISTEN` is active. With it:

* A run started or answered through a control plane wakes an idle worker in another process at
  once, instead of at its next poll (250 ms).
* A cancel reaches the step running in another process at once, instead of at the worker's
  next read of the run.
* The control plane's A2A stream carries the `Step`, `Progress`, `Custom` and `Artifact` events of a
  run a worker steps as they happen, not only the states and artifacts it finds by polling.

It is Postgres only (MongoDB has no equivalent here, and `adam-service` is Postgres only), needs
no variable, and changes nothing about correctness: `NOTIFY` is at most once and not durable,
polling stays on at 250 ms, and a run completes with the listener gone, only later. The
listener holds one connection of the store's pool, and needs a direct or session-mode
connection: behind a transaction-mode pooler (PgBouncer's default) `LISTEN` silently does
nothing and the system falls back to polling. A library user gets the in-process behaviour from
`Coder::new` and `Coder::control_plane`, or passes their own `LiveSignals` to
`Coder::new_with` and `Coder::control_plane_with`.

By default any worker may lease any run at any step. Several workers therefore need a
[workspace placement](#workspace-placement).

**Required variables by role**

| Variable | `all` | `control-plane` | `worker` |
|---|---|---|---|
| `DATABASE_URL` | yes | yes | yes |
| `A2A_BEARER_TOKENS`, `PUBLIC_URL` | yes | yes | not read |
| `MODEL_BASE_URL`, `MODEL_API_KEY`, `MODEL`, and `GITHUB_TOKEN` or the `GITHUB_APP_*` variables | yes | not read | yes |
| the rest of the table above (`OPENCODE_*`, `ALLOWED_REPO_HOSTS`, `ALLOW_LOCAL_REPOS`, `GITHUB_API_URL`, `WORKSPACE_ROOT`, `WORKSPACE_PLACEMENT`, `WORKER_ID`, `WORKERS`, `MAX_CHECK_CYCLES`, `CHECK_*`, `GIT_AUTHOR_*`, `PR_DRAFT`) | read, defaulted | not read | read, defaulted |

A missing required value is a configuration error (exit 78) listed with every other problem.
What a role does not read it does not validate either: a worker ignores `A2A_BEARER_TOKENS` and
`PUBLIC_URL`, and a control plane ignores a malformed `GITHUB_API_URL` or `WORKERS=0`.

**A control plane needs no model or GitHub configuration.** Starting a run needs only the
agent's name and its `init`, which `CoderStarter` provides (`CoderAgent::init` delegates to it, so
the two cannot disagree). `serve` builds the model client, the GitHub client, the workspaces and
the `CoderAgent` (`build_agent`, which also creates the workspace root) only when
`Config::worker` is `Some`; otherwise it registers the starter only (`Coder::control_plane`). A control plane
never steps a run, so a `run_worker` on it would claim nothing (`adam-runtime` claims only
registered agents, not starters). The library keeps `Coder::new(store, CoderAgent, ..)` for
processes that step runs. See [ADR 0001](../../docs/decisions/0001-library-first-host-roles.md),
decision 6.

`SIGTERM` stops the control plane first (open connections get 10 seconds), then the workers,
without a bound, so they finish and commit the steps they are in. A component that stops on its
own stops the others and ends the process with a `HostError`, exit 70.

### Which repositories, and where the token goes

The repository URL comes from the model, which took it from the user, so it
is treated as hostile input. The GitHub credential (the token, or the App's installation
token) is bound to `ALLOWED_REPO_HOSTS`
twice over, and a third layer decides whether the repository may be used at all:

0. **Only a repository the person named.** `prepare_workspace` refuses, before
   anything else, a repository that is not named in the person's own messages
   of the run (see [the rules](#the-rules-in-code)); quoted findings do not name
   one.
1. `Workspaces::allow_hosts` refuses any other host before a process is
   spawned, a request is made or a credential is asked for (the model gets the
   reason as a tool error). URLs with embedded credentials, ssh/scp forms, and
   anything that is not `https://<host>/<owner>/<repo>` are refused by the
   parser, and git is handed the URL rebuilt from the parsed parts, never the
   raw string.
2. The credentials are a `ScopedToken` for the same hosts (a `HostScoped<GitHubApp>` in App mode, which
   checks the host *before* it signs anything or calls GitHub), which refuses
   every other host even if a caller forgot the check.

Local paths, `file://` and plain `http://` are refused unless
`ALLOW_LOCAL_REPOS=true`, which exists for development and tests; local
remotes never receive the token.

### GitHub credentials: a token or an App installation

A worker authenticates to GitHub one of two ways, **exactly one**
([ADR 0009](../../docs/decisions/0009-github-per-installation-read-through-mcp.md)):

| | token | GitHub App installation |
|---|---|---|
| Variables | `GITHUB_TOKEN` | `GITHUB_APP_ID`, `GITHUB_APP_INSTALLATION_ID`, and one of `GITHUB_APP_PRIVATE_KEY_PATH` (a file) or `GITHUB_APP_PRIVATE_KEY` (the PEM; `\n` escapes accepted) |
| `GITHUB_TOKEN` | required | must be unset or empty |
| Whose | a person's, until it is revoked | the App's, scoped to what the App was granted on the installation; its tokens last an hour |
| Credentials | `ScopedToken` for `ALLOWED_REPO_HOSTS` | `HostScoped<GitHubApp>` for the same hosts |

Both set, an App set that is partial (`GITHUB_APP_ID` without the installation or the key, a key with both its
file and its variable), an installation ID that is not a positive integer, a key file that cannot be read, and a
key that is not an unencrypted RSA key in PEM form (PKCS#1, as GitHub lets the owner download it, or PKCS#8) are
configuration errors: exit 78, every problem listed, the name of the variable and never a value. The key is
**parsed at startup**, so a deployment learns of a bad one when it rolls out, not at the first push. A rotated
key needs a restart. `GITHUB_APP_ID` is the App's application ID or its client ID (the JWT's `iss`).

```mermaid
sequenceDiagram
  participant S as a step (clone, push, pull request)
  participant C as credentials (HostScoped, then GitHubApp)
  participant G as GITHUB_API_URL
  participant R as the redactor
  S->>C: token_for(repository)
  C->>C: the host is in ALLOWED_REPO_HOSTS? (else refused, nothing is signed)
  alt a cached token has more than 5 minutes left
    C-->>S: the cached token
  else none, or about to expire (one caller at a time)
    C->>G: POST /app/installations/{id}/access_tokens (Bearer: a JWT, RS256, iat now-60s, exp now+540s)
    G-->>C: 201 {token, expires_at}
    C->>R: add(token)
    C-->>S: the new token
  end
  S->>G: git: x-access-token:token, REST: Bearer token
```

```mermaid
stateDiagram-v2
  [*] --> Empty: startup, the key is parsed
  Empty --> Fresh: minted
  Fresh --> Expiring: 5 minutes or less left
  Expiring --> Fresh: minted
  Empty --> Empty: a mint failed, nothing cached
  Expiring --> Expiring: a mint failed, the next call tries again
```

A token the App minted is a secret from the moment it exists: the redactor is shared, and the credentials add
each token they hand out (at most 16 are remembered, oldest forgotten first), so a tool result, an error or a
log line that quotes one is scrubbed (the App's PEM and its Base64 body are registered at startup). The key is
hidden from OpenCode and from the project's commands as `GITHUB_TOKEN` is; a key file is a path, and sits
wherever the deployment mounted it (the chart: `/var/run/secrets/github-app/private-key.pem`, read-only, mode
0440, group `fsGroup`).

A refused mint is told to the person as it is for a bad token: `401`, `403` and `404` from GitHub are an
authentication error that names `GITHUB_APP_ID`, `GITHUB_APP_INSTALLATION_ID` and the key, and a run that ends at
one fails naming them (and that the App must be installed on the repository, with write access to its contents
and pull requests). A rate limit is a rate limit (with the wait GitHub asked for), and a `5xx` or a transport
failure is transient. The token endpoint is `{GITHUB_API_URL}/app/installations/{id}/access_tokens`, so GitHub
Enterprise Server and a mock need no other variable. The deployment's own example is
`dev/compose.github-app.yaml` (an init service makes a throwaway key; the mock gives an installation token that
lasts four minutes, so the coder renews it all the time) and `deploy/coder` (`github.auth: app`).

### GitHub over MCP: read-only

The coder **reads** GitHub (the files, branches, commits, issues and pull requests of any repository its credentials
can see, including repositories it was not given) through the official
[GitHub MCP server](https://github.com/github/github-mcp-server), and **writes** only through its own tools. The
shipped `agent/mcp.json` ([ADR 0009](../../docs/decisions/0009-github-per-installation-read-through-mcp.md),
decision 8):

```json
{ "mcpServers": { "github": {
  "command": "github-mcp-server",
  "args": ["stdio", "--read-only", "--toolsets", "context,repos,issues,pull_requests"],
  "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN:-}", "GITHUB_APP_ID": "${GITHUB_APP_ID:-}",
           "GITHUB_APP_INSTALLATION_ID": "${GITHUB_APP_INSTALLATION_ID:-}",
           "GITHUB_APP_PRIVATE_KEY_PATH": "${GITHUB_APP_PRIVATE_KEY_PATH:-}", "GITHUB_HOST": "${GITHUB_MCP_HOST:-}" },
  "tools": ["get_me", "search_repositories", "get_file_contents", "list_branches", "list_commits", "get_commit",
            "search_code", "list_issues", "issue_read", "search_issues", "list_pull_requests", "pull_request_read"] } } }
```

* **Read-only twice over.** The server is started with `--read-only` (it offers no write tool) and the `tools`
  allow-list names twelve reads, so the model sees `github__get_me`, `github__search_repositories`,
  `github__get_file_contents`, `github__list_branches`, `github__list_commits`, `github__get_commit`,
  `github__search_code`, `github__list_issues`, `github__issue_read`, `github__search_issues`,
  `github__list_pull_requests` and `github__pull_request_read`, after the coder's own tools, and nothing that
  writes. Pushes and pull requests go through `commit_and_push` and `open_pull_request`, behind the gate. Reading a
  repository there does not put it in the workspace and does not make it one the person named: it still cannot be
  pushed to ([the rules](#the-rules-in-code)). A tool the server does not list stops the coder at startup (the
  allow-list is checked), so a change of the server's tool names cannot go unseen.
* **The credentials are the coder's.** The same variables, by the names the server reads, in either mode: a
  **token** (`GITHUB_TOKEN`, handed over as `GITHUB_PERSONAL_ACCESS_TOKEN`) or a **GitHub App installation**
  (`GITHUB_APP_ID`, `GITHUB_APP_INSTALLATION_ID`, `GITHUB_APP_PRIVATE_KEY_PATH`; the server signs its own JWT and
  trades it for an installation token). The mode that is not in use is an empty variable, which the server counts
  as unset (*verified* 2026-10-01 against v1.12.2, by source and by running it in both modes), so no variable is
  conditional. `GITHUB_MCP_HOST` (empty: github.com) is the server's `GITHUB_HOST`, for GitHub Enterprise
  (`https://<host>`; the coder's own `GITHUB_API_URL` is separate).
* **The App's key must be a file.** `GITHUB_APP_PRIVATE_KEY` (the key in a variable) is not handed to a child
  process. A server that has an App and no key does not start, and the coder stops at startup (exit 69, with a "cannot
  connect ... Broken pipe" message that does not say why: run the server by hand to read its own complaint);
  the chart mounts the key as a file (`GITHUB_APP_PRIVATE_KEY_PATH`) and needs nothing. A deployment that keeps the key in a
  variable has no GitHub MCP server: mount a copy of the folder without that server (`ADAM_AGENT_DIR`).
* **A deployment must allow it.** The server is a local process: `MCP_ALLOW_STDIO=true` and the binary on `PATH`
  (the image has both; a control plane connects nothing). Without the variable the process stops at startup with
  exit 78 naming it, and with it and no binary with exit 69, never in the middle of a run, and no message holds a
  credential. The image pins the binary by tag and digest (v1.12.2) and its build and the container smoke test list
  its tools over stdio, with no credential at all (the server lists tools without calling GitHub, *verified*; a
  *call* with no credential starts its OAuth login, so no credential is never a way to run).
* **Development and the e2e do not start it.** The compose file mounts `dev/coder-agent/mcp.json` over the folder's
  `mcp.json`: the same twelve tools from `mock-github-mcp`, a WireMock of the server's streamable HTTP endpoint over
  plain `http` (`MCP_ALLOW_INSECURE=true`), behind a bearer. The default script of `mock-coder` reads
  `github__list_branches` of `local/sandbox` right after `prepare_workspace`, and `dev/coder-e2e.sh` asserts the mock
  saw `initialize`, `tools/list` and exactly one such call, and that the model was given the answer.

```mermaid
sequenceDiagram
  participant P as the coder process (worker)
  participant S as github-mcp-server (child, stdio)
  participant G as GitHub
  participant M as the model
  P->>P: MCP_ALLOW_STDIO set? (else exit 78), the command on PATH? (else exit 69)
  P->>S: start, env: the coder's credentials by the server's names
  P->>S: initialize, tools/list
  S-->>P: 25 tools (read-only, four toolsets): the twelve of the allow-list must all be there
  Note over P,S: serving: the model is offered the twelve as github__<name>
  M->>P: github__list_branches {owner, repo}
  P->>S: tools/call
  S->>G: REST or GraphQL, the token or the installation token
  G-->>S: the answer
  S-->>P: the result (an error result is the model's to read)
  P-->>M: the tool result
```

### Secrets in output

Text from things this process does not control (OpenCode's stderr tail in an
"ACP agent exited" error, a check's output, a provider's error body) reaches
clients as run errors, events and tool results. A `Redactor` built from the
configuration replaces the *values* of `MODEL_API_KEY`, `GITHUB_TOKEN` or the App's private key (the PEM and its
Base64 body), each only where the role holds them, every
`A2A_BEARER_TOKENS` entry and the `DATABASE_URL` password (and their Base64
forms), and every installation token an App mints, from the moment it is minted (the redactor is shared, and
the credentials add each token they hand out, up to 16 at a time), with `[redacted]` in tool results and errors, in OpenCode's and the
checks' progress lines, in the `checks` artifact's findings and summary, in the agent's final
failure message, and in the process's own `adam-coder failed` log line.
A failed step's error crosses one boundary (`boundary_error` in
`src/agent.rs`): its whole cause chain is flattened into the message, scrubbed,
and cut to `MAX_FAILURE_TEXT` (2048 bytes, ` [truncated]` appended) *after*
scrubbing, so a secret on the cut cannot leave its front half. The retry hint
survives; the `source` does not. It is exact-value replacement, not a detector: a secret that
was transformed (hashed, split) is not found, and values shorter than 4
characters are not registered.

SIGTERM stops accepting connections and lets in-flight steps finish and commit
(see [Roles](#roles) for what stops in which order); a step cut short by a hard kill
is taken over by the next start when its lease expires. Logs are JSON on stdout
(`RUST_LOG` filters).

Deployment: `docker/coder/Dockerfile` and the chart in `deploy/coder/`.

## Errors

The library errors it composes are classified (see
[`adam-error`](../../crates/adam-error/README.md)); this crate adds `ConfigError`
(`Invalid`: the same environment never works; it lists every problem and never
a secret). A component of the process that stops while still needed (the server
or the workers) is an `adam_host::HostError`, which names the component and is
`Internal`.

A failure ends the process with one structured log line, `adam-coder failed`
(JSON on stdout, fields `error`, the whole scrubbed cause chain, and `code`),
and nothing on stderr. The exit code (`src/exit.rs`, `exit_code`) comes from
walking the `anyhow` chain from the outside in and taking the first match (the walk is
`adam_service::exit_code_with`, which knows the errors of the service; `src/exit.rs` adds the coder's own, a
workspace, the agent files and their assembly):

| Exit code | Meaning | Root cause |
|---|---|---|
| 0 | clean shutdown after SIGTERM or Ctrl-C | not an error |
| 78 (`EX_CONFIG`) | configuration; do not restart | `ConfigError`, or an `Invalid` `StoreError`, `OpenAiConfigError`, `WorkspaceError` or `RuntimeError` |
| 69 (`EX_UNAVAILABLE`) | a dependency is unreachable; restart later | a `Transient`, `RateLimited` or `Conflict` one of those, such as Postgres at boot |
| 71 (`EX_OSERR`) | the OS refused something | an `io::Error` with no typed error above it: a listener that cannot bind |
| 70 (`EX_SOFTWARE`) | internal | `HostError` (a component stopped, panicked or ended before shutdown, whatever its own cause), a panicked task, or a `Corrupt` or `Internal` typed error (including `OpenAiConfigError::Client`) |
| 1 | anything else | for example `NotFound`, `Rejected`, `Unauthenticated` (a bad `GITHUB_TOKEN`, or a refused App) or an untyped error |

The typed errors it looks for are `StoreError`, `OpenAiConfigError`,
`WorkspaceError`, `RuntimeError`, `HostError`, `AgentFilesError` and the assembly's (including an MCP server
that is down: 69). Because the walk goes
outside in, an unreachable Postgres is 69 although an `io::Error` is at the
bottom of its chain. The values are BSD `sysexits.h`'s, *unverified* (from
memory).

At run time, tool failures are flattened once where they cross to the model:
a `WorkspaceError` or `AcpError` becomes `ToolError::Transient` when
`is_retryable()` and `ToolError::Permanent` otherwise, with the chain printed
once (`adam_error::report`) and then scrubbed.

## Tests

Everything is offline (`cargo test -p adam-coder`; with
`ADAM_TEST_POSTGRES_URL` set the Postgres variants run too, each case in a
database of its own, so the role needs `CREATEDB`):

* `tests/e2e.rs`: a real A2A client and server, the scripted `MockModel`, a
  local bare git repository as the remote, the `adam-acp` fake agent as OpenCode
  and a wiremock GitHub. **Every case runs once per store** (`memory::*`, and
  `postgres::*` when the variable is set): the happy path (working, progress,
  checks, artifacts (`checks`, the `checks` bound to the pushed commit, `branch`, `pull_request`), completed, branch on the remote, PR request at the mock),
  the `input-required` round trip, a plain-text stop that delivered nothing (the
  owner's "Hi": `input-required` with the text as the question, the answer reaches the
  model and the run goes on to a pull request; no text at all; checks then text;
  a message in the context of a parked run continues it; cancel while parked), an invented
  repository refused and the model sent to `ask_user`, a repository quoted in `untrusted` findings not
  named while the one in the `request` fence is, an empty stop after work asking what to do next, red checks
  with cycles left then text (a question, not a failure), red checks N times (failed, findings, no PR),
  the explicit-acceptance path, ownership, the wrong bearer token (401 at the
  coder's own router; card and `/healthz` open), five crash points (inside
  `run_checks`, inside `commit_and_push`, after it was journaled, inside
  `open_pull_request`, inside `delegate_to_opencode`) with a second worker taking
  over: one commit, one push, one pull request, one `checks` artifact per
  commit (the one bound to the pushed commit is emitted once), the same worktree; OpenCode crashing on every attempt
  (run fails after the retry budget, with the child's stderr) and once
  (retried, completes); two concurrent tasks on one repository (two branches,
  two pull requests); a GitHub 401 (run fails and names `GITHUB_TOKEN`, or the `GITHUB_APP_*` variables when the coder is an App
  installation); an App installation token that is minted, used for every call to the repositories' API and never
  echoed (a tool result that quotes it is scrubbed).
  Asking with choices, over A2A with the screen's real catalog (a copy of the web's, in
  `adam-ui`'s fixtures): three questions as one form (`input-required` with the question as text and one
  `application/a2ui+json` part, a Choices of db, auth and deploy under the screen's `catalogId`), the person's
  A2UI action as the answer, the model's next words and the run going on to the end; the catalog read again
  over a fake thread-tools endpoint when the message only names its digest, and the tools of that endpoint offered
  to the model and called; a screen the coder cannot read (no inline catalog, no grant) getting the options as
  text, with no A2UI part.
* `tests/binary.rs`: the `adam-coder` binary as a process. All problems of a
  bad configuration reported together with exit 78; Postgres unreachable at boot
  (a clear "connecting to Postgres: ..." chain in exactly one `adam-coder failed`
  line, exit 69, nothing on stderr, no password, no panic; sqlx retries the
  connection for its 30 s acquire timeout first); with Postgres: the card and
  `/healthz`, a clean exit 0 on SIGTERM, and
  SIGTERM in the middle of OpenCode's turn (the process waits for the step,
  commits it, exits 0; a second process over the same database and workspace
  finishes the run with one commit, one push and one pull request). The last
  one runs the whole binary against a wiremock model (`/chat/completions`) and
  a wiremock GitHub reached through `GITHUB_API_URL`, under the production
  repository policy. Roles: an unknown `ROLE` (exit 78, names the variable and the accepted
  values); the variables each role is missing (exit 78); a `worker` that starts without
  `A2A_BEARER_TOKENS` and `PUBLIC_URL`, answers `/healthz` and 404 to everything A2A, and
  creates its workspace root; a `control-plane` that starts with **no model, GitHub or
  workspace variables**, serves A2A and never creates the workspace root; and **a control
  plane and a worker as two processes over one database**: the control plane is given no model
  or GitHub configuration and its log shows none; the task is sent to it before the worker
  exists and waits unclaimed (the model is not asked, the run's version does not move), then the worker starts, steps it to a
  branch and a pull request, and the control plane's stream reports the artifacts and
  `completed`, and a `working` update carrying the text of the worker's progress line (an update of
  its `prepare_workspace` step, `preparing a worktree of ...`), which exists only as a live event and so proves events
  crossed the two processes over `NOTIFY`. Both processes log `listening for notifications`.
* **GitHub over MCP** ([above](#github-over-mcp-read-only)). `tests/agent_files.rs`: the shipped `mcp.json` is the
  GitHub server over stdio with `--read-only`, four toolsets and exactly the twelve reads (none starts with a verb
  that writes), and hands the child only the credentials the coder holds (never the key itself); a deployment that
  does not allow local processes does not get it and is told which variable decides. `tests/binary.rs`: the worker
  on the embedded files stops with 78 without `MCP_ALLOW_STDIO` and 69 without the binary, naming the server and
  never a credential; and, with `ADAM_TEST_GITHUB_MCP_SERVER` set to a `github-mcp-server` (CI copies it out of the
  image the Dockerfile pins; the case skips itself without it), the whole binary connects the **real** server as a
  child and a call reaches a mock of GitHub with the right credential: the model is offered the twelve tools in
  order after the coder's own and none that writes, `github__get_me` returns the mock's user, no secret is in a log,
  in both modes (a token, and an App with an empty token variable, where the server trades a JWT at the
  installation's token endpoint). The other tests of those two files use the shipped folder without its `mcp.json`
  (`common::plain_folder`), since a test machine has no such binary; `adam-mcp`'s `wiremock_compose` connects
  `mock-github-mcp`; `dev/coder-e2e.sh` and `docker/coder/test/*.sh` check the stack and the image.
* `tests/agent_files.rs`: the prompt, limits and card in `agent/instructions.md`, against the Rust they replaced and
  against the instruction snapshot. `the_prompt_carries_the_rules_the_code_relies_on` runs on the assembled prompt;
  `the_prompt_gives_the_agent_a_name_and_asks_for_plain_words` pins what #55 added (the persona lines first, the
  greeting rule, the plain-words answer, what it cannot do, no tool list); the prompt equals
  `tests/fixtures/agent/prompt.txt` (the **instruction snapshot**: it began as the old Rust constant, and #55 is its
  first deliberate change) for several limits, with `{{display_name}}` as `Coder`, but for its final newline, which the
  loader drops from every body; the limits, the tool order and the model alias are the old ones; a run through a runtime on a `MockModel` sends the old system prompt, tools and `max_output_tokens` and
  journals the old step names (`model:0`, `tool:<call id>`, so a run journaled before replays); the assembly's
  card equals `agent_card`. A model alias the assembly refuses (empty, whitespace) is an error from `try_new`.
  The card is also pinned by a unit test in `src/app.rs` against `tests/fixtures/agent/card.json`, the card as the
  Rust literal built it.
  The second half runs the coder on **a copy of the shipped `agent/` in a temp dir** (`ADAM_AGENT_DIR`'s folder,
  `AgentFiles::load`): it assembles to the embedded agent (same digest, same `AgentInfo`, same card); **editing
  `instructions.md` changes the system prompt the model is sent** (and the limit is still the process's); the card
  comes from the folder; a folder named `other` is refused naming `name: coder`; a folder without
  `max_check_cycles` and one with a tool the coder does not have fail at assembly naming them; `tools:` narrows
  the tools; and a `subagents/reviewer.md` is registered as `coder/reviewer`, called by the model, run as a child
  run and its text comes back to the coder; **`a_greeting_gets_a_greeting_and_the_folder_changes_what_it_says`** runs
  "hi" through a runtime on a model scripted by the prompt itself (it greets from the two persona lines): the run waits
  with the greeting as its question, no tool ran, and a folder with another `display_name` and summary changes the
  answer; `the_display_name_var_is_the_name_in_the_prompt` renames the prompt. The unit tests of `src/files.rs` (a missing folder, every diagnostic
  of a broken one in the message once, another name, warnings), `src/config.rs` (`ADAM_AGENT_DIR` read by every role,
  must be a directory, reported with the other problems) and `src/exit.rs` (78 for the files and their assembly)
  cover the rest (the exit-code walk, the shared configuration, the roles' components and the Postgres-backed `serve`
  are tested in [`adam-service`](../../crates/adam-service/README.md#tests); here only what the coder adds). In `tests/binary.rs`: a missing folder (exit 78, names the variable), a folder with two broken
  subagents (exit 78 for every role, before anything connects, both findings as `path:line` in the one failure line),
  another agent's name, a worker whose folder cannot be assembled (exit 78, names the var; Postgres), a control
  plane serving the card of the folder with the `agent files` line and the warning logged (Postgres), and the
  embedded copy logged as `source=embedded`.
* `src/tools/delegate.rs` (unit): OpenCode's tool calls as child steps (the kind and icon of each ACP kind, the state of each status, a call moved and ended by its updates, what OpenCode says scrubbed and cut, a call that never ended closed with the turn) and `tests/tools.rs`' `delegate_to_opencode_streams_updates_and_returns_the_summary` (the call is a `subagent` step labelled OpenCode, the fake agent's tool call is a child with the edit icon that ends `completed`, its reply is a `message` child and the progress lines are updates of the call's own step); `tests/agent_files.rs` and the card golden pin `steps/v1` and `text-stream/v1` on the card. The tests' models stream too (the agent calls `stream`): a model that does its work in `complete` only is not asked for it any more, so `HangingModel` of `tests/e2e.rs` and the greeting model of `tests/agent_files.rs` do it in `stream` as well.
* `src/tools/files.rs` (unit) and `tests/tools.rs`: the file tools. `confine` has a case each for `../`, an absolute path,
  `.git/x` and `.GIT/x`, a symlink to a file outside, a symlinked directory outside, a write through a symlink that
  stays inside, a link into `.git`, a directory and a file in the wrong place; `read_file` (a range with numbers, a
  range past the end, a cut at 256 KiB also inside a multi-byte character, a binary file, a directory),
  `write_file` (parents, replace, the mode kept, nothing left behind, the 1 MiB limit, the progress line), and
  `apply_patch` (two files, one a new one; a hunk that does not match changes nothing; a malformed, empty, oversized or
  NUL patch; a patch for `.git/config`, `.GIT/hooks`, `../x`, a path through a symlink, a symlink created by the patch, a
  rename into `.git` and a binary patch, each refused with the worktree and the repository's config untouched; wrong
  hunk counts applied by their lines). A patch applied or a file written is seen by the next `run_checks`, and a commit
  after a later edit is not bound to those checks. `tests/e2e.rs`'
  `the_coder_fixes_a_line_with_apply_patch_and_opens_the_pull_request` (per store): `read_file`, `apply_patch`,
  `run_checks`, `commit_and_push` and `open_pull_request` over A2A with OpenCode unavailable, the checks artifact bound
  to the pushed commit. The compose scenario is `SCENARIO=files sh dev/coder-e2e.sh` (the `[mock:files]` script).
* The workspace of several slots: `tests/tools.rs` (`a_second_repository_joins_the_workspace_and_repo_says_which_one`: a
  second repository is added next to the first, a missing `repo` with two slots is an error that lists them, a slot is chosen by
  name and by the repository's address, a file of one slot is not in the other, writes and patches go where `repo` says,
  progress lines name the slot, asking for a repository again keeps its slot and its work;
  `checks_and_pushes_are_per_slot_and_the_gate_binds_by_the_tree`: each push goes to its own remote, the `checks` artifact
  names its repository, the same tree checked in one slot binds a commit in the other and a different tree does not;
  `a_pull_request_needs_the_most_recent_check_of_its_code_whichever_slot_ran_it`: green in the other slot opens it, a red
  afterwards on the same code refuses it; `a_run_that_began_in_the_old_layout_keeps_working`), the unit tests of
  `src/tools/notes.rs` (the most recent check on a tree decides whatever slot ran it, the history keeps 32 and a replayed call
  replaces its own record, notes from before the history decide by `last`);
  `tests/janitor.rs` (what a sweep removes and keeps: done, failed and unknown runs, the legacy worktree; open runs, a
  directory that is not a run, the notes, the mirror and the run's branch; a cancelled sweep; the component sweeping at start and
  again and stopping when told to; a janitor that is off waiting for the stop); `tests/binary.rs`
  (`the_janitor_removes_the_workspace_of_a_finished_run_and_keeps_an_open_one`, with Postgres: the process sweeps on its
  own, a run that finishes later loses its workspace, SIGTERM stops it with exit 0; `a_sweep_of_zero_seconds_turns_the_janitor_off`;
  a bad `WORKSPACE_SWEEP_SECS` exits 78 with the other problems) and `src/config.rs`.
* GitHub credentials: the unit tests of `src/config.rs` (a token is one way; an App is the other, with the key from a
  file or from the variable, in PKCS#1 or PKCS#8, `\n` escapes accepted; both, a partial App set, an installation ID that is
  not a positive integer and a key given twice are every problem at once and never a value; a key that cannot be used is
  refused at startup naming the variable and not the key; a control plane reads none of it, bad values included), of
  `src/redact.rs` (a secret added later is scrubbed by every clone, only the latest 16 are kept, a token is registered as it is
  handed out, an App's key is redacted as its PEM and as its body) and of `src/repos.rs` (an App mints at the API root, for the
  allowed hosts only); in `tests/binary.rs`, `a_github_app_configuration_is_checked_at_startup_and_exits_78_with_every_problem`
  and `a_github_app_installation_gets_its_token_minted_and_opens_the_pull_request` (with Postgres: the real binary against a
  mock that hands a token for a JWT, every call to the repositories' API carries it, and no secret is in the output); in
  `tests/tools.rs`, `a_token_minted_while_the_process_runs_is_scrubbed_from_what_the_tools_return`; in `tests/e2e.rs`
  (per store), `a_refused_github_app_fails_the_run_naming_its_own_variables`. The compose scenarios run twice, the second time
  as an App (`GITHUB_AUTH=app`, `-f dev/compose.github-app.yaml`: an init service makes a throwaway key), and assert that the
  mock saw the trade and that every call to `/repos/...` carried the installation token and never the JWT.
* Scratch projects: `tests/tools.rs` (`a_scratch_project_is_built_checked_and_committed_locally`: the file tools, `run_checks`
  (an artifact with no `repository`), `run_command` (a stray file and a sneaky commit are undone, as in a worktree) and OpenCode
  work in it, `commit_and_push` is a local commit with no artifact and no remote branch, `open_pull_request` is refused and
  points to `publish_scratch`; `a_scratch_project_needs_a_plain_name_that_no_repository_has`;
  `publish_scratch_works_only_on_a_repository_the_person_named`: refused before any mirror is made;
  `a_scratch_project_published_to_an_empty_repository_keeps_the_checks_it_passed`: the empty first commit is all `main`
  holds, the verdict bound to the pushed commit is the one the project earned (the same tree), the pull request is against
  `main`, and the project says where it went when it is changed afterwards; `publishing_twice_is_the_same_publication`;
  `a_publication_that_died_after_the_first_commit_goes_on`; `a_repository_that_has_files_needs_a_directory_or_permission_to_overwrite`
  (a hostile `path` is refused, a collision lists what is in the way and changes nothing, `overwrite` replaces what differs and
  nothing else); `an_empty_project_is_not_published_and_leaves_the_repository_empty`; which project to publish when there are
  several; `a_cancelled_run_publishes_nothing`; `the_history_of_the_project_is_not_carried_into_the_repository`),
  `tests/e2e.rs`' `a_scratch_project_is_published_to_the_repository_the_person_names` (per store: build and check, the run parks
  on the question with nothing pushed, the answer names an empty repository, publish, push, pull request),
  `tests/janitor.rs`' `a_scratch_project_goes_with_its_run_unless_the_run_is_waiting`, and the compose scenario
  `SCENARIO=scratch sh dev/coder-e2e.sh` (the `[mock:scratch]` script, which `wiremock_compose` of `adam-model-openai` plays
  against a real WireMock).
* `tests/tools.rs`: each tool against real worktrees, including the hostile
  `repo_url` shapes against the production repository policy, malformed arguments,
  `prepare_workspace` refusing a repository the person did not name (the refusal names
  `ask_user`, nothing is created) and accepting each written form of a named one.
  The unit tests of `src/tools/named.rs` pin the normalisation (every spelling, the
  sandbox address of the vendored e2e mocks, local paths, default ports, `www.github.com`,
  the configured default host) and the removal of `untrusted` fences (3 and 4 backticks, tildes,
  unclosed, the exact shape of the orchestrator's rework prompt); those of `src/agent.rs` pin what
  counts as the person's words (assistant text, other tools' results and colliding call ids do not), what
  a continued conversation adds (a repository named in an earlier task is named, the omission marker is
  skipped and nothing that only starts like it is, parts are read one by one), which `commit_and_push`
  results name a pushed branch, and the format of the stop's call id. `a_later_task_continues_the_pushed_branch_and_reports_the_same_pull_request`
  in `tests/tools.rs` is the rework at the tool level (refusals of a branch that was not pushed here, of
  another repository's, of one outside `agent/`, of one that is not on the remote; the continued worktree
  has the first task's work; one branch, two commits, one pull request reported as already open), and
  `a_second_task_continues_the_first_tasks_branch_and_pull_request` and
  `a_continued_task_refuses_what_only_the_model_or_a_fence_mentions` in `tests/e2e.rs` are it over A2A with
  `referenceTaskIds` (the model of the second task is shown the first's conversation, `continued_from` says
  which run it continued, the repository of the first task is named in the second without being repeated;
  a repository only the model or an `untrusted` fence mentions, and a branch that was not pushed here, are
  refused; `a_continued_task_finds_the_branch_in_the_history_when_the_earlier_notes_are_gone` removes the notes
  of the first run and shows the branch is still found from the two last lines of its `commit_and_push` result, and
  `a_continued_task_with_red_checks_leaves_the_branch_and_its_pull_request_alone` that the failed verdict says the
  pull request was not updated). The unit test `a_same_size_rewrite_within_the_index_tick_is_in_the_tree_id` in `src/tools/gitcli.rs`
  pins the fix of a flaky `checks_then_commit_binds_a_passing_verdict_to_the_pushed_commit`: the copy of
  the index that the tree id is computed in keeps the index's mtime, or git trusts the stat data of a file
  rewritten with the same size in the same clock tick and the tree holds its old content.
* `an_unnamed_repository_is_not_probed_for_its_default_branch` (no request, no mirror, for an unnamed
  repository with `base_branch` left out), `a_continued_branch_keeps_the_base_of_its_pull_request`,
  `a_comment_that_fails_leaves_the_update_delivered_and_the_note_is_posted_once`,
  `run_command_undoes_changes_to_the_git_configuration_and_to_refs`,
  `run_command_still_works_when_the_tree_cannot_be_computed` and
  `a_project_dependency_is_to_be_installed_and_a_nested_not_found_is_a_failed_check` are in `tests/tools.rs` too.
* `run_command`, the shell and the missing toolchain are tested in `tests/tools.rs`
  (`run_command_looks_around_without_reporting_checks_or_using_cycles`: no artifact, no cycle, `cat` of a
  missing file five times with a budget of three; `run_command_undoes_a_change_and_says_where_changes_go`: a new
  file, a second edit of an already modified file, deletions and a nested repository, a commit, a new branch, a
  reset, `sed -i`, each undone with the run's uncommitted work intact to the byte, ignored output allowed, reads
  that look like writes allowed; `a_missing_toolchain_is_reported_and_costs_nothing`: from both tools and from
  inside a script, no cycle, no artifact, a really failing check still costs one;
  `both_tools_run_bash_when_there_is_bash`), in `tests/e2e.rs`
  (`a_question_is_answered_from_a_look_around_and_costs_no_check_cycles`: the owner's thread, with a repository
  whose default is `master`: a guessed base refused with the list of branches, the default taken when it is left
  out, a missing file, a missing `mvn`, `git branch -r`, and a run that parks as a question with a budget of one
  cycle) and in `src/tools/shell.rs` (`missing_tool` for dash and bash lines, `login_shell`).
  The tests use a name no image has for the missing tool (`nosuchbuild`): a CI image that carries `mvn` would
  otherwise run it.
* `tests/environment.rs`: the three tools that run a process go through a fake `Environment`
  (`FakeEnvironment` in `tests/common/mod.rs`): the command that runs is the one its session prepared (a variable the session
  adds is seen by a check and by the fake OpenCode), the spec has the slot's path and hides the right names and carries no
  secret, a check that times out and an OpenCode that is cancelled each tell the session which command to kill, what `ensure`
  says shows as steps of the tool call with the secret scrubbed from a detail, an environment that cannot be made (a build
  that failed with its log, a runtime that is down) is a permanent or a transient result for the model that runs nothing, uses
  no check cycle and records nothing, and is made again at the next call, and a cancel stops the wait for `ensure`.
* `tests/janitor.rs` also covers the environment: it is released before the workspace is removed (and never for an open run),
  a release that fails keeps the workspace for the next sweep, and what an environment holds for runs that are over or unknown
  is released without a workspace (`orphans`), once, leaving an open run's and a name that is no run.
* `tests/tool_specs.rs`: each tool's `ToolSpec` equals `tests/fixtures/tool-specs/<tool>.json`, the JSON of
  the hand-written tools, so a change to what the model is told is a reviewed diff. The one expected difference
  is normalised: an optional argument is `"type": ["string", "null"]` in a derived schema. Regenerate with
  `ADAM_UPDATE_SNAPSHOTS=1 cargo test -p adam-coder --test tool_specs`. The same file checks that the
  agent refuses to build without the `ToolEnv` as state.
* `adam-workspace/tests/workspace.rs`: the host allowlist, local paths, scoped
  tokens, and a wiremock "evil" git host that must never be contacted.
* `tests/binary.rs` also covers placement: a pinning placement without `WORKER_ID`, `a2a-only` and
  an unknown placement each exit 78 naming the variable and connecting to nothing, and an
  `affinity` worker creates `WORKSPACE_ROOT/<WORKER_ID>` (with Postgres).
* unit tests: configuration (including `ROLE`: the default, each value, an unknown one, the
  variables each role requires, and that `Config::worker` is `Some` exactly for the roles that
  run workers; and placement: the default, each variant, `WORKER_ID` required by the pinning
  ones and validated as a safe name, `a2a-only` and unknown values refused, a control plane
  ignoring both), the folder each placement gives the workspaces (`src/repos.rs`), prompt, OpenCode config (the spec for an environment to prepare, and the ACP command made of what it
  prepared), shell execution (timeout
  kills the process group and tells the session, output tail, cwd confinement, hidden secrets, a spec the environment
  refuses, a program that cannot start), run
  notes, the exit code of each root cause (`src/exit.rs`), and the scrubbing
  and bounding of failure text (`src/agent.rs`, `src/redact.rs`).

The fake agent binary is built by the tests themselves (`CARGO_BIN_EXE_*`
exists only inside `adam-acp`): `tests/common/mod.rs` runs
`cargo build -p adam-acp --bin adam-acp-fake-agent` with the same profile and
target directory as the running test executable (derived from
`current_exe()`), through `$CARGO`.

## Live smoke test (manual, not run in CI)

Proves the real chain: gateway, OpenCode and GitHub. Needs a sandbox repository
you can push branches to and open pull requests in (for example
`you/adam-coder-sandbox`, with a `README.md` and a trivial check such as a
`justfile` with `test:` or a `cargo test`), a GitHub token with `contents` and
`pull requests` write on it, an OpenAI-compatible gateway with a model that can
call tools, and `opencode` on `PATH`.

```sh
docker compose up -d --wait postgres     # from the repository root
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/adam_test
export MODEL_BASE_URL=https://your-gateway.example/v1
export MODEL_API_KEY=...
export MODEL=your-model-alias
export GITHUB_TOKEN=github_pat_...
export A2A_BEARER_TOKENS=dev-token
export PUBLIC_URL=http://127.0.0.1:8080/
export WORKSPACE_ROOT=$(mktemp -d)
cargo run -p adam-coder                  # JSON logs on stdout
```

In another terminal (the wire shape is A2A 1.0 JSON-RPC over SSE):

```sh
curl -N http://127.0.0.1:8080/ \
  -H 'Authorization: Bearer dev-token' -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":"1","method":"SendStreamingMessage","params":{"message":{
        "messageId":"m1","role":"ROLE_USER","parts":[{"text":
        "In https://github.com/you/adam-coder-sandbox (base branch main), add a file hello.txt containing hello. Run the repository checks before opening a pull request."}]}}}'
```

Expected: a stream of status updates whose messages include OpenCode's tool calls (`Write ...`, then
`Write ...: done`) and `running checks: ...`, then artifacts `checks` (twice: of `HEAD`, then bound to the pushed commit), `branch` and `pull_request`,
then `TASK_STATE_COMPLETED`. Send only "Hi" instead and the task ends `TASK_STATE_INPUT_REQUIRED` with
the model's question, which should be a greeting that says the agent's name (`Coder`) and what it does in one
sentence and asks which repository and what to change ([Who it is](#who-it-is)); "What is your name?" and "List me all
your tools" (each a new task, or an answer to the open one) should be answered in plain words, with no tool signatures.
That is what the instructions ask for; that a live model follows them is *unverified* until this has been run. Verify:

* the pull request URL from the artifact opens on GitHub, from a branch
  `agent/<run id prefix>` with one commit, and its body has a summary and a
  verification section;
* `git ls-remote` on the sandbox shows the branch;
* the logs never contain the tokens;
* kill the process mid-run (`kill -9`), start it again: the run continues, and
  no second pull request appears;
* re-run with a task whose check cannot pass (`... and make `false` pass`) and
  `MAX_CHECK_CYCLES=2`: the task ends `TASK_STATE_FAILED` with the findings and
  opens nothing.

For a run without a real gateway or GitHub, `compose.yaml` provides a mock
model, a mock GitHub API and a local git remote; see "Local development" in the
repository README. The models are scripted: a task ends in a branch on the git
remote and one pull request on the mock GitHub, and `dev/coder-e2e.sh` (run by
`.github/workflows/coder.yml` on the image it builds) checks exactly that.

Record the pull request URL and the log excerpts in the pull request that lands
this change. (Not run by the author of this crate: no gateway or GitHub access
in the environment it was written in.)
