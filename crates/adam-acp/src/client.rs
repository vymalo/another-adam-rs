//! The client: process supervision, the JSON-RPC connection, sessions and turns.
//!
//! # Structure
//!
//! ```text
//!  AcpClient / Session ──Cmd──▶ connection task (owns the SDK connection)
//!        ▲                            │ handlers: session/update, fs/*, permission
//!        └──── per-turn channel ◀─────┘
//!  supervisor task: owns the child, records its exit, fails pending turns
//! ```
//!
//! Handlers run on the SDK's single dispatch loop, so none of them awaits the
//! peer; anything slow (a human answering a permission prompt) is spawned.
//! Updates and the turn's end travel down **one** channel per turn, which is
//! what guarantees every update arrives before `TurnEnded`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo, Responder};
use futures::StreamExt as _;
use futures::stream::BoxStream;
use tokio::io::AsyncBufReadExt as _;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tokio_util::sync::CancellationToken;

use crate::command::AcpCommand;
use crate::error::AcpError;
use crate::guard::{FsError, FsGuard};
use crate::policy::{
    self, ClientPolicy, PermissionChoice, PermissionDecision, PermissionKind, PermissionMode,
    PermissionRequest,
};
use crate::update::{AcpUpdate, McpServerSpec, ToolStatuses, map_update, wire_str};

/// Stderr kept for [`AcpError::Exited`]: at most this many lines...
const STDERR_TAIL_LINES: usize = 200;
/// ...and this many bytes.
const STDERR_TAIL_BYTES: usize = 64 * 1024;
/// A single stderr line is truncated to this many bytes in the tail.
const STDERR_LINE_MAX: usize = 4096;
/// How long [`AcpClient::kill`] waits for the killed agent to be reaped.
const KILL_WAIT: Duration = Duration::from_secs(10);
/// JSON-RPC code agents use for "authenticate first".
const AUTH_REQUIRED_CODE: i32 = -32000;

/// Timeouts of a client. The defaults suit an interactive coding agent.
#[derive(Debug, Clone)]
pub struct AcpOptions {
    /// Limit for `initialize` and for `session/new` (agents start MCP
    /// servers there). Default 60 s.
    pub request_timeout: Duration,
    /// A turn fails with [`AcpError::Timeout`] (and is cancelled) when the
    /// agent produces no update for this long. Paused while a permission
    /// prompt is pending. `None` disables it. Default 10 minutes.
    pub turn_idle_timeout: Option<Duration>,
    /// How long a graceful [`AcpClient::shutdown`] waits for the child to
    /// exit after its stdin closed, before killing it. Default 5 s.
    pub shutdown_grace: Duration,
}

impl Default for AcpOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(60),
            turn_idle_timeout: Some(Duration::from_secs(600)),
            shutdown_grace: Duration::from_secs(5),
        }
    }
}

/// What the agent said about itself in `initialize`; handy for logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    /// Agent name (`"unknown"` if it sent none).
    pub name: String,
    /// Human-friendly name, if any.
    pub title: Option<String>,
    /// Agent version (empty if not sent).
    pub version: String,
    /// Negotiated ACP protocol version.
    pub protocol_version: u16,
    /// Supports `session/load`.
    pub load_session: bool,
    /// Accepts image content in prompts.
    pub image_prompts: bool,
    /// Accepts audio content in prompts.
    pub audio_prompts: bool,
    /// Accepts embedded resources in prompts.
    pub embedded_context: bool,
    /// Can connect to MCP servers over HTTP.
    pub mcp_http: bool,
    /// Can connect to MCP servers over SSE.
    pub mcp_sse: bool,
    /// Ids of the authentication methods it offers.
    pub auth_methods: Vec<String>,
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

type TurnItem = Result<AcpUpdate, AcpError>;

struct TurnSlot {
    tx: mpsc::UnboundedSender<TurnItem>,
    cancel: CancellationToken,
    tools: ToolStatuses,
}

#[derive(Debug)]
struct ExitInfo {
    code: Option<i32>,
    stderr_tail: String,
    /// The exit was asked for (shutdown or drop), not a crash.
    expected: bool,
}

