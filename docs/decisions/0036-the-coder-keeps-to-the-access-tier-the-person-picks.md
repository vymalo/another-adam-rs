# 0036. The coder keeps to the access tier the person picks

Status: **Accepted** (2026-10-11), amended after a security review the same day (*Amended 2026-10-11*, below)
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
coder reads, what each tier lets it do, and how that is made to hold, and **where it does not hold** (decision 6).

## Decision

1. **The card lists the tiers up to the deployment's cap** (`MAX_ACCESS_TIER`, chart `accessTiers.max`, default **`auto`**:
   `bypass` is the deployment's opt-in, fail closed) with `default: auto` (`ExtensionConfig::access_tier`, `AccessTier` and
   `AccessTiers::up_to` in `adam-a2a`). `AccessTier` is a closed enum. Every role reads the cap: the control plane declares the
   tiers and refuses a higher one, the workers' tools check it again against the tier in the notes.
2. **The backend reads the tier of a message that activated the extension** (`RuntimeTaskBackend::with_access_tiers`,
   `crates/adam-a2a-runtime/src/tier.rs`). The metadata must be `{"tier": "<one of the four, lower case>"}` and a tier the card
   honours, and the payload must be able to carry it, or the message is refused as `InvalidParams` **before anything is written**
   (a tier is never dropped silently). The tier becomes the run's inbound context key `vymalo.accessTier`, so the newest message
   wins from the agent's next step. The key is one of `PROTECTED_CONTEXT_KEYS` of `adam-llm-agent`: when the rest of a message's
   context is too large and dropped, the tier is kept. A message that did not activate the extension carries no tier, whatever its
   metadata says. To a caller that activated it, a task says the tier it works under in its metadata (the answer of
   `SendMessage`, `GetTask`, the first frame of a stream) and a stream says it again when it changes.
3. **A task never inherits a tier.** A task that continues another is started with the context of the earlier one; the coder's
   `CoderStarter::init_continuing` leaves this key behind, so the new task has its message's tier or the default. (A *rework* of a
   task is the orchestrator's to send with a tier: see decision 13.)
4. **Only a message sets the tier.** The agent writes it into the run's notes before each step, from the conversation and the
   messages waiting in the inbox, never from the model or a tool result, and never above the cap.
