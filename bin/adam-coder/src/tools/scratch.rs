//! `start_scratch { name? }` and `publish_scratch { repo_url, scratch?, base_branch?, path?, overwrite? }`.
//!
//! A scratch project is a place to start before any repository is named: a local git repository
//! in a slot of the run's workspace ([`adam_workspace::Scratch`]), where the file tools,
//! OpenCode and the checks work as they do in a repository's worktree, and where
//! `commit_and_push` only commits locally. It lives as long as the run: what it holds reaches a
//! repository only through [`publish_scratch`], which copies its files into the slot of a
//! repository **the person named** and leaves the rest (the checks, the push, the pull request)
//! to the usual tools behind their gate.
//!
//! # Why a copy, and why the gate still holds
//!
//! The scratch history is not carried (ADR 0008, decision 5): the pull request carries the new
//! commits made in the repository's worktree. What binds the checks to those commits is the
//! tree: a check run on the scratch project is recorded with the tree it ran on
//! (`RunNotes::checked`), and a tree id is a content address, so when the files land unchanged in
//! an empty repository the worktree has the same tree and the check that passed on the project is
//! the check of the pushed commit. Anything else (a repository that had files, a `path`, a file
//! that was left out) is a different tree, which is unchecked until `run_checks` has run on it.
//!
//! # Retry safety
//!
//! `start_scratch` is idempotent (the project that is already there is returned). A
//! `publish_scratch` that died after it gave an empty remote its first commit finds the remote no
//! longer empty on the second run: it finds the repository's slot, or makes it, and goes on to
//! the copy, which skips the files that already are what the project has; a repository whose
//! only commit has the empty tree is not "a repository that already has files".

use adam::prelude::*;
use adam_workspace::{
    ChangedFile, CopyReport, GitIdentity, RepoRef, RunWorkspace, Scratch, Slot, WorkspaceError,
    copy_into,
};

use super::gitcli::{head_tree, working_tree_id};
use super::named::key_of_argument;
use super::prepare::not_named;
use super::{Outcome, ToolEnv, cancelled, non_empty, notes_error, workspace_error};

/// The name of a scratch project when the model gives none.
const DEFAULT_NAME: &str = "scratch";

/// The id git gives the tree with no entries: what the first commit of an empty repository
/// holds (`Workspaces::initialize_empty`), and so what a repository without files has.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The base branch an empty repository is given.
const DEFAULT_BASE: &str = "main";

/// Most files a result lists by name.
const MAX_LISTED: usize = 20;

// Starts a scratch project: `RunWorkspace::add_scratch`, which is idempotent per name, so a
// restarted or repeated call returns the project with whatever is in it.

/// Start a scratch project: a new local git repository in your workspace, to build and test
/// something in when the person has named no repository yet (or before one exists). Write the
/// files with write_file, apply_patch or delegate_to_opencode and run the real checks with
/// run_checks, passing `repo` with the project's name when the workspace has more than one slot.
/// A scratch project is temporary: it exists only while this task is open, and nothing in it is
/// kept unless it is published to a repository the person names, with publish_scratch. Tell the
/// person so. Calling it again with the same name is harmless.
#[tool]
pub async fn start_scratch(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Name of the project, which is also the name of its directory (`repo` in the other tools): lowercase letters, digits, `.`, `_` and `-`, starting with a letter or a digit, at most 64 characters, not ending in `.git`. Leave out for `scratch`.
    name: Option<String>,
) -> Outcome {
    let name = name.as_deref().and_then(non_empty).unwrap_or(DEFAULT_NAME);
    let run = ctx.run_id().to_string();
    let workspace = env.workspaces.run(&run).map_err(|e| workspace_error(&e))?;
    let slot = match workspace.add_scratch(name, &env.settings.identity).await {
        Ok(slot) => slot,
        Err(e @ (WorkspaceError::Invalid(_) | WorkspaceError::Conflict(_))) => {
            return Ok(ToolOutput::error(format!(
                "{e}. Give the project another name (or leave `name` out)."
            )));
        }
        Err(e) => return Err(workspace_error(&e)),
    };
    ctx.emit_progress(format!("started the scratch project {}", slot.dir()))
        .await;
    let mut text = format!(
        "Scratch project ready.\nslot: {}\npath: {}\nWrite files in it with write_file, \
         apply_patch or delegate_to_opencode, and run its checks with run_checks (say \
         `repo: {}` when the workspace has more than one slot).\nIt is temporary: it exists only \
         while this task is open, and nothing in it is kept unless it is published to a \
         repository the person names (publish_scratch). Tell the person that.",
        slot.dir(),
        slot.path().display(),
        slot.dir()
    );
    if let Some(note) = published_note(&slot) {
        text.push('\n');
        text.push_str(&note);
    }
    Ok(ToolOutput::text(text))
}

