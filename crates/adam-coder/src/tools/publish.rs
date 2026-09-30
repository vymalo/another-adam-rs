//! `commit_and_push { message }` and `open_pull_request { title, body }`.

use adam::Artifact;
use adam::prelude::*;
use adam_llm_agent::TRUNCATION_MARKER_PREFIX;
use adam_workspace::{NewPullRequest, WorkspaceError};
use serde_json::json;

use super::checks::ChecksReport;
use super::gitcli::{commits_ahead, head_sha, head_tree};
use super::named::key_of_argument;
use super::notes::{PullRequestNote, PushedBranch};
use super::{Outcome, ToolEnv, cancelled, non_empty, notes_error, workspace_error};

/// The name of [`commit_and_push`], which the agent also needs to tell its results apart in the
/// conversation (see [`pushed_in`]).
pub(crate) const COMMIT_AND_PUSH: &str = "commit_and_push";

/// The repository and branch that the text of a `commit_and_push` result ends with: the last two
/// lines, `repository: <url>` then `branch: <name>`, which name the line of work a later task may
/// continue (the run's own branch, or the branch it continued).
///
/// This is the **fallback** for learning which branches exist: the tool records them itself in the
/// run's notes (`RunNotes::pushed_branches`), and a run that continues another reads the notes of
/// that run. The text is read only when those are not there (another worker's volume), and with
/// the care a text calls for: only the final two lines count (a line like them anywhere else is
/// whatever the tool or a repository wrote), and a result that history truncation cut
/// ([`TRUNCATION_MARKER_PREFIX`]) is not read at all, because the cut is exactly where the lines
/// were.
pub(crate) fn pushed_in(text: &str) -> Option<(String, String)> {
    if text.contains(TRUNCATION_MARKER_PREFIX) {
        return None;
    }
    let mut last = text.lines().rev();
    let branch = last.next()?.strip_prefix("branch: ")?.trim();
    let repository = last.next()?.strip_prefix("repository: ")?.trim();
    (!repository.is_empty() && !branch.is_empty())
        .then(|| (repository.to_owned(), branch.to_owned()))
}

// Commits everything in the worktree and pushes the run's branch.
//
// Idempotent by construction: with no changes `commit_all` does nothing, the
// reported commit is `HEAD`, and pushing a commit the remote already has is a
// no-op. So running it twice (a crash before the result was journaled) neither
// duplicates a commit nor a push.
//
// It also gives the pushed commit its verdict: a `checks` artifact bound to the pushed
// SHA (see `checks_for_pushed`), emitted before `branch`. The verdict is a pure function
// of the run's notes, the SHA and its tree, so the second run of a step emits the same
// artifact, with the same content-derived id.

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
    // The line of work this push belongs to, recorded by the tool itself: what a later task of the
    // conversation may continue. Its own branch, or the branch the run continues.
    if let Some(repo) = key_of_argument(&wt.repo().url) {
        notes.name_pushed_branches([PushedBranch {
            repo,
            branch: wt.branch().to_owned(),
        }]);
    }
    env.notes
        .save(&run, &notes)
        .await
        .map_err(|e| notes_error(&e))?;

    // The verdict on exactly what was pushed, before the branch that names it.
    let pushed_tree = head_tree(wt.path()).await;
    let checks =
        checks_for_pushed(&notes, &sha, pushed_tree.as_deref()).into_artifact(&env.redactor);

    let own = wt.local_branch();
    let mut summary = match &committed {
        Some(_) => format!("Committed {sha} and pushed branch {own}."),
        None => format!("Nothing new to commit; branch {own} is pushed at {sha}."),
    };
    if let Some(line) = wt.continues() {
        summary.push_str(&format!(
            "\nThis run continues {line}, which has not been changed: open_pull_request moves it \
             to this commit (and so updates its pull request) once the checks have passed on \
             exactly this code."
        ));
    }
    // What a later task of this conversation may continue, as the last two lines (see
    // `pushed_in`): the line of work, not necessarily the branch that was just pushed.
    let summary = format!(
        "{summary}\nrepository: {}\nbranch: {}",
        wt.repo().url,
        wt.branch()
    );
    Ok(ToolOutput::text(summary)
        .with_artifact(checks)
        .with_artifact(Artifact {
            name: "branch".into(),
            mime_type: Some("application/json".into()),
            data: branch_data(wt.repo().url.as_str(), &wt, base, &sha),
        }))
}

/// The data of the `branch` artifact: the branch the commit was pushed to (the run's own), and,
/// when the run continues another branch that the commit has not been published to yet, which
/// one (`continues`).
fn branch_data(
    repository: &str,
    wt: &adam_workspace::Worktree,
    base: &str,
    commit: &str,
) -> serde_json::Value {
    let mut data = json!({
        "repository": repository,
        "branch": wt.local_branch(),
        "base_branch": base,
        "commit": commit,
    });
    if let Some(line) = wt.continues() {
        data["continues"] = json!(line);
    }
    data
}

