//! `merge_pull_request { repo, number, method?, delete_branch? }`, `rename_repository { repo, new_name }`
//! and `delete_repository { repo, confirm }`: what the access tiers of the person unlock.
//!
//! | Tool | Tier | On which repository |
//! |---|---|---|
//! | `merge_pull_request` | `auto` | any on the code host's own host: its own pull requests at once, others by asking the person |
//! | `rename_repository`, `delete_repository` | `bypass` | one the person named, on the code host's own host |
//!
//! The tier is checked by the tool set, by every tool again (`Gated`) and by these three themselves
//! from the notes of the run ([`tier`](crate::tier)). **GitHub's own rules decide at every tier**:
//! branch protection, required checks and reviews, and the permissions the App was granted. A refusal
//! of GitHub is the tool's error result **in GitHub's words**, with the class of the error, and the
//! model tells the person; none of these tools looks for another way.
//!
//! **A merge never reads the person's words.** What the person typed is text, and a model can put
//! words in a form, so no rule here guesses at what they meant. The tool decides from what the host
//! says, from the commits the conversation pushed (written by `commit_and_push` itself) and from the
//! answer to its own question:
//!
//! 1. *The checks passed* on the head commit ([`Checks::Passing`](adam_workspace::Checks)), and that
//!    very commit is the one merged (`MergePullRequest::expected_sha`). One still running is waited
//!    for, always. One that failed, or that nobody ran, stops the merge, **unless** the pull request
//!    was opened by this conversation with `accept_red_checks` (`RunNotes::red_pulls`) **and its head
//!    is still the conversation's own commit** (step 2): that is an override and goes by the third
//!    step. After somebody else's push the acceptance says nothing about what is there.
//! 2. *Path A, a pull request of the conversation's own*: its head is in this very repository and is a
//!    commit the conversation pushed for that branch (`RunNotes::pushed_commit`, an earlier task's
//!    too), it is against the base branch the conversation pushed the branch for
//!    (`RunNotes::pushed_base`: a pull request somebody opened from the agent's branch into another
//!    branch is another's), and where the host reports no check at all, the run's own checks passed on
//!    that commit (`RunNotes::verified_shas`). It is merged at `auto` and above with no question.
//! 3. *Path B, any other pull request, or a red-checks override*: at `auto` the tool asks the person
//!    itself, in a form it draws ([`merge_question`]): the question is fixed ("Merge
//!    owner/name#N into base?"), the options are fixed (`merge`, `not_now`), and the question's id
//!    is a digest of the repository, the number, the head commit, the method, whether the branch is
//!    deleted and whether the checks are accepted red. Only that form's own answer, made on the
//!    surface the call drew it on, is a yes (`CoderAgent` records it), and it is a yes to that and
//!    nothing else: a head that moved, or checks that changed for the worse, are another question.
//!    At `bypass` it asks nothing (that is what the tier means) and merges after the same checks of
//!    the host, the repository and the commit.
//!
//! **Safe to repeat.** Merging a pull request that is merged already reports it as it is
//! ([`CodeHost::merge_pull_request`](adam_workspace::CodeHost)). A rename and a deletion are noted
//! in the run's notes once the host answered, and a repeat after that is the same answer; a repeat
//! of a call that died between the host's answer and the note meets the host's `404` (deleted) or
//! `422` (the name is taken), which is reported as such.
//!
//! `delete_repository` takes `confirm`, which must be `owner/name` exactly as the repository is
//! called: a guard against a slip of the model (a repository named by its URL in `repo` and another
//! in `confirm`), checked before the host is asked.

use std::str::FromStr as _;

use adam::prelude::*;
use adam_a2a::AccessTier;
use adam_error::{Classify, ErrorClass};
use adam_workspace::{
    Checks, MergeMethod, MergePullRequest as HostMerge, PullRequestStatus, RepoRef, WorkspaceError,
};

use serde_json::{Value, json};

use super::consent::{MERGE, NOT_NOW, ask, merge_question};
use super::create::{valid_name, valid_owner};
use super::named::{default_host_key, key_of_argument, split_key};
use super::notes::{RenamedRepo, RunNotes};
use super::prepare::not_named;
use super::{Outcome, ToolEnv, cancelled, non_empty, notes_error, resolve_slot};

/// The tool's name.
pub const MERGE_PULL_REQUEST: &str = "merge_pull_request";
/// The tool's name.
pub const RENAME_REPOSITORY: &str = "rename_repository";
/// The tool's name.
pub const DELETE_REPOSITORY: &str = "delete_repository";

