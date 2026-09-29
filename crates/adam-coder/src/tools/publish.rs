//! `commit_and_push { message }` and `open_pull_request { title, body }`.

use adam::Artifact;
use adam::prelude::*;
use adam_workspace::NewPullRequest;
use serde_json::json;

use super::gitcli::{commits_ahead, head_sha, head_tree};
use super::notes::PullRequestNote;
use super::{Outcome, ToolEnv, cancelled, non_empty, notes_error, workspace_error};

// Commits everything in the worktree and pushes the run's branch.
//
// Idempotent by construction: with no changes `commit_all` does nothing, the
// reported commit is `HEAD`, and pushing a commit the remote already has is a
// no-op. So running it twice (a crash before the result was journaled) neither
// duplicates a commit nor a push.

/// Commit every change in the worktree with the given message and push the
/// branch. Use a Conventional Commit message (feat(scope): ..., fix: ...).
/// Make small, focused commits: call it after each coherent piece of work.
#[tool]
pub async fn commit_and_push(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Commit message
    message: String,
) -> Outcome {
    if ctx.is_cancelled() {
        return Err(cancelled("nothing was committed or pushed"));
    }
    let Some(message) = non_empty(&message) else {
        return Ok(ToolOutput::error("message is required"));
    };
    let wt = match env.worktree(ctx).await {
        Ok(wt) => wt,
        Err(outcome) => return outcome,
    };
    let run = ctx.run_id().to_string();
    let mut notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    if notes.cycles_exhausted(env.settings.max_check_cycles) {
        return Ok(ToolOutput::error(
            "Check-cycle limit reached with failing checks: nothing may be committed or \
             pushed. Stop now and report what you did and which check still fails.",
        ));
    }

    let committed = match wt.commit_all(message, &env.settings.identity).await {
        Ok(sha) => sha,
        Err(e @ adam_workspace::WorkspaceError::Invalid(_)) => {
            return Ok(ToolOutput::error(e.to_string()));
        }
        Err(e) => return Err(workspace_error(&e)),
    };
    let base = &wt.repo().base_branch;
    let ahead = commits_ahead(wt.path(), base).await;
    if committed.is_none() && ahead == Some(0) {
        return Ok(ToolOutput::error(format!(
            "Nothing to commit: the worktree has no changes and the branch has no commits \
             beyond origin/{base}. Have OpenCode make the change first."
        )));
    }
    let Some(sha) = head_sha(wt.path()).await else {
        return Err(ToolError::Transient(
            "cannot read HEAD of the worktree".into(),
        ));
    };

    ctx.emit_progress(format!(
        "pushing {} ({})",
        wt.branch(),
        &sha[..sha.len().min(10)]
    ))
    .await;
    if let Err(e) = wt.push().await {
        return Err(env.delivery_error(ctx, &e).await);
    }

    notes.pushed_sha = Some(sha.clone());
    env.notes
        .save(&run, &notes)
        .await
        .map_err(|e| notes_error(&e))?;

    let summary = match &committed {
        Some(_) => format!("Committed {sha} and pushed branch {}.", wt.branch()),
        None => format!(
            "Nothing new to commit; branch {} is pushed at {sha}.",
            wt.branch()
        ),
    };
    Ok(ToolOutput::text(summary).with_artifact(Artifact {
        name: "branch".into(),
        mime_type: Some("application/json".into()),
        data: json!({
            "repository": wt.repo().url,
            "branch": wt.branch(),
            "base_branch": base,
            "commit": sha,
        }),
    }))
}

// Opens the pull request for the pushed branch.
//
// Refuses unless the branch's `HEAD` was pushed by `commit_and_push` and the
// last check run passed **on exactly the code the pull request contains** (the
// tree of `HEAD`; a change made after the run, committed or not, is
// unverified). The one override is `accept_red_checks: true`, which
// the prompt reserves for explicit user consent; it never overrides an
// exhausted check budget, and a pull request opened that way says so in its
// body. Idempotent: `CodeHost::open_pull_request` returns the open pull
// request of the same head instead of creating another.