5. **The mapping of the coder's tools** (the owner's choice; `bin/adam-coder/src/tier.rs`, `needs`):

   | Tier | Tools | Forms |
   |---|---|---|
   | `plan` | read-only: `read_file`, `run_command` (its changes are undone), `prepare_workspace`, `share_file`, the helpers, GitHub and web reads, `ask_user`. The answer is a plan | the form for a repository the person did not name stays |
   | `edits` | also `write_file`, `edit_file`, `apply_patch`, `run`, `run_checks`, `delegate_to_opencode`, `start_scratch`, `publish_scratch`, `commit_and_push`, `open_pull_request` | stays; a bare name does **not** count as named |
   | `auto` (default) | also `merge_pull_request` (decision 7) and `create_repository` (below) | a repository the person wrote by its name alone counts as named when exactly one owner applies (decision 10); any other unnamed repository still asks; `create_repository` still asks its yes or no |
   | `bypass` (the deployment's opt-in) | also `rename_repository` and `delete_repository` | none: any repository the host allows is granted, and `create_repository` asks nothing |

   A tool the coder does not know by name is classified by the name of a server's tool (`<server>__<tool>`): a reading name
   (`get_`, `list_`, `search_`, `web_`, `..._read`) is `plan`, anything else is `auto`. **An unknown tool is never offered to a
   person who asked for a plan.**
6. **It is enforced in code against the model, four times, and it is not a boundary against a command.**
   (a) `TierSource` (a `ToolSource`) holds back, on each model turn, the tools above the tier, the agent's own and an MCP
   server's alike: the model is not shown them (`ToolSource::withhold`, new in `adam-llm-agent`). The tier of the root run is the
   one in its context; **a subagent's is the root's, read from the notes of the root run** (`SourceCtx::root_run_id`), and a
   child run also starts with the protected context keys of its parent. (b) The loop reads `withhold` again inside the step of a
   call, so a call to a tool above the tier, named anyway, is refused with the reason and the tool is never run. (c) Every tool of
   `coder_tools` checks the tier in the notes of the **root** run when it is called (`Gated`); **no notes are a `plan`**.
   (d) `merge_pull_request`, `rename_repository` and `delete_repository` check it again in their own body.
   **What this does not do.** In the default deployment (`runPods.enabled: false`, `DEVCONTAINER_RUNTIME=off`) `run_command`,
   `run`, `run_checks` and OpenCode run in the coder's own container, as the user that holds the GitHub App's private key (a
   file, `GITHUB_APP_PRIVATE_KEY_PATH`; with the inline variable, the environment of the process). A command can read it, mint an
   installation token and do whatever the App may, at any tier, a `plan` included. The tier is a promise the coder keeps for what
   the **model** does through its tools; it does not confine a command. The coder logs a startup warning when it runs with an App
   and the commands run locally (`WorkerConfig::commands_can_read_the_app_key`). A deployment that needs a boundary runs the
   commands in run pods (`runPods.enabled`) or in Podman devcontainers (`DEVCONTAINER_RUNTIME=podman`), or gives the App only the
   permissions it can live with. **Follow-up (the real fix, not done here):** run the coder's local commands as a uid that cannot
   read `/var/run/secrets`, or refuse App mode unless the commands run in run pods or Podman.
7. **The new tools** (`bin/adam-coder/src/tools/admin.rs`, each a `#[tool]`):
   * `merge_pull_request { repo, number, method: squash | merge | rebase = squash, delete_branch? }`, icon `git`. **It merges only
     when all of these hold**, each read from what the host says and from what the person wrote, never from what the model says:
     1. *The pull request is one this conversation may touch*: its head branch is in the repository itself (not a fork) and one a
        run of the conversation pushed (`RunNotes::pushed_branches`, an earlier task's too), or the person named the pull request
        (its URL, `#12`, `PR 12`: `RunNotes::named_pulls`).
     2. *The checks passed* on the head commit: the host reports every status and every check run complete and successful
        (`neutral` and `skipped` do not fail a merge), or calls the pull request `clean`
        (`CodeHost::pull_request_status`, `Checks::Passing`); and **that very commit is the one merged**
        (`MergePullRequest::expected_sha`, sent as the merge's `sha`).
     3. *Red checks need a fresh yes.* A pull request this conversation opened with `accept_red_checks` that mattered
        (`RunNotes::red_pulls`) is merged only when the person's **newest** message names it and says to merge it
        (`RunNotes::merge_asks`: the word `merge`, not negated, not `merge conflict`, with its number or URL). Then checks that
        failed or never ran no longer stop it; checks still running do. A pull request that was not opened red is never merged
        on red checks.
     4. *The prompt says so*: merge only when the person asked.
     The tool is on a repository of the workspace or one the person named, **on the code host's own host** (the first of
     `ALLOWED_REPO_HOSTS`); a bare name is never a grant for it. The code host merges the commit that was checked and deletes the
     branch only when it is the repository's own and not the base.
   * `rename_repository { repo, new_name }`, icon `git`, and `delete_repository { repo, confirm }`, icon `delete`. Only on a
     repository the person named (their words, a consent, or a repository the run created), on the code host's own host: a bare
     name is never enough for an act that cannot be taken back. `confirm` must equal `owner/name`, checked before the host is asked.
   * `CodeHost` gains `pull_request_status`, `merge_pull_request`, `rename_repository` and `delete_repository` with defaults that
     say the host cannot, so no implementer breaks (Rule 2). `GitCredentials` gains `acts_for`, also provided.
8. **GitHub decides at every tier.** Branch protection, required checks, required reviews and the permissions the App was granted
   are not bypassed by any tier. A refusal is the tool's error result **in GitHub's words** with the class of the error
   (`rejected` for a merge the rules refuse, `unauthenticated` for a permission; the tools decide from the class, never from the
   variant or the text), and the model tells the person; none of these tools looks for another way.
9. **`create_repository` is on by default for a GitHub App that names its accounts.** With no `CREATE_REPO_OWNERS` the owners are
   the accounts the App acts for (`GITHUB_APP_OWNERS` as listed, or the pinned installation's account: `GitHubApp::accounts`);
   **`GITHUB_APP_OWNERS=*` does not turn it on** (every account the App is installed on, which anyone may add): list the owners in
   `CREATE_REPO_OWNERS`. A list restricts to exactly its owners; a token has no such list and keeps today's behaviour (off until
   the variable names owners).
10. **A bare repository name counts as named** from `auto` when exactly one owner applies: the owner of the repositories already in
    the workspace (or created by the run), else the one account the credentials act for. The name must be **a word that stands
    alone in the person's newest message** (`RunNotes::said_words`; never a piece of a URL, a path or a query, and not a word of
    an earlier message), on the default host. Two owners, or none, leave the form as it is.
11. **Rename, delete, merge and the checks follow no redirect and ask first.** A repository that was renamed or transferred
    answers its old address with a redirect, and an address that is free again can be somebody else's. The code host reads
    `GET /repos/{owner}/{name}` on a client that follows no redirect and acts only when the answer names exactly that repository
    (`Conflict`, class `rejected`, otherwise); the change itself is sent on that client too.
12. **A deployment caps the tiers** (decision 1): `MAX_ACCESS_TIER`, validated at startup (exit 78), rendered by the chart
    (`accessTiers.max`) and the operator (`AgentConfig` `maxAccessTier`) for every role.
13. **The system side's duty.** The orchestrator sends a tier with every message and carries one on a rework of a task (a rework
    that names none runs at the default, `auto`). This repository does not hold a tier from one task to the next (decision 3).

