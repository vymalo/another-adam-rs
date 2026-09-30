//! `run_checks { command }`, and the `checks` artifact it reports.
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
//! The report and the tree it checked are kept in the run's notes, so that `commit_and_push` can bind
//! it to the commit it pushes when that commit has the same tree (see [`ChecksReport::bound_to`]),
//! and otherwise says the commit was not checked ([`ChecksReport::unchecked`]).

use adam::Artifact;
use adam::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::redact::Redactor;

use super::notes::CheckRecord;
use super::shell::{
    MissingTool, ShellOutcome, missing_tool, project_dependency_hint, resolve_cwd, run_shell,
};
use super::{Outcome, ToolEnv, non_empty, notes_error};

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

/// The data part of the `checks` artifact.
///
/// | Field | |
/// |---|---|
/// | `passed` | the check passed, and the report is complete |
/// | `commit` | the 40-hex SHA of the `HEAD` of the run's workspace when the check ran; empty only when it could not be determined (`passed` is then `false`) |
/// | `tree` | the git tree id (40 hex) of the code the check ran on, as `git add -A` would commit it; absent when it could not be computed |
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
    /// One line about the run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// What failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
}

/// How a summary says the check ran on uncommitted changes; `sha10)` follows.
const DIRTY_NOTE: &str = " (on uncommitted changes on top of ";

/// What a `run_checks` call knows about the run, for the report.
struct Run<'a> {
    /// The command, already scrubbed.
    command: &'a str,
    outcome: &'a ShellOutcome,
    timeout: std::time::Duration,
    /// `HEAD` of the workspace repository, if it has one.
    head: Option<&'a str>,
    /// The worktree differed from `HEAD` when the check ran.
    dirty: bool,
    /// The tree of the worktree as it would be committed.
    tree: Option<&'a str>,
}

