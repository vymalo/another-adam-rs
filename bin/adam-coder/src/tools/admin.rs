//! `merge_pull_request { repo, number, method?, delete_branch? }`, `rename_repository { repo, new_name }`
//! and `delete_repository { repo, confirm }`: what the access tiers of the person unlock.
//!
//! | Tool | Tier | On which repository |
//! |---|---|---|
//! | `merge_pull_request` | `auto` | one of the workspace, or one the person named, on the code host's own host |
//! | `rename_repository`, `delete_repository` | `bypass` | one the person named, on the code host's own host |
//!
//! The tier is checked by the tool set, by every tool again (`Gated`) and by these three themselves
//! from the notes of the run ([`tier`](crate::tier)). **GitHub's own rules decide at every tier**:
//! branch protection, required checks and reviews, and the permissions the App was granted. A refusal
//! of GitHub is the tool's error result **in GitHub's words**, with the class of the error, and the
//! model tells the person; none of these tools looks for another way.
//!
//! **A merge needs more than the tier.** All of these hold, each checked here from what the host
//! says and from what the person wrote, never from what the model says:
//!
//! 1. *The pull request is one this conversation may touch*: its head branch is in this repository
//!    and one a run of the conversation pushed (`RunNotes::pushed_branches`, an earlier task's too),
//!    or the person named the pull request (its number or URL, `RunNotes::named_pulls`).
//! 2. *The checks passed* on the head commit ([`Checks::Passing`](adam_workspace::Checks)), and that
//!    very commit is the one merged (`MergePullRequest::expected_sha`).
//! 3. *Red checks need a fresh yes*: a pull request this conversation opened with
//!    `accept_red_checks` (`RunNotes::red_pulls`) is merged only when the person's **newest**
//!    message names it and says to merge it (`RunNotes::merge_asks`); with that message, checks that
//!    failed or never ran no longer stop it, a check still running does.
//! 4. *The prompt says so*: merge only when the person asked for it.
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