/// The `checks` verdict on the pushed commit `sha`, whose tree is `tree`.
///
/// If the last `run_checks` of the run ran on that very tree (the worktree as `git add -A` would
/// commit it, which is what `commit_all` stages), its report is bound to `sha`. Otherwise the
/// commit was not checked: `passed: false` and a finding saying which trees differ. So a green
/// report for a commit always means the code in that commit was checked.
fn checks_for_pushed(
    notes: &super::notes::RunNotes,
    sha: &str,
    tree: Option<&str>,
) -> ChecksReport {
    let last = notes.checks.last.as_ref();
    match (last, tree) {
        (Some(last), Some(tree)) if last.tree.as_deref() == Some(tree) => match &last.report {
            Some(report) => report.bound_to(sha, tree, last.passed),
            // Notes written before reports were kept: the verdict without the findings.
            None => ChecksReport {
                passed: last.passed,
                commit: sha.to_owned(),
                tree: Some(tree.to_owned()),
                summary: Some(format!(
                    "`{}` {}; checked on the identical tree before it was committed",
                    last.command,
                    if last.passed { "passed" } else { "failed" }
                )),
                findings: Vec::new(),
            },
        },
        _ => ChecksReport::unchecked(
            sha,
            tree,
            last.and_then(|l| l.tree.as_deref()),
            last.is_some(),
        ),
    }
}

// Opens the pull request for the pushed branch.
//
// Refuses unless the branch's `HEAD` was pushed by `commit_and_push` and the
// last check run passed **on exactly the code the pull request contains** (the
// tree of `HEAD`; a change made after the run, committed or not, is
// unverified). The one override is `accept_red_checks: true`, which
// the prompt reserves for explicit user consent; it never overrides an
// exhausted check budget, and a pull request opened that way says so in its
// body (on a pull request that was already open, in a comment). When the run continues a branch,
// its commits are on the run's own branch until this point: only after the gate has passed is the
// continued branch fast-forwarded to the pushed commit (`Worktree::publish`, never forced), so a
// pull request that is open for it never carries code the gate did not see. Idempotent: a pull
// request that is already open for the branch (the run continued a branch an earlier task opened
// it for, or this call is repeated) is reported as it is, and `CodeHost::open_pull_request` would
// return it instead of creating another in any case.

/// Open the pull request for the pushed branch. Requires that everything is
/// pushed with commit_and_push and that the last check run passed on exactly
/// this code (re-run the checks after your last change). The body must have a
/// summary and a verification section listing the commands you ran and their
/// results. If you continue a branch that already has an open pull request, this
/// updates it with your commits (after the same check) instead of opening another.
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

    // A run that continues a branch has pushed its commits to its own branch only. Until this
    // point nothing has touched the branch (or the pull request) it continues: the gate above is
    // what lets the work in. Now the continued branch is moved to the pushed commit, never
    // forced, so a pull request that is open for it carries exactly the code that was verified.
    if let Some(line) = wt.continues() {
        ctx.emit_progress(format!("updating {line} with {}", wt.local_branch()))
            .await;
        if let Err(e) = wt.publish().await {
            return match e {
                WorkspaceError::Conflict(_) => Ok(ToolOutput::error(format!(
                    "Refusing to update {line}: the branch moved on the remote since this task \
                     started (someone pushed to it), and adding this run's commits would overwrite \
                     that. Nothing was changed on {line}; this run's commits are safe on {}. Do not \
                     retry and do not push anywhere else: tell the person what happened and ask how \
                     to go on with ask_user.",
                    wt.local_branch()
                ))),
                e => Err(env.delivery_error(ctx, &e).await),
            };
        }
    }

    let mut body = body.to_owned();
    if red {
        body.push_str(RED_NOTE_IN_BODY);
    }
    // A pull request that already exists for the branch (the run continues a branch an earlier
    // task opened it for) is what this call reports: the branch now carries this run's commits,
    // and nothing else is to be opened.
    let existing = match env
        .code_host
        .find_pull_request(wt.repo(), wt.branch())
        .await
    {
        Ok(found) => found,
        Err(e) => return Err(env.delivery_error(ctx, &e).await),
    };
    let reused = existing.is_some();
    let pr = match existing {
        Some(pr) => pr,
        None => {
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
            match opened {
                Ok(pr) => pr,
                Err(e) => return Err(env.delivery_error(ctx, &e).await),
            }
        }
    };
    // The body of a pull request that was already open is not ours to rewrite, so the note that
    // the update was not verified goes into a comment. (A crash between the comment and the
    // journaling of this call would post it again when the call repeats.)
    if reused
        && red
        && let Err(e) = env
            .code_host
            .comment_on_pull_request(wt.repo(), pr.number, RED_NOTE_IN_COMMENT)
            .await
    {
        return Err(env.delivery_error(ctx, &e).await);
    }

    notes.pull_request = Some(PullRequestNote {
        url: pr.url.clone(),
        number: pr.number,
        red_checks_accepted: red,
    });
    env.notes
        .save(&run, &notes)
        .await
        .map_err(|e| notes_error(&e))?;

    let mut text = if reused {
        let mut text = format!(
            "Pull request #{} for {} was already open, and it now carries this run's commits: \
             {}\nIts title and description are unchanged.",
            pr.number,
            wt.branch(),
            pr.url
        );
        if red {
            text.push_str(" A comment says the update was not verified.");
        }
        text
    } else {
        format!("Pull request #{} is open: {}", pr.number, pr.url)
    };
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

/// What a pull request opened on unverified code says in its body.
const RED_NOTE_IN_BODY: &str = "\n\n> **Note:** the checks were not green for this exact code (failing, not run, or run \
     before the last change) when this pull request was opened. The requester \
     explicitly accepted that.\n";

/// What an update of an open pull request with unverified code says in a comment.
const RED_NOTE_IN_COMMENT: &str = "> **Note:** this update was added with checks that were not green for this exact code \
     (failing, not run, or run before the last change). The requester explicitly accepted \
     that.";
