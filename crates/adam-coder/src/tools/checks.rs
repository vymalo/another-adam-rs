//! `run_checks { command }`.

use std::sync::Arc;

use adam_llm_agent::{Tool, ToolCtx, ToolOutput};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::notes::CheckRecord;
use super::shell::{ShellOutcome, resolve_cwd, run_shell};
use super::{Outcome, ToolEnv, notes_error, str_arg};

/// Runs a command in the worktree and reports how it went.
///
/// * `sh -lc <command>`, cwd inside the worktree (`cwd`, if given, must stay
///   inside it), the process group killed after the timeout, only the output
///   tail kept.
/// * A failed run costs one check cycle. After `max_check_cycles` failures the
///   tool no longer runs anything and tells the model to stop and report.
pub struct RunChecks {
    env: Arc<ToolEnv>,
}

impl RunChecks {
    /// The tool over `env`.
    pub fn new(env: Arc<ToolEnv>) -> Self {
        Self { env }
    }
}

/// `900s`, or `500ms` below a second.
fn human(d: std::time::Duration) -> String {
    if d.as_secs() >= 1 {
        format!("{}s", d.as_secs())
    } else {
        format!("{}ms", d.as_millis())
    }
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

#[async_trait]
impl Tool for RunChecks {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_checks".into(),
            description: "Run a shell command in your worktree (a project check such as `cargo test`, \
                          `pnpm test` or `just ci`) and get its exit code and the tail of its output. \
                          Only exit code 0 counts as passing. Every failed run uses up one of your \
                          limited check cycles."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command, run with `sh -lc` in the worktree"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional sub-directory of the worktree to run in (relative, inside the worktree)"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Outcome {
        let Some(command) = str_arg(&args, "command") else {
            return Ok(ToolOutput::error("command is required"));
        };
        let max = self.env.settings.max_check_cycles;
        let run = ctx.run_id().to_string();
        let mut notes = self
            .env
            .notes
            .load(&run)
            .await
            .map_err(|e| notes_error(&e))?;

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

        let wt = match self.env.worktree(ctx).await {
            Ok(wt) => wt,
            Err(outcome) => return outcome,
        };
        let dir = match resolve_cwd(wt.path(), str_arg(&args, "cwd")) {
            Ok(dir) => dir,
            Err(reason) => return Ok(ToolOutput::error(reason)),
        };

        // What the command printed is not ours to publish: scrub the process's
        // secrets before the tail reaches the model, the notes, an event or a
        // failed run's findings. The command line is shown scrubbed too.
        let redactor = &self.env.redactor;
        let shown = redactor.scrub(command).into_owned();
        ctx.emit_progress(format!("running checks: {shown}")).await;
        let mut outcome = run_shell(
            &dir,
            command,
            self.env.settings.check_timeout,
            self.env.settings.check_output_tail,
        )
        .await
        .map_err(|e| {
            adam_llm_agent::ToolError::Transient(format!("cannot start the shell: {e}"))
        })?;

        outcome.tail = redactor.scrub_string(std::mem::take(&mut outcome.tail));

        // The code the command just ran on, so a pull request can be tied to
        // the exact tree that was verified.
        let tree = super::gitcli::working_tree_id(wt.path()).await;
        let passed = outcome.passed();
        let text = render(&shown, &outcome, self.env.settings.check_timeout);
        let failures = notes.record_check(CheckRecord {
            call_id: ctx.call_id().to_owned(),
            command: shown.clone(),
            passed,
            exit_code: outcome.exit_code,
            tail: outcome.tail.clone(),
            tree,
        });
        self.env
            .notes
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
            return Ok(ToolOutput::text(text));
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
        Ok(ToolOutput::error(text))
    }
}
