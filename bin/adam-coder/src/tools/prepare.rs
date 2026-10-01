//! `prepare_workspace { repo_url, base_branch?, branch? }`.

use adam::prelude::*;
use adam_workspace::RepoRef;

use super::named::{key_of_argument, listed};
use super::notes::RunNotes;
use super::{Outcome, ToolEnv, non_empty, notes_error, workspace_error};

// Adds the repository to this run's workspace: a slot with a worktree of it.
//
// A workspace holds several repositories (one slot each, named after the repository: the name the
// other tools take as `repo`), and a repository is added once: asking again returns its slot, with
// whatever is in it.
//
// `base_branch` is optional: without it the worktree starts from the repository's own default
// branch (what the remote's `HEAD` names), so a repository whose default is `master` needs no
// guess. A branch that does not exist is reported with the branches that do (the workspace
// lists the first thirty), so that the model can pick one or ask.
//
// The run id is the workspace's run key, so a restarted or retried call finds
// the slot it already made (`RunWorkspace::add_repository` is idempotent per repository)
// and keeps whatever is in it.
//
// The repository must be one the person named. The agent records the repositories of the
// person's own messages in the run notes (`RunNotes::named_repos`) before every step, and the
// argument is compared with those, never trusted on its own: a model that was given too little
// (a greeting, a vague task) must ask, not pick a repository.
//
// With `branch` the worktree continues a branch an earlier task of this conversation pushed, so
// that a rework updates the same pull request. The branch is checked the same way: only one that
// a `commit_and_push` result of this conversation reported for this repository
// (`RunNotes::pushed_branches`, filled by the agent from the history) is accepted, never a name
// the model found in the repository or was told by its content. The workspace adds its own
// limits: an `agent/*` branch that exists on the remote, pushed without force.