impl ExitInfo {
    fn to_error(&self) -> AcpError {
        AcpError::Exited {
            code: self.code,
            stderr_tail: self.stderr_tail.clone(),
        }
    }
}

#[derive(Default)]
struct StderrTail {
    lines: VecDeque<String>,
    bytes: usize,
}

impl StderrTail {
    fn push(&mut self, line: &str) {
        let mut end = line.len().min(STDERR_LINE_MAX);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        self.bytes += end + 1;
        self.lines.push_back(line[..end].to_owned());
        while self.lines.len() > STDERR_TAIL_LINES || self.bytes > STDERR_TAIL_BYTES {
            match self.lines.pop_front() {
                Some(old) => self.bytes -= old.len() + 1,
                None => break,
            }
        }
    }

    fn snapshot(&self) -> String {
        self.lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

struct Shared {
    turns: Mutex<HashMap<String, TurnSlot>>,
    stderr: Mutex<StderrTail>,
    exit: watch::Sender<Option<Arc<ExitInfo>>>,
    /// Ask-mode permission prompts currently waiting for an answer.
    pending_permissions: AtomicUsize,
    shutdown_requested: AtomicBool,
    exit_wait: Duration,
    guard: FsGuard,
    mode: PermissionMode,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn dispatch_update(&self, session_id: &str, update: acp::SessionUpdate) {
        let mut turns = lock(&self.turns);
        if let Some(slot) = turns.get_mut(session_id) {
            if let Some(mapped) = map_update(update, &mut slot.tools) {
                let _ = slot.tx.send(Ok(mapped));
            }
        } else {
            tracing::trace!(session = session_id, "update outside a turn dropped");
        }
    }

    fn finish_turn(&self, session_id: &str, item: TurnItem) {
        if let Some(slot) = lock(&self.turns).remove(session_id) {
            let _ = slot.tx.send(item);
        }
    }

    fn cancel_token(&self, session_id: &str) -> Option<CancellationToken> {
        lock(&self.turns).get(session_id).map(|s| s.cancel.clone())
    }

    fn cancel_turn(&self, session_id: &str) {
        if let Some(t) = self.cancel_token(session_id) {
            t.cancel();
        }
    }

    fn fail_all(&self, err: impl Fn() -> AcpError) {
        let drained: Vec<TurnSlot> = lock(&self.turns).drain().map(|(_, s)| s).collect();
        for slot in drained {
            let _ = slot.tx.send(Err(err()));
        }
    }

    /// The error to report once the connection is gone: the child's exit if
    /// it is (about to be) known, otherwise a plain "closed".
    async fn exited_error(&self) -> AcpError {
        let mut rx = self.exit.subscribe();
        let info = match tokio::time::timeout(self.exit_wait, rx.wait_for(Option::is_some)).await {
            Ok(Ok(v)) => v.clone(),
            _ => None,
        };
        info.map_or(AcpError::Closed, |i| i.to_error())
    }

    async fn map_rpc_error(&self, e: agent_client_protocol::Error) -> AcpError {
        if agent_client_protocol::is_incoming_transport_closed(&e) {
            return self.exited_error().await;
        }
        let code = i32::from(e.code);
        if code == AUTH_REQUIRED_CODE {
            AcpError::AuthRequired(e.message)
        } else {
            AcpError::Rpc {
                code,
                message: e.message,
            }
        }
    }
}

struct PendingPermission(Arc<Shared>);

impl PendingPermission {
    fn new(shared: &Arc<Shared>) -> Self {
        shared.pending_permissions.fetch_add(1, Ordering::SeqCst);
        Self(shared.clone())
    }
}

impl Drop for PendingPermission {
    fn drop(&mut self) {
        self.0.pending_permissions.fetch_sub(1, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Public handles
// ---------------------------------------------------------------------------

enum Cmd {
    NewSession {
        cwd: PathBuf,
        mcp_servers: Vec<McpServerSpec>,
        reply: oneshot::Sender<Result<String, AcpError>>,
    },
    Prompt {
        session_id: String,
        text: String,
        events: mpsc::UnboundedSender<TurnItem>,
    },
    Cancel {
        session_id: String,
    },
    Shutdown,
}

/// State shared by a client and its sessions. When the last handle drops the
/// child is killed.
struct Core {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    shared: Arc<Shared>,
    cancel: CancellationToken,
    opts: AcpOptions,
}

impl Drop for Core {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A running agent process and the connection to it.
///
/// Dropping the last handle (the client and every [`Session`]) kills the
/// child; use [`shutdown`](Self::shutdown) for a graceful exit.
pub struct AcpClient {
    core: Arc<Core>,
    info: AgentInfo,
}

impl std::fmt::Debug for AcpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpClient")
            .field("agent", &self.info)
            .finish_non_exhaustive()
    }
}

impl AcpClient {
    /// Start the agent and run the `initialize` handshake, with default
    /// [`AcpOptions`].
    pub async fn spawn(cmd: AcpCommand, policy: ClientPolicy) -> Result<Self, AcpError> {
        Self::spawn_with(cmd, policy, AcpOptions::default()).await
    }

    /// Like [`spawn`](Self::spawn) with explicit timeouts.
    #[tracing::instrument(name = "acp.spawn", skip_all, fields(program = %cmd.program.display()))]
    pub async fn spawn_with(
        cmd: AcpCommand,
        policy: ClientPolicy,
        opts: AcpOptions,
    ) -> Result<Self, AcpError> {
        if policy.terminal {
            return Err(AcpError::Config(
                "ClientPolicy::terminal is not supported yet (terminal/* is not implemented)"
                    .to_owned(),
            ));
        }
        let guard = FsGuard::new(&policy.fs_root).await?;
        if !cmd.cwd.is_dir() {
            return Err(AcpError::Config(format!(
                "agent working directory {} is not a directory",
                cmd.cwd.display()
            )));
        }
        let program = cmd.resolve_program()?;
        let mut command = tokio::process::Command::new(&program);
        command
            .args(&cmd.args)
            .envs(&cmd.env)
            .current_dir(&cmd.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so that killing the agent can take along
        // what it started (a coding agent runs shell commands, servers...).
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|source| AcpError::Spawn {
            program: cmd.program.display().to_string(),
            source,
        })?;
        tracing::info!(pid = child.id(), program = %program.display(), "agent process started");

        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(AcpError::Spawn {
                program: cmd.program.display().to_string(),
                source: std::io::Error::other("child stdio pipe missing"),
            });
        };

        let (exit_tx, _) = watch::channel(None);
        let shared = Arc::new(Shared {
            turns: Mutex::default(),
            stderr: Mutex::default(),
            exit: exit_tx,
            pending_permissions: AtomicUsize::new(0),
            shutdown_requested: AtomicBool::new(false),
            exit_wait: opts.shutdown_grace + Duration::from_secs(1),
            guard,
            mode: policy.permission,
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        // From here on, dropping `core` (e.g. on an early error return) kills the child.
        let core = Arc::new(Core {
            cmd_tx,
            shared: shared.clone(),
            cancel: cancel.clone(),
            opts: opts.clone(),
        });

        let stderr_task = tokio::spawn(drain_stderr(stderr, shared.clone()));
        let (ready_tx, ready_rx) = oneshot::channel();
        let conn = run_connection(stdin, stdout, shared.clone(), cmd_rx, ready_tx);
        tokio::spawn(supervise(
            child,
            conn,
            stderr_task,
            shared.clone(),
            cancel,
            opts.shutdown_grace,
        ));

        let info = match tokio::time::timeout(opts.request_timeout, ready_rx).await {
            Ok(Ok(Ok(info))) => info,
            Ok(Ok(Err(e))) => return Err(e),
            Ok(Err(_)) => return Err(shared.exited_error().await),
            Err(_) => {
                return Err(AcpError::Timeout {
                    operation: "initialize",
                    after: opts.request_timeout,
                });
            }
        };
        tracing::info!(
            agent = %info.name,
            version = %info.version,
            protocol = info.protocol_version,
            "ACP agent initialized"
        );
        Ok(Self { core, info })
    }

    /// What the agent reported in `initialize`.
    pub fn agent_info(&self) -> &AgentInfo {
        &self.info
    }

    /// Open a session working in `cwd` (made absolute if relative), with the
    /// given MCP servers attached for its lifetime.
    #[tracing::instrument(name = "acp.new_session", skip(self, mcp_servers), fields(cwd = %cwd.display(), mcp = mcp_servers.len()))]
    pub async fn new_session(
        &self,
        cwd: &Path,
        mcp_servers: Vec<McpServerSpec>,
    ) -> Result<Session, AcpError> {
        let cwd = std::path::absolute(cwd)
            .map_err(|e| AcpError::Config(format!("cwd {}: {e}", cwd.display())))?;
        let (reply, rx) = oneshot::channel();
        let shared = &self.core.shared;
        if self
            .core
            .cmd_tx
            .send(Cmd::NewSession {
                cwd,
                mcp_servers,
                reply,
            })
            .is_err()
        {
            return Err(shared.exited_error().await);
        }
        let limit = self.core.opts.request_timeout;
        let id = match tokio::time::timeout(limit, rx).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => return Err(shared.exited_error().await),
            Err(_) => {
                return Err(AcpError::Timeout {
                    operation: "session/new",
                    after: limit,
                });
            }
        };
        tracing::info!(session = %id, "ACP session created");
        Ok(Session {
            id,
            core: self.core.clone(),
        })
    }

    /// Graceful shutdown: close the agent's stdin, give it
    /// [`AcpOptions::shutdown_grace`] to exit, then kill it.
    ///
    /// `Ok` when the agent went away because we asked; [`AcpError::Exited`]
    /// when it had already died.
    #[tracing::instrument(name = "acp.shutdown", skip_all)]
    pub async fn shutdown(self) -> Result<(), AcpError> {
        let core = &self.core;
        core.shared.shutdown_requested.store(true, Ordering::SeqCst);
        let _ = core.cmd_tx.send(Cmd::Shutdown);
        let mut rx = core.shared.exit.subscribe();
        let bound = core.opts.shutdown_grace + Duration::from_secs(3);
        let info = match tokio::time::timeout(bound, rx.wait_for(Option::is_some)).await {
            Ok(Ok(v)) => v.clone(),
            _ => None,
        };
        core.cancel.cancel();
        match info {
            Some(i) if i.expected => Ok(()),
            Some(i) => Err(i.to_error()),
            None => Err(AcpError::Timeout {
                operation: "shutdown",
                after: bound,
            }),
        }
    }
}

impl AcpClient {
    /// Kill the agent now and wait until it is gone.
    ///
    /// The agent gets `SIGKILL`, and so does everything left in its process
    /// group (the commands it started); the call returns after the agent has
    /// been reaped, so no process of ours outlives it. Use it when a graceful
    /// [`shutdown`](Self::shutdown) is not wanted, for example after a
    /// cancelled turn.
    ///
    /// `Ok` when the agent went away because we asked; [`AcpError::Exited`]
    /// when it had already died, [`AcpError::Timeout`] if it could not be
    /// reaped within ten seconds.
    #[tracing::instrument(name = "acp.kill", skip_all)]
    pub async fn kill(self) -> Result<(), AcpError> {
        let core = &self.core;
        let mut rx = core.shared.exit.subscribe();
        // Only a live agent is killed on purpose: an exit that happened
        // before this call keeps reading as the crash it was.
        if rx.borrow().is_none() {
            core.shared.shutdown_requested.store(true, Ordering::SeqCst);
        }
        core.cancel.cancel();
        let info = match tokio::time::timeout(KILL_WAIT, rx.wait_for(Option::is_some)).await {
            Ok(Ok(v)) => v.clone(),
            _ => None,
        };
        match info {
            Some(i) if i.expected => Ok(()),
            Some(i) => Err(i.to_error()),
            None => Err(AcpError::Timeout {
                operation: "kill",
                after: KILL_WAIT,
            }),
        }
    }
}

/// One conversation with the agent. Cheap to hold; keeps the agent process
/// alive together with the client.
pub struct Session {
    id: String,
    core: Arc<Core>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// The agent's id for this session.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Send a prompt and stream what the agent does until the turn ends.
    ///
    /// The prompt is sent immediately. The stream ends after its last item:
    /// [`AcpUpdate::TurnEnded`] on success, or one `Err` (agent exit,
    /// idle timeout, RPC error). Every update the agent sent for the turn is
    /// delivered before `TurnEnded`. Dropping the stream before it ends
    /// cancels the turn.
    pub fn prompt(&self, text: String) -> BoxStream<'static, Result<AcpUpdate, AcpError>> {
        let (events, rx) = mpsc::unbounded_channel();
        let send_failed = self
            .core
            .cmd_tx
            .send(Cmd::Prompt {
                session_id: self.id.clone(),
                text,
                events,
            })
            .is_err();
        let state = TurnStream {
            rx,
            core: self.core.clone(),
            session_id: self.id.clone(),
            send_failed,
            done: false,
        };
        futures::stream::unfold(state, |mut st| async move {
            let item = st.next_item().await?;
            Some((item, st))
        })
        .boxed()
    }

    /// Ask the agent to stop the running turn (`session/cancel`). The turn's
    /// stream then ends with `TurnEnded { stop_reason: "cancelled" }`.
    /// Pending permission prompts are answered as cancelled.
    #[tracing::instrument(name = "acp.cancel", skip(self), fields(session = %self.id))]
    pub async fn cancel(&self) -> Result<(), AcpError> {
        let sent = self.core.cmd_tx.send(Cmd::Cancel {
            session_id: self.id.clone(),
        });
        if sent.is_err() {
            return Err(self.core.shared.exited_error().await);
        }
        Ok(())
    }
}

struct TurnStream {
    rx: mpsc::UnboundedReceiver<TurnItem>,
    core: Arc<Core>,
    session_id: String,
    send_failed: bool,
    done: bool,
}

impl TurnStream {
    fn send_cancel(&self) {
        let _ = self.core.cmd_tx.send(Cmd::Cancel {
            session_id: self.session_id.clone(),
        });
    }

    async fn next_item(&mut self) -> Option<TurnItem> {
        if self.done {
            return None;
        }
        if self.send_failed {
            self.done = true;
            return Some(Err(self.core.shared.exited_error().await));
        }
        let item = loop {
            let Some(idle) = self.core.opts.turn_idle_timeout else {
                break self.rx.recv().await;
            };
            match tokio::time::timeout(idle, self.rx.recv()).await {
                Ok(item) => break item,
                Err(_) if self.core.shared.pending_permissions.load(Ordering::SeqCst) > 0 => {}
                Err(_) => {
                    self.done = true;
                    self.send_cancel();
                    return Some(Err(AcpError::Timeout {
                        operation: "turn (no agent activity)",
                        after: idle,
                    }));
                }
            }
        };
        match item {
            Some(Ok(update @ AcpUpdate::TurnEnded { .. })) => {
                self.done = true;
                Some(Ok(update))
            }
            Some(Ok(update)) => Some(Ok(update)),
            Some(Err(e)) => {
                self.done = true;
                Some(Err(e))
            }
            None => {
                self.done = true;
                Some(Err(self.core.shared.exited_error().await))
            }
        }
    }
}

impl Drop for TurnStream {
    fn drop(&mut self) {
        if !self.done {
            self.send_cancel();
        }
    }
}

// ---------------------------------------------------------------------------
// Supervisor and stderr
// ---------------------------------------------------------------------------

async fn drain_stderr(stderr: ChildStderr, shared: Arc<Shared>) {
    let mut reader = tokio::io::BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end_matches(['\n', '\r']);
                tracing::info!(target: "adam_acp::agent_stderr", "{line}");
                lock(&shared.stderr).push(line);
            }
        }
    }
}