## Alternatives considered

* **A prompt that says what each tier allows, and nothing else.** Rejected: the prompt is a request, and the owner asked for
  autonomy *and* a limit he can rely on. The tools must not exceed a tier because the model made a mistake or followed text it
  read. (The prompt still tells the model the tiers, how to decline, and to merge only when asked.)
* **Enforcing only in the tool set offered each turn.** Rejected: a model can name a tool it was not shown, or one it was shown a
  turn ago, and a subagent has a context of its own. Hence the check at the call and the check from the root's notes.
* **Treating the tier as a boundary.** Not possible while commands run next to the App's key (decision 6): the tier limits what
  the model does through its tools, and a deployment that needs more confines the commands.
* **A bare name resolved by the model.** Rejected: the owner of a bare name is the one owner the deployment or the workspace
  already has, decided in code, never guessed.

## Open

* **The creation form at `auto`.** The system's design page reads the mapping so, and the owner's words ("Create repos by
  default") are about availability. The coder keeps the yes or no at `auto` and asks nothing at `bypass`, which is the reading
  that changes least and that the owner can widen with one line (`standing` in `create_repository`). *Unconfirmed by the owner.*

## Consequences

* A person who picks `plan` gets read-only tools and a plan; `auto` merges (under decision 7) and creates; only `bypass`, and only
  where the deployment set `MAX_ACCESS_TIER=bypass`, renames or deletes. The default for a message that names none is `auto`, so
  **a deployment that upgrades gets `merge_pull_request` and, with an App that names its accounts, `create_repository` at once**:
  that is the owner's choice, and the permissions of the App decide what GitHub allows.
* **What the App needs** (`deploy/coder/README.md`): Contents write (push, merge, delete the head branch), Pull requests write
  (open) and read (the status), **Checks read** and **Commit statuses read** (a merge reads the checks of the head commit),
  Administration write (create, rename, delete), Workflows write (a pull request that changes `.github/workflows`).
* `ToolSource::withhold`, `Withheld`, `SourceCtx::root_run_id`, `PROTECTED_CONTEXT_KEYS`,
  `CodeHost::{pull_request_status, merge_pull_request, rename_repository, delete_repository}`, `GitCredentials::acts_for`,
  `Workspaces::acts_for`, `GitHubApp::accounts`, `AccessTier`, `AccessTiers`, `ExtensionConfig::access_tier`,
  `RuntimeTaskBackend::with_access_tiers`, `Agents::access_tiers` and `Service::with_access_tiers` are new, all additive.
  `CoderSettings` gains `create_for_installed_owners` and `max_access_tier`, `Config` gains `max_access_tier`, `RunNotes` gains
  `tier`, `said_words`, `named_pulls`, `merge_asks`, `red_pulls`, `renamed_repos`, `deleted_repos`.
* **Not done here:** `adam-agent` (an agent folder) does not list the extension: it has no tool that a tier would limit, and a card
  that lists a tier nothing enforces would be a false promise.
* **The limit of a name:** `ask_agent` is allowed at every tier (asking is reading); the prompt says not to ask for a change the
  tier forbids, and the code cannot see inside the asked agent (a remote one least). A server's write tool with a reading name
  would pass as a read.
* **Known limits, accepted.** The notes are read, changed and written by each tool, so two calls of one step can lose an update.
  The calls of a step use the tier noted at its start: a message that arrives during the step applies from the next. A
  repository the person named or agreed to under one tier stays granted after a later message lowers it (the tier limits what the
  tools do, not what was named). A deployment allowlist of the repositories the tools may act on, beyond the host check, is a
  follow-up. `GitHubApp::accounts` is not cached, so `GITHUB_APP_OWNERS=*` asks GitHub on each call that needs the list (at most
  three requests): a follow-up. If the notes of a run that opened a pull request with red checks are lost (another worker's
  volume), the red mark is lost with them; the check of rule 2 still stands.
* The cap of the prompt (30,000 characters) is nearly reached: the tiers cost a section of it, paid for by cuts elsewhere.

## Verified and unverified (2026-10-11)

*Verified 2026-10-11*, `docs.github.com/en/rest/authentication/permissions-required-for-github-apps` (read through a summariser):
`PATCH` and `DELETE /repos/{owner}/{repo}` and `POST /orgs/{org}/repos` and `POST /user/repos` need **Administration** (write);
`PUT /repos/{owner}/{repo}/pulls/{n}/merge` and `DELETE /repos/{owner}/{repo}/git/refs/{ref}` need **Contents** (write);
`GET /repos/{owner}/{repo}/pulls/{n}` needs **Pull requests** (read); `GET /repos/{owner}/{repo}/commits/{ref}/status` needs
**Commit statuses** (read); `GET /repos/{owner}/{repo}/commits/{ref}/check-runs` needs **Checks** (read);
`GET /repos/{owner}/{repo}` needs **Metadata** (read). **Workflows** and **Repository creation** are repository permissions of their
own. *Verified 2026-10-11*, `docs.github.com/en/rest/pulls/pulls` (merge): `405` is "Method Not Allowed if merge cannot be
performed" and `409` is "Conflict if sha was provided and pull request head did not match"; `sha` is "SHA that pull request head
must match to allow merge". `docs.github.com/en/rest/checks/runs`: the `conclusion` of a check run is one of `success`, `failure`,
`neutral`, `cancelled`, `skipped`, `timed_out`, `action_required` or null, its `status` one of `queued`, `in_progress`,
`completed`, `waiting`, `requested`, `pending`; `per_page` is at most 100. `docs.github.com/en/rest/repos/repos`: deleting a
repository requires admin access, and classic tokens need the `delete_repo` scope.
*Unverified:* that the **Workflows** permission is what GitHub requires to push a change under `.github/workflows` (the permissions
page names the permission and does not state the path rule); whether an installation needs **Repository creation** in addition to
Administration to create a repository; what `mergeable_state` values mean (the REST page documents the field as a string and lists
none: the coder takes `clean` as the host calling the pull request ready, as the owner's rule says, and reads the statuses and the
check runs otherwise); that GitHub counts `neutral` and `skipped` as passing for a merge (the coder does); that a repository that
moved answers its old address with a redirect (301 or 307) on every method; the exact text of GitHub's refusals (the tools pass
them on as they come).

## Amended 2026-10-11 (a security review)

A review of the first version found that `merge_pull_request` merged any pull request of a named repository without looking at the
checks; that a command could read the App's key (this ADR said a tier could not be exceeded "by text it read", which is true of the
model's tools and false of a command: decision 6 now says so); that a large context could drop the tier; that a subagent's MCP
tools were not held to the root's tier; that rename, delete and merge ignored the repository's host and followed redirects; that a
bare name was any word of any message; and that `bypass` was on for every deployment. Decisions 1, 2, 6, 7, 9, 10, 11, 12 and 13
are the answers; the known limits and the follow-ups above are what is left.