/// The permission a GitHub App is told to need for each tool, in the words of the refusal.
const MERGE_PERMISSION: &str = "Pull requests: write and Contents: write";
const ADMIN_PERMISSION: &str = "Administration: write";

/// A repository the model named, as an address the code host takes.
struct Target {
    repo: RepoRef,
    /// `owner/name` as the address spells it.
    full_name: String,
    /// The [`named`](super::named) key.
    key: String,
}

/// The address `repo` names: a URL or a path as it is, `owner/name` on the default host.
fn address_of(repo: &str, default_host: &str) -> Option<String> {
    let repo = repo.trim();
    if repo.contains("://") || repo.starts_with('/') {
        return Some(repo.to_owned());
    }
    let (owner, name) = repo.split_once('/')?;
    (valid_owner(owner) && valid_name(name) && !name.contains('/'))
        .then(|| format!("https://{default_host}/{owner}/{name}"))
}

/// What a tool does with the repository it is given.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Use {
    /// A merge: `repo` may be the name of a slot of the workspace or any repository on the code
    /// host's host. Whether the pull request is one to merge is decided with the pull request: the
    /// conversation pushed its head, or the person said yes to the tool's question about it.
    Merge,
    /// A rename or a deletion: the repository must be one the person named, agreed to add or had
    /// the run create (`RunNotes::named_repos`).
    Change,
}

/// The repository `repo` names, checked against the policy of the workspace, the code host's own
/// host and what the person gave this run (see [`Use`]). A refusal is the result for the model.
async fn target(
    env: &ToolEnv,
    ctx: &ToolCtx,
    notes: &RunNotes,
    repo: &str,
    using: Use,
) -> Result<Target, ToolOutput> {
    let host = &env.settings.default_repo_host;
    let mut address = None;
    if using == Use::Merge
        && let Ok(workspace) = env.workspaces.run(&ctx.root_run_id().to_string())
        && let Ok(slots) = workspace.slots().await
        && let Ok(slot) = resolve_slot(slots, non_empty(repo), host)
        && let Some(wt) = slot.worktree()
    {
        address = Some(wt.repo().url.clone());
    }
    let Some(address) = address.or_else(|| address_of(repo, host)) else {
        return Err(ToolOutput::error(format!(
            "{repo:?} is not a repository: give its address (https://github.com/<owner>/<name>) \
             or `owner/name`"
        )));
    };
    let at = RepoRef::new(address.as_str(), "main");
    let Some(key) = key_of_argument(&address) else {
        return Err(ToolOutput::error(format!(
            "{repo:?} is not a repository address"
        )));
    };
    if let Err(e) = env.workspaces.check_repository(&at) {
        return Err(ToolOutput::error(format!(
            "{address} cannot be used here: {e}"
        )));
    }
    let loc = at
        .locate()
        .map_err(|e| ToolOutput::error(format!("{address} cannot be read: {e}")))?;
    // The code host acts on its own host's repositories: an address on another host (a mirror, a
    // look-alike) is not one it can be told to merge, rename or delete.
    let host_key = default_host_key(host);
    if split_key(&key).is_none_or(|(on, _, _)| on != host_key) {
        return Err(ToolOutput::error(format!(
            "{address} is not on {host}, the host this coder's code host serves: nothing is merged, \
             renamed or deleted anywhere else"
        )));
    }
    if using == Use::Change && !notes.named_repos.contains(&key) {
        return Err(ToolOutput::error(not_named(
            &address,
            &notes.named_repos,
            "act on (the full address, or `owner/name`) and call this tool again with it; a \
             repository you were not given is not one to merge, rename or delete.",
        )));
    }
    Ok(Target {
        full_name: format!("{}/{}", loc.owner, loc.name),
        repo: at,
        key,
    })
}

/// What the host refused, as the result for the model: its own words and the class of the error. A
/// failure worth trying again is the tool's transient error instead. Decided from the error's class,
/// never from which error it is or what it says.
fn refused(what: &str, permission: &str, e: &WorkspaceError) -> Outcome {
    let said = adam_error::report(e);
    let class = e.class();
    if class.is_retryable() {
        return Err(ToolError::Transient(format!(
            "GitHub could not {what}: {said}"
        )));
    }
    let more = match class {
        ErrorClass::Unauthenticated => format!(
            " The credentials may not do this: a GitHub App needs the {permission} permission on \
             the repository."
        ),
        ErrorClass::Rejected => " GitHub's own rules decide at every tier (branch protection, \
             required checks and reviews, conflicts, a repository that moved)."
            .to_owned(),
        _ => String::new(),
    };
    let class = format!("{class:?}").to_ascii_lowercase();
    Ok(ToolOutput::error(format!(
        "GitHub refused to {what}: {said} (class: {class}).{more} Do not look for another way: \
         tell the person what GitHub said."
    )))
}

