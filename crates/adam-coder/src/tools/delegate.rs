//! `delegate_to_opencode { instructions }`.

use std::sync::Arc;
use std::time::Duration;

use adam_acp::{AcpClient, AcpError, AcpUpdate, ClientPolicy, Session};
use adam_error::{Classify, report};
use adam_llm_agent::{Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::ToolSpec;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};

use crate::redact::Redactor;

use super::{Outcome, ToolEnv, cancelled, str_arg};

/// Most of the agent's reply kept for the summary.
const SUMMARY_CAP: usize = 8 * 1024;
/// Longest progress line.
const PROGRESS_CAP: usize = 300;
/// Most changed files listed in the result.
const MAX_LISTED_FILES: usize = 40;
/// How long OpenCode gets to end its turn after `session/cancel` before it is
/// killed.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// Has OpenCode make a change in the worktree, over ACP.
///
/// Starts the configured ACP program in the worktree, opens a session there and
/// sends `instructions` as one prompt. What the agent reports while it works
/// (tool calls, plan, text) becomes progress events; the result is its own
/// summary plus the files that changed. The agent may read and write files only
/// under the worktree (`ClientPolicy::fs_root`), and its permission requests
/// are answered by `PermissionMode::AllowWithinRoot`.
///
/// **Cancellation.** When the run is cancelled ([`ToolCtx::cancelled`]) while
/// OpenCode works, the tool sends ACP `session/cancel`, gives OpenCode a couple
/// of seconds to end its turn, then kills it and everything it started (its
/// process group) and waits until it is reaped, before returning. Nothing is
/// left running once the tool has returned.
pub struct DelegateToOpenCode {
    env: Arc<ToolEnv>,
}

impl DelegateToOpenCode {
    /// The tool over `env`.
    pub fn new(env: Arc<ToolEnv>) -> Self {
        Self { env }
    }
}

fn acp_error(e: &AcpError) -> ToolError {
    // Journaled and shown to the model: a boundary, so the chain is flattened here, once.
    let text = format!("OpenCode: {}", report(e));
    if e.is_retryable() {
        ToolError::Transient(text)
    } else {
        ToolError::Permanent(text)
    }
}

/// Stop OpenCode after a cancel: ask it to end its turn (`session/cancel`),
/// give it [`CANCEL_GRACE`] to do so, then kill it and its process group and
/// wait until it is reaped.
async fn stop(
    client: AcpClient,
    session: Option<&Session>,
    turn: Option<BoxStream<'static, Result<AcpUpdate, AcpError>>>,
) {
    if let Some(session) = session
        && let Err(e) = session.cancel().await
    {
        tracing::debug!(error = %e, "could not send session/cancel");
    }
    if let Some(mut turn) = turn {
        // A well-behaved agent ends the turn with `cancelled` (or dies).
        let drained =
            tokio::time::timeout(CANCEL_GRACE, async { while turn.next().await.is_some() {} })
                .await;
        if drained.is_err() {
            tracing::warn!("OpenCode did not end its turn after session/cancel; killing it");
        }
    }
    if let Err(e) = client.kill().await {
        tracing::debug!(error = %e, "OpenCode was already gone or could not be reaped");
    }
}

fn clip(text: &str, cap: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= cap {
        return text.to_owned();
    }
    let clipped: String = text.chars().take(cap).collect();
    format!("{clipped}...")
}

