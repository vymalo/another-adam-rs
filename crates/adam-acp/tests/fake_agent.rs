//! End-to-end tests against the scripted `adam-acp-fake-agent` binary.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use adam_acp::{
    AcpClient, AcpCommand, AcpError, AcpOptions, AcpUpdate, ClientPolicy, PermissionKind,
    PermissionMode, PlanEntry, Session, StaticPrompt,
};
use futures::StreamExt as _;
use futures::stream::BoxStream;

const FAKE: &str = env!("CARGO_BIN_EXE_adam-acp-fake-agent");

async fn within<T>(fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .expect("test step hung")
}

fn fake(cwd: &Path, scenario: &str) -> AcpCommand {
    AcpCommand::new(FAKE, cwd).env("FAKE_ACP_SCENARIO", scenario)
}

fn root() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

async fn collect(
    mut turn: BoxStream<'static, Result<AcpUpdate, AcpError>>,
) -> Vec<Result<AcpUpdate, AcpError>> {
    within(async {
        let mut items = Vec::new();
        while let Some(item) = turn.next().await {
            items.push(item);
        }
        items
    })
    .await
}

fn text(s: &str) -> AcpUpdate {
    AcpUpdate::AgentText(s.to_owned())
}

fn tool_update(status: &str, output: Option<&str>) -> AcpUpdate {
    AcpUpdate::ToolCallUpdate {
        id: "tc-1".into(),
        status: status.into(),
        output: output.map(str::to_owned),
    }
}

fn ended(reason: &str) -> AcpUpdate {
    AcpUpdate::TurnEnded {
        stop_reason: reason.into(),
    }
}

/// The updates every `script` turn starts with, up to the permission request.
fn script_prefix() -> Vec<AcpUpdate> {
    vec![
        AcpUpdate::Thought("thinking".into()),
        text("hello "),
        text("world"),
        AcpUpdate::Plan(vec![
            PlanEntry {
                content: "write inside.txt".into(),
                priority: "high".into(),
                status: "in_progress".into(),
            },
            PlanEntry {
                content: "write outside".into(),
                priority: "low".into(),
                status: "pending".into(),
            },
        ]),
        AcpUpdate::ToolCall {
            id: "tc-1".into(),
            title: "Write inside.txt".into(),
            kind: "edit".into(),
            status: "pending".into(),
        },
    ]
}

async fn start(cmd: AcpCommand, policy: ClientPolicy, cwd: &Path) -> (AcpClient, Session) {
    let client = within(AcpClient::spawn(cmd, policy)).await.expect("spawn");
    let session = within(client.new_session(cwd, vec![]))
        .await
        .expect("new_session");
    (client, session)
}

fn ok(items: Vec<Result<AcpUpdate, AcpError>>) -> Vec<AcpUpdate> {
    items.into_iter().map(|i| i.expect("turn item")).collect()
}

#[tokio::test]
async fn scripted_turn_streams_exact_updates_and_enforces_fs_root() {
    let dir = root();
    let outside_dir = root();
    let outside = outside_dir.path().join("outside.txt");
    let cmd =
        fake(dir.path(), "script").env("FAKE_ACP_OUTSIDE_PATH", outside.display().to_string());
    let (client, session) = start(cmd, ClientPolicy::new(dir.path()), dir.path()).await;

    assert_eq!(client.agent_info().name, "fake-agent");
    assert_eq!(client.agent_info().version, "0.1.0");
    assert_eq!(client.agent_info().protocol_version, 1);
    assert_eq!(session.id(), "sess-1");

    let items = ok(collect(session.prompt("go".into())).await);

    let mut expected = script_prefix();
    expected.extend([
        text("permission: once"),
        tool_update("in_progress", None),
        text("write inside: ok"),
        text("<outside write result>"),
        text("terminal/create: error -32601"),
        tool_update("completed", Some("done")),
        ended("end_turn"),
    ]);
    let denied_at = expected.len() - 4;
    // The denial message names the path and the reason; check it separately.
    match &items[denied_at] {
        AcpUpdate::AgentText(t) => {
            assert!(t.starts_with("write outside: error: "), "{t}");
            assert!(t.contains("denied"), "{t}");
            assert!(t.contains("outside the allowed root"), "{t}");
            assert!(t.contains(&outside.display().to_string()), "{t}");
        }
        other => panic!("unexpected {other:?}"),
    }
    let mut items = items;
    items[denied_at] = expected[denied_at].clone();
    assert_eq!(items, expected);

    assert_eq!(
        std::fs::read_to_string(dir.path().join("inside.txt")).unwrap(),
        "inside"
    );
    assert!(
        !outside.exists(),
        "the outside write must not have happened"
    );

    // Same session, second turn: the slot was released; the repeated write is idempotent.
    let items = ok(collect(session.prompt("again".into())).await);
    assert_eq!(items.last(), Some(&ended("end_turn")));
    assert!(items.contains(&text("write inside: ok")));

    within(client.shutdown()).await.expect("graceful shutdown");
}