/// The notes of the run, as the tools read them to decide: **no notes are a plan**, as in `Gated`.
async fn run_notes(env: &ToolEnv, run: &str) -> Result<RunNotes, ToolError> {
    Ok(env
        .notes
        .load_existing(run)
        .await
        .map_err(|e| notes_error(&e))?
        .unwrap_or_else(|| RunNotes {
            tier: Some(AccessTier::Plan),
            ..RunNotes::default()
        }))
}

/// The refusal for the model when the run works under a tier below `needs` for `tool`, read from the
/// notes of the run.
fn below(env: &ToolEnv, notes: &RunNotes, tool: &str, needs: AccessTier) -> Option<ToolOutput> {
    let tier = env.tier_of(notes);
    (needs > tier).then(|| ToolOutput::error(crate::tier::refusal(tool, needs, tier)))
}

/// A longest piece of the host's words about a pull request that is shown to the person.
const MAX_SHOWN: usize = 200;

/// `text`, the host's words, as one line the person reads: whitespace collapsed, nothing that is
/// not visible, at most [`MAX_SHOWN`] characters.
fn one_line(text: &str) -> String {
    let flat = text
        .split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|c| !adam_ui::is_format_or_control(*c))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut out: String = flat.chars().take(MAX_SHOWN).collect();
    if flat.chars().count() > MAX_SHOWN {
        out.push('…');
    }
    out
}

/// Whether the pull request is the conversation's own: **Path A**. Its head is in this very
/// repository and is a commit this conversation pushed for that branch (a branch of the same name that
/// somebody else pushed to since, or one in a fork, is not that), **and** it is against the base branch
/// the conversation pushed the branch for: somebody can open a pull request from the agent's branch
/// into another branch (a release branch), and that one is not what the person asked for.
fn is_the_conversations_own(notes: &RunNotes, target: &Target, status: &PullRequestStatus) -> bool {
    status
        .head_repo
        .as_deref()
        .is_some_and(|head| head.eq_ignore_ascii_case(&target.full_name))
        && notes.pushed_commit(&target.key, &status.head_branch, &status.head_sha)
        && notes.pushed_base(&target.key, &status.head_branch) == Some(status.base_branch.as_str())
}

/// How the checks of pull request `number` of the repository with `key` stand: `Ok(false)` when they
/// passed, `Ok(true)` when they did not but the conversation opened it with `accept_red_checks` (an
/// override that the person must say yes to), and `Err` with why they stop the merge. `own` is
/// whether it is the conversation's own ([`is_the_conversations_own`]).
fn judge_checks(
    notes: &RunNotes,
    key: &str,
    number: u64,
    status: &PullRequestStatus,
    own: bool,
) -> Result<bool, String> {
    let red_accepted = notes.red_accepted(key, number);
    match &status.checks {
        Checks::Pending(which) => Err(format!(
            "The checks of pull request #{number} have not finished: {which}. Do not merge it yet: \
             wait for them and look again, or tell the person they are still running."
        )),
        // The override is for the conversation's own commit: after somebody else's push the
        // acceptance that opened the pull request says nothing about what is there now.
        Checks::Failing(_) | Checks::Unreported(_) if red_accepted && own => Ok(true),
        // Nobody can say yes to these for the person: the checks must pass.
        Checks::Failing(why) => Err(format!(
            "The checks of pull request #{number} did not pass: {why}. It is merged only when they \
             pass: do not merge it. Tell the person which failed."
        )),
        Checks::Unreported(why) => Err(format!(
            "No check passed for pull request #{number}: {why}. It is merged only when its checks \
             pass: do not merge it. Tell the person."
        )),
        Checks::Passing => {
            let nothing_checked_it =
                !status.ci_reported && own && !notes.is_verified(&status.head_sha);
            match (nothing_checked_it, red_accepted) {
                (false, _) => Ok(false),
                (true, true) => Ok(true),
                (true, false) => Err(format!(
                    "Nothing has checked the head of pull request #{number}: the host reports no \
                     check for it and the checks you ran did not pass on this very commit. Run the \
                     project's checks on it (`run_checks`, `commit_and_push`, `open_pull_request`), \
                     or tell the person."
                )),
            }
        }
    }
}

