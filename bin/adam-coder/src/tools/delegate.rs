//! `delegate_to_opencode { instructions }`.

use std::collections::HashMap;
use std::time::Duration;

use adam::prelude::*;
use adam::{StepEvent, StepIcon, StepKind, StepState};
use adam_acp::{AcpClient, AcpError, AcpUpdate, ClientPolicy, Session};
use adam_error::{Classify, report};
use futures::StreamExt;
use futures::stream::BoxStream;

use adam_workspace::{EnvKind, EnvSession};

use crate::opencode::acp_command;
use crate::redact::Redactor;

use super::notes::OpenCodeCheck;
use super::shell::run_in;
use super::{Outcome, ToolEnv, cancelled, environment_error, non_empty, notes_error, run_error};

/// Most of the agent's reply kept for the summary.
const SUMMARY_CAP: usize = 8 * 1024;
/// Longest progress line.
const PROGRESS_CAP: usize = 300;
/// Longest label of a step for one of OpenCode's tool calls (its title).
const STEP_LABEL_CAP: usize = 160;
/// Most of a tool call's output a step carries.
const STEP_DETAIL_CAP: usize = 300;
/// Most of OpenCode's reply the `message` step at the end carries.
const SUMMARY_STEP_CAP: usize = 1000;
/// Most changed files listed in the result.
const MAX_LISTED_FILES: usize = 40;
/// How long OpenCode gets to end its turn after `session/cancel` before it is
/// killed.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

fn acp_error(e: &AcpError) -> ToolError {
    // Journaled and shown to the model: a boundary, so the chain is flattened here, once.
    let text = format!("OpenCode: {}", report(e));
    if e.is_retryable() {
        ToolError::Transient(text)
    } else {
        ToolError::Permanent(text)
    }
}

/// How long `opencode --version` gets in a devcontainer.
const VERSION_TIMEOUT: Duration = Duration::from_secs(60);

/// What the model is told when OpenCode cannot reach its model from a devcontainer with no network.
const NO_NETWORK: &str = "OpenCode cannot work in this workspace: its commands run in a devcontainer \
that has no network (this deployment sets DEVCONTAINER_NETWORK=none), and OpenCode reaches its model \
over the network. Make the change yourself with read_file, write_file and apply_patch; run_command \
and run_checks work.";