impl ChecksReport {
    /// The report of one run.
    fn of(run: &Run<'_>) -> Self {
        let passed = run.outcome.passed();
        let commit = run.head.filter(|h| is_sha(h));
        let mut findings = Vec::new();
        let verdict = verdict(run.outcome, run.timeout);
        if !passed {
            let tail = run.outcome.tail.trim();
            let message = if tail.is_empty() {
                verdict.clone()
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

        let command = one_line(run.command, MAX_SUMMARY_COMMAND);
        let mut summary = if passed {
            format!("`{command}` passed")
        } else {
            format!("`{command}` failed: {verdict}")
        };
        if let (true, Some(sha)) = (run.dirty, commit) {
            summary.push_str(&format!("{DIRTY_NOTE}{})", &sha[..sha.len().min(10)]));
        }
        Self {
            passed: passed && commit.is_some(),
            commit: commit.unwrap_or_default().to_owned(),
            tree: run.tree.filter(|t| is_sha(t)).map(str::to_owned),
            summary: Some(summary),
            findings: cap_findings(findings),
        }
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
        Artifact {
            name: CHECKS_ARTIFACT.into(),
            mime_type: Some("application/json".into()),
            data,
        }
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

/// `text` without its start, at most `max` bytes, cut on a character boundary.
fn cut_tail(text: &str, max: usize) -> &str {
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

/// What the model is told when a command failed because the workspace lacks a tool (see
/// [`missing_tool`]): what is missing, that no check cycle was used, and that the way on is to tell
/// the person and wait. It must not retry variants, hunt the filesystem, or install a system
/// toolchain. A tool the **project** brings itself (`jest`, `vitest`, `tsc` with a `package.json`:
/// `install` is the project's own install command, from [`project_dependency_hint`]) is another
/// story: the project's dependencies are installed with the project's own command.
pub(crate) fn missing_tool_text(missing: &MissingTool, install: Option<&str>) -> String {
    let name = &missing.name;
    if let Some(install) = install {
        return format!(
            "`{name}` is not on the PATH, but it is one of this project's own dependencies, which \
             are not installed in the workspace yet: that is not a missing toolchain and not a \
             failing check (no check cycle was used, nothing was recorded as a check). Install \
             the project's dependencies with its own command (`{install}`), with \
             delegate_to_opencode or run_checks, then run this again. Only if that fails, tell the \
             person with ask_user."
        );
    }
    format!(
        "The workspace has no `{name}`: the shell could not find it (exit code 127). That is a \
         missing toolchain, not a failing check: no check cycle was used and nothing was \
         recorded as a check. Do not retry variants of the command, do not search the \
         filesystem for the tool and do not try to install it (system toolchains are not yours to \
         install). Tell the person which toolchain is missing (`{name}`) with ask_user, and wait \
         for their answer."
    )
}

fn render(command: &str, outcome: &ShellOutcome, timeout: std::time::Duration) -> String {
    let verdict = if outcome.timed_out {
        format!(
            "TIMED OUT after {} (the command was killed): FAILED",
            human(timeout)
        )
    } else {
        match outcome.exit_code {
            Some(0) => "exit code 0: passed".to_owned(),
            Some(code) => format!("exit code {code}: FAILED"),
            None => "killed by a signal: FAILED".to_owned(),
        }
    };
    let mut text = format!("$ {command}\n{verdict}\n");
    if outcome.truncated {
        text.push_str(&format!(
            "(output truncated: the last {} bytes follow)\n",
            outcome.tail.len()
        ));
    }
    text.push_str("--- output ---\n");
    text.push_str(&outcome.tail);
    if !outcome.tail.ends_with('\n') {
        text.push('\n');
    }
    text
}

// Runs a command in the worktree and reports how it went.
//
// * `bash -lc <command>` (`sh -lc` without bash), cwd inside the worktree (`cwd`, if given, must stay
//   inside it), the process group killed after the timeout, only the output
//   tail kept.
// * A failed run costs one check cycle. After `max_check_cycles` failures the
//   tool no longer runs anything and tells the model to stop and report.

/// Run one of the project's own checks in your worktree (the commands its CI, README or
/// Makefile run: `cargo test`, `pnpm test`, `just ci`) and get its exit code and the tail of
/// its output. Only exit code 0 counts as passing. Every failed run uses up one of your limited
/// check cycles and is reported as a check: never use it to look around (use run_command). A
/// command the shell cannot find means the workspace lacks that tool: that is reported, costs no
/// cycle, and is for the person to decide.
#[tool]
pub async fn run_checks(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Shell command, run with `bash -lc` (`sh -lc` without bash) in the worktree
    command: String,
    /// Optional sub-directory of the worktree to run in (relative, inside the worktree)
    cwd: Option<String>,
) -> Outcome {
    let Some(command) = non_empty(&command) else {
        return Ok(ToolOutput::error("command is required"));
    };
    let max = env.settings.max_check_cycles;
    let run = ctx.run_id().to_string();
    let mut notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;

    // A replayed call that was already counted may run again; a new call
    // past the limit may not.
    let replay = notes.checks.counted.iter().any(|c| c == ctx.call_id());
    if notes.cycles_exhausted(max) && !replay {
        let findings = notes
            .checks
            .last
            .as_ref()
            .map(|c| c.tail.clone())
            .unwrap_or_default();
        return Ok(ToolOutput::error(format!(
            "Check-cycle limit reached: {} failed check runs (limit {max}). Do not run \
             checks, commit, push or open a pull request any more. Stop now and report \
             what you did, which check still fails, and these findings from the last run:\n{findings}",
            notes.checks.failures
        )));
    }

    let wt = match env.worktree(ctx).await {
        Ok(wt) => wt,
        Err(outcome) => return outcome,
    };
    let dir = match resolve_cwd(wt.path(), cwd.as_deref().and_then(non_empty)) {
        Ok(dir) => dir,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };

    // What the command printed is not ours to publish: scrub the process's
    // secrets before the tail reaches the model, the notes, an event or a
    // failed run's findings. The command line is shown scrubbed too.
    let redactor = &env.redactor;
    let shown = redactor.scrub(command).into_owned();
    ctx.emit_progress(format!("running checks: {shown}")).await;
    let mut outcome = run_shell(
        &dir,
        command,
        env.settings.check_timeout,
        env.settings.check_output_tail,
    )
    .await
    .map_err(|e| ToolError::Transient(format!("cannot start the shell: {e}")))?;

    outcome.tail = redactor.scrub_string(std::mem::take(&mut outcome.tail));

    // A command the shell could not find is a missing toolchain, not a failing check: the
    // workspace lacks the tool, and no change to the code would make the check pass. So it is not
    // recorded (no cycle used, no `checks` artifact, nothing for the gate to see), and the model
    // is told to report it and wait, which is all it can do: nothing is installed here.
    if let Some(missing) = missing_tool(&outcome, command) {
        ctx.emit_progress(format!("the workspace lacks a tool: {shown}"))
            .await;
        let install = project_dependency_hint(&[dir.as_path(), wt.path()], &missing.name);
        return Ok(ToolOutput::error(missing_tool_text(
            &missing,
            install.as_deref(),
        )));
    }

    // The code the command just ran on, so a pull request can be tied to
    // the exact tree that was verified.
    let tree = super::gitcli::working_tree_id(wt.path()).await;
    let head = super::gitcli::head_sha(wt.path()).await;
    let dirty = match (&tree, super::gitcli::head_tree(wt.path()).await) {
        (Some(tree), Some(head_tree)) => *tree != head_tree,
        _ => false,
    };
    let passed = outcome.passed();
    let report = ChecksReport::of(&Run {
        command: &shown,
        outcome: &outcome,
        timeout: env.settings.check_timeout,
        head: head.as_deref(),
        dirty,
        tree: tree.as_deref(),
    })
    .scrubbed(redactor);
    let artifact = report.clone().into_artifact(redactor);
    let text = render(&shown, &outcome, env.settings.check_timeout);
    let failures = notes.record_check(CheckRecord {
        call_id: ctx.call_id().to_owned(),
        command: shown.clone(),
        passed,
        exit_code: outcome.exit_code,
        tail: outcome.tail.clone(),
        tree,
        report: Some(report),
    });
    env.notes
        .save(&run, &notes)
        .await
        .map_err(|e| notes_error(&e))?;
    ctx.emit_progress(if passed {
        format!("checks passed: {shown}")
    } else {
        format!("checks failed ({failures} of {max} cycles used): {shown}")
    })
    .await;

    if passed {
        return Ok(ToolOutput::text(text).with_artifact(artifact));
    }
    let mut text = text;
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
    Ok(ToolOutput::error(text).with_artifact(artifact))
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
        }
    }

    fn report(o: &ShellOutcome, head: Option<&str>, dirty: bool) -> ChecksReport {
        ChecksReport::of(&Run {
            command: "cargo test",
            outcome: o,
            timeout: Duration::from_secs(900),
            head,
            dirty,
            tree: Some(TREE),
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
        let r = ChecksReport::of(&Run {
            command: &long,
            outcome: &o,
            timeout: Duration::from_secs(1),
            head: Some(SHA),
            dirty: false,
            tree: None,
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
        ChecksReport::of(&Run {
            command,
            outcome: &o,
            timeout: Duration::from_secs(1),
            head: Some(SHA),
            dirty: false,
            tree: None,
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
        assert!(text.contains("[redacted]"), "{text}");
    }
}