/// The arguments of `ask_user` for the form that asks whether to merge: the words are the tool's,
/// from what the host said, and the one question's id says what a yes is a yes to
/// ([`merge_question`]).
fn merge_form(
    target: &Target,
    status: &PullRequestStatus,
    method: MergeMethod,
    delete_branch: bool,
    red: bool,
    id: &str,
) -> Value {
    let number = status.number;
    let lead = format!(
        "Merge {}#{number} into {}?",
        target.full_name,
        one_line(&status.base_branch)
    );
    let from = match status.head_repo.as_deref() {
        Some(head) if head.eq_ignore_ascii_case(&target.full_name) => String::new(),
        Some(head) => format!(" from the fork {}", one_line(head)),
        None => " from a fork that no longer exists".to_owned(),
    };
    let checks = match &status.checks {
        Checks::Passing if status.ci_reported => "passed".to_owned(),
        Checks::Passing => "the host reports none".to_owned(),
        Checks::Failing(why) => format!("FAILED, {}", one_line(why)),
        Checks::Unreported(why) => format!("none ran, {}", one_line(why)),
        Checks::Pending(which) => format!("still running, {}", one_line(which)),
    };
    let sha: String = status.head_sha.chars().take(10).collect();
    let mut why = format!(
        "Branch {}{from}, commit {sha}. Checks: {checks}. Method: {}{}.",
        one_line(&status.head_branch),
        method.as_str(),
        if delete_branch {
            ", and its branch is deleted after"
        } else {
            ""
        },
    );
    if red {
        why.push_str(" You accepted red checks to open it; this is a separate yes.");
    }
    why.push(' ');
    why.push_str(&one_line(&status.url));
    json!({
        "question": lead,
        "title": "Merge a pull request",
        "description": why,
        "choices": [{
            "id": id,
            "question": lead,
            "options": [
                {"value": MERGE, "label": "Merge it"},
                {"value": NOT_NOW, "label": "Not now"},
            ],
            "multiple": false,
        }],
    })
}