/// The repository `repo` names, checked against the policy of the workspace and against what the
/// person gave this run: with `in_workspace`, a repository that has a slot in the workspace is
/// enough; otherwise (and for any other repository) the person must have named it, agreed to add it
/// or had it created by this run (`RunNotes::named_repos`). A refusal is the result for the model.
async fn target(
    env: &ToolEnv,
    ctx: &ToolCtx,
    notes: &RunNotes,
    repo: &str,
    in_workspace: bool,
) -> Result<Target, ToolOutput> {
    let host = &env.settings.default_repo_host;
    let mut address = None;
    if in_workspace
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
    let in_a_slot = address_in_slots(env, ctx, &key).await;
    let granted = notes.named_repos.contains(&key) || (in_workspace && in_a_slot);
    if !granted {
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

/// Whether some slot of the run's workspace is the repository with `key`.
async fn address_in_slots(env: &ToolEnv, ctx: &ToolCtx, key: &str) -> bool {
    let Ok(workspace) = env.workspaces.run(&ctx.root_run_id().to_string()) else {
        return false;
    };
    let Ok(slots) = workspace.slots().await else {
        return false;
    };
    slots.iter().any(|slot| {
        slot.worktree()
            .and_then(|wt| key_of_argument(&wt.repo().url))
            .is_some_and(|k| k == key)
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

/// The refusal for the model when the run works under a tier below `needs` for `tool`, read from the
/// notes of the run.
fn below(env: &ToolEnv, notes: &RunNotes, tool: &str, needs: AccessTier) -> Option<ToolOutput> {
    let tier = env.tier_of(notes);
    (needs > tier).then(|| ToolOutput::error(crate::tier::refusal(tool, needs, tier)))
}

/// Why pull request `number` of the repository with `key` is not one to touch, or `None` when it is:
/// its head branch is in this very repository and one this conversation pushed, or the person named
/// the pull request.
fn untouchable(
    notes: &RunNotes,
    target: &Target,
    status: &PullRequestStatus,
    number: u64,
) -> Option<String> {
    let in_this_repository = status
        .head_repo
        .as_deref()
        .is_some_and(|head| head.eq_ignore_ascii_case(&target.full_name));
    if in_this_repository && notes.has_pushed(&target.key, &status.head_branch) {
        return None;
    }
    if notes.names_pull(&target.key, number) {
        return None;
    }
    Some(format!(
        "Pull request #{number} of {} is not one to merge: its branch `{}` was not pushed by this \
         conversation{}, and the person has not named the pull request (its number or its URL in \
         their words). Do not merge it. Tell the person what it is and ask them to say, in a message \
         that names #{number}, that it should be merged.",
        target.full_name,
        status.head_branch,
        if in_this_repository {
            ""
        } else {
            " (it comes from another repository, a fork)"
        }
    ))
}

/// Why the checks of pull request `number` of the repository with `key` stop the merge, or `None`
/// when they do not (rules 2 and 3 of the module docs).
fn stopped_by_checks(notes: &RunNotes, key: &str, number: u64, checks: &Checks) -> Option<String> {
    let red = notes.red_accepted(key, number);
    let asked = notes.asks_to_merge(key, number);
    let fresh_yes = "Do not merge it. Tell the person; if they want it merged anyway they say so in a \
        new message that names the pull request (its number or URL) and says to merge it.";
    if red && !asked {
        return Some(format!(
            "Pull request #{number} was opened with red checks that the person accepted for opening \
             it, not for merging it. {fresh_yes}"
        ));
    }
    match checks {
        Checks::Passing => None,
        Checks::Failing(why) | Checks::Unreported(why) if red => {
            tracing::info!(number, %why, "merging a pull request with red checks: the person said to");
            None
        }
        Checks::Failing(why) => Some(format!(
            "The checks of pull request #{number} did not pass: {why}. {fresh_yes}"
        )),
        Checks::Unreported(why) => Some(format!(
            "No check passed for pull request #{number}: {why}. {fresh_yes}"
        )),
        Checks::Pending(which) => Some(format!(
            "The checks of pull request #{number} have not finished: {which}. Do not merge it yet: \
             wait for them and look again, or tell the person they are still running."
        )),
    }
}

/// Merge a pull request, only when the person asked for it. The pull request is one this
/// conversation pushed the branch of, or one the person named (its number or URL); its checks have
/// passed (checked here, on the head commit that is merged); and, if it was opened with
/// `accept_red_checks`, the person's newest message names it and says to merge it. `repo` is a
/// repository of your workspace or one the person named. `method` is `squash` (the default), `merge`
/// or `rebase`; with `delete_branch: true` the pull request's branch is deleted afterwards, when it
/// is a branch of that very repository. GitHub's own rules decide too (branch protection, required
/// reviews, conflicts): if it refuses you get its words, and you tell the person; do not look for
/// another way. It needs the auto access tier or above.
#[tool(label = "Merge a pull request", icon = "git")]
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
    let run = ctx.root_run_id().to_string();
    let notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    if let Some(refusal) = below(&env, &notes, MERGE_PULL_REQUEST, AccessTier::Auto) {
        return Ok(refusal);
    }
    let target = match target(&env, ctx, &notes, &repo, true).await {
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
    if let Some(why) = untouchable(&notes, &target, &status, number) {
        return Ok(ToolOutput::error(why));
    }
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
    if let Some(why) = stopped_by_checks(&notes, &target.key, number, &status.checks) {
        return Ok(ToolOutput::error(why));
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
            delete_branch: delete_branch.unwrap_or(false),
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
    let notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    if let Some(refusal) = below(&env, &notes, RENAME_REPOSITORY, AccessTier::Bypass) {
        return Ok(refusal);
    }
    let target = match target(&env, ctx, &notes, &repo, false).await {
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
    let notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    if let Some(refusal) = below(&env, &notes, DELETE_REPOSITORY, AccessTier::Bypass) {
        return Ok(refusal);
    }
    let target = match target(&env, ctx, &notes, &repo, false).await {
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