// Publishes a scratch project to a repository the person named.
//
// 1. The repository must be granted: named in the person's own messages (the same rule, and the
//    same notes, as `prepare_workspace`), before anything talks to a remote.
// 2. The repository's slot is found or made. A repository with no ref at all is given an empty
//    first commit on its base branch first (`Workspaces::initialize_empty`: never forced, only
//    when the remote has no ref); the worktree then starts from it.
// 3. A repository that already has files needs a `path` (or `overwrite`): the project is not
//    poured into the root of somebody's project on the model's say-so.
// 4. The files are copied, all or nothing (`copy_into`), and the project remembers where it went.
//
// Pushing, the checks and the pull request stay with `commit_and_push` and `open_pull_request`,
// behind their gate.

/// Put the files of a scratch project into a repository the person named, so that the usual
/// tools can check, commit, push and open a pull request there. The repository is added to your
/// workspace as a slot, as prepare_workspace does, and a brand-new empty repository is first given
/// an empty first commit on its base branch, so that the pull request has a base. The files are
/// copied all or nothing: a file that is already there with other content stops the copy unless
/// `overwrite` is true. A repository that already has files needs `path` (the directory of the
/// repository to put the project in, for example `apps/fib`) or `overwrite`: ask the person which
/// with ask_user, do not guess. It works only on a repository the person named. Afterwards the
/// result says whether the checks you ran on the project still hold for the code in the
/// repository: if not, run them again in the new slot, then commit_and_push and
/// open_pull_request with `repo` set to that slot.
#[tool]
pub async fn publish_scratch(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// https://github.com/<owner>/<repo>: a repository the person named
    repo_url: String,
    /// The scratch project to publish: its name. Leave out when the workspace has one.
    scratch: Option<String>,
    /// Branch the pull request will be against. Leave out for the repository's default branch (`main` for an empty repository).
    base_branch: Option<String>,
    /// Directory of the repository to put the files in, relative to its root (for example `apps/fib`). Leave out for the root; a repository that already has files needs it, or `overwrite`.
    path: Option<String>,
    /// Replace files that are already there with other content. Set true ONLY when the person said the files may be replaced. Never set it on your own judgement.
    overwrite: Option<bool>,
) -> Outcome {
    if ctx.is_cancelled() {
        return Err(cancelled("nothing was published"));
    }
    let Some(url) = non_empty(&repo_url) else {
        return Ok(ToolOutput::error("repo_url is required"));
    };
    let run = ctx.run_id().to_string();

    // The grant comes before anything that talks to a remote, as in prepare_workspace: a
    // repository nobody named costs no request, no credential and no mirror. An argument the
    // workspace cannot read (not a URL or an absolute path) is its error to report, below.
    let notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    if let Some(key) = key_of_argument(url)
        && !notes.named_repos.contains(&key)
    {
        return Ok(ToolOutput::error(not_named(
            url,
            &notes.named_repos,
            "publish the project to, then call publish_scratch with the one they name.",
        )));
    }

    let workspace = env.workspaces.run(&run).map_err(|e| workspace_error(&e))?;
    let slots = workspace.slots().await.map_err(|e| workspace_error(&e))?;
    let source = match pick_scratch(&slots, scratch.as_deref().and_then(non_empty)) {
        Ok(slot) => slot,
        Err(why) => return Ok(ToolOutput::error(why)),
    };
    let Some(project) = source.scratch() else {
        return Ok(ToolOutput::error(format!(
            "`{}` is not a scratch project",
            source.dir()
        )));
    };
    let files = project.files().await.map_err(|e| workspace_error(&e))?;
    if files.is_empty() {
        return Ok(ToolOutput::error(format!(
            "The scratch project `{}` has no files yet: write something in it first (nothing is \
             published, and the repository is untouched).",
            source.dir()
        )));
    }

    let base = base_branch.as_deref().and_then(non_empty);
    let (slot, initialized) = match find_or_make_slot(&env, ctx, &workspace, url, base).await {
        Ok(found) => found,
        Err(outcome) => return outcome,
    };
    let Some(wt) = slot.worktree() else {
        return Ok(ToolOutput::error(format!(
            "`{}` is a scratch project, not a repository",
            slot.dir()
        )));
    };

    let destination = path.as_deref().and_then(non_empty);
    let overwrite = overwrite.unwrap_or(false);
    // A repository whose files are somebody's: where the project goes is the person's to say.
    if destination.is_none()
        && !overwrite
        && head_tree(wt.path()).await.as_deref() != Some(EMPTY_TREE)
    {
        return Ok(ToolOutput::error(format!(
            "{url} already has files on {}. Do not guess where the project goes: ask the person \
             with ask_user which directory of the repository to put it in (call publish_scratch \
             again with `path`), or whether its files may replace the ones that are there \
             (`overwrite: true`). Nothing was copied.",
            wt.repo().base_branch
        )));
    }

    ctx.emit_progress(format!(
        "copying {} into {} ({})",
        source.dir(),
        slot.dir(),
        destination.unwrap_or(".")
    ))
    .await;
    let report = match copy_into(project, wt, destination.unwrap_or("."), overwrite).await {
        Ok(report) => report,
        Err(e @ WorkspaceError::Invalid(_)) => return Ok(ToolOutput::error(e.to_string())),
        Err(e) => return Err(workspace_error(&e)),
    };
    if !report.collisions.is_empty() {
        return Ok(ToolOutput::error(collisions_said(
            &report,
            source.dir(),
            slot.dir(),
        )));
    }
    project
        .set_published_to(&wt.repo().url)
        .await
        .map_err(|e| workspace_error(&e))?;

    // What the model has to do next depends on what the checks said about this very code.
    let tree = working_tree_id(wt.path()).await;
    let verdict = match tree.as_deref().and_then(|tree| notes.checked(tree)) {
        Some(record) if record.passed => format!(
            "The checks passed on exactly this code (`{}`, run in `{}`): commit_and_push now.",
            record.command,
            record.slot.as_deref().unwrap_or("the scratch project")
        ),
        Some(record) => format!(
            "The most recent check of exactly this code FAILED (`{}`): fix the cause, run the \
             checks again in `{}`, then commit_and_push.",
            record.command,
            slot.dir()
        ),
        None => format!(
            "This is not the code the checks ran on (the repository had its own files, or \
             `path` or `overwrite` changed what was copied): run the checks again in `{}` with \
             run_checks before you commit_and_push.",
            slot.dir()
        ),
    };
    Ok(ToolOutput::text(published_text(&Published {
        project: source.dir(),
        slot: slot.dir(),
        url: &wt.repo().url,
        base: &wt.repo().base_branch,
        branch: wt.branch(),
        initialized,
        report: &report,
        verdict: &verdict,
    })))
}

