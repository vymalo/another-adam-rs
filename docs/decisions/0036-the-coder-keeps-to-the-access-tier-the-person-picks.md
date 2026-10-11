# 0036. The coder keeps to the access tier the person picks

Status: **Accepted** (2026-10-11)
Implements the A2A extension `https://agents.vymalo.com/a2a/extensions/access-tier/v1`, designed in
`vymalo/another-agentic-system` ([ADR 0061](https://github.com/vymalo/another-agentic-system/blob/main/docs/decisions/0061-the-person-picks-an-access-tier-for-each-message-the-agent-keeps-to-it.md),
[`docs/api/access-tier-v1.md`](https://github.com/vymalo/another-agentic-system/blob/main/docs/api/access-tier-v1.md)).
Amends [ADR 0008](0008-a-workspace-holds-several-repositories.md) (a repository is granted only when the person named it or
agreed) and [ADR 0009](0009-github-per-installation-read-through-mcp.md) (`create_repository` is off unless
`CREATE_REPO_OWNERS` names the owner, and asks every time). Builds on [ADR 0006](0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md)
(the extensions) and [ADR 0017](0017-a-github-app-works-on-every-account-it-is-installed-on.md) (an App on several accounts).

## Context

The owner, 2026-10-11, about the coder: *"The goal is for the agent to be autonomous."* A person could not say how much the
coder may do alone: it asked a form before each repository it was not given, could not merge, and could not make a repository
unless the deployment named the owner. Asked which widenings of GitHub he approves, he chose all of them: *"Merge tool, Create
repos by default, No consent for bare names, Only via access tiers, Even renaming or deleting repos should be allowed, but only
via bypass tier."* That is the approval for everything below, and every part of it is gated by the tier the person picks **for
each message**.

The orchestrator defines the extension and the web draws the chip. This ADR is the agent's side: what the card says, what the
coder reads, what each tier lets it do, and how that is made to hold.

## Decision

1. **The card lists the extension** with `tiers: [plan, edits, auto, bypass]` and `default: auto`
   (`ExtensionConfig::access_tier`, `AccessTier` and `AccessTiers` in `adam-a2a`). `AccessTier` is a closed enum.
2. **The backend reads the tier of a message that activated the extension** (`RuntimeTaskBackend::with_access_tiers`,
   `crates/adam-a2a-runtime/src/tier.rs`). The metadata must be `{"tier": "<one of the four, lower case>"}` and a tier the card
   honours, or the message is refused as `InvalidParams` **before anything is written**. The tier becomes the run's inbound
   context key `vymalo.accessTier`, so the newest message wins from the agent's next step. A message that did not activate the
   extension carries no tier, whatever its metadata says. To a caller that activated it, a task says the tier it works under in
   its metadata (the answer of `SendMessage`, `GetTask`, the first frame of a stream) and a stream says it again when it changes.
3. **A task never inherits a tier.** A task that continues another is started with the context of the earlier one; the coder's
   `CoderStarter::init_continuing` leaves this key behind, so the new task has its message's tier or the default.
4. **Only a message sets the tier.** The agent writes it into the run's notes before each step, from the conversation and the
   messages waiting in the inbox, never from the model or a tool result.
5. **The mapping of the coder's tools** (the owner's choice; `bin/adam-coder/src/tier.rs`, `needs`):

   | Tier | Tools | Forms |
   |---|---|---|
   | `plan` | read-only: `read_file`, `run_command` (its changes are undone), `prepare_workspace`, `share_file`, the helpers, GitHub and web reads, `ask_user`. The answer is a plan | the form for a repository the person did not name stays |
   | `edits` | also `write_file`, `edit_file`, `apply_patch`, `run`, `run_checks`, `delegate_to_opencode`, `start_scratch`, `publish_scratch`, `commit_and_push`, `open_pull_request` | stays; a bare name does **not** count as named |
   | `auto` (default) | also `merge_pull_request` and `create_repository` (below) | a repository the person wrote by its name alone counts as named when exactly one owner applies; any other unnamed repository still asks; `create_repository` still asks its yes or no |
   | `bypass` | also `rename_repository` and `delete_repository` | none: any repository the host allows is granted, and `create_repository` asks nothing |

   A tool the coder does not know by name is classified by the name of a server's tool (`<server>__<tool>`): a reading name
   (`get_`, `list_`, `search_`, `web_`, `..._read`) is `plan`, anything else is `auto`. **An unknown tool is never offered to a
   person who asked for a plan.**
6. **It is enforced in code, three times.** (a) `TierSource` (a `ToolSource`) holds back, on each model turn, the tools above the
   tier, the agent's own and an MCP server's alike: the model is not shown them (`ToolSource::withhold`, new in `adam-llm-agent`).
   (b) The loop reads `withhold` again inside the step of a call, so a call to a tool above the tier, named anyway, is refused with
   the reason and the host is never asked. (c) Every tool of `coder_tools` checks the tier in the notes of the **root** run when it
   is called, which is what keeps a subagent to the tier of the run it serves. The prompt tells the model the tiers and how to
   decline, and the words of the turn name the tier of the message; the code is what holds.
7. **The new tools** (`bin/adam-coder/src/tools/admin.rs`, each a `#[tool]`):
   * `merge_pull_request { repo, number, method: squash | merge | rebase = squash, delete_branch? }`, icon `git`. On a repository
     of the workspace or one the person named. The code host reads the pull request, merges the head it read (`sha`) so a head that
     moved is refused, and deletes the branch only when it is the repository's own and not the base.
   * `rename_repository { repo, new_name }`, icon `git`, and `delete_repository { repo, confirm }`, icon `delete`. Only on a
     repository the person named (their words, a consent, or a repository the run created): a bare name is never enough for an act
     that cannot be taken back. `confirm` must equal `owner/name`, checked before the host is asked.
   * `CodeHost` gains `merge_pull_request`, `rename_repository` and `delete_repository` with defaults that say the host cannot, so
     no implementer breaks (Rule 2). `GitCredentials` gains `acts_for`, also provided.
8. **GitHub decides at every tier.** Branch protection, required checks, required reviews and the permissions the App was granted
   are not bypassed by any tier. A refusal is the tool's error result **in GitHub's words** with the class of the error
   (`rejected` for a merge the rules refuse, `unauthenticated` for a permission), and the model tells the person; none of these
   tools looks for another way.
9. **`create_repository` is on by default for a GitHub App.** With no `CREATE_REPO_OWNERS` the owners are the accounts the App acts
   for (`GITHUB_APP_OWNERS` as listed, the pinned installation's account, or every account it is installed on: `GitHubApp::accounts`);
   a list still restricts to exactly its owners; a token has no such list and keeps today's behaviour (off until the variable names
   owners).
10. **A bare repository name counts as named** from `auto` when exactly one owner applies: the owner of the repositories already in
    the workspace (or created by the run), else the one account the credentials act for. The name must be a word of the person's
    own messages (`RunNotes::said_words`), on the default host. Two owners, or none, leave the form as it is.

## Alternatives considered

* **A prompt that says what each tier allows, and nothing else.** Rejected: the prompt is a request, and the owner asked for
  autonomy *and* a limit he can rely on. The model must not be able to exceed a tier by a mistake or by text it read.
* **Enforcing only in the tool set offered each turn.** Rejected: a model can name a tool it was not shown, or one it was shown a
  turn ago, and a subagent has a context of its own. Hence the check at the call and the check from the root's notes.
* **Dropping the creation form at `auto`.** The system's design page reads the mapping so, and the owner's words ("Create repos by
  default") are about availability. The coder keeps the yes or no at `auto` and asks nothing at `bypass`, which is the reading
  that changes least and that the owner can widen with one line (`standing` in `create_repository`). *Unconfirmed by the owner.*
* **A bare name resolved by the model.** Rejected: the owner of a bare name is the one owner the deployment or the workspace
  already has, decided in code, never guessed.

## Consequences

* A person who picks `plan` gets read-only tools and a plan; `auto` merges and creates; only `bypass` renames or deletes. The
  default for a message that names none is `auto`, so **a deployment that upgrades gets `merge_pull_request` and, with an App,
  `create_repository` at once**: that is the owner's choice, and the permissions of the App decide what GitHub allows.
* **What the App needs** (`deploy/coder/README.md`): Pull requests and Contents write (open and merge, delete the head branch),
  Workflows write (a pull request that changes `.github/workflows`), Administration write (create, rename, delete).
* `ToolSource::withhold`, `Withheld`, `CodeHost::{merge_pull_request, rename_repository, delete_repository}`,
  `GitCredentials::acts_for`, `Workspaces::acts_for`, `GitHubApp::accounts`, `AccessTier`, `AccessTiers`, `ExtensionConfig::access_tier`,
  `RuntimeTaskBackend::with_access_tiers`, `Agents::access_tiers` and `Service::with_access_tiers` are new, all additive.
  `CoderSettings` gains `create_for_installed_owners`. `RunNotes` gains `tier`, `said_words`, `renamed_repos`, `deleted_repos`.
* **Not done here:** `adam-agent` (an agent folder) does not list the extension: it has no tool that a tier would limit, and a card
  that lists a tier nothing enforces would be a false promise. The tier is a promise by the agent, not a boundary: a deployment that
  needs one gives the agent a credential that cannot do the thing.
* **The limit of a name:** `ask_agent` is allowed at every tier (asking is reading); the prompt says not to ask for a change the
  tier forbids, and the code cannot see inside the asked agent. A server's write tool with a reading name would pass as a read.
* The cap of the prompt (30,000 characters) is nearly reached: the tiers cost a section of it, paid for by cuts elsewhere.

## Verified and unverified (2026-10-11)

*Verified 2026-10-11*, `docs.github.com/en/rest/overview/permissions-required-for-github-apps` (read through a summariser):
`PATCH` and `DELETE /repos/{owner}/{repo}`, `POST /orgs/{org}/repos` and `POST /user/repos` are under the repository permission
**Administration** (write); `PUT /repos/{owner}/{repo}/pulls/{n}/merge` and `DELETE /repos/{owner}/{repo}/git/refs/{ref}` under
**Contents** (write); **Workflows** and **Repository creation** are repository permissions of their own. *Verified 2026-10-11*,
`docs.github.com/en/rest/pulls/pulls` (merge): `405` is "Method Not Allowed if merge cannot be performed" and `409` is "Conflict if
sha was provided and pull request head did not match". `docs.github.com/en/rest/repos/repos`: deleting a repository requires admin
access, and classic tokens need the `delete_repo` scope.
*Unverified:* that the **Workflows** permission is what GitHub requires to push a change under `.github/workflows` (the permissions
page names the permission and does not state the path rule); whether an installation needs **Repository creation** in addition to
Administration to create a repository; the exact text of GitHub's refusals (the tools pass them on as they come).