enum Ev {
    ConnDone(Result<(), agent_client_protocol::Error>),
    ChildExit(std::io::Result<std::process::ExitStatus>),
    Cancelled,
}

async fn supervise(
    mut child: Child,
    conn: impl Future<Output = Result<(), agent_client_protocol::Error>>,
    stderr_task: JoinHandle<()>,
    shared: Arc<Shared>,
    cancel: CancellationToken,
    grace: Duration,
) {
    tokio::pin!(conn);
    let group = child.id();
    // `biased`, cancel first: dropping the last handle cancels *and* closes
    // the command channel, which ends the connection. Without the priority the
    // connection ending could win the race and turn a kill into a graceful
    // exit, which does not sweep the agent's process group.
    let ev = tokio::select! {
        biased;
        () = cancel.cancelled() => Ev::Cancelled,
        r = &mut conn => Ev::ConnDone(r),
        s = child.wait() => Ev::ChildExit(s),
    };
    let mut expected = false;
    let mut wind_down = false;
    let status = match ev {
        Ev::ConnDone(result) => {
            match result {
                Ok(()) => tracing::debug!("ACP connection closed"),
                Err(e) => tracing::warn!(error = %e, "ACP connection ended with an error"),
            }
            match tokio::time::timeout(grace, child.wait()).await {
                Ok(s) => s,
                Err(_) => {
                    tracing::warn!("agent did not exit after its stdin closed; killing it");
                    kill_group(group).await;
                    let _ = child.start_kill();
                    child.wait().await
                }
            }
        }
        Ev::ChildExit(s) => {
            wind_down = true;
            s
        }
        Ev::Cancelled => {
            expected = true;
            // Before the agent is reaped: its pid names the group only while
            // the group leader still exists.
            kill_group(group).await;
            let _ = child.start_kill();
            child.wait().await
        }
    };
    // The pipe closes with the process; bound the wait in case a grandchild holds it.
    let _ = tokio::time::timeout(Duration::from_secs(1), stderr_task).await;
    let code = status
        .as_ref()
        .ok()
        .and_then(std::process::ExitStatus::code);
    match &status {
        Ok(s) => tracing::info!(status = %s, "agent process exited"),
        Err(e) => tracing::warn!(error = %e, "waiting for the agent process failed"),
    }
    let info = ExitInfo {
        code,
        stderr_tail: lock(&shared.stderr).snapshot(),
        expected: expected || shared.shutdown_requested.load(Ordering::SeqCst),
    };
    let err_info = Arc::new(info);
    shared.exit.send_replace(Some(err_info.clone()));
    shared.fail_all(|| err_info.to_error());
    if wind_down {
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut conn).await;
    }
}