/// What a successful `publish_scratch` says.
struct Published<'a> {
    project: &'a str,
    slot: &'a str,
    url: &'a str,
    base: &'a str,
    branch: &'a str,
    /// The repository was empty and was given its first commit.
    initialized: bool,
    report: &'a CopyReport,
    verdict: &'a str,
}

fn published_text(p: &Published<'_>) -> String {
    let mut text = format!(
        "Published the scratch project `{}` to {}.\nslot: {}\nbase branch: {}\nbranch: {}",
        p.project, p.url, p.slot, p.base, p.branch
    );
    if p.initialized {
        text.push_str(&format!(
            "\nThe repository was empty: it now has an empty first commit on {}, the base of \
             the pull request.",
            p.base
        ));
    }
    text.push_str(&format!(
        "\ncopied ({}): {}",
        p.report.copied.len(),
        names(p.report.copied.iter().map(|p| p.display().to_string()))
    ));
    if !p.report.unchanged.is_empty() {
        text.push_str(&format!(
            "\nunchanged ({}): {}",
            p.report.unchanged.len(),
            names(p.report.unchanged.iter().map(|p| p.display().to_string()))
        ));
    }
    text.push_str(&format!(
        "\n{}\nNext: commit_and_push, then open_pull_request, both with `repo: {}`. The scratch \
         project `{}` is left as it is: changes made in it from now on are not part of this \
         repository's pull request, make them in `{}`.",
        p.verdict, p.slot, p.project, p.slot
    ));
    text
}