/// Merge a pull request. One this conversation pushed (its head is a commit you pushed), whose
/// checks passed on that commit, is merged at once. Any other pull request, or one you opened with
/// `accept_red_checks`, is put to the person by this tool itself, in a form of its own with "Merge
/// it" and "Not now": do not ask them first in words, and after they chose "Merge it" call this tool
/// again with the same arguments. After "Not now" do not call it again for that pull request unless
/// the person asks. A check still running is waited for, and a failed one stops the merge. `repo`
/// is a repository of your workspace or `owner/name` on the code host. `method` is `squash` (the
/// default), `merge` or `rebase`; with `delete_branch: true` the pull request's branch is deleted
/// afterwards, when it is a branch of that very repository. GitHub's own rules decide too (branch
/// protection, required reviews, conflicts): if it refuses you get its words, and you tell the
/// person; do not look for another way. It needs the auto access tier or above; at bypass nothing
/// is asked.
#[tool(asks_user, label = "Merge a pull request", icon = "git")]
pub async fn merge_pull_request(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// The repository: its slot's name in your workspace, `owner/name`, or its address
    repo: String,
    /// The pull request's number
    number: u64,
    /// `squash` (the default), `merge` or `rebase`
    method: Option<String>,
    /// Delete the pull request's branch after the merge (only a branch of that repository)
    delete_branch: Option<bool>,
) -> Outcome {
    if ctx.is_cancelled() {
        return Err(cancelled("nothing was merged"));
    }
    let method = match method.as_deref().and_then(non_empty) {
        None => MergeMethod::default(),
        Some(word) => match MergeMethod::from_str(word) {
            Ok(method) => method,
            Err(e) => return Ok(ToolOutput::error(e.to_string())),
        },
    };
    let delete_branch = delete_branch.unwrap_or(false);
    let run = ctx.root_run_id().to_string();
    let notes = run_notes(&env, &run).await?;
    if let Some(refusal) = below(&env, &notes, MERGE_PULL_REQUEST, AccessTier::Auto) {
        return Ok(refusal);
    }
    let target = match target(&env, ctx, &notes, &repo, Use::Merge).await {
        Ok(target) => target,
        Err(refusal) => return Ok(refusal),
    };
    let what = format!(
        "merge pull request #{number} of {} ({})",
        target.full_name,
        method.as_str()
    );
    let status = match env
        .code_host
        .pull_request_status(&target.repo, number)
        .await
    {
        Ok(status) => status,
        Err(e) => return refused(&what, MERGE_PERMISSION, &e),
    };
    if status.merged {
        return Ok(ToolOutput::text(format!(
            "Pull request #{number} of {} was merged already: nothing was done now.\npull request: {}",
            target.full_name, status.url
        )));
    }
    if !status.open {
        return Ok(ToolOutput::error(format!(
            "Pull request #{number} of {} is closed, not merged: there is nothing to merge.",
            target.full_name
        )));
    }
    let own = is_the_conversations_own(&notes, &target, &status);
    let red = match judge_checks(&notes, &target.key, number, &status, own) {
        Ok(red) => red,
        Err(why) => return Ok(ToolOutput::error(why)),
    };
    // Path A merges. Path B (another's pull request, or red checks accepted) merges at `bypass`, and
    // below it only on the person's yes to this tool's own question about exactly this.
    if (red || !own) && env.tier_of(&notes) < AccessTier::Bypass {
        let id = merge_question(
            &target.key,
            number,
            &status.head_sha,
            red,
            method.as_str(),
            delete_branch,
        );
        if !notes.agreed(MERGE_PULL_REQUEST, &id) {
            let form = merge_form(&target, &status, method, delete_branch, red, &id);
            return ask(&env, ctx, form).await;
        }
    }
    ctx.emit_progress(format!("merging #{number} of {}", target.full_name))
        .await;
    let merged = env
        .code_host
        .merge_pull_request(HostMerge {
            repo: target.repo.clone(),
            number,
            // The commit the checks above are about is the one that is merged.
            expected_sha: Some(status.head_sha),
            method,
            delete_branch,
        })
        .await;
    let merged = match merged {
        Ok(merged) => merged,
        Err(e) => return refused(&what, MERGE_PERMISSION, &e),
    };
    let mut text = if merged.already_merged {
        format!(
            "Pull request #{number} of {} was merged already (commit {}): nothing was done now.",
            target.full_name, merged.sha
        )
    } else {
        format!(
            "Merged pull request #{number} of {} ({}).\ncommit: {}",
            target.full_name,
            method.as_str(),
            merged.sha
        )
    };
    text.push_str(&format!("\npull request: {}", merged.url));
    match (&merged.deleted_branch, &merged.branch_note) {
        (Some(branch), _) => text.push_str(&format!("\nbranch deleted: {branch}")),
        (None, Some(note)) => text.push_str(&format!("\nbranch kept: {note}")),
        (None, None) => {}
    }
    Ok(ToolOutput::text(text))
}

/// Rename a repository the person named, to `new_name` (the same owner). GitHub keeps redirecting the
/// old address. It needs the bypass access tier, and GitHub's own rules and the permissions of your
/// credentials decide whether it happens: if it refuses you get its words, and you tell the person.
#[tool(label = "Rename a repository", icon = "git")]
pub async fn rename_repository(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// The repository the person named: `owner/name`, or its address
    repo: String,
    /// The new name: letters, digits, `.`, `_` and `-`, at most 100 characters
    new_name: String,
) -> Outcome {
    if ctx.is_cancelled() {
        return Err(cancelled("nothing was renamed"));
    }
    let Some(new_name) = non_empty(&new_name) else {
        return Ok(ToolOutput::error("new_name is required"));
    };
    if !valid_name(new_name) {
        return Ok(ToolOutput::error(format!(
            "{new_name:?} is not a repository name: use letters, digits, `.`, `_` and `-` (at \
             most 100 characters, not `.` or `..`, not ending in `.git`)"
        )));
    }
    let run = ctx.root_run_id().to_string();
    let notes = run_notes(&env, &run).await?;
    if let Some(refusal) = below(&env, &notes, RENAME_REPOSITORY, AccessTier::Bypass) {
        return Ok(refusal);
    }
    let target = match target(&env, ctx, &notes, &repo, Use::Change).await {
        Ok(target) => target,
        Err(refusal) => return Ok(refusal),
    };
    if let Some(done) = notes.renamed_repos.iter().find(|r| r.from == target.key) {
        return Ok(ToolOutput::text(renamed_text(done)));
    }
    let current = target.full_name.rsplit('/').next().unwrap_or_default();
    if current.eq_ignore_ascii_case(new_name) {
        return Ok(ToolOutput::error(format!(
            "{} is called {new_name} already: nothing to rename",
            target.full_name
        )));
    }
    let what = format!("rename {} to {new_name}", target.full_name);
    ctx.emit_progress(format!("renaming {} to {new_name}", target.full_name))
        .await;
    let renamed = match env
        .code_host
        .rename_repository(&target.repo, new_name)
        .await
    {
        Ok(renamed) => renamed,
        Err(e) => return refused(&what, ADMIN_PERMISSION, &e),
    };
    // The new address is one the run may work on: the person asked for exactly this repository.
    let record = RenamedRepo {
        from: target.key,
        to: key_of_argument(&renamed.clone_url).unwrap_or_default(),
        full_name: renamed.full_name,
        html_url: renamed.html_url,
    };
    let mut notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    if !record.to.is_empty() {
        notes.name_repos([record.to.clone()]);
    }
    let text = renamed_text(&record);
    notes.renamed_repos.push(record);
    env.notes
        .save(&run, &notes)
        .await
        .map_err(|e| notes_error(&e))?;
    Ok(ToolOutput::text(text))
}

