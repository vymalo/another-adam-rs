//! `run_checks { command | commands, cwd?, repo? }`, and the `checks` artifact it reports.
//!
//! # The `checks` artifact
//!
//! Every run of a command (not a refusal: an unusable `cwd`, the exhausted check budget) emits one
//! artifact named `checks`, mirroring how [`publish`](super::publish) emits `branch` and
//! `pull_request`: a [`ToolOutput`] artifact with the media type `application/json`, which the
//! agent emits with the tool's journaled result. The A2A layer derives the artifact id from the
//! content (`adam_a2a_runtime::artifact_id`), and the content holds nothing that varies between
//! executions of the same step, so replaying a journaled step emits the same artifact under the
//! same id, which a subscriber sees once. A step that died before its result was journaled ran
//! again, emitted nothing the first time, and emits once now.
//!
//! The data part is a [`ChecksReport`]. A consumer that sees several `checks` artifacts takes the
//! last one for a commit.
//!
//! # Several independent checks at once
//!
//! A call names one command with `command`, or several **independent** ones with `commands`:
//! exactly one of the two, never both. Independent is the whole point: a command that writes the
//! worktree, or reads what another of the list writes, is not the same check when it runs beside it,
//! and goes in a call of its own. The commands run at the same time, at most
//! [`CoderSettings::max_parallel_tool_calls`](super::CoderSettings::max_parallel_tool_calls) at once
//! (the run's own bound on the tool calls of a turn, so a deployment that sets it to 1 runs them one
//! after another), each through the process pool (`MAX_PROCESSES`, ADR 0065). The call reports what
//! running them one after another would: every command the caller gave still runs, each one's own
//! output is kept whole (the same tail bound), `passed` is true only when every one of them passed,
//! and `findings` names the failing ones **in the order the caller gave them**. A call costs **one**
//! check cycle when any of its commands fails (the call is the unit), and the failure the notes
//! answer with is keyed on the whole list: the same commands, in the same order, on the same tree.
//!
//! The report and the tree it checked are kept in the run's notes (the last one, and a short history
//! of them, in every slot of the workspace), so that `commit_and_push` can bind the most recent
//! report on a tree to the commit it pushes when that commit has the same tree, whichever slot ran
//! the check (see [`ChecksReport::bound_to`] and `RunNotes::checked`), and otherwise says the commit
//! was not checked ([`ChecksReport::unchecked`]).
//!
//! # A check is not run again on the same tree
//!
//! When the notes hold a record for the same call — the same commands in the same order,
//! directory, slot and timeout on exactly the tree the commands are about to run on (the tree id
//! of the worktree as it would be committed: tracked changes and untracked, not ignored files,
//! nothing else; see [`super::gitcli::working_tree_id`]; the call is told apart by the SHA-256 of
//! its commands' raw text, each behind its length, so two calls that differ only in a secret, or
//! in where one command ends and the next begins, are two checks) — the commands are not run. The
//! key is the tree id and the call, **never the time**.
//!
//! A **passed** record is the tree's verdict and is reused whatever the run's file/environment
//! epoch is now — never by time or by the epoch. The answer says the checks passed and were not
//! run again because this call already passed on the same tree, and its `checks` artifact is the
//! earlier report marked [`reused`](ChecksReport::reused): the report says which results were
//! reused. No check cycle is spent and no new record is written (the earlier pass is the record
//! for this tree).
//!
//! A **failed** record is reused only at the `ChecksNotes::epoch` it ran at (`run` and the file
//! tools bump it; nothing a check reads may have moved since): the result quotes that run's
//! findings, names the call that made
//! them, spends no check cycle and emits no `checks` artifact (the earlier run's is the verdict on
//! this tree). It is still a failure, so the gate refuses a pull request on the tree. A
//! pre-existing failure is never reused (it spends no cycle and the model is told the failure is
//! the repository's), and the most recent run of the check on the tree decides: a pass after a
//! failure means the command runs again, not that the older failure is the answer.

use adam::Artifact;
use adam::prelude::*;
use adam_llm_agent::sha256_hex;
use adam_workspace::{EnvDescription, EnvKind, EnvSession};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::redact::Redactor;

use super::notes::{CheckKey, CheckRecord};
use super::preexisting::{self, OnBase};
use super::shell::{
    MissingTool, ShellOutcome, cwd_key, missing_tool, project_dependency_hint, resolve_cwd,
    shell_spec,
};
use super::{Outcome, ToolEnv, non_empty, notes_error, run_error};

/// The most commands one `run_checks` call may take: the tool's time limit grows with the count
/// (one wave per `MAX_PARALLEL_TOOL_CALLS` commands, plus one re-run per failing command), so an
/// uncapped list would turn the loop's backstop into no bound at all.
const MAX_COMMANDS: usize = 16;

/// The name of the artifact `run_checks` emits.
pub const CHECKS_ARTIFACT: &str = "checks";

/// Most findings a [`ChecksReport`] carries.
pub const MAX_FINDINGS: usize = 20;

/// Most bytes a [`ChecksReport`]'s findings carry in total (the names and messages).
pub const MAX_FINDINGS_BYTES: usize = 16 * 1024;

/// Longest check name kept, in bytes.
const MAX_CHECK_NAME: usize = 256;

/// Longest command shown in the summary, in bytes.
const MAX_SUMMARY_COMMAND: usize = 160;

/// Room kept for the finding that says findings were left out.
const OMITTED_RESERVE: usize = 256;

/// Room kept for the mark on a message that was cut.
const CUT_MARK_RESERVE: usize = 64;

/// A failing check: its name and the tail of its output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The check's name: the command that ran.
    pub check: String,
    /// How it failed, then the tail of its output. Starts with a `[cut: ...]` mark when the tail
    /// was shortened to fit [`MAX_FINDINGS_BYTES`].
    pub message: String,
}

/// Where a check ran, when it was not in the coder's own container: the `environment` of the
/// `checks` artifact. Absent for a run in this container, so nothing changes for a local deployment;
/// a consumer that does not know the field ignores it.
///
/// `kind` is `kubernetes` (the run's own pod). A report of an older coder may say `devcontainer`
/// with the file it was built from in `source`; this coder never makes one (ADR 0052).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentReport {
    /// What the environment is: `kubernetes` (the run's own pod).
    pub kind: String,
    /// The `devcontainer.json` a report of an older coder was built from, as the repository calls
    /// it; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The image it runs.
    pub image: String,
}

impl EnvironmentReport {
    /// What `description` says, or `None` for the coder's own container.
    pub fn of(description: &EnvDescription) -> Option<Self> {
        match &description.kind {
            EnvKind::Kubernetes { image, .. } => Some(Self {
                kind: "kubernetes".to_owned(),
                source: None,
                image: image.clone().unwrap_or_default(),
            }),
            _ => None,
        }
    }
}

/// The data part of the `checks` artifact.
///
/// | Field | |
/// |---|---|
/// | `passed` | the check passed, and the report is complete |
/// | `commit` | the 40-hex SHA of the `HEAD` of the run's workspace when the check ran; empty only when it could not be determined (`passed` is then `false`) |
/// | `tree` | the git tree id (40 hex) of the code the check ran on, as `git add -A` would commit it; absent when it could not be computed |
/// | `repository` | the URL of the repository of the slot the check ran in (for the verdict on a pushed commit, the repository it was pushed to); absent for a scratch project and in reports of an older coder |
/// | `environment` | where the check ran, when that was the run's own pod: `{kind, image}` (`kubernetes`); absent for a run in the coder's own container and in reports of an older coder. A report of an older coder may say `devcontainer` with a `source` |
/// | `preexisting` | `true` when the command also fails on the base commit (`base_commit`), before the run changed anything: the failure is the repository's and the run's change did not cause it. Absent otherwise, and in reports of an older coder. `passed` is still `false`: a consumer that gates on the checks reads this to accept the failure |
/// | `base_commit` | with `preexisting`: the commit `origin/<base>` was at when the command failed there too (40 hex) |
/// | `reused` | `true` when the results are an earlier call's, not a new run: the same call had already passed on exactly the tree of this call and nothing ran again. The summary says which results were reused. Absent otherwise, and in reports of an older coder |
/// | `summary` | one line |
/// | `findings` | the failing checks; at most [`MAX_FINDINGS`] and [`MAX_FINDINGS_BYTES`] in total, the cut marked |
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksReport {
    /// Whether the checks passed.
    pub passed: bool,
    /// The commit the checks ran on.
    pub commit: String,
    /// The tree of the code the check ran on: the worktree as `commit_and_push` would commit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// The repository of the slot the check ran in, or the repository the commit was pushed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Where it ran, if not in the coder's own container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentReport>,
    /// The command also fails on [`base_commit`](Self::base_commit): the failure is not the run's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preexisting: bool,
    /// The commit `origin/<base>` was at when the command failed there too; with `preexisting`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// The results are an earlier call's on this tree, not a new run: the same call had already
    /// passed on exactly this code and the commands were not run again. Only a `passed` report is
    /// ever marked ([`ChecksReport::reused_at`]); the summary says which results were reused.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reused: bool,
    /// One line about the run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// What failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
}