/// What the model is told when the copy was refused because of what is in the way.
fn collisions_said(report: &CopyReport, project: &str, slot: &str) -> String {
    let mut text = format!(
        "Nothing was copied: {} file(s) of `{project}` are in the way in `{slot}`:",
        report.collisions.len()
    );
    for collision in report.collisions.iter().take(MAX_LISTED) {
        text.push_str(&format!(
            "\n- {}: {}",
            collision.path.display(),
            collision.reason
        ));
    }
    if report.collisions.len() > MAX_LISTED {
        text.push_str(&format!(
            "\n- ... and {} more",
            report.collisions.len() - MAX_LISTED
        ));
    }
    text.push_str(
        "\nAsk the person with ask_user: put the project in another directory of the repository \
         (call publish_scratch again with `path`), or may it replace the files that differ \
         (`overwrite: true`; a directory or a symbolic link in the way is never replaced)?",
    );
    text
}

/// The scratch project `wanted` names, or the only one; or what to tell the model.
fn pick_scratch<'a>(slots: &'a [Slot], wanted: Option<&str>) -> Result<&'a Slot, String> {
    let projects: Vec<&Slot> = slots.iter().filter(|s| s.scratch().is_some()).collect();
    let listed = || {
        projects
            .iter()
            .map(|s| format!("`{}`", s.dir()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (wanted, projects.as_slice()) {
        (_, []) => Err(
            "there is no scratch project in this workspace: start one with \
                        start_scratch"
                .to_owned(),
        ),
        (Some(name), _) => projects
            .iter()
            .find(|s| s.dir() == name)
            .copied()
            .ok_or_else(|| {
                format!(
                    "`{name}` is not a scratch project of this workspace; its scratch projects \
                     are {}",
                    listed()
                )
            }),
        (None, [only]) => Ok(only),
        (None, _) => Err(format!(
            "this workspace has {} scratch projects: {}. Say which to publish with `scratch`",
            projects.len(),
            listed()
        )),
    }
}

/// The slot of the repository `url` (found, or made: the remote is given its first commit if it
/// has no ref at all) and whether this call gave it that commit. An error is what the tool
/// returns: a result for the model, or a failure to retry.
async fn find_or_make_slot(
    env: &ToolEnv,
    ctx: &ToolCtx,
    workspace: &RunWorkspace,
    url: &str,
    base: Option<&str>,
) -> Result<(Slot, bool), Outcome> {
    let said = |e: WorkspaceError| Ok(ToolOutput::error(e.to_string()));
    match workspace.slot_for(&RepoRef::new(url, "HEAD")).await {
        Ok(Some(slot)) => return Ok((slot, false)),
        Ok(None) => {}
        Err(e @ WorkspaceError::Invalid(_)) => return Err(said(e)),
        Err(e) => return Err(Err(workspace_error(&e))),
    }
    ctx.emit_progress(format!("looking at {url}")).await;
    let empty = match env.workspaces.remote_is_empty(url).await {
        Ok(empty) => empty,
        Err(e @ (WorkspaceError::Invalid(_) | WorkspaceError::NotFound(_))) => {
            return Err(said(e));
        }
        Err(e) => return Err(Err(env.delivery_error(ctx, &e).await)),
    };
    let base = match (base, empty) {
        (Some(base), _) => base.to_owned(),
        (None, true) => DEFAULT_BASE.to_owned(),
        (None, false) => match env.workspaces.default_branch(url).await {
            Ok(base) => base,
            Err(e @ (WorkspaceError::Invalid(_) | WorkspaceError::NotFound(_))) => {
                return Err(Ok(ToolOutput::error(format!(
                    "{e} Pass base_branch, or ask the person which branch to use with ask_user."
                ))));
            }
            Err(e) => return Err(Err(env.delivery_error(ctx, &e).await)),
        },
    };
    let repo = RepoRef::new(url, &base);
    let mut initialized = false;
    if empty {
        // The one push of this tool, and the one outside `agent/*`: a run that was cancelled
        // meanwhile must not make it.
        if ctx.is_cancelled() {
            return Err(Err(cancelled("nothing was published")));
        }
        ctx.emit_progress(format!("giving {url} its first commit on {base}"))
            .await;
        match env
            .workspaces
            .initialize_empty(&repo, &env.settings.identity)
            .await
        {
            Ok(_) => initialized = true,
            Err(e @ (WorkspaceError::Invalid(_) | WorkspaceError::Conflict(_))) => {
                return Err(said(e));
            }
            Err(e) => return Err(Err(env.delivery_error(ctx, &e).await)),
        }
    }
    match workspace.add_repository(&repo).await {
        Ok(slot) => Ok((slot, initialized)),
        Err(
            e @ (WorkspaceError::Invalid(_)
            | WorkspaceError::NotFound(_)
            | WorkspaceError::Conflict(_)),
        ) => Err(said(e)),
        Err(e) => Err(Err(env.delivery_error(ctx, &e).await)),
    }
}

/// `names` as a comma list, cut after [`MAX_LISTED`].
fn names(all: impl Iterator<Item = String>) -> String {
    let all: Vec<String> = all.collect();
    let mut shown = all
        .iter()
        .take(MAX_LISTED)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if all.len() > MAX_LISTED {
        shown.push_str(&format!(" and {} more", all.len() - MAX_LISTED));
    }
    shown
}

/// For the tools that go on working in a scratch project that was published: what they change
/// there no longer reaches the repository it was copied into.
pub(crate) fn published_note(slot: &Slot) -> Option<String> {
    let to = slot.scratch()?.published_to()?;
    Some(format!(
        "Note: `{}` was published to {to}, and what changes here is not part of that \
         repository's pull request: make changes in its slot, or call publish_scratch again to \
         copy them (a file that differs needs `overwrite: true`).",
        slot.dir()
    ))
}

/// The files `slot` has changed since its last commit, whatever the slot is.
pub(crate) async fn changed_files(slot: &Slot) -> Result<Vec<ChangedFile>, WorkspaceError> {
    match (slot.worktree(), slot.scratch()) {
        (Some(wt), _) => wt.status().await,
        (None, Some(scratch)) => scratch.status().await,
        (None, None) => Ok(Vec::new()),
    }
}

/// `commit_and_push` in a scratch project: a commit, and nothing else. There is no remote, no
/// `branch` and no bound `checks`: the verdict on a pushed commit exists only for a pushed commit.
pub(crate) async fn commit_locally(
    slot: &Slot,
    scratch: &Scratch,
    message: &str,
    identity: &GitIdentity,
) -> Outcome {
    let committed = match scratch.commit_all(message, identity).await {
        Ok(sha) => sha,
        Err(e @ WorkspaceError::Invalid(_)) => return Ok(ToolOutput::error(e.to_string())),
        Err(e) => return Err(workspace_error(&e)),
    };
    let head = super::gitcli::head_sha(slot.path()).await;
    let at = head.as_deref().unwrap_or("HEAD");
    let mut text = match committed {
        Some(sha) => format!(
            "Committed {sha} in the scratch project `{}`, locally: nothing is published until the \
             person names a repository (publish_scratch).",
            slot.dir()
        ),
        None => format!(
            "Nothing new to commit in the scratch project `{}` (it is at {at}). Nothing is \
             published until the person names a repository (publish_scratch).",
            slot.dir()
        ),
    };
    if let Some(note) = published_note(slot) {
        text.push('\n');
        text.push_str(&note);
    }
    Ok(ToolOutput::text(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_of_names_is_cut_after_twenty() {
        let few = names(["a", "b"].into_iter().map(str::to_owned));
        assert_eq!(few, "a, b");
        let many = names((0..25).map(|n| format!("f{n}")));
        assert!(many.starts_with("f0, f1,"), "{many}");
        assert!(many.ends_with("f19 and 5 more"), "{many}");
    }
}