/// Check the repository out into your private workspace, on a fresh branch
/// created from origin/<base_branch>, which is the repository's default branch
/// when you leave base_branch out. Call it once, first. Calling it again
/// for the same repository is harmless and keeps your changes; another repository
/// is added next to it, and the tools then take `repo` to say which one. It works only on
/// a repository the person named in their messages: otherwise it refuses, and
/// you ask the person which one with ask_user. To carry on with work an earlier
/// task of this conversation pushed (a rework, a follow-up), pass that branch as
/// `branch`: the worktree starts from it and open_pull_request updates its pull request.
#[tool]
pub async fn prepare_workspace(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// https://github.com/<owner>/<repo>
    repo_url: String,
    /// Branch to start from and to open the pull request against, e.g. main. Leave out for the repository's default branch.
    base_branch: Option<String>,
    /// A branch that commit_and_push reported earlier in this conversation (agent/...), to carry on with it and update its pull request. Leave out to start a new branch.
    branch: Option<String>,
) -> Outcome {
    let Some(url) = non_empty(&repo_url) else {
        return Ok(ToolOutput::error("repo_url is required"));
    };
    let run = ctx.run_id().to_string();
    let continuing = branch.as_deref().and_then(non_empty);
    // An argument the workspace cannot read (not a URL or an absolute path) is its error to
    // report, below; every other one must be a repository of the person's. This comes **before**
    // anything that talks to a remote (the default branch is asked of the remote with the
    // credentials): a repository nobody named costs no request, no credential and no mirror, and
    // cannot be probed by leaving `base_branch` out.
    let key = key_of_argument(url);
    let mut recorded_base = None;
    if key.is_some() || continuing.is_some() {
        let notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
        if let Some(key) = &key
            && !notes.named_repos.contains(key)
        {
            return Ok(ToolOutput::error(not_named(
                url,
                &notes.named_repos,
                "work on (and which base branch), then call prepare_workspace with the one \
                 they name.",
            )));
        }
        if let Some(branch) = continuing {
            if !key.as_deref().is_some_and(|k| notes.has_pushed(k, branch)) {
                return Ok(ToolOutput::error(not_pushed(
                    branch,
                    url,
                    &notes,
                    key.as_deref(),
                )));
            }
            // The pull request of that branch is against the base it was opened with: a
            // continuing run works against the same one, whatever the model says now.
            recorded_base = key
                .as_deref()
                .and_then(|k| notes.pushed_base(k, branch))
                .map(str::to_owned);
        }
    }
    let base = match (&recorded_base, base_branch.as_deref().and_then(non_empty)) {
        (Some(recorded), _) => recorded.clone(),
        (None, Some(base)) => base.to_owned(),
        (None, None) => match default_base(&env, ctx, url).await {
            Ok(base) => base,
            Err(outcome) => return outcome,
        },
    };
    let base = base.as_str();
    match continuing {
        Some(branch) => {
            ctx.emit_progress(format!(
                "preparing a worktree of {url} on {branch} ({base})"
            ))
            .await;
        }
        None => {
            ctx.emit_progress(format!("preparing a worktree of {url} ({base})"))
                .await;
        }
    }
    let repo = RepoRef::new(url, base);
    let workspace = match env.workspaces.run(&run) {
        Ok(workspace) => workspace,
        Err(e) => return Err(workspace_error(&e)),
    };
    let prepared = match continuing {
        Some(branch) => workspace.add_repository_continuing(&repo, branch).await,
        None => workspace.add_repository(&repo).await,
    };

    let slot = match prepared {
        Ok(slot) => slot,
        Err(
            e @ (adam_workspace::WorkspaceError::Invalid(_)
            | adam_workspace::WorkspaceError::NotFound(_)
            | adam_workspace::WorkspaceError::Conflict(_)),
        ) => {
            // The model gave a bad repository or branch: tell it, do not fail.
            // The base of a continued branch is the base of its pull request: when that branch is
            // gone upstream, choosing another is not the way out.
            let fixed = match (&recorded_base, &e) {
                (Some(base), adam_workspace::WorkspaceError::NotFound(_))
                    if e.to_string()
                        .contains(&format!("branch {base} does not exist")) =>
                {
                    format!(
                        " {base} is the base of the pull request of {}: it is fixed by that pull \
                         request, so do not pick another base. Tell the person with ask_user.",
                        continuing.unwrap_or("the branch")
                    )
                }
                _ => String::new(),
            };
            return Ok(ToolOutput::error(format!("{e}{fixed}")));
        }
        Err(e) => return Err(env.delivery_error(ctx, &e).await),
    };
    let Some(wt) = slot.worktree() else {
        return Ok(ToolOutput::error(format!(
            "`{}` is a scratch project, not a repository",
            slot.dir()
        )));
    };
    if let Some(line) = wt.continues() {
        // What the verdict of a run that ends without a pull request says was not updated.
        let mut notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
        if notes.continues.as_deref() != Some(line) {
            notes.continues = Some(line.to_owned());
            env.notes
                .save(&run, &notes)
                .await
                .map_err(|e| notes_error(&e))?;
        }
    }
    let mut text = format!(
        "Worktree ready.\nrepository: {url}\nslot: {}\nbase branch: {base}\nbranch: {}\npath: {}",
        slot.dir(),
        wt.branch(),
        wt.path().display()
    );
    if continuing.is_some() {
        text.push_str(&format!(
            "\nThis continues {0}, the branch an earlier task pushed: its work is in the \
             worktree. commit_and_push pushes to a branch of this run's own; once the checks \
             have passed, open_pull_request moves {0} to that commit, which updates its pull \
             request.",
            wt.branch()
        ));
    }
    Ok(ToolOutput::text(text))
}