/// The last `cap` bytes of `text`, on a char boundary.
fn tail(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[async_trait]
impl Tool for DelegateToOpenCode {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delegate_to_opencode".into(),
            description: "Have OpenCode, a coding agent working inside your worktree, make a change. \
                          Give precise instructions: what to change, where, and how it will be \
                          verified. One concern per call. It reads and edits files itself; do not \
                          ask it to commit, push or open pull requests. Returns its summary and the \
                          files that changed."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "instructions": {
                        "type": "string",
                        "description": "What OpenCode should do"
                    }
                },
                "required": ["instructions"]
            }),
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Outcome {
        let Some(instructions) = str_arg(&args, "instructions") else {
            return Ok(ToolOutput::error("instructions is required"));
        };
        let wt = match self.env.worktree(ctx).await {
            Ok(wt) => wt,
            Err(outcome) => return outcome,
        };
        let dir = wt.path().to_path_buf();

        ctx.emit_progress("starting OpenCode").await;
        let command = self.env.settings.opencode.command(&dir);
        // A cancel before the process is up needs no cleanup here: the spawn
        // future kills what it started when it is dropped.
        let client = tokio::select! {
            biased;
            () = ctx.cancelled() => return Err(cancelled("OpenCode was stopped")),
            client = AcpClient::spawn(command, ClientPolicy::new(&dir)) => client,
        }
        .map_err(|e| acp_error(&e))?;
        let session = tokio::select! {
            biased;
            () = ctx.cancelled() => {
                stop(client, None, None).await;
                return Err(cancelled("OpenCode was stopped"));
            }
            session = client.new_session(&dir, Vec::new()) => session,
        }
        .map_err(|e| acp_error(&e))?;

        let mut turn = session.prompt(instructions.to_owned());
        let mut reply = String::new();
        let mut line = String::new();
        let mut stop_reason = None;
        loop {
            let update = tokio::select! {
                biased;
                () = ctx.cancelled() => {
                    stop(client, Some(&session), Some(turn)).await;
                    return Err(cancelled("OpenCode was stopped"));
                }
                update = turn.next() => update,
            };
            let Some(update) = update else { break };
            let update = update.map_err(|e| acp_error(&e))?;
            match update {
                AcpUpdate::AgentText(chunk) => {
                    reply.push_str(&chunk);
                    line.push_str(&chunk);
                    if line.contains('\n') || line.len() >= PROGRESS_CAP {
                        flush(ctx, &self.env.redactor, &mut line).await;
                    }
                }
                AcpUpdate::Thought(_) => {}
                other => {
                    flush(ctx, &self.env.redactor, &mut line).await;
                    if let Some(message) = describe(&other) {
                        ctx.emit_progress(self.env.redactor.scrub_string(message))
                            .await;
                    }
                    if let AcpUpdate::TurnEnded {
                        stop_reason: reason,
                    } = other
                    {
                        stop_reason = Some(reason);
                    }
                }
            }
        }
        flush(ctx, &self.env.redactor, &mut line).await;
        drop(turn);
        if let Err(e) = client.shutdown().await {
            tracing::debug!(error = %e, "OpenCode did not shut down cleanly");
        }

        let stop_reason = stop_reason.unwrap_or_else(|| "unknown".to_owned());
        let changed = match wt.status().await {
            Ok(files) => files,
            Err(e) => return Err(super::workspace_error(&e)),
        };
        let mut text = format!("OpenCode finished (stop reason: {stop_reason}).\n");
        let summary = tail(reply.trim(), SUMMARY_CAP);
        if summary.is_empty() {
            text.push_str("\nIt gave no summary.\n");
        } else {
            text.push_str("\nSummary from OpenCode:\n");
            text.push_str(summary);
            text.push('\n');
        }
        if changed.is_empty() {
            text.push_str("\nNo files are changed in the worktree.\n");
        } else {
            text.push_str(&format!("\nChanged files ({}):\n", changed.len()));
            for file in changed.iter().take(MAX_LISTED_FILES) {
                text.push_str(&format!("- {} ({:?})\n", file.path, file.status));
            }
            if changed.len() > MAX_LISTED_FILES {
                text.push_str(&format!(
                    "- ... and {} more\n",
                    changed.len() - MAX_LISTED_FILES
                ));
            }
        }
        Ok(if stop_reason == "end_turn" {
            ToolOutput::text(text)
        } else {
            ToolOutput::error(text)
        })
    }
}

async fn flush(ctx: &ToolCtx, redactor: &Redactor, line: &mut String) {
    let text = clip(&redactor.scrub(line), PROGRESS_CAP);
    line.clear();
    if !text.is_empty() {
        ctx.emit_progress(format!("opencode: {text}")).await;
    }
}

/// A progress line for an update worth showing.
fn describe(update: &AcpUpdate) -> Option<String> {
    match update {
        AcpUpdate::ToolCall {
            title,
            kind,
            status,
            ..
        } => Some(format!("opencode: {kind}: {} [{status}]", clip(title, 160))),
        AcpUpdate::ToolCallUpdate { id, status, output } => {
            if matches!(status.as_str(), "completed" | "failed") {
                let detail = output
                    .as_deref()
                    .map(|o| format!(": {}", clip(o, 160)))
                    .unwrap_or_default();
                Some(format!("opencode: tool call {id} {status}{detail}"))
            } else {
                None
            }
        }
        AcpUpdate::Plan(entries) => {
            let done = entries.iter().filter(|e| e.status == "completed").count();
            Some(format!(
                "opencode: plan, {done} of {} steps done",
                entries.len()
            ))
        }
        AcpUpdate::TurnEnded { stop_reason } => {
            Some(format!("opencode: turn ended ({stop_reason})"))
        }
        AcpUpdate::AgentText(_) | AcpUpdate::Thought(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_and_clip_respect_char_boundaries() {
        assert_eq!(tail("héllo wörld", 6), "wörld");
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(clip("  hello  ", 10), "hello");
        assert_eq!(clip("abcdef", 3), "abc...");
    }

    #[test]
    fn only_finished_tool_calls_and_starts_are_described() {
        let call = AcpUpdate::ToolCall {
            id: "t".into(),
            title: "Write hello.txt".into(),
            kind: "edit".into(),
            status: "pending".into(),
        };
        assert_eq!(
            describe(&call).as_deref(),
            Some("opencode: edit: Write hello.txt [pending]")
        );
        let running = AcpUpdate::ToolCallUpdate {
            id: "t".into(),
            status: "in_progress".into(),
            output: None,
        };
        assert_eq!(describe(&running), None);
        let done = AcpUpdate::ToolCallUpdate {
            id: "t".into(),
            status: "completed".into(),
            output: Some("ok".into()),
        };
        assert_eq!(
            describe(&done).as_deref(),
            Some("opencode: tool call t completed: ok")
        );
    }
}
