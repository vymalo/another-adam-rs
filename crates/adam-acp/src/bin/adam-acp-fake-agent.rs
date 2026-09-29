//! A scripted ACP agent over stdio, for tests. Not for production use.
//!
//! Build: `cargo build -p adam-acp --bin adam-acp-fake-agent`; the binary lands
//! next to the other build outputs (`target/<profile>/adam-acp-fake-agent`).
//! Integration tests inside `adam-acp` get it as
//! `env!("CARGO_BIN_EXE_adam-acp-fake-agent")`.
//!
//! The scenario is chosen with `FAKE_ACP_SCENARIO` (default `script`). All
//! scenarios report what the client answered as `agent_message_chunk` text, so
//! a test sees it in the update stream.
//!
//! | scenario | behaviour on `session/prompt` |
//! |---|---|
//! | `script` | thought, two text chunks, a plan, a tool call, a permission request (locations: `FAKE_ACP_PERM_LOCATION`, default `<cwd>/inside.txt`), `fs/write_text_file` of `<cwd>/inside.txt`, another write to `FAKE_ACP_OUTSIDE_PATH` (if set), a `terminal/create` probe, tool call update, `end_turn` |
//! | `slow` | one text chunk, then waits for `session/cancel` and ends with `cancelled` |
//! | `crash` | one text chunk, writes to stderr, exits with code 3 |
//! | `write-file` | writes `FAKE_ACP_WRITE_CONTENT` to `FAKE_ACP_WRITE_PATH` (relative paths are joined to the session cwd) through `fs/write_text_file`, then `end_turn`. With `FAKE_ACP_WRITE_PERMISSION=1` it asks permission first |
//! | `hang-init` | never answers `initialize` |
//!
//! `FAKE_ACP_PID_FILE`, if set, receives the process id at `initialize`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{Agent, Client, ConnectionTo, Error, Stdio};
use tokio::sync::Notify;