/// The branch to start from when the model gave none: the one this run's workspace already uses
/// for `url` (a repeated call needs no network), else the remote's default branch.
async fn default_base(env: &ToolEnv, ctx: &ToolCtx, url: &str) -> Result<String, Outcome> {
    if let Ok(workspace) = env.workspaces.run(&ctx.run_id().to_string())
        && let Ok(Some(slot)) = workspace.slot_for(&RepoRef::new(url, "HEAD")).await
        && let Some(existing) = slot.worktree()
    {
        return Ok(existing.repo().base_branch.clone());
    }
    match env.workspaces.default_branch(url).await {
        Ok(base) => Ok(base),
        Err(
            e @ (adam_workspace::WorkspaceError::Invalid(_)
            | adam_workspace::WorkspaceError::NotFound(_)),
        ) => Err(Ok(ToolOutput::error(format!(
            "{e} Pass base_branch, or ask the person which branch to start from with ask_user."
        )))),
        Err(e) => Err(Err(env.delivery_error(ctx, &e).await)),
    }
}

/// What the model is told when it asks to continue a branch that no `commit_and_push` of this
/// conversation reported for the repository.
fn not_pushed(branch: &str, url: &str, notes: &RunNotes, key: Option<&str>) -> String {
    let known: Vec<&str> = notes
        .pushed_branches
        .iter()
        .filter(|p| Some(p.repo.as_str()) == key)
        .map(|p| p.branch.as_str())
        .collect();
    let said = if known.is_empty() {
        format!("No commit_and_push of this conversation pushed a branch of {url}.")
    } else {
        format!(
            "The branches of {url} pushed in this conversation: {}.",
            known.join(", ")
        )
    };
    format!(
        "Refused: {branch} is not a branch that an earlier task of this conversation pushed. \
         {said} Do not pass a branch you found anywhere else. Call prepare_workspace again \
         without `branch` to start a new branch from the base branch."
    )
}

/// What the model is told when it picks a repository the person did not name: `then` finishes the
/// sentence "Ask the person with ask_user which repository to ..." (what to ask, and which tool the
/// answer goes to).
pub(super) fn not_named(url: &str, named: &[String], then: &str) -> String {
    // Only what the person wrote is ever listed, and not the words that are files.
    let named = listed(named);
    let said = if named.is_empty() {
        "The person has not named any repository.".to_owned()
    } else {
        format!("The repositories the person named: {}.", named.join(", "))
    };
    format!(
        "Refused: {url} is not a repository the person named in their messages. {said} Do not \
         choose or guess a repository. Ask the person with ask_user which repository to {then}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const THEN: &str = "work on, then call prepare_workspace with the one they name.";

    #[test]
    fn the_branch_refusal_lists_only_what_was_pushed_for_that_repository() {
        use crate::tools::notes::PushedBranch;
        let mut notes = RunNotes::default();
        let a_b = Some("github.com/a/b");
        let none = not_pushed("agent/x", "https://github.com/a/b", &notes, a_b);
        assert!(none.contains("No commit_and_push"), "{none}");
        assert!(none.contains("without `branch`"), "{none}");
        notes.name_pushed_branches([
            PushedBranch {
                repo: "github.com/a/b".into(),
                branch: "agent/one".into(),
                base: None,
            },
            PushedBranch {
                repo: "github.com/c/d".into(),
                branch: "agent/other".into(),
                base: None,
            },
        ]);
        let some = not_pushed("agent/x", "https://github.com/a/b", &notes, a_b);
        assert!(some.contains("agent/one"), "{some}");
        assert!(
            !some.contains("agent/other"),
            "another repository's branch is not offered: {some}"
        );
    }

    #[test]
    fn the_refusal_names_the_way_out() {
        let none = not_named("https://github.com/rust-lang/rust-clippy", &[], THEN);
        assert!(none.contains("ask_user"), "{none}");
        assert!(none.contains("has not named any repository"), "{none}");
        assert!(none.contains("rust-clippy"), "{none}");
        let some = not_named(
            "https://github.com/a/b",
            &["github.com/acme/widgets".into()],
            THEN,
        );
        assert!(some.contains("github.com/acme/widgets"), "{some}");
        assert!(some.contains("ask_user"), "{some}");
        // A word that is a file is not offered as a repository.
        let file = not_named(
            "https://github.com/a/b",
            &["github.com/src/main.rs".into()],
            THEN,
        );
        assert!(file.contains("has not named any repository"), "{file}");
        assert!(!file.contains("main.rs"), "{file}");
    }
}