async fn script_texts(
    policy: ClientPolicy,
    extra_env: &[(&str, String)],
) -> (Vec<AcpUpdate>, tempfile::TempDir) {
    let dir = root();
    let mut cmd = fake(dir.path(), "script");
    for (k, v) in extra_env {
        cmd = cmd.env(*k, v.clone());
    }
    let policy = ClientPolicy {
        fs_root: dir.path().to_owned(),
        ..policy
    };
    let (client, session) = start(cmd, policy, dir.path()).await;
    let items = ok(collect(session.prompt("go".into())).await);
    drop((session, client));
    (items, dir)
}

fn rejected_sequence(choice: &str) -> Vec<AcpUpdate> {
    let mut expected = script_prefix();
    expected.extend([
        text(&format!("permission: {choice}")),
        tool_update("failed", None),
        ended("end_turn"),
    ]);
    expected
}

#[tokio::test]
async fn permission_deny_all_rejects_and_nothing_is_written() {
    let policy = ClientPolicy::new("/").with_permission(PermissionMode::DenyAll);
    let (items, dir) = script_texts(policy, &[]).await;
    let inside = dir.path().join("inside.txt");
    assert_eq!(items, rejected_sequence("reject"));
    assert!(!inside.exists());
}

#[tokio::test]
async fn permission_allow_within_root_rejects_locations_outside_the_root() {
    let elsewhere = root();
    let location = elsewhere.path().join("x.txt").display().to_string();
    let (items, dir) = script_texts(
        ClientPolicy::new("/"),
        &[("FAKE_ACP_PERM_LOCATION", location)],
    )
    .await;
    let inside = dir.path().join("inside.txt");
    assert_eq!(items, rejected_sequence("reject"));
    assert!(!inside.exists());
}

#[tokio::test]
async fn permission_ask_delegates_to_the_prompt() {
    for (kind, chosen) in [
        (PermissionKind::AllowOnce, "once"),
        (PermissionKind::RejectOnce, "reject"),
        // An always-option is passed through only if the prompt picks it.
        (PermissionKind::AllowAlways, "always"),
    ] {
        let prompt = StaticPrompt::choosing(kind);
        let policy = ClientPolicy::new("/").with_permission(PermissionMode::Ask(prompt.clone()));
        let (items, dir) = script_texts(policy, &[]).await;
        let inside = dir.path().join("inside.txt");
        assert!(
            items.contains(&text(&format!("permission: {chosen}"))),
            "{chosen}: {items:?}"
        );
        assert_eq!(inside.exists(), chosen == "once", "{chosen}");

        let seen = prompt.requests();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].session_id, "sess-1");
        assert_eq!(seen[0].tool_call_id, "tc-1");
        assert_eq!(seen[0].title.as_deref(), Some("Write inside.txt"));
        assert_eq!(seen[0].kind.as_deref(), Some("edit"));
        assert_eq!(seen[0].locations.len(), 1);
        assert!(seen[0].locations[0].ends_with("inside.txt"));
        assert_eq!(seen[0].options.len(), 3);
    }

    let prompt = StaticPrompt::cancelling();
    let policy = ClientPolicy::new("/").with_permission(PermissionMode::Ask(prompt));
    let (items, _) = script_texts(policy, &[]).await;
    assert_eq!(items, rejected_sequence("cancelled"));
}