/// Open the pull request for the pushed branch. Requires that everything is
/// pushed with commit_and_push and that the last check run passed on exactly
/// this code (re-run the checks after your last change). The body must have a
/// summary and a verification section listing the commands you ran and their
/// results.
#[tool]
pub async fn open_pull_request(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Conventional Commit style title
    title: String,
    /// Markdown: summary, then verification
    body: String,
    /// Set true ONLY when the user explicitly said, in this conversation, that a pull request with failing (or no) checks is acceptable. Never set it on your own judgement.
    accept_red_checks: Option<bool>,
) -> Outcome {
    if ctx.is_cancelled() {
        return Err(cancelled("no pull request was opened"));
    }
    let (Some(title), Some(body)) = (non_empty(&title), non_empty(&body)) else {
        return Ok(ToolOutput::error("title and body are required"));
    };
    let accept_red = accept_red_checks.unwrap_or(false);
    let wt = match env.worktree(ctx).await {
        Ok(wt) => wt,
        Err(outcome) => return outcome,
    };
    let run = ctx.run_id().to_string();
    let mut notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    let max = env.settings.max_check_cycles;

    if notes.cycles_exhausted(max) {
        return Ok(ToolOutput::error(format!(
            "Check-cycle limit reached ({} of {max}) with failing checks: no pull request may \
             be opened. Stop now and report what you did and which check still fails.",
            notes.checks.failures
        )));
    }
    let Some(head) = head_sha(wt.path()).await else {
        return Err(ToolError::Transient(
            "cannot read HEAD of the worktree".into(),
        ));
    };
    if notes.pushed_sha.as_deref() != Some(head.as_str()) {
        return Ok(ToolOutput::error(format!(
            "The branch is not pushed at its current commit ({head}). Call commit_and_push \
             first (uncommitted changes are not in the pull request)."
        )));
    }

    // "Green" means: the last check run passed, on exactly the code this
    // pull request contains (the tree of the pushed HEAD). A change made
    // after the checks passed, committed or not, is not verified.
    let tree = head_tree(wt.path()).await;
    let verified = notes.verified_tree(tree.as_deref());
    let red = !verified;
    if red && !accept_red {
        let why = match &notes.checks.last {
            None => "no check was run".to_owned(),
            Some(last) if !last.passed => format!(
                "the last check run (`{}`) failed (exit code {:?})",
                last.command, last.exit_code
            ),
            Some(last) => format!(
                "the code changed since the last passing check run (`{}`): the branch is not \
                 the code that was verified",
                last.command
            ),
        };
        return Ok(ToolOutput::error(format!(
            "Refusing to open a pull request: {why}. Fix the problem, then re-run the checks \
             after your last change and push again. Only if the user has explicitly accepted a \
             pull request without verified checks (ask with ask_user if unsure), call again \
             with accept_red_checks: true."
        )));
    }

    let mut body = body.to_owned();
    if red {
        body.push_str(
            "\n\n> **Note:** the checks were not green for this exact code (failing, not run, or run \
             before the last change) when this pull request was opened. The requester \
             explicitly accepted that.\n",
        );
    }
    ctx.emit_progress(format!("opening a pull request from {}", wt.branch()))
        .await;
    let opened = env
        .code_host
        .open_pull_request(NewPullRequest {
            repo: wt.repo().clone(),
            head: wt.branch().to_owned(),
            title: title.to_owned(),
            body,
            draft: env.settings.draft_pull_requests,
        })
        .await;
    let pr = match opened {
        Ok(pr) => pr,
        Err(e) => return Err(env.delivery_error(ctx, &e).await),
    };

    notes.pull_request = Some(PullRequestNote {
        url: pr.url.clone(),
        number: pr.number,
        red_checks_accepted: red,
    });
    env.notes
        .save(&run, &notes)
        .await
        .map_err(|e| notes_error(&e))?;

    let mut text = format!("Pull request #{} is open: {}", pr.number, pr.url);
    if let Ok(dirty) = wt.status().await
        && !dirty.is_empty()
    {
        let names: Vec<&str> = dirty.iter().take(10).map(|f| f.path.as_str()).collect();
        text.push_str(&format!(
            "\nNote: uncommitted changes in the worktree are not part of the pull request: {}",
            names.join(", ")
        ));
    }
    Ok(ToolOutput::text(text).with_artifact(Artifact {
        name: "pull_request".into(),
        mime_type: Some("application/json".into()),
        // The number is a string: A2A carries JSON numbers as
        // floats (`7` would arrive as `7.0`).
        data: json!({
            "url": pr.url,
            "number": pr.number.to_string(),
            "branch": pr.head,
            "repository": wt.repo().url,
        }),
    }))
}