/// `SIGKILL` the process group led by the agent (`pid`; it was started with
/// `process_group(0)`, so its group id is its pid), so that what the agent
/// started dies with it.
///
/// Called only while the agent is still unreaped, because only then its pid
/// cannot have been reused. When the agent exits by itself its group is not
/// swept: by then the pid is free and could belong to something else.
///
/// Uses the shell's `kill` builtin: `kill(2)` needs `libc` and `unsafe`, and
/// `kill(1)` may be absent from slim images. Best effort: a failure is logged
/// and the agent alone is killed by the caller.
#[cfg(unix)]
async fn kill_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let killed = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("kill -s KILL -- -{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if !matches!(killed, Ok(s) if s.success()) {
        tracing::warn!(
            pid,
            "could not kill the agent's process group; killing the agent only"
        );
    }
}

#[cfg(not(unix))]
async fn kill_group(_pid: Option<u32>) {}

// ---------------------------------------------------------------------------
// The connection
// ---------------------------------------------------------------------------

fn to_rpc_error(e: FsError) -> agent_client_protocol::Error {
    match e {
        FsError::Denied(msg) => {
            tracing::warn!("{msg}");
            agent_client_protocol::Error::new(-32602, msg)
        }
        FsError::NotFound(path) => agent_client_protocol::Error::resource_not_found(Some(path)),
        FsError::Io(msg) => agent_client_protocol::Error::new(-32603, msg),
    }
}