/// How a summary says the check ran on uncommitted changes; `sha10)` follows.
const DIRTY_NOTE: &str = " (on uncommitted changes on top of ";

/// One command of a call, and how it went.
struct Run<'a> {
    /// The command, already scrubbed.
    command: &'a str,
    outcome: &'a ShellOutcome,
    timeout: std::time::Duration,
    /// The base commit the command also failed on: a pre-existing failure.
    preexisting: Option<&'a str>,
}

/// What a `run_checks` call knows about the run, for the report: the code the commands ran on, and
/// each of them in the order the caller gave them.
struct Batch<'a> {
    /// The commands it ran, in the caller's order.
    runs: &'a [Run<'a>],
    /// `HEAD` of the workspace repository, if it has one.
    head: Option<&'a str>,
    /// The worktree differed from `HEAD` when the checks ran.
    dirty: bool,
    /// The tree of the worktree as it would be committed.
    tree: Option<&'a str>,
    /// The repository of the slot the commands ran in.
    repository: Option<&'a str>,
    /// Where the commands ran.
    environment: Option<EnvironmentReport>,
}

impl ChecksReport {
    /// The report of one call: every command it ran, in the caller's order.
    fn of(batch: &Batch<'_>) -> Self {
        let commit = batch.head.filter(|h| is_sha(h));
        let failing: Vec<&Run<'_>> = batch
            .runs
            .iter()
            .filter(|run| !run.outcome.passed())
            .collect();
        let mut findings = Vec::new();
        for run in &failing {
            let verdict = verdict(run.outcome, run.timeout);
            let tail = run.outcome.tail.trim();
            let message = if tail.is_empty() {
                verdict
            } else {
                format!("{verdict}\n{tail}")
            };
            findings.push(Finding {
                check: cut_head(run.command.trim(), MAX_CHECK_NAME).to_owned(),
                message,
            });
        }
        if commit.is_none() {
            findings.push(Finding {
                check: "commit".to_owned(),
                message: "cannot determine the commit the checks ran on: the workspace has no \
                          repository or no HEAD commit"
                    .to_owned(),
            });
        }
        let preexisting = preexisting_of(&failing);
        let summary = summary_of(batch, &failing, commit);
        Self {
            passed: failing.is_empty() && commit.is_some(),
            commit: commit.unwrap_or_default().to_owned(),
            tree: batch.tree.filter(|t| is_sha(t)).map(str::to_owned),
            repository: batch.repository.map(str::to_owned),
            environment: batch.environment.clone(),
            preexisting: preexisting.is_some(),
            base_commit: preexisting.map(str::to_owned),
            reused: false,
            summary: Some(summary),
            findings: cap_findings(findings),
        }
    }

    /// The same report, marked as reused: the results are the earlier call's run on tree `tree`,
    /// not a new one. The summary says which results were reused; marking twice changes nothing.
    #[must_use]
    pub fn reused_at(mut self, tree: &str) -> Self {
        if self.reused {
            return self;
        }
        self.reused = true;
        let short = &tree[..tree.len().min(10)];
        let mark = format!("these results were reused: the same call passed on tree {short}");
        self.summary = Some(match self.summary.take() {
            Some(summary) => format!("{summary}; {mark}"),
            None => mark,
        });
        self
    }

    /// This report for the repository `url`: what a verdict on a pushed commit names.
    #[must_use]
    pub fn in_repository(mut self, url: &str) -> Self {
        self.repository = Some(url.to_owned());
        self
    }

    /// This report with the values `redactor` knows scrubbed out of its text.
    pub fn scrubbed(mut self, redactor: &Redactor) -> Self {
        self.summary = self.summary.map(|s| redactor.scrub_string(s));
        for f in &mut self.findings {
            f.check = redactor.scrub_string(std::mem::take(&mut f.check));
            f.message = redactor.scrub_string(std::mem::take(&mut f.message));
        }
        self
    }

    /// The verdict on a commit whose tree this report checked: the same report, bound to `commit`.
    ///
    /// `command_passed` is whether the command itself passed (a report can also fail for lack of a
    /// commit, which no longer applies). The result is a pure function of its inputs, so
    /// binding again on a replay yields the same artifact under the same id.
    pub fn bound_to(&self, commit: &str, tree: &str, command_passed: bool) -> Self {
        let summary = self.summary.as_deref().unwrap_or("checks ran");
        let core = summary.split(DIRTY_NOTE).next().unwrap_or(summary);
        Self {
            passed: command_passed,
            commit: commit.to_owned(),
            tree: Some(tree.to_owned()),
            repository: None,
            environment: self.environment.clone(),
            preexisting: self.preexisting && !command_passed,
            base_commit: self
                .base_commit
                .clone()
                .filter(|_| self.preexisting && !command_passed),
            reused: self.reused,
            summary: Some(format!(
                "{core}; checked on the identical tree before it was committed as {}",
                &commit[..commit.len().min(10)]
            )),
            findings: self
                .findings
                .iter()
                .filter(|f| f.check != "commit")
                .cloned()
                .collect(),
        }
    }

    /// The verdict on a commit whose tree no check ran on: `last_tree` is the tree of the last
    /// `run_checks` of the run, `None` if none ran (or its tree is unknown).
    pub fn unchecked(commit: &str, tree: Option<&str>, last_tree: Option<&str>, ran: bool) -> Self {
        let short = |t: &str| t[..t.len().min(10)].to_owned();
        let last = match (ran, last_tree) {
            (false, _) => "no check ran in this run".to_owned(),
            (true, None) => "the last checks ran on an unknown tree".to_owned(),
            (true, Some(t)) => format!("the last checks ran on {}", short(t)),
        };
        let has = tree.map_or("an unknown tree".to_owned(), short);
        Self {
            passed: false,
            commit: commit.to_owned(),
            tree: tree.filter(|t| is_sha(t)).map(str::to_owned),
            repository: None,
            environment: None,
            preexisting: false,
            base_commit: None,
            reused: false,
            summary: Some(format!(
                "the pushed commit {} was not checked",
                &commit[..commit.len().min(10)]
            )),
            findings: vec![Finding {
                check: "checks".to_owned(),
                message: format!("the pushed tree was not checked: {last}, the commit has {has}"),
            }],
        }
    }

    /// The artifact, with the values `redactor` knows scrubbed out of it.
    pub fn into_artifact(self, redactor: &Redactor) -> Artifact {
        let mut data = serde_json::to_value(&self).unwrap_or(Value::Null);
        redactor.scrub_value(&mut data);
        Artifact::new(CHECKS_ARTIFACT, Some("application/json".into()), data)
    }
}

/// `sha` is a full SHA-1 object name.
fn is_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `text` on one line, at most `max` bytes.
fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.len() <= max {
        return line;
    }
    let mut end = max.saturating_sub(3);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &line[..end])
}

/// The summary line of a call: for one command exactly what it always was, and for several how many
/// failed out of how many, with the same notes about uncommitted changes and a pre-existing failure.
fn summary_of(batch: &Batch<'_>, failing: &[&Run<'_>], commit: Option<&str>) -> String {
    let mut summary = if let [run] = batch.runs {
        let command = one_line(run.command, MAX_SUMMARY_COMMAND);
        if run.outcome.passed() {
            format!("`{command}` passed")
        } else {
            format!("`{command}` failed: {}", verdict(run.outcome, run.timeout))
        }
    } else {
        let total = batch.runs.len();
        let failed = failing.len();
        if failed == 0 {
            format!("{total} of {total} checks passed")
        } else {
            format!("{failed} of {total} checks failed")
        }
    };
    if let (true, Some(sha)) = (batch.dirty, commit) {
        summary.push_str(&format!("{DIRTY_NOTE}{})", &sha[..sha.len().min(10)]));
    }
    if let Some(base) = preexisting_of(failing) {
        summary.push_str(&format!(
            "; it fails on the base ({}) too: a pre-existing failure",
            &base[..base.len().min(10)]
        ));
    }
    summary
}