fn renamed_text(record: &RenamedRepo) -> String {
    format!(
        "Renamed to {}. GitHub keeps redirecting the old address.\nbrowse: {}",
        record.full_name, record.html_url
    )
}

/// Delete a repository the person named, for good. It needs the bypass access tier, and `confirm`
/// must be the repository's `owner/name` exactly (a guard against a slip: nothing is deleted when
/// it differs). GitHub's own rules and the permissions of your credentials decide whether it
/// happens: if it refuses you get its words, and you tell the person.
#[tool(label = "Delete a repository", icon = "delete")]
pub async fn delete_repository(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// The repository the person named: `owner/name`, or its address
    repo: String,
    /// The repository's `owner/name` again, exactly: the guard against deleting the wrong one
    confirm: String,
) -> Outcome {
    if ctx.is_cancelled() {
        return Err(cancelled("nothing was deleted"));
    }
    let run = ctx.root_run_id().to_string();
    let notes = run_notes(&env, &run).await?;
    if let Some(refusal) = below(&env, &notes, DELETE_REPOSITORY, AccessTier::Bypass) {
        return Ok(refusal);
    }
    let target = match target(&env, ctx, &notes, &repo, Use::Change).await {
        Ok(target) => target,
        Err(refusal) => return Ok(refusal),
    };
    if !confirm.trim().eq_ignore_ascii_case(&target.full_name) {
        return Ok(ToolOutput::error(format!(
            "Nothing was deleted: `confirm` must be `{}`, exactly the repository `repo` names, and \
             it is {:?}. Call again with `confirm` set to the repository you mean, only if the \
             person asked for it to be deleted.",
            target.full_name,
            confirm.trim()
        )));
    }
    let lowered = target.full_name.to_ascii_lowercase();
    if notes.deleted_repos.contains(&lowered) {
        return Ok(ToolOutput::text(deleted_text(&target.full_name)));
    }
    let what = format!("delete {}", target.full_name);
    ctx.emit_progress(format!("deleting {}", target.full_name))
        .await;
    if let Err(e) = env.code_host.delete_repository(&target.repo).await {
        return refused(&what, ADMIN_PERMISSION, &e);
    }
    let mut notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    notes.deleted_repos.push(lowered);
    env.notes
        .save(&run, &notes)
        .await
        .map_err(|e| notes_error(&e))?;
    Ok(ToolOutput::text(deleted_text(&target.full_name)))
}

fn deleted_text(full_name: &str) -> String {
    format!("Deleted {full_name}. It cannot be taken back.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_is_an_address_or_owner_slash_name() {
        let host = "github.com";
        assert_eq!(
            address_of("acme/widgets", host).as_deref(),
            Some("https://github.com/acme/widgets")
        );
        assert_eq!(
            address_of(" https://github.com/acme/widgets ", host).as_deref(),
            Some("https://github.com/acme/widgets")
        );
        assert_eq!(
            address_of("/srv/git/widgets.git", host).as_deref(),
            Some("/srv/git/widgets.git")
        );
        // A bare word is no address, and neither is a path with a segment too many.
        for bad in [
            "widgets",
            "",
            "a/b/c",
            "acme/",
            "/widgets",
            "a b/c",
            "acme/x.git",
        ] {
            let got = address_of(bad, host);
            assert!(
                got.is_none() || bad.starts_with('/'),
                "{bad:?} gave {got:?}"
            );
        }
    }
}