/// Whether OpenCode starts in the run's devcontainer: `opencode --version` is run there once and the
/// answer is kept in the run's notes (until the environment is made again). `Some(why)` is the
/// refusal to give the model: OpenCode cannot run there (a musl image, another architecture), and
/// the other tools still work. Nothing is asked of the coder's own container, which has the
/// OpenCode it was built with, and nothing of a launcher that is not OpenCode.
async fn opencode_refusal(
    env: &ToolEnv,
    ctx: &ToolCtx,
    session: &dyn EnvSession,
    dir: &std::path::Path,
) -> Result<Option<String>, ToolError> {
    let description = session.describe();
    if !env.settings.opencode.is_opencode()
        || !matches!(
            description.kind,
            EnvKind::DevContainer { .. } | EnvKind::Kubernetes { .. }
        )
    {
        return Ok(None);
    }
    // The setting is the devcontainers' (DEVCONTAINER_NETWORK): a run pod's network is the cluster's.
    if env.settings.container_network_none
        && matches!(description.kind, EnvKind::DevContainer { .. })
    {
        return Ok(Some(NO_NETWORK.to_owned()));
    }
    let run = ctx.root_run_id().to_string();
    let mut notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    let check = match notes.environment.opencode.clone() {
        Some(check) => check,
        None => {
            let outcome = run_in(
                session,
                env.settings.opencode.version_spec(dir, session),
                VERSION_TIMEOUT,
                1024,
                &ctx.cancel_token(),
            )
            .await
            .map_err(|e| run_error(&env.redactor, &e))?;
            let check = OpenCodeCheck {
                works: outcome.passed(),
                detail: if outcome.passed() {
                    String::new()
                } else {
                    let said = env.redactor.scrub(outcome.tail.trim()).into_owned();
                    match outcome.exit_code {
                        _ if outcome.timed_out => "it did not answer in time".to_owned(),
                        Some(code) => format!("exit code {code}: {said}"),
                        None => format!("killed by a signal: {said}"),
                    }
                },
            };
            notes.environment.opencode = Some(check.clone());
            env.notes
                .save(&run, &notes)
                .await
                .map_err(|e| notes_error(&e))?;
            check
        }
    };
    Ok((!check.works).then(|| {
        format!(
            "OpenCode cannot start in this workspace's environment ({}): `opencode --version` \
             failed there ({}). The coder's OpenCode is a native Linux binary built for glibc, so an \
             image of another kind (musl, another architecture) cannot run it. Do not retry. Make \
             the change yourself with read_file, write_file and apply_patch; run_command and \
             run_checks work.",
            description.summary, check.detail
        )
    }))
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

// Has OpenCode make a change in the worktree, over ACP.
//
// Starts the configured ACP program in the worktree, opens a session there and
// sends `instructions` as one prompt. What the agent reports while it works
// becomes steps (`steps/v1`): the call is a `subagent` step labelled OpenCode, each tool call OpenCode
// makes is a child step (`Children`), its plan and the lines of its reply are updates of the OpenCode step,
// and its reply is one `message` child at the end. The result is its own summary plus the files that
// changed. The agent may read and write files only
// under the worktree (`ClientPolicy::fs_root`), and its permission requests
// are answered by `PermissionMode::AllowWithinRoot`.
//
// **Cancellation.** When the run is cancelled ([`ToolCtx::cancelled`]) while
// OpenCode works, the tool sends ACP `session/cancel`, gives OpenCode a couple
// of seconds to end its turn, then kills it and everything it started (its
// process group) and waits until it is reaped, before returning. Nothing is
// left running once the tool has returned.

/// Have OpenCode, a coding agent working inside your worktree, make a change. It runs in the
/// workspace's environment, like your commands (the repository's own devcontainer when it has one),
/// so everything it starts has the repository's tools. Give precise instructions: what to change, where, and how it will be
/// verified. One concern per call. It reads and edits files itself; do not ask
/// it to commit, push or open pull requests. Returns its summary and the files
/// that changed.
#[tool(type = DelegateToOpenCode, step = "subagent", label = "Hand to OpenCode", icon = "opencode")]
pub async fn delegate_to_opencode(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// What OpenCode should do
    instructions: String,
    /// The slot OpenCode works in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    let Some(instructions) = non_empty(&instructions) else {
        return Ok(ToolOutput::error("instructions is required"));
    };
    // OpenCode works in a repository's worktree or in a scratch project alike.
    let slot = match env.slot(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    let dir = slot.path().to_path_buf();

    ctx.emit_progress("starting OpenCode").await;
    // OpenCode runs where the run's processes run, and the command is the one that environment
    // prepared (the files it works on are the same ones, at the same paths).
    let environment = env.session(ctx).await?;
    if let Some(refusal) = opencode_refusal(&env, ctx, &*environment, &dir).await? {
        return Ok(ToolOutput::error(refusal));
    }
    let prepared = environment
        .prepare(&env.settings.opencode.exec_spec(&dir, &*environment))
        .map_err(|e| environment_error(&env.redactor, &e))?;
    let command = acp_command(&prepared).map_err(|e| environment_error(&env.redactor, &e))?;
    // A cancel before the process is up needs no cleanup here: the spawn
    // future kills what it started when it is dropped (and the environment is told).
    let client = tokio::select! {
        biased;
        () = ctx.cancelled() => {
            environment.kill(&prepared.exec).await;
            return Err(cancelled("OpenCode was stopped"));
        }
        client = AcpClient::spawn(command, ClientPolicy::new(&dir)) => client,
    }
    .map_err(|e| acp_error(&e))?;
    let session = tokio::select! {
        biased;
        () = ctx.cancelled() => {
            stop(client, None, None).await;
            environment.kill(&prepared.exec).await;
            return Err(cancelled("OpenCode was stopped"));
        }
        session = client.new_session(&dir, Vec::new()) => session,
    }
    .map_err(|e| acp_error(&e))?;

    let mut turn = session.prompt(instructions.to_owned());
    let mut reply = String::new();
    let mut line = String::new();
    let mut stop_reason = None;
    let mut children = Children::new(ctx.call_id(), &env.redactor);
    loop {
        let update = tokio::select! {
            biased;
            () = ctx.cancelled() => {
                stop(client, Some(&session), Some(turn)).await;
                environment.kill(&prepared.exec).await;
                children.close(ctx, StepState::Canceled).await;
                return Err(cancelled("OpenCode was stopped"));
            }
            update = turn.next() => update,
        };
        let Some(update) = update else { break };
        let update = match update {
            Ok(update) => update,
            Err(e) => {
                children.close(ctx, StepState::Failed).await;
                // The client is dropped on return, which kills what it started here; what lives
                // in the environment is for the session.
                environment.kill(&prepared.exec).await;
                return Err(acp_error(&e));
            }
        };
        match update {
            AcpUpdate::AgentText(chunk) => {
                reply.push_str(&chunk);
                line.push_str(&chunk);
                if line.contains('\n') || line.len() >= PROGRESS_CAP {
                    flush(ctx, &env.redactor, &mut line).await;
                }
            }
            AcpUpdate::Thought(_) => {}
            other => {
                flush(ctx, &env.redactor, &mut line).await;
                match other {
                    AcpUpdate::ToolCall {
                        id,
                        title,
                        kind,
                        status,
                    } => children.tool_call(ctx, &id, &title, &kind, &status).await,
                    AcpUpdate::ToolCallUpdate { id, status, output } => {
                        children
                            .tool_call_update(ctx, &id, &status, output.as_deref())
                            .await;
                    }
                    AcpUpdate::Plan(entries) => {
                        let done = entries.iter().filter(|e| e.status == "completed").count();
                        ctx.emit_progress(format!("plan: {done} of {} done", entries.len()))
                            .await;
                    }
                    AcpUpdate::TurnEnded {
                        stop_reason: reason,
                    } => stop_reason = Some(reason),
                    AcpUpdate::AgentText(_) | AcpUpdate::Thought(_) => {}
                }
            }
        }
    }
    flush(ctx, &env.redactor, &mut line).await;
    // A tool call that never reported its end ended with the turn.
    children.close(ctx, StepState::Canceled).await;
    drop(turn);
    if let Err(e) = client.shutdown().await {
        tracing::debug!(error = %e, "OpenCode did not shut down cleanly");
    }

    // Its reply, once, as a line of its own in the tree.
    let summary = tail(reply.trim(), SUMMARY_STEP_CAP);
    if !summary.is_empty() {
        ctx.report_step(
            StepEvent::new(
                format!("acp:{}:summary", ctx.call_id()),
                StepKind::Message,
                "OpenCode's summary",
                StepState::Completed,
            )
            .with_detail(env.redactor.scrub(summary)),
        )
        .await;
    }

    let stop_reason = stop_reason.unwrap_or_else(|| "unknown".to_owned());
    let changed = match super::scratch::changed_files(&slot).await {
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
            text.push_str(&format!("- {} ({})\n", file.path, file.status));
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

/// A line of OpenCode's reply as an update of the OpenCode step.
async fn flush(ctx: &ToolCtx, redactor: &Redactor, line: &mut String) {
    let text = clip(&redactor.scrub(line), PROGRESS_CAP);
    line.clear();
    if !text.is_empty() {
        ctx.emit_progress(text).await;
    }
}

/// The kind and the icon of the step for a tool call of ACP kind `kind`: a command for `execute`, a
/// tool otherwise, and the icon of the same name when the contract has one (`switch_mode` and `other`
/// have none).
fn style_of(kind: &str) -> (StepKind, Option<StepIcon>) {
    let step = if kind == "execute" {
        StepKind::Command
    } else {
        StepKind::Tool
    };
    (step, StepIcon::parse(kind))
}

/// Where a step stands for ACP's status of a tool call: `pending` and `in_progress` are running,
/// `completed` and `failed` end it.
fn state_of(status: &str) -> StepState {
    match status {
        "completed" => StepState::Completed,
        "failed" => StepState::Failed,
        _ => StepState::Running,
    }
}

/// The tool calls OpenCode makes, as steps that run under the OpenCode step: `acp:<call id>:<ACP id>`.
/// Everything that comes from OpenCode (a title, an output) is scrubbed first, as every other line of
/// the coder is, and cut: the steps are shown to the person.
struct Children<'a> {
    call_id: String,
    redactor: &'a Redactor,
    /// The tool calls that started and have not ended, by ACP id: what an update of one needs.
    open: HashMap<String, Open>,
}

/// What an update needs of the tool call it is about.
struct Open {
    kind: StepKind,
    label: String,
    icon: Option<StepIcon>,
}

impl<'a> Children<'a> {
    fn new(call_id: &str, redactor: &'a Redactor) -> Self {
        Self {
            call_id: call_id.to_owned(),
            redactor,
            open: HashMap::new(),
        }
    }

    fn step(&self, id: &str, open: &Open, state: StepState) -> StepEvent {
        let step = StepEvent::new(
            format!("acp:{}:{id}", self.call_id),
            open.kind,
            &open.label,
            state,
        );
        match open.icon {
            Some(icon) => step.with_icon(icon),
            None => step,
        }
    }

    /// OpenCode started a tool call (or reported one that is already over).
    async fn tool_call(&mut self, ctx: &ToolCtx, id: &str, title: &str, kind: &str, status: &str) {
        let (step_kind, icon) = style_of(kind);
        let open = Open {
            kind: step_kind,
            label: clip(&self.redactor.scrub(title), STEP_LABEL_CAP),
            icon,
        };
        let state = state_of(status);
        ctx.report_step(self.step(id, &open, state)).await;
        if !state.is_end() {
            self.open.insert(id.to_owned(), open);
        }
    }

    /// OpenCode says how a tool call stands: nothing new, unless it ended or has output.
    async fn tool_call_update(
        &mut self,
        ctx: &ToolCtx,
        id: &str,
        status: &str,
        output: Option<&str>,
    ) {
        let state = state_of(status);
        if !state.is_end() && output.is_none() {
            return;
        }
        let known = match self.open.get(id) {
            Some(open) => Open {
                kind: open.kind,
                label: open.label.clone(),
                icon: open.icon,
            },
            None => Open {
                kind: StepKind::Tool,
                label: format!("tool call {id}"),
                icon: None,
            },
        };
        let mut step = self.step(id, &known, state);
        if let Some(output) = output {
            let output = clip(&self.redactor.scrub(output), STEP_DETAIL_CAP);
            if !output.is_empty() {
                step = step.with_detail(output);
            }
        }
        ctx.report_step(step).await;
        if state.is_end() {
            self.open.remove(id);
        }
    }

    /// The turn is over (or was stopped): the tool calls that never said they ended end `state`.
    async fn close(&mut self, ctx: &ToolCtx, state: StepState) {
        let mut open: Vec<(String, Open)> = self.open.drain().collect();
        open.sort_by(|a, b| a.0.cmp(&b.0));
        for (id, open) in open {
            ctx.report_step(self.step(&id, &open, state)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use adam_runtime::{CollectingSink, RunEvent};

    use super::*;

    #[test]
    fn tail_and_clip_respect_char_boundaries() {
        assert_eq!(tail("héllo wörld", 6), "wörld");
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(clip("  hello  ", 10), "hello");
        assert_eq!(clip("abcdef", 3), "abc...");
    }

    #[test]
    fn an_acp_kind_is_a_step_kind_and_the_icon_of_its_name() {
        assert_eq!(
            style_of("execute"),
            (StepKind::Command, Some(StepIcon::Execute))
        );
        assert_eq!(style_of("edit"), (StepKind::Tool, Some(StepIcon::Edit)));
        for (kind, icon) in [
            ("read", StepIcon::Read),
            ("delete", StepIcon::Delete),
            ("move", StepIcon::Move),
            ("search", StepIcon::Search),
            ("think", StepIcon::Think),
            ("fetch", StepIcon::Fetch),
        ] {
            assert_eq!(style_of(kind), (StepKind::Tool, Some(icon)), "{kind}");
        }
        // ACP kinds the contract has no picture for.
        assert_eq!(style_of("switch_mode"), (StepKind::Tool, None));
        assert_eq!(style_of("other"), (StepKind::Tool, None));
        assert_eq!(style_of(""), (StepKind::Tool, None));
    }

    #[test]
    fn an_acp_status_is_running_until_it_completes_or_fails() {
        assert_eq!(state_of("pending"), StepState::Running);
        assert_eq!(state_of("in_progress"), StepState::Running);
        assert_eq!(state_of("completed"), StepState::Completed);
        assert_eq!(state_of("failed"), StepState::Failed);
        assert_eq!(state_of("something new"), StepState::Running);
    }

    /// The steps `Children` reports for `calls`, with the events of the call's own step left out.
    fn steps_of(sink: &CollectingSink) -> Vec<StepEvent> {
        sink.events()
            .into_iter()
            .filter_map(|e| match e.event {
                RunEvent::Step(step) => Some(step),
                _ => None,
            })
            .collect()
    }

    fn rig(secret: &str) -> (ToolCtx, CollectingSink, Redactor) {
        let sink = CollectingSink::new();
        let ctx = ToolCtx::detached("delegate_to_opencode", "c2", Arc::new(sink.clone()));
        (ctx, sink, Redactor::new([secret.to_owned()]))
    }

    #[tokio::test]
    async fn a_tool_call_is_a_child_step_that_its_updates_move_and_end() {
        let (ctx, sink, redactor) = rig("hunter2-hunter2");
        let mut children = Children::new(ctx.call_id(), &redactor);
        children
            .tool_call(&ctx, "tc-1", "npm test", "execute", "pending")
            .await;
        // Nothing new: no report.
        children
            .tool_call_update(&ctx, "tc-1", "in_progress", None)
            .await;
        // Output while it runs is an update; the end carries what it printed.
        children
            .tool_call_update(&ctx, "tc-1", "in_progress", Some("12 passed"))
            .await;
        children
            .tool_call_update(&ctx, "tc-1", "failed", Some("1 failed"))
            .await;
        // An update for a call that was never announced still ends something, with a label of its own.
        children
            .tool_call_update(&ctx, "tc-9", "completed", None)
            .await;

        let under = "tool:c2";
        let command = |state| {
            StepEvent::new("acp:c2:tc-1", StepKind::Command, "npm test", state)
                .under(under)
                .with_icon(StepIcon::Execute)
        };
        assert_eq!(
            steps_of(&sink),
            [
                command(StepState::Running),
                command(StepState::Running).with_detail("12 passed"),
                command(StepState::Failed).with_detail("1 failed"),
                StepEvent::new(
                    "acp:c2:tc-9",
                    StepKind::Tool,
                    "tool call tc-9",
                    StepState::Completed
                )
                .under(under),
            ]
        );
        assert!(children.open.is_empty());
    }

    #[tokio::test]
    async fn what_opencode_says_is_scrubbed_and_cut_before_it_is_shown() {
        let secret = "hunter2-hunter2";
        let (ctx, sink, redactor) = rig(secret);
        let mut children = Children::new(ctx.call_id(), &redactor);
        children
            .tool_call(
                &ctx,
                "tc-1",
                &format!("curl -H 'Token: {secret}' {}", "x".repeat(400)),
                "execute",
                "pending",
            )
            .await;
        children
            .tool_call_update(
                &ctx,
                "tc-1",
                "completed",
                Some(&format!("{secret} {}", "y".repeat(600))),
            )
            .await;
        let steps = steps_of(&sink);
        for step in &steps {
            assert!(!format!("{step:?}").contains(secret), "{step:?}");
        }
        assert!(
            steps[0].label.starts_with("curl -H 'Token: [redacted]"),
            "{}",
            steps[0].label
        );
        assert!(
            steps[0].label.chars().count() <= STEP_LABEL_CAP + 3,
            "{}",
            steps[0].label.len()
        );
        let detail = steps[1].detail.as_deref().unwrap();
        assert!(detail.starts_with("[redacted] yyy"), "{detail}");
        assert!(
            detail.chars().count() <= STEP_DETAIL_CAP + 3,
            "{}",
            detail.len()
        );
    }

    #[tokio::test]
    async fn a_tool_call_that_never_ended_ends_when_the_turn_does() {
        let (ctx, sink, redactor) = rig("hunter2-hunter2");
        let mut children = Children::new(ctx.call_id(), &redactor);
        children
            .tool_call(&ctx, "b", "read b", "read", "in_progress")
            .await;
        children
            .tool_call(&ctx, "a", "read a", "read", "in_progress")
            .await;
        children
            .tool_call(&ctx, "c", "read c", "read", "completed")
            .await;
        children.close(&ctx, StepState::Canceled).await;
        let steps = steps_of(&sink);
        let states: Vec<(&str, StepState)> =
            steps.iter().map(|s| (s.id.as_str(), s.state)).collect();
        assert_eq!(
            states,
            [
                ("acp:c2:b", StepState::Running),
                ("acp:c2:a", StepState::Running),
                // A call that is over when it is announced is never open.
                ("acp:c2:c", StepState::Completed),
                // The rest end canceled, in a stable order.
                ("acp:c2:a", StepState::Canceled),
                ("acp:c2:b", StepState::Canceled),
            ]
        );
        children.close(&ctx, StepState::Canceled).await;
        assert_eq!(
            steps_of(&sink).len(),
            5,
            "closing twice reports nothing more"
        );
    }
}