#[tokio::test]
async fn cancel_ends_a_long_turn_with_cancelled() {
    let dir = root();
    let (client, session) = start(
        fake(dir.path(), "slow"),
        ClientPolicy::new(dir.path()),
        dir.path(),
    )
    .await;
    let mut turn = session.prompt("take your time".into());
    let first = within(turn.next()).await.expect("first item").expect("ok");
    assert_eq!(first, text("working"));
    within(session.cancel()).await.expect("cancel");
    let rest = collect(turn).await;
    assert_eq!(ok(rest), vec![ended("cancelled")]);
    within(client.shutdown()).await.expect("shutdown");
}

#[tokio::test]
async fn idle_turn_times_out_and_is_cancelled() {
    let dir = root();
    let opts = AcpOptions {
        turn_idle_timeout: Some(Duration::from_millis(300)),
        ..AcpOptions::default()
    };
    let client = within(AcpClient::spawn_with(
        fake(dir.path(), "slow"),
        ClientPolicy::new(dir.path()),
        opts,
    ))
    .await
    .unwrap();
    let session = client.new_session(dir.path(), vec![]).await.unwrap();
    let items = collect(session.prompt("hang".into())).await;
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0].as_ref().unwrap(), &text("working"));
    let err = items[1].as_ref().unwrap_err();
    assert!(matches!(err, AcpError::Timeout { .. }), "{err:?}");
    assert!(err.is_retryable());
}

#[tokio::test]
async fn crash_mid_turn_yields_exited_with_stderr_tail() {
    let dir = root();
    let (client, session) = start(
        fake(dir.path(), "crash"),
        ClientPolicy::new(dir.path()),
        dir.path(),
    )
    .await;
    let items = collect(session.prompt("boom".into())).await;
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0].as_ref().unwrap(), &text("about to crash"));
    match items[1].as_ref().unwrap_err() {
        e @ AcpError::Exited { code, stderr_tail } => {
            assert_eq!(*code, Some(3));
            assert!(stderr_tail.contains("simulated crash"), "{stderr_tail:?}");
            assert!(e.is_retryable());
            assert!(e.to_string().contains("simulated crash"));
        }
        other => panic!("expected Exited, got {other:?}"),
    }
    // The connection is gone: further calls fail fast, with the same cause.
    let err = within(client.new_session(dir.path(), vec![]))
        .await
        .unwrap_err();
    assert!(
        matches!(err, AcpError::Exited { code: Some(3), .. }),
        "{err:?}"
    );
    let err = within(session.cancel()).await.unwrap_err();
    assert!(
        matches!(err, AcpError::Exited { code: Some(3), .. }),
        "{err:?}"
    );
    let err = within(client.shutdown()).await.unwrap_err();
    assert!(
        matches!(err, AcpError::Exited { code: Some(3), .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn write_file_scenario_writes_through_the_client() {
    let dir = root();
    let cmd = fake(dir.path(), "write-file")
        .env("FAKE_ACP_WRITE_PATH", "sub/dir/hello.txt")
        .env("FAKE_ACP_WRITE_CONTENT", "hi")
        .env("FAKE_ACP_WRITE_PERMISSION", "1");
    let (_client, session) = start(cmd, ClientPolicy::new(dir.path()), dir.path()).await;
    let items = ok(collect(session.prompt("write hello".into())).await);
    assert!(items.contains(&text("write: ok")), "{items:?}");
    assert_eq!(items.last(), Some(&ended("end_turn")));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("sub/dir/hello.txt")).unwrap(),
        "hi"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_escape_is_denied_through_the_protocol() {
    let dir = root();
    let elsewhere = root();
    std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("link")).unwrap();
    let cmd = fake(dir.path(), "write-file").env("FAKE_ACP_WRITE_PATH", "link/escaped.txt");
    let (_client, session) = start(cmd, ClientPolicy::new(dir.path()), dir.path()).await;
    let items = ok(collect(session.prompt("write".into())).await);
    let msg = items
        .iter()
        .find_map(|u| match u {
            AcpUpdate::AgentText(t) if t.starts_with("write: error") => Some(t.clone()),
            _ => None,
        })
        .expect("write result");
    assert!(msg.contains("outside the allowed root"), "{msg}");
    assert!(!elsewhere.path().join("escaped.txt").exists());
}

#[cfg(target_os = "linux")]
fn process_gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        // A zombie is dead; it only waits to be reaped.
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z')),
    }
}