/// The base commit a call's failure is pre-existing on: `Some` only when **every** failing command
/// failed on the same base commit too (then the call spends no check cycle, as one command always
/// did). `None` when nothing failed, when a failure is the run's, or when the bases differ.
fn preexisting_of<'a>(failing: &[&Run<'a>]) -> Option<&'a str> {
    let mut base: Option<&'a str> = None;
    for run in failing {
        let here = run.preexisting.filter(|b| is_sha(b))?;
        match base {
            Some(seen) if seen != here => return None,
            _ => base = Some(here),
        }
    }
    base
}

/// The commands of a call as one text, in the caller's order: the command itself when there is one,
/// as it always was.
fn joined(shown: &[String]) -> String {
    shown.join("; ")
}

/// What makes two calls the same check: the SHA-256 of their commands' raw text, in order. One
/// command hashes as it always did (`sha256(command)`), so a failure recorded before a call could
/// name a list is still reused; several hash each command behind its length in bytes, so no two
/// lists share a digest. [`commands_of`] refuses a NUL, which would make the length prefix the only
/// thing telling `["a\0b"]` from `["a", "b"]`.
fn call_sha(commands: &[String]) -> String {
    if let [one] = commands {
        return sha256_hex(one.as_bytes());
    }
    let mut bytes = Vec::new();
    for command in commands {
        bytes.extend_from_slice(&(command.len() as u64).to_le_bytes());
        bytes.extend_from_slice(command.as_bytes());
    }
    sha256_hex(&bytes)
}

/// The results of `jobs`, polled at most `bound` at a time, in the order they were given.
async fn at_most<T, F: std::future::Future<Output = T>>(jobs: Vec<F>, bound: usize) -> Vec<T> {
    use futures::StreamExt as _;
    futures::stream::iter(jobs)
        .buffered(bound.max(1))
        .collect()
        .await
}