#[derive(Default)]
struct State {
    cwd: Option<PathBuf>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn scenario() -> String {
    env("FAKE_ACP_SCENARIO").unwrap_or_else(|| "script".to_owned())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let state = Arc::new(Mutex::new(State::default()));
    let cancel = Arc::new(Notify::new());
    let (s_new, s_prompt, c_cancel, c_prompt) = (state.clone(), state, cancel.clone(), cancel);

    Agent
        .builder()
        .name("adam-acp-fake-agent")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                if let Some(file) = env("FAKE_ACP_PID_FILE") {
                    let _ = std::fs::write(file, std::process::id().to_string());
                }
                if scenario() == "hang-init" {
                    std::future::pending::<()>().await;
                }
                responder.respond(
                    InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(AgentCapabilities::new())
                        .agent_info(Implementation::new("fake-agent", "0.1.0")),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, _cx| {
                if let Ok(mut st) = s_new.lock() {
                    st.cwd = Some(req.cwd);
                }
                responder.respond(NewSessionResponse::new("sess-1"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_n: CancelNotification, _cx| {
                c_cancel.notify_waiters();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, cx: ConnectionTo<Client>| {
                // Anything that awaits the client must not run in the handler:
                // it would block the dispatch loop that delivers the answer.
                let (state, cancel, cx2) = (s_prompt.clone(), c_prompt.clone(), cx.clone());
                cx.spawn(async move {
                    responder.respond_with_result(run_turn(req, &cx2, state, cancel).await)
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await
}

fn update(cx: &ConnectionTo<Client>, sid: &SessionId, u: SessionUpdate) -> Result<(), Error> {
    cx.send_notification(SessionNotification::new(sid.clone(), u))
}

fn say(cx: &ConnectionTo<Client>, sid: &SessionId, text: impl Into<String>) -> Result<(), Error> {
    update(
        cx,
        sid,
        SessionUpdate::AgentMessageChunk(ContentChunk::new(text.into().into())),
    )
}

fn tool_status(id: &str, status: ToolCallStatus) -> SessionUpdate {
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        id.to_owned(),
        ToolCallUpdateFields::new().status(status),
    ))
}

/// Ask for permission; the result is the chosen option id (None if cancelled).
async fn ask(
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
    tool_id: &str,
    title: &str,
    location: &Path,
) -> Result<Option<String>, Error> {
    let resp = cx
        .send_request(RequestPermissionRequest::new(
            sid.clone(),
            ToolCallUpdate::new(
                tool_id.to_owned(),
                ToolCallUpdateFields::new()
                    .title(title)
                    .kind(ToolKind::Edit)
                    .locations(vec![ToolCallLocation::new(location)]),
            ),
            vec![
                PermissionOption::new("once", "Allow once", PermissionOptionKind::AllowOnce),
                PermissionOption::new("always", "Always allow", PermissionOptionKind::AllowAlways),
                PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
            ],
        ))
        .block_task()
        .await?;
    Ok(match resp.outcome {
        RequestPermissionOutcome::Selected(s) => Some(s.option_id.to_string()),
        _ => None,
    })
}

async fn write(cx: &ConnectionTo<Client>, sid: &SessionId, path: &Path, content: &str) -> String {
    match cx
        .send_request(WriteTextFileRequest::new(sid.clone(), path, content))
        .block_task()
        .await
    {
        Ok(_) => "ok".to_owned(),
        Err(e) => format!("error: {}", e.message),
    }
}

async fn run_turn(
    req: PromptRequest,
    cx: &ConnectionTo<Client>,
    state: Arc<Mutex<State>>,
    cancel: Arc<Notify>,
) -> Result<PromptResponse, Error> {
    let sid = req.session_id.clone();
    let cwd = state
        .lock()
        .ok()
        .and_then(|s| s.cwd.clone())
        .unwrap_or_default();
    match scenario().as_str() {
        "slow" => {
            let cancelled = cancel.notified(); // register before announcing
            say(cx, &sid, "working")?;
            cancelled.await;
            Ok(PromptResponse::new(StopReason::Cancelled))
        }
        "crash" => {
            say(cx, &sid, "about to crash")?;
            tokio::time::sleep(Duration::from_millis(200)).await; // let the chunk flush
            eprintln!("fake agent: fatal: simulated crash");
            std::process::exit(3);
        }
        "write-file" => write_file_turn(cx, &sid, &cwd).await,
        _ => script_turn(cx, &sid, &cwd).await,
    }
}

async fn write_file_turn(
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
    cwd: &Path,
) -> Result<PromptResponse, Error> {
    let path = cwd.join(env("FAKE_ACP_WRITE_PATH").unwrap_or_else(|| "hello.txt".to_owned()));
    let content = env("FAKE_ACP_WRITE_CONTENT").unwrap_or_else(|| "hi".to_owned());
    let title = format!("Write {}", path.display());
    update(
        cx,
        sid,
        SessionUpdate::ToolCall(
            ToolCall::new("tc-1", title.as_str())
                .kind(ToolKind::Edit)
                .status(ToolCallStatus::Pending)
                .locations(vec![ToolCallLocation::new(path.as_path())]),
        ),
    )?;
    if env("FAKE_ACP_WRITE_PERMISSION").as_deref() == Some("1") {
        let chosen = ask(cx, sid, "tc-1", &title, &path).await?;
        if chosen.as_deref() != Some("once") {
            say(cx, sid, "permission refused")?;
            update(cx, sid, tool_status("tc-1", ToolCallStatus::Failed))?;
            return Ok(PromptResponse::new(StopReason::EndTurn));
        }
    }
    update(cx, sid, tool_status("tc-1", ToolCallStatus::InProgress))?;
    let result = write(cx, sid, &path, &content).await;
    say(cx, sid, format!("write: {result}"))?;
    let status = if result == "ok" {
        ToolCallStatus::Completed
    } else {
        ToolCallStatus::Failed
    };
    update(cx, sid, tool_status("tc-1", status))?;
    Ok(PromptResponse::new(StopReason::EndTurn))
}

async fn script_turn(
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
    cwd: &Path,
) -> Result<PromptResponse, Error> {
    update(
        cx,
        sid,
        SessionUpdate::AgentThoughtChunk(ContentChunk::new("thinking".into())),
    )?;
    say(cx, sid, "hello ")?;
    say(cx, sid, "world")?;
    update(
        cx,
        sid,
        SessionUpdate::Plan(Plan::new(vec![
            PlanEntry::new(
                "write inside.txt",
                PlanEntryPriority::High,
                PlanEntryStatus::InProgress,
            ),
            PlanEntry::new(
                "write outside",
                PlanEntryPriority::Low,
                PlanEntryStatus::Pending,
            ),
        ])),
    )?;

    let inside = cwd.join("inside.txt");
    let location = env("FAKE_ACP_PERM_LOCATION").map_or_else(|| inside.clone(), PathBuf::from);
    update(
        cx,
        sid,
        SessionUpdate::ToolCall(
            ToolCall::new("tc-1", "Write inside.txt")
                .kind(ToolKind::Edit)
                .status(ToolCallStatus::Pending)
                .locations(vec![ToolCallLocation::new(location.as_path())]),
        ),
    )?;

    let chosen = ask(cx, sid, "tc-1", "Write inside.txt", &location).await?;
    say(
        cx,
        sid,
        format!("permission: {}", chosen.as_deref().unwrap_or("cancelled")),
    )?;
    if chosen.as_deref() != Some("once") {
        update(cx, sid, tool_status("tc-1", ToolCallStatus::Failed))?;
        return Ok(PromptResponse::new(StopReason::EndTurn));
    }
    update(cx, sid, tool_status("tc-1", ToolCallStatus::InProgress))?;

    let r = write(cx, sid, &inside, "inside").await;
    say(cx, sid, format!("write inside: {r}"))?;
    if let Some(outside) = env("FAKE_ACP_OUTSIDE_PATH") {
        let r = write(cx, sid, Path::new(&outside), "outside").await;
        say(cx, sid, format!("write outside: {r}"))?;
    }
    // A request the client has no feature for must be answered, not hang.
    let t = tokio::time::timeout(
        Duration::from_secs(5),
        cx.send_request(CreateTerminalRequest::new(sid.clone(), "ls"))
            .block_task(),
    )
    .await;
    let probe = match t {
        Ok(Ok(_)) => "created".to_owned(),
        Ok(Err(e)) => format!("error {}", i32::from(e.code)),
        Err(_) => "no answer".to_owned(),
    };
    say(cx, sid, format!("terminal/create: {probe}"))?;

    update(
        cx,
        sid,
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "tc-1",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec!["done".into()]),
        )),
    )?;
    Ok(PromptResponse::new(StopReason::EndTurn))
}