#[cfg(target_os = "linux")]
async fn wait_gone(pid: u32) -> bool {
    for _ in 0..100 {
        if process_gone(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[cfg(target_os = "linux")]
fn read_pid(file: &Path) -> u32 {
    std::fs::read_to_string(file)
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid")
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn dropping_the_client_kills_the_child() {
    let dir = root();
    let pid_file = dir.path().join("agent.pid");
    let cmd = fake(dir.path(), "slow").env("FAKE_ACP_PID_FILE", pid_file.display().to_string());
    let (client, session) = start(cmd, ClientPolicy::new(dir.path()), dir.path()).await;
    let pid = read_pid(&pid_file);
    assert!(!process_gone(pid), "agent should be running");
    // Mid-turn, to make sure a busy agent dies too.
    let mut turn = session.prompt("busy".into());
    assert_eq!(within(turn.next()).await.unwrap().unwrap(), text("working"));
    drop(turn);
    drop(session);
    drop(client);
    assert!(wait_gone(pid).await, "child {pid} survived the client");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn graceful_shutdown_lets_the_child_exit() {
    let dir = root();
    let pid_file = dir.path().join("agent.pid");
    let cmd = fake(dir.path(), "script").env("FAKE_ACP_PID_FILE", pid_file.display().to_string());
    let client = within(AcpClient::spawn(cmd, ClientPolicy::new(dir.path())))
        .await
        .unwrap();
    let pid = read_pid(&pid_file);
    let started = std::time::Instant::now();
    within(client.shutdown()).await.expect("shutdown");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "should not have needed the kill"
    );
    assert!(wait_gone(pid).await);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn initialize_timeout_kills_the_child() {
    let dir = root();
    let pid_file = dir.path().join("agent.pid");
    let cmd =
        fake(dir.path(), "hang-init").env("FAKE_ACP_PID_FILE", pid_file.display().to_string());
    let opts = AcpOptions {
        request_timeout: Duration::from_millis(500),
        ..AcpOptions::default()
    };
    let err = within(AcpClient::spawn_with(
        cmd,
        ClientPolicy::new(dir.path()),
        opts,
    ))
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            AcpError::Timeout {
                operation: "initialize",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(wait_gone(read_pid(&pid_file)).await);
}

#[tokio::test]
async fn spawn_reports_bad_program_and_bad_config() {
    let dir = root();
    let err = AcpClient::spawn(
        AcpCommand::new("adam-acp-no-such-program", dir.path()),
        ClientPolicy::new(dir.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, AcpError::Spawn { .. }), "{err:?}");
    assert!(!err.is_retryable());

    let err = AcpClient::spawn(
        fake(dir.path(), "script"),
        ClientPolicy::new(dir.path().join("missing")),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, AcpError::Config(_)), "{err:?}");

    let mut policy = ClientPolicy::new(dir.path());
    policy.terminal = true;
    let err = AcpClient::spawn(fake(dir.path(), "script"), policy)
        .await
        .unwrap_err();
    assert!(matches!(err, AcpError::Config(_)), "{err:?}");
}

#[tokio::test]
async fn a_second_prompt_while_one_runs_is_rejected() {
    let dir = root();
    let (client, session) = start(
        fake(dir.path(), "slow"),
        ClientPolicy::new(dir.path()),
        dir.path(),
    )
    .await;
    let mut first = session.prompt("one".into());
    assert_eq!(
        within(first.next()).await.unwrap().unwrap(),
        text("working")
    );
    let second = collect(session.prompt("two".into())).await;
    assert!(
        matches!(second.as_slice(), [Err(AcpError::TurnInProgress)]),
        "{second:?}"
    );
    within(session.cancel()).await.unwrap();
    assert_eq!(ok(collect(first).await), vec![ended("cancelled")]);
    drop(client);
}