fn permission_request_of(req: &acp::RequestPermissionRequest) -> PermissionRequest {
    let fields = &req.tool_call.fields;
    PermissionRequest {
        session_id: req.session_id.to_string(),
        tool_call_id: req.tool_call.tool_call_id.to_string(),
        title: fields.title.clone(),
        kind: fields.kind.as_ref().map(wire_str),
        locations: fields
            .locations
            .iter()
            .flatten()
            .map(|l| l.path.clone())
            .collect(),
        raw_input: fields.raw_input.clone(),
        options: req
            .options
            .iter()
            .filter_map(|o| {
                let kind = match o.kind {
                    acp::PermissionOptionKind::AllowOnce => PermissionKind::AllowOnce,
                    acp::PermissionOptionKind::AllowAlways => PermissionKind::AllowAlways,
                    acp::PermissionOptionKind::RejectOnce => PermissionKind::RejectOnce,
                    acp::PermissionOptionKind::RejectAlways => PermissionKind::RejectAlways,
                    // An option we cannot interpret is never chosen.
                    _ => return None,
                };
                Some(PermissionChoice {
                    id: o.option_id.to_string(),
                    name: o.name.clone(),
                    kind,
                })
            })
            .collect(),
    }
}

fn agent_info_of(resp: &acp::InitializeResponse) -> AgentInfo {
    let caps = &resp.agent_capabilities;
    let (name, title, version) = resp.agent_info.as_ref().map_or_else(
        || ("unknown".to_owned(), None, String::new()),
        |i| (i.name.clone(), i.title.clone(), i.version.clone()),
    );
    AgentInfo {
        name,
        title,
        version,
        protocol_version: serde_json::to_value(resp.protocol_version)
            .ok()
            .and_then(|v| v.as_u64())
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(0),
        load_session: caps.load_session,
        image_prompts: caps.prompt_capabilities.image,
        audio_prompts: caps.prompt_capabilities.audio,
        embedded_context: caps.prompt_capabilities.embedded_context,
        mcp_http: caps.mcp_capabilities.http,
        mcp_sse: caps.mcp_capabilities.sse,
        auth_methods: resp
            .auth_methods
            .iter()
            .map(|m| m.id().to_string())
            .collect(),
    }
}