/// `text` without its start, at most `max` bytes, cut on a character boundary.
pub(super) fn cut_tail(text: &str, max: usize) -> &str {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// `text` without its end, at most `max` bytes, cut on a character boundary.
fn cut_head(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// How a run ended, in words.
fn verdict(outcome: &ShellOutcome, timeout: std::time::Duration) -> String {
    if outcome.timed_out {
        format!("timed out after {}", human(timeout))
    } else if let Some(stopped) = outcome.stopped {
        format!("stopped: {stopped}")
    } else {
        match outcome.exit_code {
            Some(0) => "exit code 0".to_owned(),
            Some(code) => format!("exit code {code}"),
            None => "killed by a signal".to_owned(),
        }
    }
}

/// `findings` within [`MAX_FINDINGS`] and [`MAX_FINDINGS_BYTES`] (names and messages, and the
/// finding that marks the cut, counted): a message that does not fit keeps its end, behind a
/// `[cut: ...]` mark, and findings left out are counted in a last finding named `findings`.
fn cap_findings(findings: Vec<Finding>) -> Vec<Finding> {
    let size = |f: &Finding| f.check.len() + f.message.len();
    if findings.len() <= MAX_FINDINGS
        && findings.iter().map(size).sum::<usize>() <= MAX_FINDINGS_BYTES
    {
        return findings;
    }
    let total = findings.len();
    let mut left = MAX_FINDINGS_BYTES - OMITTED_RESERVE;
    let mut kept: Vec<Finding> = Vec::new();
    for mut finding in findings {
        if kept.len() + 1 >= MAX_FINDINGS || left <= finding.check.len() + CUT_MARK_RESERVE {
            break;
        }
        left -= finding.check.len();
        if finding.message.len() > left {
            let keep = left - CUT_MARK_RESERVE;
            let tail = cut_tail(&finding.message, keep);
            finding.message = format!(
                "[cut: the last {} of {} bytes]\n{tail}",
                tail.len(),
                finding.message.len()
            );
        }
        left -= finding.message.len();
        kept.push(finding);
    }
    if kept.len() < total {
        kept.push(Finding {
            check: "findings".to_owned(),
            message: format!(
                "[cut: {} more findings left out; at most {MAX_FINDINGS} findings and {} KiB]",
                total - kept.len(),
                MAX_FINDINGS_BYTES / 1024
            ),
        });
    }
    kept
}

/// `900s`, or `500ms` below a second.
fn human(d: std::time::Duration) -> String {
    if d.as_secs() >= 1 {
        format!("{}s", d.as_secs())
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// The answer to a command that failed because the workspace lacks a tool (see [`missing_tool`]),
/// and the record of it: the first time a tool is reported missing in the run, a tool that the
/// **project** brings (see [`project_dependency_hint`]) is answered with the project's install
/// command; every other case, and the second time the same tool is reported, with the ask-the-person
/// text. `dirs` are where the project's files are looked for.
pub(crate) async fn missing_tool_answer(
    env: &ToolEnv,
    ctx: &ToolCtx,
    missing: &MissingTool,
    dirs: &[&std::path::Path],
    environment: &EnvKind,
) -> Result<String, ToolError> {
    let run = ctx.root_run_id().to_string();
    // Through `update`: `run_command` answers a missing tool here beside the other calls of its turn.
    let key = ToolEnv::call_key(ctx);
    let earlier = env
        .notes
        .update(&run, |notes| notes.record_missing_tool(&key, &missing.name))
        .await
        .map_err(|e| notes_error(&e))?;
    let install = if earlier == 0 {
        project_dependency_hint(dirs, &missing.name)
    } else {
        None
    };
    Ok(missing_tool_text(
        missing,
        install.as_deref(),
        earlier > 0,
        environment,
    ))
}

/// What the model is told when a command failed because the workspace lacks a tool: what is
/// missing, that no check cycle was used, and that the way on is to tell the person and wait. It
/// must not retry variants, hunt the filesystem, or install a system toolchain. A tool the
/// **project** brings itself (`install` is the project's own install command) is another story: the
/// project's dependencies are installed with the project's own command, **through
/// `run` and not `run_checks`** (an install that passes is a green check on code
/// nobody tested, and `run_checks` is for the project's real checks). `repeated`: the same tool was
/// already reported, so whatever was tried did not work, and the person decides. In a run pod the
/// answer says which environment lacks it and how the tool gets there (`environment`).
pub(crate) fn missing_tool_text(
    missing: &MissingTool,
    install: Option<&str>,
    repeated: bool,
    environment: &EnvKind,
) -> String {
    let name = &missing.name;
    if let Some(install) = install {
        return format!(
            "`{name}` is not on the PATH, but it is one of this project's own dependencies, which \
             are not installed in the workspace yet: that is not a missing toolchain and not a \
             failing check (no check cycle was used, nothing was recorded as a check). Install \
             the project's dependencies with its own command (`{install}`), with run (not \
             run_checks: that is only for the project's real checks), \
             then run this again. Only if that fails, tell the person with ask_user."
        );
    }
    let again = if repeated {
        " You were told about this tool before and it is still missing: whatever was tried did \
         not provide it, so stop and ask."
    } else {
        ""
    };
    let place = match environment {
        EnvKind::Kubernetes { pod, image } => format!(
            " The commands of this workspace run in a pod of their own ({pod}{}): the tool has to \
             be in the image that pod runs, so say that `{name}` must be added to it (the \
             deployment's run-pod image, or the repository's own image when the run moved to it).",
            image
                .as_deref()
                .map(|image| format!(", image {image}"))
                .unwrap_or_default()
        ),
        _ => String::new(),
    };
    format!(
        "The workspace has no `{name}`: the shell could not find it (exit code 127). That is a \
         missing toolchain, not a failing check: no check cycle was used and nothing was \
         recorded as a check. Do not retry variants of the command, do not search the \
         filesystem for the tool and do not try to install it (system toolchains are not yours to \
         install). Tell the person which toolchain is missing (`{name}`) with ask_user, and wait \
         for their answer.{place}{again}"
    )
}

fn render(command: &str, outcome: &ShellOutcome, timeout: std::time::Duration) -> String {
    let verdict = if outcome.timed_out {
        format!(
            "TIMED OUT after {} (the command was killed): FAILED",
            human(timeout)
        )
    } else if let Some(stopped) = outcome.stopped {
        format!("stopped: {stopped} (the command was killed): FAILED")
    } else {
        match outcome.exit_code {
            Some(0) => "exit code 0: passed".to_owned(),
            Some(code) => format!("exit code {code}: FAILED"),
            None => "killed by a signal: FAILED".to_owned(),
        }
    };
    let mut text = format!("$ {command}\n{verdict}\n");
    text.push_str(&super::output::section(outcome));
    text
}

/// What the model is told when a failed check is not run again: the code and everything a check
/// reads are as they were when `record` ran, so the command would fail the same way. It quotes the
/// findings, names the call that made them, says the tree is unchanged, that no cycle was spent and
/// no `checks` artifact was emitted (the earlier run's is the verdict on this tree), and that the
/// gate still refuses a pull request on it.
fn unchanged_failure_text(record: &CheckRecord, command: &str, failures: u32, max: u32) -> String {
    let tree = record.start_tree.as_deref().unwrap_or_default();
    let short = &tree[..tree.len().min(10)];
    let code = super::notes::exit_phrase(record.exit_code);
    let tail = record.tail.trim();
    // A call of one command says its code, as it always did; a call of several names each failing
    // command behind its own line, and its tail already holds them.
    let failed = record.report.as_ref().map_or(1, |report| {
        report
            .findings
            .iter()
            .filter(|finding| finding.check != "commit")
            .count()
    });
    let findings = if tail.is_empty() {
        format!("({code}, and the command printed nothing)")
    } else if failed > 1 {
        format!("the failing checks:\n{tail}")
    } else {
        format!("{code}:\n{tail}")
    };
    // One command keeps the words it always had; a call of several says the checks, in the plural,
    // whatever its findings were capped to.
    let again = if record.commands > 1 {
        "the checks were not run again, because they would fail the same way"
    } else {
        "the command was not run again, because it would fail the same way"
    };
    format!(
        "The worktree is unchanged since `{command}` failed on tree {short}, and nothing a check \
         reads has changed since (no `run` or file change since, and the environment was not \
         rebuilt): {again}. This call \
         cost no check cycle ({failures} of {max} used so far), and it emits no new `checks` \
         artifact: the one the earlier run reported is the verdict on this tree. This is still a \
         failure, not a green: a pull request on this tree is refused. What call `{}` found:\n\
         {findings}\nChange the code, then run the check again.",
        record.call_id
    )
}

/// What the model is told when a passed check is not run again: the same call already passed on
/// exactly this tree, so its results are the answer. It quotes the earlier report's summary and
/// any findings (a pass has none, but the report may carry a `commit` finding when the run's
/// `HEAD` was unknown — kept), names the call that made them, says the key of a check is the tree
/// id and the call, never the time or the epoch (so any change to the tree runs the checks again),
/// and that the `checks` result is the earlier report marked reused, not a new run.
fn unchanged_pass_text(record: &CheckRecord, command: &str) -> String {
    let tree = record.start_tree.as_deref().unwrap_or_default();
    let short = &tree[..tree.len().min(10)];
    let report = record.report.as_ref();
    let summary = report
        .and_then(|report| report.summary.as_deref())
        .unwrap_or("the checks passed");
    let mut said = format!(
        "The checks passed and were not run again: `{command}` already passed on tree {short} \
         (call `{}`), and this call starts on exactly that tree again. A check is keyed on the \
         tree id (tracked changes and untracked files, not what `.gitignore` hides) and the call, \
         never on when it ran or on the run's epoch: any change to the tree runs the checks \
         again. The results reused are those of that run: {summary}",
        record.call_id
    );
    if let Some(report) = report {
        for finding in &report.findings {
            said.push_str(&format!("\n{}: {}", finding.check, finding.message));
        }
    }
    said.push_str(
        "\nNothing ran and nothing was recorded again: the `checks` result is that earlier \
         report, marked as reused.",
    );
    said
}

/// The commands a call names: `command`, or `commands`, exactly one of the two (whether it is
/// given is decided before trimming, so a blank `command` beside a list is still both). Every
/// command is trimmed, an empty one is refused, and a NUL is refused: it is what the reuse key's
/// length prefix has to tell from the boundary between two commands (see [`call_sha`]).
fn commands_of(
    command: Option<String>,
    commands: Option<Vec<String>>,
) -> Result<Vec<String>, String> {
    match (command, commands) {
        (Some(_), Some(_)) => Err(
            "give either `command` (one command) or `commands` (a list of \
                                   independent commands), not both"
                .to_owned(),
        ),
        (None, None) => Err(
            "give `command` (one command) or `commands` (a list of independent \
                             commands to run at the same time)"
                .to_owned(),
        ),
        (Some(command), None) => {
            let Some(command) = non_empty(&command) else {
                return Err("`command` is empty: give the shell command to run".to_owned());
            };
            refuse_nul(command, "`command`")?;
            Ok(vec![command.to_owned()])
        }
        (None, Some(commands)) => {
            if commands.is_empty() {
                return Err(
                    "`commands` is empty: name at least one command, or use `command` for one"
                        .to_owned(),
                );
            }
            if commands.len() > MAX_COMMANDS {
                return Err(format!(
                    "`commands` holds {} commands: at most {MAX_COMMANDS} in one call, so that its \
                     time limit stays a bound (split a longer list into calls)",
                    commands.len()
                ));
            }
            let mut list = Vec::with_capacity(commands.len());
            for (n, command) in commands.iter().enumerate() {
                let Some(command) = non_empty(command) else {
                    return Err(format!(
                        "`commands[{n}]` is empty: every command must be a shell command to run"
                    ));
                };
                refuse_nul(command, &format!("`commands[{n}]`"))?;
                list.push(command.to_owned());
            }
            Ok(list)
        }
    }
}

/// Refuse `command` (which the call named `name`) when it holds a NUL: a shell command cannot, and
/// the reuse key could not tell it from the boundary between two commands.
fn refuse_nul(command: &str, name: &str) -> Result<(), String> {
    if command.contains('\0') {
        return Err(format!(
            "{name} holds a NUL character (U+0000), which a shell command cannot: remove it"
        ));
    }
    Ok(())
}

/// The commands the shell could not find, by their place in the call: a missing toolchain is not a
/// failing check (see [`missing_tool_text`]).
fn missing_of(commands: &[String], outcomes: &[ShellOutcome]) -> Vec<(usize, MissingTool)> {
    let mut missing = Vec::new();
    for (n, outcome) in outcomes.iter().enumerate() {
        if let Some(tool) = missing_tool(outcome, &commands[n]) {
            missing.push((n, tool));
        }
    }
    missing
}

/// Run `commands` in `dir`, at most `bound` at a time, each through the run's environment (and the
/// process pool behind it), and return what each did in the caller's order. Each command settles its
/// own output, so a long output is kept under an id of its own.
async fn run_all(
    env: &ToolEnv,
    ctx: &ToolCtx,
    session: &dyn EnvSession,
    dir: &std::path::Path,
    commands: &[String],
    timeout: std::time::Duration,
    bound: usize,
) -> Result<Vec<ShellOutcome>, ToolError> {
    let one = commands.len() == 1;
    let jobs: Vec<_> = commands
        .iter()
        .enumerate()
        .map(|(n, command)| {
            // A part of a call keeps its own output id (`o-...`): one id for every command would
            // have one output overwrite another in what `read_output` pages through. The first
            // command keeps the call's own id, so that a compaction which dropped the result still
            // knows an output of it is kept.
            let part = if one || n == 0 {
                ctx.clone()
            } else {
                ctx.clone()
                    .with_call_id(format!("{}#{}", ctx.call_id(), n + 1))
            };
            let spec = shell_spec(dir, command);
            async move {
                let mut outcome = env
                    .run(
                        &part,
                        session,
                        spec,
                        timeout,
                        env.settings.check_output_tail,
                        true,
                    )
                    .await
                    .map_err(|e| run_error(&env.redactor, &e))?;
                super::output::settle(env, &part, &mut outcome).await;
                Ok(outcome)
            }
        })
        .collect();
    let results: Vec<Result<ShellOutcome, ToolError>> = at_most(jobs, bound).await;
    results.into_iter().collect()
}

/// The exit code the record keeps: the first failing command's, in the caller's order (a timeout or
/// a signal has none). One command keeps its own code, as it always was; the record's tail names
/// each failing command's own.
fn first_failing_exit(outcomes: &[ShellOutcome]) -> Option<i32> {
    if let [outcome] = outcomes {
        return outcome.exit_code;
    }
    outcomes
        .iter()
        .find(|outcome| !outcome.passed())
        .and_then(|outcome| outcome.exit_code)
}

/// The output a record keeps for a failure asked again on the same tree: one command's tail as it
/// always was, and for several each failing command's tail behind a line that names it.
fn recorded_tail(outcomes: &[ShellOutcome], shown: &[String]) -> String {
    if let [outcome] = outcomes {
        return outcome.tail.clone();
    }
    let mut parts = Vec::new();
    for (n, outcome) in outcomes.iter().enumerate() {
        if !outcome.passed() {
            parts.push(format!("$ {}\n{}", shown[n], outcome.tail.trim_end()));
        }
    }
    parts.join("\n")
}

/// What makes two calls of a list the same failure for the repeated-call rule: the failing
/// commands' keys in order. A call of one command keeps its own key, as it always was.
fn batch_repeat_key(outcomes: &[ShellOutcome]) -> String {
    if let [outcome] = outcomes {
        return outcome.repeat_key().unwrap_or_default();
    }
    outcomes
        .iter()
        .filter_map(ShellOutcome::repeat_key)
        .collect::<Vec<_>>()
        .join(",")
}

// Runs the commands of a call in the worktree and reports how each went.
//
// * `bash -lc <command>` (`sh -lc` without bash), cwd inside the worktree (`cwd`, if given, must stay
//   inside it), the process group killed after the timeout, only the output
//   tail kept.
// * A failed call costs one check cycle, however many of its commands failed. After
//   `max_check_cycles` failures the tool no longer runs anything and tells the model to stop and
//   report.

/// Run one of the project's own checks in your worktree (the commands its CI, README or
/// Makefile run: `cargo test`, `pnpm test`, `just ci`), in the workspace's environment (a run pod of
/// the run's own when the deployment runs them), and get each one's exit code and the tail of its
/// output. Give one command with `command`, or several independent ones with `commands`: they run at
/// the same time, at most as many as the run runs tool calls, and the call passes only when every
/// one of them passed. With several repositories in the workspace, say which with `repo`. Only exit
/// code 0 counts as passing. Every failed call uses up one of your limited check cycles and is
/// reported as a check: never use it to look around (use run_command) or to make a file or change
/// the worktree (use run). A call that failed, asked again on exactly the same unchanged code, is
/// answered from the earlier run (no cycle, no new `checks` artifact, still a failure); the same
/// call that passed on exactly the same code is answered from that pass too (nothing runs, and the
/// `checks` report says the results were reused). A command
/// the shell cannot find means the workspace lacks that tool: that is reported, costs no cycle, and
/// is for the person to decide.
#[tool(label = "Run the checks", icon = "execute", parallel = "exclusive")]
pub async fn run_checks(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// One shell command, run with `bash -lc` (`sh -lc` without bash) in the worktree, in the workspace's environment. Give it or `commands`, not both
    command: Option<String>,
    /// Several independent shell commands, each run with `bash -lc` (`sh -lc` without bash) in the worktree: they run at the same time, at most as many as the run runs tool calls, and the call passes only when every one of them passed. Give it or `command`, not both. Example: {"commands":["cargo fmt --check","cargo test"]}
    commands: Option<Vec<String>>,
    /// Optional sub-directory of the worktree to run in (relative, inside the worktree)
    cwd: Option<String>,
    /// The slot to run in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
    /// Seconds before each command is killed; at most the deployment's limit, which is also the default. Example: {"commands":["npm test"],"timeout":120}
    timeout: Option<i64>,
) -> Outcome {
    let commands = match commands_of(command, commands) {
        Ok(commands) => commands,
        Err(said) => return Ok(ToolOutput::error(said)),
    };
    let run = ctx.root_run_id().to_string();
    // `run_checks` runs alone (ADR 0049): no other call of its turn changes the notes while it holds
    // them across the whole check, so it reads them once and writes them once.
    let mut notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;

    // A repository's worktree or a scratch project: the checks run on either.
    let slot = match env.slot_to_write(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    // Scratch work and repositories have a budget each, and a call spends the one of its slot.
    let scratch = slot.worktree().is_none();
    let max = env.settings.cycle_limit(scratch);

    // A replayed call that was already counted may run again; a new call
    // past the limit may not.
    // Keyed by run and call: the notes are the root run's, and a subagent's call ids are its own.
    let call = ToolEnv::call_key(ctx);
    let replay = notes.checks.counted.contains(&call);
    if notes.cycles_exhausted(scratch, max) && !replay {
        let findings = notes
            .last_in(scratch)
            .map(|c| c.tail.clone())
            .unwrap_or_default();
        return Ok(ToolOutput::error(format!(
            "Check-cycle limit reached: {} failed check runs (limit {max}). Do not run \
             checks, commit, push or open a pull request any more. Stop now and report \
             what you did, which check still fails, and these findings from the last run:\n{findings}",
            notes.failures_in(scratch)
        )));
    }
    let dir = match resolve_cwd(slot.path(), cwd.as_deref().and_then(non_empty)) {
        Ok(dir) => dir,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    let timeout = match super::output::timeout_of(&env, timeout) {
        Ok(timeout) => timeout,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };

    // What the commands print is not ours to publish: scrub the process's
    // secrets before the tails reach the model, the notes, an event or a
    // failed run's findings. The command lines are shown scrubbed too.
    let redactor = &env.redactor;
    let shown: Vec<String> = commands
        .iter()
        .map(|command| redactor.scrub(command).into_owned())
        .collect();
    let named = joined(&shown);

    // The code the commands are about to run on, computed before they run: it says whether a
    // result already seen on exactly this code (a failure at its epoch, or a pass whatever the
    // epoch) can be answered instead of running them again. The
    // report below is bound to the tree they leave (a check may write a file), as it always was.
    let at = super::gitcli::GitAt::slot(&slot);
    let tree_before = super::gitcli::working_tree_id(&at).await;
    let dir_key = cwd_key(cwd.as_deref());
    // The identity of a call is its commands' raw text, in order, not what the redactor shows: two
    // calls that differ only in a secret are two checks.
    let command_sha = call_sha(&commands);

    // A failure on this exact tree, from this exact call (raw commands, directory, slot, timeout) in
    // this kind of work, with the run's files and environment unchanged since
    // (`ChecksNotes::epoch`): the commands would fail the same way, so they are not run again. It
    // spends no cycle and emits no `checks` artifact (the earlier run's is the verdict on this
    // tree); the gate still refuses the pull request.
    let epoch = notes.checks.epoch;
    let earlier = match tree_before.as_deref() {
        Some(tree) => notes.failed_check_on(&CheckKey {
            tree,
            command_sha: &command_sha,
            cwd: &dir_key,
            slot: slot.dir(),
            epoch,
            timeout_secs: timeout.as_secs(),
            scratch,
        }),
        None => None,
    };
    if let Some(earlier) = earlier {
        let failures = notes.failures_in(scratch);
        ctx.emit_progress(format!("checks not run again (unchanged tree): {named}"))
            .await;
        return Ok(
            ToolOutput::error(unchanged_failure_text(earlier, &named, failures, max))
                .with_repeat_key(format!(
                    "unchanged:{}",
                    earlier.start_tree.as_deref().unwrap_or_default()
                )),
        );
    }

    // A pass on this exact tree, from this exact call (raw commands, directory, slot, timeout) in
    // this kind of work: the tree is the verdict's key (it holds the tracked changes and untracked
    // files of the worktree), never the time or the run's file/environment epoch
    // (`RunNotes::passed_check_on`), so the commands are not run again. The earlier report,
    // marked reused, is the answer and the `checks` artifact; no check cycle is spent and no
    // record is written (the earlier pass is the record for this tree). A record that keeps no
    // report cannot say what the earlier run found (only notes written before reports did can
    // make one): the check runs.
    let passed_earlier = match tree_before.as_deref() {
        Some(tree) => notes
            .passed_check_on(&CheckKey {
                tree,
                command_sha: &command_sha,
                cwd: &dir_key,
                slot: slot.dir(),
                epoch,
                timeout_secs: timeout.as_secs(),
                scratch,
            })
            .and_then(|earlier| earlier.report.clone().map(|report| (earlier, report))),
        None => None,
    };
    if let Some((earlier, report)) = passed_earlier {
        ctx.emit_progress(format!("checks not run again (unchanged tree): {named}"))
            .await;
        // No `with_repeat_key`: this answer never varies (it quotes the earlier report and counts
        // nothing), so the repeated-call rule already refuses a third ask of it, and that key is
        // for a failure whose text varies while its cause does not.
        return Ok(
            ToolOutput::text(unchanged_pass_text(earlier, &named)).with_artifact(
                report
                    .reused_at(earlier.start_tree.as_deref().unwrap_or_default())
                    .into_artifact(redactor),
            ),
        );
    }

    ctx.emit_progress(format!("running checks: {named}")).await;
    let environment = env.session(ctx).await?;
    // At most the run's own bound on the tool calls of a turn at once: a deployment that sets it to
    // 1 runs the commands one after another, and each takes a seat of the process pool
    // (`MAX_PROCESSES`, ADR 0065) as it runs.
    let bound = env.settings.max_parallel_tool_calls.max(1);
    let outcomes = run_all(&env, ctx, &*environment, &dir, &commands, timeout, bound).await?;

    // A command the shell could not find is a missing toolchain, not a failing check: the
    // workspace lacks the tool, and no change to the code would make the check pass. So the call is
    // not recorded (no cycle used, no `checks` artifact, nothing for the gate to see), and the
    // model is told to report it and wait, which is all it can do: nothing is installed here. The
    // other commands of the call did run; their output is shown so that none of it is lost.
    let missing = missing_of(&commands, &outcomes);
    if !missing.is_empty() {
        ctx.emit_progress(format!("the workspace lacks a tool: {named}"))
            .await;
        let kind = environment.describe().kind;
        let mut said = String::new();
        for (n, (_, tool)) in missing.iter().enumerate() {
            if n > 0 {
                said.push('\n');
            }
            said.push_str(
                &missing_tool_answer(&env, ctx, tool, &[dir.as_path(), slot.path()], &kind).await?,
            );
        }
        let ran: Vec<usize> = (0..outcomes.len())
            .filter(|n| !missing.iter().any(|(missing, _)| missing == n))
            .collect();
        if !ran.is_empty() {
            said.push_str("\nThe other commands of this call ran:\n");
            for n in ran {
                said.push_str(&render(&shown[n], &outcomes[n], timeout));
            }
        }
        return Ok(ToolOutput::error(said));
    }

    // A failure in a repository that is there on the base too is not the run's (ADR 0026). The
    // commands ran at the same time; the base is asked one command at a time, after them, because
    // the answer is kept in the run's notes.
    let mut on_base: Vec<Option<OnBase>> = vec![None; outcomes.len()];
    for (n, outcome) in outcomes.iter().enumerate() {
        let Some(wt) = slot.worktree() else {
            break;
        };
        if !outcome.passed() && !outcome.timed_out && outcome.exit_code.is_some() {
            on_base[n] = Some(
                preexisting::on_base(
                    preexisting::Failing {
                        env: &env,
                        ctx,
                        session: &*environment,
                        run: &run,
                        wt,
                        shown: shown[n].as_str(),
                        command: commands[n].as_str(),
                        cwd: cwd.as_deref(),
                    },
                    &mut notes,
                )
                .await?,
            );
        }
    }

    // The code the commands just ran on, so a pull request can be tied to
    // the exact tree that was verified. (A check may write a file: the report is bound to the tree
    // it leaves, which is what `commit_and_push` will commit.)
    let at = super::gitcli::GitAt::slot(&slot);
    let tree = super::gitcli::working_tree_id(&at).await;
    let head = super::gitcli::head_sha(&at).await;
    let dirty = match (&tree, super::gitcli::head_tree(&at).await) {
        (Some(tree), Some(head_tree)) => *tree != head_tree,
        _ => false,
    };
    let runs: Vec<Run<'_>> = (0..outcomes.len())
        .map(|n| Run {
            command: shown[n].as_str(),
            outcome: &outcomes[n],
            timeout,
            preexisting: match &on_base[n] {
                Some(OnBase::Fails(result)) => Some(result.commit.as_str()),
                _ => None,
            },
        })
        .collect();
    let report = ChecksReport::of(&Batch {
        runs: &runs,
        head: head.as_deref(),
        dirty,
        tree: tree.as_deref(),
        // A scratch project has no repository (yet).
        repository: slot.worktree().map(|wt| wt.repo().url.as_str()),
        environment: EnvironmentReport::of(&environment.describe()),
    })
    .scrubbed(redactor);
    let passed = outcomes.iter().all(ShellOutcome::passed);
    let preexisting = report.preexisting;
    let base_commit = report.base_commit.clone();
    let artifact = report.clone().into_artifact(redactor);
    let mut text = String::new();
    for (n, outcome) in outcomes.iter().enumerate() {
        text.push_str(&render(&shown[n], outcome, timeout));
    }
    let failures = notes.record_check(CheckRecord {
        call_id: call,
        command: named.clone(),
        passed,
        commands: u32::try_from(outcomes.len()).unwrap_or(u32::MAX),
        exit_code: first_failing_exit(&outcomes),
        tail: recorded_tail(&outcomes, &shown),
        tree,
        start_tree: tree_before,
        cwd: dir_key,
        epoch,
        timeout_secs: timeout.as_secs(),
        command_sha,
        report: Some(report),
        slot: Some(slot.dir().to_owned()),
        scratch,
        preexisting,
        base_commit,
    });
    env.notes
        .save(&run, &notes)
        .await
        .map_err(|e| notes_error(&e))?;
    ctx.emit_progress(if passed {
        format!("checks passed: {named}")
    } else if preexisting {
        format!("checks failed, and fail on the base too (no cycle used): {named}")
    } else {
        format!("checks failed ({failures} of {max} cycles used): {named}")
    })
    .await;

    if passed {
        return Ok(ToolOutput::text(text).with_artifact(artifact));
    }
    let key = batch_repeat_key(&outcomes);
    let branch = slot
        .worktree()
        .map_or("", |wt| wt.repo().base_branch.as_str());
    for (n, on_base) in on_base.iter().enumerate() {
        match on_base {
            Some(OnBase::Fails(result)) => {
                text.push_str(&preexisting::fails_text(
                    result,
                    branch,
                    &outcomes[n].tail,
                    failures,
                    max,
                ));
            }
            Some(OnBase::Passes(result)) => {
                text.push_str(&preexisting::passes_text(result, branch));
            }
            _ => {}
        }
    }
    // The call as a whole spent no cycle only when every failure is the repository's; one that is
    // the run's spends one, however many commands failed, and the model is told how many are left.
    if !preexisting {
        if failures >= max {
            text.push_str(&format!(
                "\nCheck-cycle limit reached ({failures} of {max}). Do not run checks, commit, \
                 push or open a pull request any more. Stop now and report what you did, which \
                 check still fails, and the output above.\n"
            ));
        } else {
            text.push_str(&format!(
                "\nThis was failed check run {failures} of {max}. Fix the cause (not the check), \
                 then run it again.\n"
            ));
        }
    }
    Ok(ToolOutput::error(text)
        .with_artifact(artifact)
        .with_repeat_key(key))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const TREE: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn outcome(code: Option<i32>, tail: &str) -> ShellOutcome {
        ShellOutcome {
            exit_code: code,
            timed_out: false,
            tail: tail.to_owned(),
            truncated: false,
            ..ShellOutcome::default()
        }
    }

    fn report(o: &ShellOutcome, head: Option<&str>, dirty: bool) -> ChecksReport {
        let run = Run {
            command: "cargo test",
            outcome: o,
            timeout: Duration::from_secs(900),
            preexisting: None,
        };
        ChecksReport::of(&Batch {
            runs: std::slice::from_ref(&run),
            head,
            dirty,
            tree: Some(TREE),
            repository: None,
            environment: None,
        })
    }

    /// The report of several commands, each given by its text and its outcome, in that order.
    fn batch(runs: &[(&str, ShellOutcome)]) -> ChecksReport {
        let runs: Vec<Run<'_>> = runs
            .iter()
            .map(|(command, outcome)| Run {
                command,
                outcome,
                timeout: Duration::from_secs(900),
                preexisting: None,
            })
            .collect();
        ChecksReport::of(&Batch {
            runs: &runs,
            head: Some(SHA),
            dirty: false,
            tree: Some(TREE),
            repository: None,
            environment: None,
        })
    }

    fn finding(n: usize, message: String) -> Finding {
        Finding {
            check: format!("check-{n}"),
            message,
        }
    }

    fn bytes(findings: &[Finding]) -> usize {
        findings
            .iter()
            .map(|f| f.check.len() + f.message.len())
            .sum()
    }

    #[test]
    fn a_pass_has_no_findings_and_serializes_without_them() {
        let r = report(&outcome(Some(0), "ok\n"), Some(SHA), false);
        assert!(r.passed);
        assert_eq!(r.commit, SHA);
        assert_eq!(r.summary.as_deref(), Some("`cargo test` passed"));
        assert!(r.findings.is_empty());
        let data = serde_json::to_value(&r).unwrap();
        assert_eq!(
            data,
            json!({"passed": true, "commit": SHA, "tree": TREE, "summary": "`cargo test` passed"})
        );
    }

    #[test]
    fn a_failure_names_the_check_and_keeps_the_output() {
        let r = report(&outcome(Some(101), "test a ... FAILED\n"), Some(SHA), false);
        assert!(!r.passed);
        assert_eq!(
            r.findings,
            [Finding {
                check: "cargo test".to_owned(),
                message: "exit code 101\ntest a ... FAILED".to_owned(),
            }]
        );
        assert_eq!(
            r.summary.as_deref(),
            Some("`cargo test` failed: exit code 101")
        );
    }

    #[test]
    fn a_timeout_and_a_signal_fail_with_their_reason() {
        let mut o = outcome(None, "");
        o.timed_out = true;
        let r = report(&o, Some(SHA), false);
        assert!(!r.passed);
        assert_eq!(r.findings[0].message, "timed out after 900s");
        let r = report(&outcome(None, ""), Some(SHA), false);
        assert_eq!(r.findings[0].message, "killed by a signal");
    }

    #[test]
    fn no_commit_fails_with_a_finding_that_says_so() {
        for head in [None, Some(""), Some("HEAD"), Some("abc123")] {
            let r = report(&outcome(Some(0), ""), head, false);
            assert!(!r.passed, "{head:?}");
            assert_eq!(r.commit, "");
            assert_eq!(r.findings.len(), 1);
            assert_eq!(r.findings[0].check, "commit");
            assert!(
                r.findings[0]
                    .message
                    .contains("cannot determine the commit")
            );
        }
        // A failing check without a commit reports both.
        let r = report(&outcome(Some(1), "boom"), None, false);
        assert_eq!(r.findings.len(), 2);
        assert_eq!(r.findings[0].check, "cargo test");
    }

    #[test]
    fn a_run_on_uncommitted_changes_says_so() {
        let r = report(&outcome(Some(0), ""), Some(SHA), true);
        assert!(r.passed);
        assert!(
            r.summary
                .unwrap()
                .contains("uncommitted changes on top of 0123456789")
        );
    }

    #[test]
    fn the_summary_is_one_short_line() {
        let o = outcome(Some(1), "");
        let long = format!("echo a\n{}", "x".repeat(1000));
        let run = Run {
            command: &long,
            outcome: &o,
            timeout: Duration::from_secs(1),
            preexisting: None,
        };
        let r = ChecksReport::of(&Batch {
            runs: std::slice::from_ref(&run),
            head: Some(SHA),
            dirty: false,
            tree: None,
            repository: None,
            environment: None,
        });
        let summary = r.summary.unwrap();
        assert!(!summary.contains('\n'));
        assert!(summary.len() < 240, "{}", summary.len());
        assert!(r.findings[0].check.len() <= MAX_CHECK_NAME);
    }

    #[test]
    fn findings_within_the_caps_are_untouched() {
        let f: Vec<_> = (0..MAX_FINDINGS).map(|n| finding(n, "m".into())).collect();
        assert_eq!(cap_findings(f.clone()), f);
    }

    #[test]
    fn more_than_twenty_findings_are_cut_and_the_cut_is_marked() {
        let f: Vec<_> = (0..45).map(|n| finding(n, "m".into())).collect();
        let capped = cap_findings(f);
        assert_eq!(capped.len(), MAX_FINDINGS);
        assert_eq!(capped[0].check, "check-0");
        assert_eq!(capped[18].check, "check-18");
        let last = capped.last().unwrap();
        assert_eq!(last.check, "findings");
        assert!(
            last.message.starts_with("[cut: 26 more findings left out"),
            "{}",
            last.message
        );
    }

    #[test]
    fn a_long_message_keeps_its_end_and_is_marked() {
        let long = format!("{}END", "x".repeat(40_000));
        let capped = cap_findings(vec![finding(0, long.clone())]);
        assert_eq!(capped.len(), 1, "one finding cut, none left out");
        assert!(bytes(&capped) <= MAX_FINDINGS_BYTES, "{}", bytes(&capped));
        let m = &capped[0].message;
        assert!(m.starts_with("[cut: the last "), "{}", &m[..80]);
        assert!(m.contains(&format!("of {} bytes]", long.len())));
        assert!(m.ends_with("xxxEND"));
    }

    #[test]
    fn the_byte_cap_counts_every_finding_and_leaves_out_what_does_not_fit() {
        let f: Vec<_> = (0..10).map(|n| finding(n, "y".repeat(5_000))).collect();
        let capped = cap_findings(f);
        assert!(bytes(&capped) <= MAX_FINDINGS_BYTES, "{}", bytes(&capped));
        assert!(capped.len() < 10);
        let last = capped.last().unwrap();
        assert_eq!(last.check, "findings");
        assert!(last.message.contains("more findings left out"));
        assert!(
            capped[..capped.len() - 1]
                .iter()
                .all(|f| f.message.starts_with('y') || f.message.starts_with("[cut"))
        );
    }

    #[test]
    fn cuts_never_split_a_character() {
        let long = "é".repeat(20_000);
        let capped = cap_findings(vec![finding(0, long)]);
        assert!(bytes(&capped) <= MAX_FINDINGS_BYTES);
        assert!(capped[0].message.ends_with('é'));
        let name = "ü".repeat(400);
        let r = report_named(&name);
        assert!(r.findings[0].check.len() <= MAX_CHECK_NAME);
    }

    fn report_named(command: &str) -> ChecksReport {
        let o = outcome(Some(1), "x");
        let run = Run {
            command,
            outcome: &o,
            timeout: Duration::from_secs(1),
            preexisting: None,
        };
        ChecksReport::of(&Batch {
            runs: std::slice::from_ref(&run),
            head: Some(SHA),
            dirty: false,
            tree: None,
            repository: None,
            environment: None,
        })
    }

    #[test]
    fn a_report_survives_the_notes_file() {
        for r in [
            report(&outcome(Some(0), ""), Some(SHA), false),
            report(&outcome(Some(1), "boom"), None, true),
        ] {
            let back: ChecksReport =
                serde_json::from_slice(&serde_json::to_vec_pretty(&r).unwrap()).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn a_report_binds_to_another_commit_of_the_same_tree() {
        let dirty = report(&outcome(Some(1), "boom\n"), Some(SHA), true);
        assert!(dirty.summary.as_ref().unwrap().contains("uncommitted"));
        let pushed = "aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd";
        let bound = dirty.bound_to(pushed, TREE, false);
        assert_eq!(bound.commit, pushed);
        assert_eq!(bound.tree.as_deref(), Some(TREE));
        assert!(!bound.passed);
        assert_eq!(bound.findings, dirty.findings);
        assert_eq!(
            bound.summary.as_deref(),
            Some(
                "`cargo test` failed: exit code 1; checked on the identical tree before it was \
                 committed as aaaaaaaaaa"
            )
        );
        assert_eq!(bound, dirty.bound_to(pushed, TREE, false), "pure");
        // A stale "no commit" finding does not travel to a commit that exists.
        let headless = report(&outcome(Some(0), ""), None, false);
        assert!(!headless.passed);
        let bound = headless.bound_to(pushed, TREE, true);
        assert!(bound.passed && bound.findings.is_empty());
    }

    /// A report marked reused says which results were reused: the summary carries the marker once,
    /// the rest of the earlier report is unchanged, and `reused` is absent while nothing was.
    #[test]
    fn a_reused_report_says_which_results_were_reused() {
        let r = report(&outcome(Some(0), "ok\n"), Some(SHA), false);
        assert!(!r.reused);
        let data = serde_json::to_value(&r).unwrap();
        assert!(data.get("reused").is_none(), "{data}");

        let reused = r.clone().reused_at(TREE);
        assert!(reused.reused);
        assert_eq!(
            reused.summary.as_deref(),
            Some(
                "`cargo test` passed; these results were reused: the same call passed on tree \
                 fedcba9876"
            )
        );
        assert_eq!(serde_json::to_value(&reused).unwrap()["reused"], true);
        // Marking twice does not repeat the marker, and the verdict is the earlier one.
        assert_eq!(reused.clone().reused_at(TREE), reused);
        assert_eq!(reused.passed, r.passed);
        assert_eq!(reused.commit, r.commit);
        assert_eq!(reused.tree, r.tree);
        assert_eq!(reused.findings, r.findings);
        // Binding to a commit keeps the mark.
        let bound = reused.bound_to("aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd", TREE, true);
        assert!(bound.reused);
    }

    #[test]
    fn an_unchecked_commit_fails_and_says_which_trees_differ() {
        let pushed = "aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd";
        let r = ChecksReport::unchecked(pushed, Some(TREE), Some(&SHA[..40]), true);
        assert!(!r.passed);
        assert_eq!(r.commit, pushed);
        assert_eq!(r.tree.as_deref(), Some(TREE));
        assert_eq!(
            r.findings,
            [Finding {
                check: "checks".to_owned(),
                message: "the pushed tree was not checked: the last checks ran on 0123456789, \
                          the commit has fedcba9876"
                    .to_owned()
            }]
        );
        let none = ChecksReport::unchecked(pushed, None, None, false);
        assert!(none.tree.is_none());
        assert!(
            none.findings[0]
                .message
                .contains("no check ran in this run")
        );
        assert!(none.findings[0].message.contains("an unknown tree"));
    }

    #[test]
    fn the_artifact_is_json_and_scrubbed_of_secrets() {
        let redactor = Redactor::new(["ghp_secretvalue123"]);
        let r = report(
            &outcome(Some(1), "token=ghp_secretvalue123\n"),
            Some(SHA),
            false,
        );
        let a = r.into_artifact(&redactor);
        assert_eq!(a.name, "checks");
        assert_eq!(a.mime_type.as_deref(), Some("application/json"));
        let text = a.data.to_string();
        assert!(!text.contains("ghp_secretvalue123"), "{text}");
        assert!(
            text.contains("[redacted: secret removed, do not reuse]"),
            "{text}"
        );
    }

    #[test]
    fn a_call_names_one_command_or_a_list_never_both() {
        assert_eq!(
            commands_of(Some("cargo test".into()), None).unwrap(),
            ["cargo test"]
        );
        assert_eq!(
            commands_of(None, Some(vec!["a".into(), " b ".into()])).unwrap(),
            ["a", "b"]
        );
        for (command, commands) in [(None, None), (Some("a".into()), Some(vec!["b".into()]))] {
            let said = commands_of(command, commands).unwrap_err();
            assert!(
                said.contains("`command`") && said.contains("`commands`"),
                "{said}"
            );
        }
        assert!(commands_of(Some("  ".into()), None).is_err());
        assert!(commands_of(None, Some(vec![])).is_err());
        let said = commands_of(None, Some(vec!["a".into(), " ".into()])).unwrap_err();
        assert!(said.contains("commands[1]"), "{said}");
        // A blank `command` beside a list is both given, not one.
        let said = commands_of(Some("  ".into()), Some(vec!["a".into()])).unwrap_err();
        assert!(
            said.contains("`command`") && said.contains("`commands`"),
            "{said}"
        );
        // A NUL is refused in either form: it is what a length prefix has to tell from a boundary.
        let said = commands_of(Some("echo a\0b".into()), None).unwrap_err();
        assert!(said.contains("`command`") && said.contains("NUL"), "{said}");
        let said = commands_of(None, Some(vec!["a".into(), "b\0c".into()])).unwrap_err();
        assert!(
            said.contains("`commands[1]`") && said.contains("NUL"),
            "{said}"
        );
    }

    #[test]
    fn one_command_hashes_as_it_always_did() {
        // The literal digest a note written before `commands` existed holds: the reuse must still
        // find it, so the single-command branch may not change (a restatement of the branch would
        // pass whatever the branch computed).
        let one = vec!["cargo test".to_owned()];
        assert_eq!(
            call_sha(&one),
            "0031fd18feb55b6b493544de1976b438eeac0b2d1278821b9e4e8eb07e8cf503"
        );
        // A list is its commands in order, each behind its length: where one ends and the next
        // begins is part of the key.
        assert_ne!(
            call_sha(&["a".to_owned(), "b".to_owned()]),
            call_sha(&["ab".to_owned(), String::new()])
        );
        assert_ne!(
            call_sha(&["a".to_owned(), "b".to_owned()]),
            call_sha(&["a\0b".to_owned()])
        );
        assert_ne!(
            call_sha(&["a".to_owned(), "b".to_owned()]),
            call_sha(&["b".to_owned(), "a".to_owned()])
        );
    }

    #[test]
    fn a_batch_passes_only_when_every_command_passed_and_says_how_many() {
        let r = batch(&[
            ("a", outcome(Some(0), "ok")),
            ("b", outcome(Some(0), "")),
            ("c", outcome(Some(0), "")),
        ]);
        assert!(r.passed);
        assert!(r.findings.is_empty());
        assert_eq!(r.summary.as_deref(), Some("3 of 3 checks passed"));

        let r = batch(&[
            ("a", outcome(Some(0), "ok")),
            ("b", outcome(Some(1), "boom")),
            ("c", outcome(Some(0), "")),
        ]);
        assert!(!r.passed);
        assert_eq!(r.summary.as_deref(), Some("1 of 3 checks failed"));
        assert_eq!(
            r.findings,
            [Finding {
                check: "b".to_owned(),
                message: "exit code 1\nboom".to_owned(),
            }]
        );
    }

    #[test]
    fn the_failing_checks_are_listed_in_the_order_the_caller_gave_them() {
        let r = batch(&[
            ("a", outcome(Some(2), "a-failed")),
            ("b", outcome(Some(0), "b-ok")),
            ("c", outcome(Some(3), "c-failed")),
        ]);
        let checks: Vec<&str> = r.findings.iter().map(|f| f.check.as_str()).collect();
        assert_eq!(checks, ["a", "c"]);
        assert_eq!(r.findings[0].message, "exit code 2\na-failed");
        assert_eq!(r.findings[1].message, "exit code 3\nc-failed");
    }

    #[test]
    fn a_batch_is_preexisting_only_when_every_failure_is() {
        let o = outcome(Some(1), "boom");
        let base = "aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd";
        let commands = ["a", "b"];
        let report_of = |preexisting: [Option<&'static str>; 2]| {
            let runs: Vec<Run<'_>> = commands
                .iter()
                .zip(preexisting)
                .map(|(command, preexisting)| Run {
                    command,
                    outcome: &o,
                    timeout: Duration::from_secs(1),
                    preexisting,
                })
                .collect();
            ChecksReport::of(&Batch {
                runs: &runs,
                head: Some(SHA),
                dirty: false,
                tree: Some(TREE),
                repository: None,
                environment: None,
            })
        };
        let all = report_of([Some(base), Some(base)]);
        assert!(all.preexisting);
        assert_eq!(all.base_commit.as_deref(), Some(base));
        assert!(
            all.summary
                .as_deref()
                .unwrap()
                .contains("pre-existing failure")
        );
        let one = report_of([Some(base), None]);
        assert!(!one.preexisting);
        assert!(one.base_commit.is_none());
        assert!(
            !one.summary
                .as_deref()
                .unwrap()
                .contains("pre-existing failure")
        );
    }

    /// Three checks that each sleep finish in about one check's time when they run at the same
    /// time, and in three times that with a bound of 1: the clock is tokio's, paused, so the sleeps
    /// cost no real time.
    #[tokio::test(start_paused = true)]
    async fn independent_checks_run_at_the_same_time_up_to_the_bound() {
        let jobs = || {
            (0..3)
                .map(|n| async move {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    n
                })
                .collect::<Vec<_>>()
        };

        let started = tokio::time::Instant::now();
        let out = at_most(jobs(), 3).await;
        let took = started.elapsed();
        assert_eq!(out, [0, 1, 2], "the results keep the order they were given");
        assert!(
            (Duration::from_secs(1)..Duration::from_secs(2)).contains(&took),
            "three checks at once took {took:?}"
        );

        let started = tokio::time::Instant::now();
        let out = at_most(jobs(), 1).await;
        let took = started.elapsed();
        assert_eq!(out, [0, 1, 2]);
        assert!(
            (Duration::from_secs(3)..Duration::from_secs(4)).contains(&took),
            "three checks one after another took {took:?}"
        );
    }

    /// A quick check that starts later does not overtake a slow one that started first: the results
    /// come back in the caller's order, however they finish.
    #[tokio::test(start_paused = true)]
    async fn a_quick_check_does_not_overtake_a_slow_one() {
        let started = tokio::time::Instant::now();
        let jobs: Vec<_> = [(3_u64, "a"), (1, "b"), (2, "c")]
            .into_iter()
            .map(|(seconds, name)| async move {
                tokio::time::sleep(Duration::from_secs(seconds)).await;
                name
            })
            .collect();
        let out = at_most(jobs, 3).await;
        assert_eq!(out, ["a", "b", "c"]);
        assert!(
            (Duration::from_secs(3)..Duration::from_secs(4)).contains(&started.elapsed()),
            "the batch took {:?}",
            started.elapsed()
        );
    }
}
