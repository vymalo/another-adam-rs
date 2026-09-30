//! `prepare_workspace { repo_url, base_branch }`.

use adam::prelude::*;
use adam_workspace::RepoRef;

use super::named::key_of_argument;
use super::{Outcome, ToolEnv, non_empty, notes_error};

/// Branch used when the model leaves `base_branch` out.
const DEFAULT_BASE_BRANCH: &str = "main";

// Checks the repository out into this run's worktree.
//
// The run id is the workspace's run key, so a restarted or retried call finds
// the worktree it already made (`Workspaces::prepare` is idempotent per run)
// and keeps whatever is in it.
//
// The repository must be one the person named. The agent records the repositories of the
// person's own messages in the run notes (`RunNotes::named_repos`) before every step, and the
// argument is compared with those, never trusted on its own: a model that was given too little
// (a greeting, a vague task) must ask, not pick a repository.

/// Check the repository out into your private worktree, on a fresh branch
/// created from origin/<base_branch>. Call it once, first. Calling it again
/// for the same repository is harmless and keeps your changes. It works only on
/// a repository the person named in their messages: otherwise it refuses, and
/// you ask the person which one with ask_user.
#[tool]
pub async fn prepare_workspace(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// https://github.com/<owner>/<repo>
    repo_url: String,
    // Optional for the code (a missing or empty branch means `main`), but the model is told it is
    // required: it must say which branch the pull request goes against.
    /// Branch to start from and to open the pull request against, e.g. main
    #[schemars(required)]
    base_branch: Option<String>,
) -> Outcome {
    let Some(url) = non_empty(&repo_url) else {
        return Ok(ToolOutput::error("repo_url is required"));
    };
    let base = base_branch
        .as_deref()
        .and_then(non_empty)
        .unwrap_or(DEFAULT_BASE_BRANCH);
    let run = ctx.run_id().to_string();
    // An argument the workspace cannot read (not a URL or an absolute path) is its error to
    // report, below; every other one must be a repository of the person's.
    if let Some(key) = key_of_argument(url) {
        let notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
        if !notes.named_repos.contains(&key) {
            return Ok(ToolOutput::error(not_named(url, &notes.named_repos)));
        }
    }
    ctx.emit_progress(format!("preparing a worktree of {url} ({base})"))
        .await;
    let repo = RepoRef::new(url, base);
    let wt = match env.workspaces.prepare(&repo, &run).await {
        Ok(wt) => wt,
        Err(
            e @ (adam_workspace::WorkspaceError::Invalid(_)
            | adam_workspace::WorkspaceError::NotFound(_)
            | adam_workspace::WorkspaceError::Conflict(_)),
        ) => {
            // The model gave a bad repository or branch: tell it, do not fail.
            return Ok(ToolOutput::error(e.to_string()));
        }
        Err(e) => return Err(env.delivery_error(ctx, &e).await),
    };
    Ok(ToolOutput::text(format!(
        "Worktree ready.\nrepository: {url}\nbase branch: {base}\nbranch: {}\npath: {}",
        wt.branch(),
        wt.path().display()
    )))
}

/// What the model is told when it picks a repository the person did not name.
fn not_named(url: &str, named: &[String]) -> String {
    let said = if named.is_empty() {
        "The person has not named any repository.".to_owned()
    } else {
        format!("The repositories the person named: {}.", named.join(", "))
    };
    format!(
        "Refused: {url} is not a repository the person named in their messages. {said} Do not \
         choose or guess a repository. Ask the person with ask_user which repository to work on \
         (and which base branch), then call prepare_workspace with the one they name."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refusal_names_the_way_out() {
        let none = not_named("https://github.com/rust-lang/rust-clippy", &[]);
        assert!(none.contains("ask_user"), "{none}");
        assert!(none.contains("has not named any repository"), "{none}");
        assert!(none.contains("rust-clippy"), "{none}");
        let some = not_named(
            "https://github.com/a/b",
            &["github.com/acme/widgets".into()],
        );
        assert!(some.contains("github.com/acme/widgets"), "{some}");
        assert!(some.contains("ask_user"), "{some}");
    }
}