#[tracing::instrument(name = "acp.fs.read", skip_all, fields(path = %req.path.display()))]
async fn handle_read(
    shared: &Shared,
    req: acp::ReadTextFileRequest,
) -> Result<acp::ReadTextFileResponse, agent_client_protocol::Error> {
    shared
        .guard
        .read_text(&req.path, req.line, req.limit)
        .await
        .map(acp::ReadTextFileResponse::new)
        .map_err(to_rpc_error)
}

#[tracing::instrument(name = "acp.fs.write", skip_all, fields(path = %req.path.display(), bytes = req.content.len()))]
async fn handle_write(
    shared: &Shared,
    req: acp::WriteTextFileRequest,
) -> Result<acp::WriteTextFileResponse, agent_client_protocol::Error> {
    shared
        .guard
        .write_text(&req.path, &req.content)
        .await
        .map(|()| acp::WriteTextFileResponse::new())
        .map_err(to_rpc_error)
}

async fn run_connection(
    stdin: ChildStdin,
    stdout: ChildStdout,
    shared: Arc<Shared>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    ready: oneshot::Sender<Result<AgentInfo, AcpError>>,
) -> Result<(), agent_client_protocol::Error> {
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let (s_update, s_perm, s_read, s_write, s_main) = (
        shared.clone(),
        shared.clone(),
        shared.clone(),
        shared.clone(),
        shared,
    );
    Client
        .builder()
        .name("adam-acp")
        .on_receive_notification(
            async move |n: acp::SessionNotification, _cx| {
                s_update.dispatch_update(&n.session_id.to_string(), n.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            // Never wait for the answer here: the dispatch loop must keep
            // running (updates, fs requests, the prompt response).
            async move |req: acp::RequestPermissionRequest,
                        responder: Responder<acp::RequestPermissionResponse>,
                        cx: ConnectionTo<Agent>| {
                let shared = s_perm.clone();
                cx.spawn(async move {
                    let request = permission_request_of(&req);
                    let _pending = PendingPermission::new(&shared);
                    let cancelled = shared.cancel_token(&request.session_id);
                    let decision = tokio::select! {
                        d = policy::decide(&shared.mode, &shared.guard, request) => d,
                        () = async {
                            match &cancelled {
                                Some(t) => t.cancelled().await,
                                None => std::future::pending().await,
                            }
                        } => PermissionDecision::Cancel,
                    };
                    let outcome = match decision {
                        PermissionDecision::Select(id) => acp::RequestPermissionOutcome::Selected(
                            acp::SelectedPermissionOutcome::new(id),
                        ),
                        PermissionDecision::Cancel => acp::RequestPermissionOutcome::Cancelled,
                    };
                    responder.respond(acp::RequestPermissionResponse::new(outcome))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::ReadTextFileRequest, responder, _cx| {
                responder.respond_with_result(handle_read(&s_read, req).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::WriteTextFileRequest, responder, _cx| {
                responder.respond_with_result(handle_write(&s_write, req).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            // Catch-all, registered last. Without it the SDK parks unknown
            // requests that carry a sessionId (terminal/*, ...) forever and
            // the agent hangs waiting for an answer.
            async move |req: acp::AgentRequest, responder, _cx| {
                let method = agent_client_protocol::JsonRpcMessage::method(&req).to_string();
                tracing::warn!(%method, "agent request not supported by this client");
                responder.respond_with_error(
                    agent_client_protocol::Error::method_not_found().data(method),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
            let shared = s_main;
            let init = cx
                .send_request(
                    acp::InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(
                            acp::ClientCapabilities::new()
                                .fs(acp::FileSystemCapabilities::new()
                                    .read_text_file(true)
                                    .write_text_file(true))
                                .terminal(false),
                        )
                        .client_info(acp::Implementation::new(
                            "adam-acp",
                            env!("CARGO_PKG_VERSION"),
                        )),
                )
                .block_task()
                .await;
            match init {
                Ok(resp) if resp.protocol_version == ProtocolVersion::V1 => {
                    let _ = ready.send(Ok(agent_info_of(&resp)));
                }
                Ok(resp) => {
                    let _ = ready.send(Err(AcpError::Protocol(format!(
                        "agent negotiated protocol version {}, only 1 is supported",
                        resp.protocol_version
                    ))));
                    return Ok(());
                }
                Err(e) => {
                    let _ = ready.send(Err(shared.map_rpc_error(e).await));
                    return Ok(());
                }
            }

            loop {
                let cmd = tokio::select! {
                    c = cmd_rx.recv() => c,
                    () = cx.incoming_closed() => None,
                };
                let Some(cmd) = cmd else { break };
                match cmd {
                    Cmd::NewSession {
                        cwd,
                        mcp_servers,
                        reply,
                    } => {
                        let sent = cx.send_request(
                            acp::NewSessionRequest::new(cwd).mcp_servers(
                                mcp_servers
                                    .iter()
                                    .map(McpServerSpec::to_sdk)
                                    .collect::<Vec<_>>(),
                            ),
                        );
                        let shared = shared.clone();
                        cx.spawn(async move {
                            let result = match sent.block_task().await {
                                Ok(r) => Ok(r.session_id.to_string()),
                                Err(e) => Err(shared.map_rpc_error(e).await),
                            };
                            let _ = reply.send(result);
                            Ok(())
                        })?;
                    }
                    Cmd::Prompt {
                        session_id,
                        text,
                        events,
                    } => {
                        {
                            let mut turns = lock(&shared.turns);
                            if turns.contains_key(&session_id) {
                                drop(turns);
                                let _ = events.send(Err(AcpError::TurnInProgress));
                                continue;
                            }
                            turns.insert(
                                session_id.clone(),
                                TurnSlot {
                                    tx: events,
                                    cancel: CancellationToken::new(),
                                    tools: ToolStatuses::default(),
                                },
                            );
                        }
                        // Keep `sent` alive until the response: dropping it
                        // would send `$/cancel_request`.
                        let sent = cx.send_request(acp::PromptRequest::new(
                            acp::SessionId::from(session_id.clone()),
                            vec![acp::ContentBlock::from(text)],
                        ));
                        let shared = shared.clone();
                        cx.spawn(async move {
                            let item = match sent.block_task().await {
                                Ok(resp) => Ok(AcpUpdate::TurnEnded {
                                    stop_reason: wire_str(&resp.stop_reason),
                                }),
                                Err(e) => Err(shared.map_rpc_error(e).await),
                            };
                            shared.finish_turn(&session_id, item);
                            Ok(())
                        })?;
                    }
                    Cmd::Cancel { session_id } => {
                        shared.cancel_turn(&session_id);
                        if let Err(e) = cx.send_notification(acp::CancelNotification::new(
                            acp::SessionId::from(session_id),
                        )) {
                            tracing::warn!(error = %e, "could not send session/cancel");
                        }
                    }
                    Cmd::Shutdown => break,
                }
            }
            Ok(())
        })
        .await
}
