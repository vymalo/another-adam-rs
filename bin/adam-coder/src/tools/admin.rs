//! `merge_pull_request { repo, number, method?, delete_branch? }`, `rename_repository { repo, new_name }`
//! and `delete_repository { repo, confirm }`: what the access tiers of the person unlock.
//!
//! | Tool | Tier | On which repository |
//! |---|---|---|
//! | `merge_pull_request` | `auto` | one of the workspace, or one the person named |
//! | `rename_repository`, `delete_repository` | `bypass` | one the person named |
//!
//! The tier is checked by the tool set and by every tool again ([`tier`](crate::tier), `Gated`), so
//! these three are never reached below it. **GitHub's own rules decide at every tier**: branch
//! protection, required checks and reviews, and the permissions the App was granted. A refusal of
//! GitHub is the tool's error result **in GitHub's words**, with the class of the error, and the
//! model tells the person; none of these tools looks for another way.
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
use adam_error::Classify;
use adam_workspace::{MergeMethod, MergePullRequest as HostMerge, RepoRef, WorkspaceError};

use super::create::{valid_name, valid_owner};
use super::named::key_of_argument;
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
/// failure worth trying again is the tool's transient error instead.
fn refused(what: &str, permission: &str, e: &WorkspaceError) -> Outcome {
    let said = adam_error::report(e);
    if e.is_retryable() {
        return Err(ToolError::Transient(format!(
            "GitHub could not {what}: {said}"
        )));
    }
    let class = format!("{:?}", e.class()).to_ascii_lowercase();
    let more = match e {
        WorkspaceError::Auth(_) => format!(
            " The credentials may not do this: a GitHub App needs the {permission} permission on \
             the repository."
        ),
        WorkspaceError::Conflict(_) => {
            " GitHub's own rules decide at every tier (branch protection, required checks and \
             reviews, conflicts)."
                .to_owned()
        }
        _ => String::new(),
    };
    Ok(ToolOutput::error(format!(
        "GitHub refused to {what}: {said} (class: {class}).{more} Do not look for another way: \
         tell the person what GitHub said."
    )))
}

/// Merge a pull request of a repository you work on (one in your workspace, or one the person
/// named). `method` is `squash` (the default), `merge` or `rebase`; with `delete_branch: true` the
/// pull request's branch is deleted afterwards, when it is a branch of that very repository. GitHub's
/// own rules decide whether it merges (branch protection, required checks and reviews, conflicts): if
/// it refuses you get its words, and you tell the person; do not look for another way. It needs the
/// auto access tier or above.
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
    let target = match target(&env, ctx, &notes, &repo, true).await {
        Ok(target) => target,
        Err(refusal) => return Ok(refusal),
    };
    let what = format!(
        "merge pull request #{number} of {} ({})",
        target.full_name,
        method.as_str()
    );
    ctx.emit_progress(format!("merging #{number} of {}", target.full_name))
        .await;
    let merged = env
        .code_host
        .merge_pull_request(HostMerge {
            repo: target.repo.clone(),
            number,
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
