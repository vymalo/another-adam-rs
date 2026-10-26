# 0036. The coder keeps to the access tier the person picks

Status: **Accepted** (2026-10-11), amended after security reviews (three *Amended 2026-10-11* sections, the first for the first two reviews, and *Amended 2026-10-17* for the fifth, below), and *Amended 2026-10-17 (an asked agent)*: an agent asked by another agent never parks on a person's consent, and (built 2026-10-17) names only the repositories the person named in the words the orchestrator carries, and *Amended 2026-10-19 (the key file next to local commands)*: decision 6's follow-up is done in part, the coder refuses to start (exit 78) where it only warned
Implements the A2A extension `https://agents.vymalo.com/a2a/extensions/access-tier/v1`, designed in
`vymalo/another-agentic-system` ([ADR 0061](https://github.com/vymalo/another-agentic-system/blob/main/docs/decisions/0061-the-person-picks-an-access-tier-for-each-message-the-agent-keeps-to-it.md),
[`docs/api/access-tier-v1.md`](https://github.com/vymalo/another-agentic-system/blob/main/docs/api/access-tier-v1.md)).
Amends [ADR 0008](0008-a-workspace-holds-several-repositories.md) (a repository is granted only when the person named it or
agreed) and [ADR 0009](0009-github-per-installation-read-through-mcp.md) (`create_repository` is off unless
`CREATE_REPO_OWNERS` names the owner, and asks every time). Builds on [ADR 0006](0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md)
(the extensions) and [ADR 0017](0017-a-github-app-works-on-every-account-it-is-installed-on.md) (an App on several accounts).

*Amended 2026-10-11 ([ADR 0038](0038-the-coder-drops-opencode-and-adam-acp-stays-a-library.md), not built yet):* `delegate_to_opencode`
leaves the `edits` row of decision 5; the coder no longer has it.

*Amended 2026-10-11 ([ADR 0039](0039-the-agent-loop-takes-opencodes-structure-and-agents-get-plugins.md), not built yet):* decision 6's
enforcement becomes rules. The tiers are presets of rules generated from `needs`, `withhold` is derived from those rules, and
`Gated` still checks the root's notes at each call. What each tier allows does not change.

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
   **Only what the person typed counts as theirs.** The repositories they named and the bare names are read from their messages
   and from the typed parts of a form's answer (`other:`, `text:`, `Note:`), never from the question ids and option values of a
   form (`typed_by_the_person`): the person sees labels, and the ids and values are what the model wrote when it drew the form,
   so a click names nothing. *Amended 2026-10-11 (a fourth review):* a merge no longer reads the person's words at all (decision
   7), an answer counts only for the call whose form it answers, and the words a typed question started with are the model's.
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
   file, `GITHUB_APP_PRIVATE_KEY_PATH`; a key given inline, `GITHUB_APP_PRIVATE_KEY`, is hidden from the commands and is not this
   exposure). A command can read the file, mint an installation token and do whatever the App may, at any tier, a `plan` included. The tier is a promise the coder keeps for what
   the **model** does through its tools; it does not confine a command. The coder logs a startup warning when it runs with an App
   whose key is a file and the commands run locally (`WorkerConfig::commands_can_read_the_app_key`). A deployment that needs a boundary runs the
   commands in run pods (`runPods.enabled`) or in Podman devcontainers (`DEVCONTAINER_RUNTIME=podman`), or gives the App only the
   permissions it can live with. **Follow-up (the real fix, not done here):** run the coder's local commands as a uid that cannot
   read `/var/run/secrets`, or refuse App mode unless the commands run in run pods or Podman. *Amended 2026-10-19:* the second half
   is done, below.
7. **The new tools** (`bin/adam-coder/src/tools/admin.rs`, each a `#[tool]`):
   * `merge_pull_request { repo, number, method: squash | merge | rebase = squash, delete_branch? }`, icon `git`, `asks_user`.
     *Amended 2026-10-11 (a fourth review):* **it never reads the person's words.** Three reviews each found another way for the
     model's text to count as theirs (a form's ids and values, an error that repeats the model, a typed question's prefilled
     words, a stale form), so the gate that guessed at what they meant ("named", "asks to merge") was replaced by a rule that
     needs no guess. It decides from what the host says, from the commits the tools wrote down and from the answer to its own
     question:
     1. *The checks passed* on the head commit: the host reports every status and every check run complete and successful
        (`neutral` and `skipped` do not fail a merge), or calls the pull request `clean`
        (`CodeHost::pull_request_status`, `Checks::Passing`); and **that very commit is the one merged**
        (`MergePullRequest::expected_sha`, sent as the merge's `sha`). Checks still running are waited for, always. Checks that
        failed, or that nobody ran and the host does not call clean, stop a merge, **unless** the conversation opened the pull
        request with `accept_red_checks` that mattered (`RunNotes::red_pulls`): an override, step 3.
     2. *Path A, the conversation's own pull request*: its head is in the repository itself (not a fork) and is **a commit this
        conversation pushed** for that branch (`PushedBranch::shas`, an earlier task's too; a branch of the same name that
        somebody else pushed to is not that); where the host reports **no check at all** (a repository with no CI:
        `PullRequestStatus::ci_reported`) the run's own checks passed on that commit when `open_pull_request` let it in
        (`RunNotes::verified_shas`). It is merged at `auto` and above with **no question**.
     3. *Path B, any other pull request, or a red-checks override*: at `auto` the tool asks the person itself (`NeedsInput`, as
        `request_repository` does) in a form it draws: a fixed question ("Merge owner/name#N into base?"), the facts the host
        gave, and two fixed options, `merge` and `not_now`. The form's one question has the id `merge-<digest>`, a digest of the
        repository, the number, the head commit, the method, the branch deletion and the red state, so a yes is to **that** and
        nothing else (a head that moved, or checks that changed for the worse, are another question). Only the answer of **that
        form on that call's surface** is a yes (decision 4's amendment, `consent::merge_answer`): not a message, typed or sent
        as text (`vymalo_inbound` marks a line of text that begins as a form's answer does, *fifth review* below), not a form the
        model drew with the same id and option, not a click on a form still on the screen from an earlier question. The next
        call, with the same arguments, merges exactly that commit. **At `bypass` nothing is asked** (that is what the tier
        means) and Path B merges after the same checks of the host, the repository and the commit.
     4. *The prompt says so*: merge only when the person asked, and the tool asks for itself.
     The tool is on a repository of the workspace or any on the code host's own host (the first of `ALLOWED_REPO_HOSTS`): whose
     it is no longer matters, because the person's yes decides for a pull request that is not the conversation's. The code host
     merges the commit that was checked and deletes the branch only when it is the repository's own and not the base.
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
    alone in the newest typed words of the person that have any** (`RunNotes::said_words`; never a piece of a URL, a path or a
    query, never an option value of a form, and not a word of an earlier message), on the default host. A bare name does not make
    a repository one to merge (decision 7). Two owners, or none, leave the form as it is.
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
  `tier`, `said_words`, `red_pulls`, `verified_shas`, `renamed_repos`, `deleted_repos` (and `named_pulls` and `merge_asks`, removed by
  the fourth amendment);
  `PushedBranch` gains `shas`; `PullRequestStatus` gains `ci_reported`.
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
checks (and a second review, that the person's "named" and "fresh yes" could be forged through a form the model drew, that an
answer did not replace an earlier ask, that a bare `#12` named that pull request of every repository, that a branch name was
taken for the conversation's, and that a `clean` pull request with no CI passed: decisions 4 and 7); that a command could read the App's key (this ADR said a tier could not be exceeded "by text it read", which is true of the
model's tools and false of a command: decision 6 now says so); that a large context could drop the tier; that a subagent's MCP
tools were not held to the root's tier; that rename, delete and merge ignored the repository's host and followed redirects; that a
bare name was any word of any message; and that `bypass` was on for every deployment. Decisions 1, 2, 6, 7, 9, 10, 11, 12 and 13
are the answers; the known limits and the follow-ups above are what is left.

## Amended 2026-10-11 (a third review)

*Superseded in part by the fourth review below: the merge's use of the person's words, and the `Order` bullet.*

A third review found two more routes by which the model's text counted as the person's words (decision 4), and two smaller ones.
All four are closed in `bin/adam-coder/src/agent.rs` (`person_sayings`), `bin/adam-coder/src/tools/named.rs`
(`typed_by_the_person`), `bin/adam-coder/src/tools/notes.rs` (`RunNotes::hear`) and
`crates/adam-a2a-runtime/src/vymalo.rs` (`vymalo_inbound`).

* **A failed `ask_user` is not an answer.** Its error repeats the model's own values in its words (``the default `merge
  acme/widgets#7` of question `q` is not one of its options``), and they became a named repository, a named pull request and an ask
  to merge. Only the result of a call that did not fail can be an answer, and not a tool's own result.
* **A typed question's `default` is the model's.** `ask_user` lets a `text` or `longText` question start with words, and the web
  sends them back when the person submits without touching the box; the A2A layer writes them as `text: "..."`, which
  `typed_by_the_person` kept. The answer is now paired with its own `ask_user` call (the result by position in the history; the
  first message of the inbox for the call a parked run waits on) and a `text:` part equal to the default of a typed question of
  that call is dropped. Where there is no call to pair with, every `text:` part is dropped. *Accepted:* a person who types
  exactly the words the model prefilled is not heard; they say it in a message. The other way, taking `default` away from typed
  questions in the coder's `ask_user`, was not needed.
* **Order.** A message that arrived while a tool result was owed is put after the results in the history, so it can sort after a
  later click. Its place is not known, so the ask to merge and the bare names are now what **both** it and the last thing before
  it say (`When::Deferred`), and what waits in the inbox is newer than all the rest (`When::Waiting`).
* **A card is not the form.** `vymalo_inbound` turned any A2UI action with the shape of `context.answers` into a form's answer.
  It now takes it only from the action `answer` of a surface whose id starts with `ask-` (the surface `ask_user` draws). It
  cannot check the one pending call, because it has no run state: the prefix and the name are what it can check.
* **Wording.** The "fresh yes" the tool asks for names the pull request as `owner/name#N` or by its URL (a bare number needs
  exactly one repository in play, decision 4), here and in the text the model reads; decision 7 said "its number or URL".
* **A whole task is tested end to end** (`bin/adam-coder/tests/tiers.rs`): `commit_and_push`, `open_pull_request` and
  `merge_pull_request` against the wiremock GitHub with no hand-written notes, the merge allowed by the commit the tools
  themselves noted and sent with the `sha` the host reported.

## Amended 2026-10-11 (a fourth review)

The fourth review found two more bypasses of the typed-question default (a default with U+FEFF, which a page's `trim()` removes
and Rust's does not; and an answer to a form that is not the one the run waits on), and the same shape in the consent tools
(a form the model drew with the question id `consent`, answered while `request_repository` waits). Three rounds had chased "guess
the person's intent from text", so the design changed instead of being patched again. In `bin/adam-coder`
(`src/agent.rs`, `src/tools/{admin,consent,named,notes}.rs`), `crates/adam-a2a-runtime/src/vymalo.rs` and `crates/adam-ui`.

* **A merge reads no words** (decision 7): Path A (the conversation's own pull request) merges with no question; Path B (any
  other, or a red-checks override) is put to the person in the tool's own form and merges on the yes to that form. `merge_asks`,
  `named_pulls` and the code that read pull requests and "merge" out of text (`pulls_in`, `bind_pulls`, `asks_to_merge`) are
  deleted. The text-inference gate was replaced, not fixed, because a model can put words in anything the person sees and clicks;
  only an answer to a question the tool wrote itself, on the surface it drew, cannot be had by writing text. A pull request
  opened red and green now is Path A (the override is for checks that are not green).
* **An answer is for its own call.** The A2A layer writes the surface a form's answer was made on into its first line
  (`The person answered through the interface (form ask-<call id>):`; `form_answer_surface`), and `ask_user` draws its form on
  `ask-<call id>` (`ask_surface_id`). `person_sayings` and `consents_in` accept a form's answer for a call only when it names
  that call's surface. Otherwise it is unpaired: for `ask_user` no default matches, so every `text:` part is dropped; for
  `request_repository`, `create_repository` and `merge_pull_request` it is no consent. The person's own words to the first two
  counted (`yes`, `no`): *changed by the fifth review below*. *Accepted limits:* a provider that sends no call ids repeated
  them (`call_0`), and then a form of an earlier turn had the same surface id as the one on the screen now (*fixed by the fifth
  review: random ids*); and a screen that cannot draw a form cannot answer a Path B merge below `bypass`.
* **Defaults are compared as a person reads them**: without whitespace and without control or format characters (Unicode Cc and
  Cf), in lowercase (`adam_ui::comparable`). `ask_user` refuses a typed default that has such a character (a tab and a line
  break are words). *Verified 2026-10-11*: the Cf ranges against Python 3's `unicodedata` (Unicode 14.0), widened for later
  versions; a character a later version adds elsewhere is not covered. *Unverified:* what each web client does to the words on
  their way back; the comparison does not depend on it.
* **Order (corrects the third amendment's bullet).** A message that arrived while a tool result was owed is put into the
  history after the results, and the history keeps no arrival time, so a click that answers a later question can come after a
  message the person sent first. Only the bare names read the newest words now (the merge does not), and the code marks such a
  message `Deferred` only while it waits: then the bare names are what both it and the last thing before it say. Once it is in
  the history the order is the history's. An arrival marker that survives the move (`Conversation::deferred` into `messages`)
  would be a change in `adam-llm-agent` and was not made: both messages are the person's words.
* **Not changed:** `rename_repository` and `delete_repository` (bypass only) still need the person to have named the repository
  in words (`RunNotes::named_repos`), now with the hardening above. A form of their own is a follow-up if that is judged too
  weak.

## Amended 2026-10-17 (a fifth review)

The fifth review found one way left to forge a merge consent, one to steer a repository consent, and five smaller gaps. In
`crates/adam-a2a-runtime` (`vymalo.rs`, `words.rs`), `crates/adam-model-openai/src/wire.rs`, `crates/adam-workspace/src/github.rs`
and `bin/adam-coder` (`agent.rs`, `tools/{admin,consent,notes}.rs`).

* **Typed text is never a form's answer.** `vymalo_inbound` copied a text part into the run unchanged, so a text part that began
  with `The person answered through the interface (form ask-<id>):` and a line `- merge-<digest>: merge` was the same text as the
  real answer, and `merge_answer` and `consents_in` read only text. Two ways in: another agent's model that answers a pending
  question as text (the orchestrator lets a re-ask do so), and the coder's own model asking the person to paste a "confirmation"
  line. Now every line of a text part that begins (after whitespace, invisible characters and case, as `adam_a2a_runtime::comparable`
  reads it; the line breaks are `\n`, `\r`, U+0085, U+2028 and U+2029) with `The person answered through the interface` is marked
  `(typed) ` first, so only an A2UI `answer` action makes such a line. `default_inbound` marks it too, so a host that wraps the
  coder in a backend with the default inbound function (an in-process host) is covered as well. *Not a structured field of the `Inbound`:* a form's answer
  reaches the run as the text of a user message or of a tool result (`Message::Tool.content`, which holds no field), so a
  marker outside the text would be a change of `adam-llm-agent`'s message type; the marking does the same work for the coder.
  **The limit that stays:** an A2A client that builds an A2UI `answer` data part itself can still answer, on any surface and with
  any question id, because the client is the person's screen. So **the orchestrator must never let a model answer a sub-agent's
  form**: a question another agent's model re-asks has to be shown to the person, not answered by an agent. A run whose caller
  is another agent's model is outside what the tiers promise. (*Amended 2026-10-17 (an asked agent), below:* a run the
  orchestration layer's grant says another agent asked asks for no consent; a model that drives the coder as a plain A2A client
  still is outside.)
* **After a form, only its answer counts; a text question needs words that name the subject.** A model could write "Shall I also
  fix the typo? Reply yes or no" and call `request_repository(acme/infra)` in the same turn, and the person's "yes" granted
  `acme/infra`. The consent tools now note, by the call's id (`RunNotes::text_questions`), the question that went out as text
  (`NeedsInput` with no `ui`). For a question that went out as a form, or one the notes do not know, only the form's own answer
  is a consent. For a text question words count only as the label of the yes option (`Yes, add acme/infra`, `Create o/n`),
  compared as `comparable` reads it, and are never a no; a bare `yes` or `y` is neither. A person on a screen that cannot draw
  the form types the label, which is shown in the question. This replaces the fourth amendment's "the person's own words still
  count".
* **Path A needs the conversation's own base.** The pull request's base must be the one `commit_and_push` recorded for the branch
  (`RunNotes::pushed_base`); somebody can open a pull request from the agent's branch into `release`, and an injected issue could
  have got it merged. A base the notes do not know is another's: Path B.
* **The red-checks override needs the conversation's own head** (`red_accepted && own`). After somebody else's force-push of a
  failing commit the form would have said "You accepted red checks to open it" about a commit that is not the agent's; now it
  is refused as failing checks, at every tier.
* **Call ids are random where the provider sends none.** `adam-model-openai` made `call_0`, `call_1`... again in every turn, so two
  forms of a run shared a surface id. It now makes `call_` and 16 random hexadecimal digits for each call, the same in the stream
  and in the assembled message. The agent journals the model's response with its ids (the step `model:<turn>`), so a resumed run
  reads the ones that were made and the model is not asked again (a test in `adam-llm-agent`). The fourth amendment's accepted
  limit on repeated ids is closed for that client; another client that sends repeated ids has it still.
* **Skipped and neutral checks are not "reported".** `PullRequestStatus::ci_reported` was true for any status or check run, so a
  commit whose runs were all skipped or neutral passed Path A without `verified_shas`. It is now true only when something
  **succeeded** (a successful status, or a completed run whose conclusion is `success`); all skipped or neutral is unreported,
  and such a pull request of the conversation's is merged only when the run's own checks passed on that commit. *Unverified:*
  what GitHub reports for a required check that was skipped by a path filter; the coder treats it as not run.
* **Housekeeping.** `adam_ui::{comparable, is_format_or_control}` now live in `adam-a2a-runtime` (the marking needs them, and
  `adam-ui` depends on it) and are re-exported by `adam-ui`. The two README diagrams show the answer's first line with its
  surface.

## Amended 2026-10-17 (an asked agent)

The fifth review left "a run whose caller is another agent's model is outside what the tiers promise". The orchestration layer's
`ask_agent` makes such runs. Its asks carried no UI catalog (they carry the thread's since vymalo/another-agentic-system#267, which
also ends an ask whose agent draws a form, but a thread whose screen sent none still sends none), so the coder's consent question
went out as text, where the yes label in words is a consent (the fifth review): the asking model reads the label in the question
and can type it. A prompt-injected chat
model could so grant `request_repository` and `create_repository` to a coder it asked; a merge's Path B already needed the form's
answer. Decided on the owner's direction: **an agent asked by another agent never parks on a person's consent.** In
`crates/adam-a2a-runtime` (`caller.rs`, `vymalo.rs`), `crates/adam-llm-agent` (`PROTECTED_CONTEXT_KEYS`) and `bin/adam-coder`
(`agent.rs`, `tools/{consent,create,admin}.rs`).

* **The signal is the `caller` claim of the thread-tools token.** The orchestrator mints a token for the messages it sends to an
  agent whose card lists `thread-tools/v1` (the coder's does), with `caller: "main"` and `depth: 0` for the thread's addressed agent
  and `caller: "ask:<n>"` for the n-th agent asked in the job (*verified 2026-10-17*: `docs/api/thread-tools-v1.md`, "The token" and
  "The child task", and `orchestrator/crates/app/src/dispatcher/ask.rs`, `Caller::Ask`, of `vymalo/another-agentic-system` at
  `5cac58d`). It sends the message with no grant when the card it read for that send lacks the URI, the job has no grant, the
  deployment has no issuer, or the token cannot be minted (`orchestrator/crates/agent-a2a/src/thread_tools.rs`, `mint`, which
  never fails the message, at the same commit). `vymalo_inbound` reads the claims (the middle segment; the signature is not checked, the key being the orchestrator's)
  into the context key `vymalo.caller` of every message that carries the extension. No text reaches it: the A2A caller writes the
  metadata, and a model's words travel as text parts. What is trusted is the authenticated A2A caller, as for a form's answer and the
  tier. The other candidates were weaker: the orchestrator's bearer is one per agent, for asks and turns alike; an ask carries no
  mentions, and neither does most of what a person sends; the context `<thread>-ask-<agent>` is a naming convention, not a contract.
* **Fail closed.** A message that carries the grant and whose token's claims cannot be read (not three base64url segments, not a
  JSON object, a `caller` other than `main` and `ask:<n>`, a `main` whose `depth` is not 0) is `unreadable`, which is no person. A
  message without the grant (a plain A2A client, `dev/coder-e2e.sh`) leaves the key as it is, so such a client is read as before. The
  key is one of `PROTECTED_CONTEXT_KEYS` (a large context does not drop it, a child run inherits it) and has no `expiresAt`, so it
  outlives the token. **Once asked, always asked:** the context or any message waiting in the inbox that names an asked agent
  makes the run asked, and the coder keeps that for the rest of the run and the tasks that continue it
  (`RunNotes::asked_by_an_agent`), so a message for the thread's agent that comes later does not turn the asked agent's words, in
  the inbox or in the history, into the person's. **The gap:** an ask that arrives with no grant (above) reads as a plain
  client's, and its run asks as a person's would. That the orchestrator never sends an ask it cannot mint a grant for is a
  follow-up of the orchestration layer. *Landed 2026-10-17:* vymalo/another-agentic-system#270 (an ask to a card that lists
  `thread-tools/v1` is never sent without its grant; it fails instead: *verified 2026-10-17* at its commit 591fc28 and its
  `docs/api/thread-tools-v1.md`, "An ask is never sent without the grant").
* **The consent tools ask nobody there.** `request_repository`, `create_repository` and Path B of `merge_pull_request` ask through
  `consent::ask`, which then returns an error result ("No person is in this conversation: another agent asked you, so nobody can
  agree to <what>, and nobody was asked", then: carry on without it, or end the answer by saying that it needs the person's yes, so
  that the asking agent can ask them), draws no form and notes no question. A consent tool added later asks through the same
  function.
* **No words are a consent in such a run** (`consents_in`): the defence for a run parked on a text question before it was asked.
  A form's answer still counts, because only a screen sends an A2UI action.
* **Not changed:** `ask_user` still asks in words, and the asking model answers (the ask and continue flow of the orchestration
  layer); Path A merges with no question; `bypass` asks nothing, the tier being the consent (the orchestrator sends an ask under at
  most the asked agent's default tier, capped by the person's roles: *verified 2026-10-17* in
  `orchestrator/crates/app/src/dispatcher/ask.rs` of vymalo/another-agentic-system, the cap by the roles being its #267).
* **Decided, a follow-up (the owner, 2026-10-17): in a run another agent started, only the repositories the person named in their
  own message count as named.** The orchestrator will pass that list to the asked agent as a structured field of the ask, and the
  coder will read it instead of the ask's words; this needs a change in both repositories and is not part of this amendment.
  **Until it lands, an asked coder still counts the repositories named in its messages as named** (the ask's text, and the
  asking model's answers to its `ask_user` questions in a later ask), which is the behaviour before this amendment. So, today,
  the refusal of `request_repository` protects nothing in an asked run: the asking model can name the repository instead. What
  this amendment closes now is a consent in words to `create_repository` below `bypass` (and to any consent tool added later);
  Path B of a merge needed its form already.

  *Built 2026-10-17:* what the orchestrator passes is the person's **words**, not a list of repositories: beside the grant of an
  ask's message, a member `person` `{said, omitted?}`, the text of every message the person wrote in the thread (every job of it,
  a fork's parent included), oldest first, as written, a credential in it redacted, each entry cut at 4 KiB, the oldest left out
  whole beyond 64 KiB and counted in `omitted`; never on a `main` message, and a person's form answers are not in it
  (vymalo/another-agentic-system [#271](https://github.com/vymalo/another-agentic-system/pull/271):
  [`docs/api/thread-tools-v1.md`, "The `person` member"](https://github.com/vymalo/another-agentic-system/blob/main/docs/api/thread-tools-v1.md#the-person-member)
  and the fourth note of 2026-10-17 of its ADR 0026, *verified 2026-10-17* at its commit 55ac8ab, the head of that PR's contract). Words, because the rule of
  what names a repository (hosts, the default host, a bare name, a URL, a `git@` form) is this agent's
  (`bin/adam-coder/src/tools/named.rs`), and the orchestrator, protocol-agnostic, knows nothing of repositories. `vymalo_inbound`
  reads the member into the context key `vymalo.person` of every message that carries the grant, `null` when it is missing or
  malformed (`crates/adam-a2a-runtime/src/person.rs`); in a run that is asked (`RunNotes::asked_by_an_agent`) the coder hears
  **only** those words, from the newest message that carries the key (`carried_sayings`, `bin/adam-coder/src/agent.rs`), applies
  `named_in` to them, and never reads the ask's text or the asking model's answers to `ask_user`: without the member nothing is
  named, and `request_repository` refuses as above. What the person named before the run was asked stays named (the notes only
  add); `consents_in` is unchanged. The key is not one of `PROTECTED_CONTEXT_KEYS`: a protected value is capped at 1 KiB, far
  under the words, and losing them is fail closed (nothing named); so a child run inherits `vymalo.caller` and not
  `vymalo.person` (only the protected keys pass to a child, `ToolCtx::start_child`), and a child of an asked run names
  nothing, as intended. The gap that remains is the orchestrator's: a person's form
  answers (a typed field, a note) are not carried, so a repository named only there is not named in an asked run; the person can
  write it in a message.

## Amended 2026-10-19 (the key file next to local commands)

Decision 6 left the exposure of a GitHub App's key file to a startup warning, and a follow-up: "refuse App mode unless the commands
run in run pods or Podman". The warning was the only guard, and the chart's defaults (`runPods.enabled: false`, `DEVCONTAINER_RUNTIME=off`)
were the configuration it warned about, so a deployment that never read the log ran with an App that any command could act as, at
any access tier. [vymalo/another-adam-rs#120](https://github.com/vymalo/another-adam-rs/issues/120) closes that half of the follow-up:

* **The coder refuses to start (exit 78)** with an App whose private key is a **file** (`GITHUB_APP_PRIVATE_KEY_PATH`) while the commands
  of a run run in its own container (`RUN_ENVIRONMENT=local` and `DEVCONTAINER_RUNTIME=off`: `WorkerConfig::commands_can_read_the_app_key`
  is the whole test, the same that chose the warning), unless the deployment sets `ALLOW_COMMANDS_NEXT_TO_SECRETS=true`. The refusal is
  a problem of the configuration like any other, reported with the others in one round trip; the message says why and names every way
  out: run pods (`RUN_ENVIRONMENT=kubernetes`), Podman devcontainers (`DEVCONTAINER_RUNTIME=podman`), or the variable. Accepted, the coder starts and **still logs the warning**. The variable is read as the other
  flags are (`true`, `1`, `false`, `0`; anything else is exit 78), by the roles that run workers only; a control plane holds no key.
* **The chart refuses to render the same configuration** (`github.auth: app`, `runPods.enabled: false`) unless
  `runPods.allowCommandsNextToSecrets: true`, which it renders as the variable, and only when it is `true`. A GitOps sync that fails
  keeps the running pods; a coder that exits 78 would have replaced them with a crash loop. `config.extraEnv` may not set the variable.
* **A deployment that ran an App without run pods breaks on upgrade**: it must turn run pods on or accept the risk. The upgrade note is
  [Upgrading an App deployment](../guides/deploy-the-coder.md#upgrading-an-app-deployment). The operator makes no run pods (ADR 0029) and
  does not check: an `AgentConfig` with `github.app` takes the variable in `spec.extraEnv`, and the coder refuses to start without it.
* **What does not change:** a token, an inline key, run pods and Podman devcontainers start as before; the tier is still not a boundary
  against a command wherever the commands run in the coder's own container, and the other half of the follow-up (a uid that cannot read
  `/var/run/secrets`) is still not done.
