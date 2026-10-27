//! The server: accept a connection, run one git command, stream its output back.
//!
//! The interaction and the life of a command are drawn in the crate's README ("A command from end to
//! end"). In short: the first frame is read within 10 s and checked (version, arguments, a directory that
//! is a plain directory below the root, the environment allow-list); a command waits for one of
//! `GIT_SANDBOX_MAX_COMMANDS` places; the server's own `git` runs in a process group of its own; its
//! output goes back as frames, and its log line is written before the exit frame.
//!
//! A command never outlives its connection: when git exits the server kills the process group it ran
//! in (a filter that daemonised does not stay), and when the client goes away, or the server gets
//! SIGTERM, it kills the group while the command still runs.

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::cwd::Root;
use crate::environment;
use crate::error::SandboxError;
use crate::protocol::{
    ClientFrame, DATA_CHUNK, Exit, PROTOCOL_VERSION, ProtocolError, Refusal, RefusalReason,
    Request, ServerFrame, read_client_frame, write_server_frame,
};
use crate::subcommand::subcommand;

/// How long a client has to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the output of a command that has ended is waited for once its group is dead.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the last frames are given to reach a client that has not gone.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the connections are given to kill their commands after SIGTERM.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// The most standard input one command is given (256 MiB); a client that sends more is cut off.
const MAX_STDIN: usize = 256 * 1024 * 1024;

/// A server bound to its socket. [`serve`](Self::serve) runs it.
#[derive(Debug)]
pub struct Server {
    listener: UnixListener,
    socket: PathBuf,
    ctx: Arc<Context>,
}

#[derive(Debug)]
struct Context {
    git: PathBuf,
    root: Root,
    permits: Semaphore,
    shutdown: CancellationToken,
}

impl Server {
    /// Open the root and make the socket (mode `0600`). A path that is taken by something that is
    /// not a socket is an error; a socket that nobody answers on (a server that died) is replaced;
    /// one that answers is an error.
    ///
    /// Call it inside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// [`SandboxError::Config`] when the root cannot be opened, [`SandboxError::Bind`] when the socket
    /// cannot be made.
    pub fn bind(config: &Config) -> Result<Self, SandboxError> {
        let root = Root::open(&config.root).map_err(|e| {
            SandboxError::Config(crate::config::ConfigError {
                problems: vec![format!(
                    "GIT_SANDBOX_ROOT {} cannot be opened as a directory: {e}",
                    config.root.display()
                )],
            })
        })?;
        let bind = |source: io::Error| SandboxError::Bind {
            path: config.listen.clone(),
            source,
        };
        make_room(&config.listen).map_err(bind)?;
        let listener = bind_private(&config.listen).map_err(bind)?;
        Ok(Self {
            listener,
            socket: config.listen.clone(),
            ctx: Arc::new(Context {
                git: config.git.clone(),
                root,
                permits: Semaphore::new(config.max_commands),
                shutdown: CancellationToken::new(),
            }),
        })
    }

    /// The socket this server listens on.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Serve until `shutdown` resolves, then kill the commands that still run, remove the socket and
    /// return.
    pub async fn serve(self, shutdown: impl Future<Output = ()>) {
        tracing::info!(
            socket = %self.socket.display(),
            root = %self.ctx.root.path().display(),
            git = %self.ctx.git.display(),
            "adam-git-sandbox listening"
        );
        let mut tasks = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        tasks.spawn(handle(Arc::clone(&self.ctx), stream));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot accept a connection");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                },
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
        tracing::info!("shutting down: the commands that still run are killed");
        self.ctx.shutdown.cancel();
        let finished = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        if finished.is_err() {
            tasks.abort_all();
        }
        // Only a socket is removed, whatever has been put at the path since.
        if std::fs::symlink_metadata(&self.socket).is_ok_and(|m| m.file_type().is_socket()) {
            let _ = std::fs::remove_file(&self.socket);
        }
    }
}

/// Clear `path` for a new socket: nothing there, or a socket nobody answers on.
fn make_room(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(meta) if meta.file_type().is_socket() => {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "a server already answers on this socket",
                )),
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path)
                }
                Err(e) => Err(e),
            }
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the path exists and is not a socket; it is not replaced",
        )),
    }
}

/// Bind `path` so that it never exists with a mode other than `0600`.
fn bind_private(path: &Path) -> io::Result<UnixListener> {
    let previous = rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o177));
    let bound = UnixListener::bind(path);
    rustix::process::umask(previous);
    let listener = bound?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// What the server will run, once the request is accepted.
struct Plan {
    argv: Vec<OsString>,
    cwd: PathBuf,
    env: Vec<(OsString, OsString)>,
}

fn refusal(reason: RefusalReason, message: impl Into<String>) -> Refusal {
    Refusal {
        reason,
        message: message.into(),
    }
}

/// Check a request and make what is to be run of it.
async fn plan(ctx: &Context, request: &Request) -> Result<Plan, Refusal> {
    if ctx.shutdown.is_cancelled() {
        return Err(refusal(
            RefusalReason::ShuttingDown,
            "the git sandbox is shutting down",
        ));
    }
    if request.version != PROTOCOL_VERSION {
        return Err(refusal(
            RefusalReason::UnsupportedVersion,
            format!("this server speaks protocol {PROTOCOL_VERSION}"),
        ));
    }
    if request.argv.is_empty() {
        return Err(refusal(RefusalReason::NoArguments, "git needs arguments"));
    }
    let argv: Vec<OsString> = request
        .argv
        .iter()
        .map(|a| a.as_os_str().to_owned())
        .collect();
    let has_nul = |s: &std::ffi::OsStr| std::os::unix::ffi::OsStrExt::as_bytes(s).contains(&0);
    if argv.iter().any(|a| has_nul(a)) || has_nul(request.cwd.as_os_str()) {
        return Err(refusal(
            RefusalReason::BadRequest,
            "an argument or the directory holds a NUL byte",
        ));
    }
    let wanted = PathBuf::from(request.cwd.as_os_str());
    let root = ctx.root.clone();
    let cwd = tokio::task::spawn_blocking(move || root.resolve(&wanted))
        .await
        .map_err(|_| {
            refusal(
                RefusalReason::BadDirectory,
                "the directory could not be checked",
            )
        })?
        .map_err(|e| refusal(RefusalReason::BadDirectory, e.to_string()))?;
    let env = environment::filter(request.env.iter().map(|pair| {
        (
            OsString::from(&pair.name),
            pair.value.as_os_str().to_owned(),
        )
    }));
    Ok(Plan { argv, cwd, env })
}

/// The text of `path` for a log line: nothing that could pass for the end of the line.
fn printable(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// What the log says of a command: the subcommand and the directory, and when it started.
struct Note {
    subcommand: String,
    cwd: String,
    started: Instant,
}

impl Note {
    /// The one line a command leaves in the log: the subcommand, the directory, how it ended and how
    /// long it took. Never another argument, a value of the environment or a byte of output. It is
    /// written **before** the client is told the command has ended, so a caller that has the answer
    /// can read the line.
    fn log(&self, status: &str) {
        let duration_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            subcommand = %self.subcommand,
            cwd = %self.cwd,
            status,
            duration_ms,
            "git command"
        );
    }
}

/// Tell the client why its command is not run. Best effort: it may have gone.
async fn refuse(mut writer: OwnedWriteHalf, refusal: &Refusal) {
    let _ = tokio::time::timeout(
        FLUSH_TIMEOUT,
        write_server_frame(&mut writer, &ServerFrame::Refused(refusal.clone())),
    )
    .await;
    let _ = writer.shutdown().await;
}

async fn handle(ctx: Arc<Context>, stream: UnixStream) {
    let (mut reader, writer) = stream.into_split();
    let started = Instant::now();
    let request = match tokio::time::timeout(REQUEST_TIMEOUT, read_client_frame(&mut reader)).await
    {
        Ok(Ok(Some(ClientFrame::Request(request)))) => request,
        other => {
            let why = match other {
                Err(_) => "no request in time".to_owned(),
                Ok(Ok(None)) => return,
                Ok(Ok(Some(_))) => "the first frame is not a request".to_owned(),
                Ok(Err(e)) => adam_error::report(&e),
            };
            tracing::warn!(reason = %why, "a connection sent no usable request");
            refuse(writer, &refusal(RefusalReason::BadRequest, why)).await;
            return;
        }
    };
    let note = Note {
        subcommand: subcommand(&request.argv),
        cwd: printable(Path::new(request.cwd.as_os_str())),
        started,
    };
    let plan = match plan(&ctx, &request).await {
        Ok(plan) => plan,
        Err(why) => {
            note.log(&format!("refused: {:?}", why.reason));
            refuse(writer, &why).await;
            return;
        }
    };
    let permit = tokio::select! {
        permit = ctx.permits.acquire() => permit,
        () = ctx.shutdown.cancelled() => {
            let why = refusal(RefusalReason::ShuttingDown, "the git sandbox is shutting down");
            note.log("refused: ShuttingDown");
            refuse(writer, &why).await;
            return;
        }
    };
    let Ok(_permit) = permit else { return };
    run(&ctx, plan, reader, writer, &note).await;
}

/// What the client sends the command's standard input, in order.
enum Input {
    Data(Vec<u8>),
    End,
}

/// Run the command to the end, and log how it ended ([`Note::log`]) before saying so to the client.
async fn run(
    ctx: &Context,
    plan: Plan,
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    note: &Note,
) {
    let mut command = Command::new(&ctx.git);
    command
        .args(&plan.argv)
        .env_clear()
        .envs(plan.env.iter().map(|(n, v)| (n, v)))
        .current_dir(&plan.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // A group of its own: what the command starts can be killed with it.
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            let why = refusal(
                RefusalReason::SpawnFailed,
                format!("cannot start git ({})", e.kind()),
            );
            note.log("refused: SpawnFailed");
            refuse(writer, &why).await;
            return;
        }
    };
    let group = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(Pid::from_raw);

    let (frames, outbox) = mpsc::channel::<ServerFrame>(8);
    let gone = CancellationToken::new();
    let writer_task = tokio::spawn(write_frames(writer, outbox, gone.clone()));
    let stdout_task = child
        .stdout
        .take()
        .map(|out| tokio::spawn(pump(out, frames.clone(), ServerFrame::Stdout)));
    let stderr_task = child
        .stderr
        .take()
        .map(|err| tokio::spawn(pump(err, frames.clone(), ServerFrame::Stderr)));
    let (input, inbox) = mpsc::unbounded_channel::<Input>();
    let feeder = tokio::spawn(feed_stdin(child.stdin.take(), inbox));
    let listener = tokio::spawn(listen(reader, input, gone.clone()));

    let outcome = tokio::select! {
        status = child.wait() => Some(status),
        () = gone.cancelled() => None,
        () = ctx.shutdown.cancelled() => None,
    };
    // Whatever the command left running dies with it; and a command that still runs is stopped.
    kill_group(group);

    match outcome {
        Some(Ok(exit)) => {
            let (mut out, mut err) = (stdout_task, stderr_task);
            let drained = tokio::time::timeout(DRAIN_TIMEOUT, async {
                for task in [&mut out, &mut err].into_iter().flatten() {
                    let _ = task.await;
                }
            })
            .await;
            if drained.is_err() {
                // Something outside the group still holds the pipes: what it writes is not the
                // command's any more.
                tracing::warn!("the output of a command did not end after its group was killed");
                for task in [out, err].into_iter().flatten() {
                    task.abort();
                }
            }
            let exit = Exit {
                code: exit.code(),
                signal: exit.signal(),
            };
            note.log(&match (exit.code, exit.signal) {
                (Some(code), _) => format!("exit {code}"),
                (None, Some(signal)) => format!("signal {signal}"),
                (None, None) => "ended".to_owned(),
            });
            let _ = frames.send(ServerFrame::Exit(exit)).await;
            drop(frames);
            let _ = tokio::time::timeout(FLUSH_TIMEOUT, writer_task).await;
        }
        Some(Err(e)) => {
            tracing::warn!(error = %e, "cannot wait for a command");
            note.log("wait failed");
            writer_task.abort();
        }
        None => {
            let _ = child.wait().await;
            writer_task.abort();
            for task in [stdout_task, stderr_task].into_iter().flatten() {
                task.abort();
            }
            note.log(if ctx.shutdown.is_cancelled() {
                "killed: shutdown"
            } else {
                "killed: client gone"
            });
        }
    }
    listener.abort();
    feeder.abort();
}

/// SIGKILL to a process group; one that is already gone is fine.
fn kill_group(group: Option<Pid>) {
    let Some(group) = group else { return };
    match rustix::process::kill_process_group(group, Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(e) => tracing::warn!(error = %e, "cannot kill a process group"),
    }
}

/// Write frames to the client in the order they come. A write that fails means the client has gone.
async fn write_frames(
    mut writer: OwnedWriteHalf,
    mut outbox: mpsc::Receiver<ServerFrame>,
    gone: CancellationToken,
) {
    while let Some(frame) = outbox.recv().await {
        if write_server_frame(&mut writer, &frame).await.is_err() {
            gone.cancel();
            return;
        }
    }
    let _ = writer.shutdown().await;
}

/// Copy what a command writes into frames, until it closes the pipe or the client is gone.
async fn pump<R: AsyncRead + Unpin>(
    mut from: R,
    frames: mpsc::Sender<ServerFrame>,
    frame: fn(Vec<u8>) -> ServerFrame,
) {
    let mut chunk = vec![0u8; DATA_CHUNK];
    loop {
        match from.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if frames.send(frame(chunk[..n].to_vec())).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Give the command the input the client sends, and close it when the client says it ends. A
/// command that stops reading (it ended, or does not want it) ends the copy.
async fn feed_stdin(stdin: Option<ChildStdin>, mut inbox: mpsc::UnboundedReceiver<Input>) {
    let Some(mut stdin) = stdin else { return };
    while let Some(item) = inbox.recv().await {
        match item {
            Input::Data(bytes) => {
                if stdin.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            Input::End => break,
        }
    }
    // Dropped: the command sees the end of its input.
}

/// Read what the client sends after its request. The client stays connected until it has read the
/// last frame, so a connection that ends here is a client that has gone: the command is stopped.
async fn listen(
    mut reader: OwnedReadHalf,
    input: mpsc::UnboundedSender<Input>,
    gone: CancellationToken,
) {
    let mut total = 0usize;
    loop {
        match read_client_frame(&mut reader).await {
            Ok(Some(ClientFrame::Stdin(bytes))) => {
                total = total.saturating_add(bytes.len());
                if total > MAX_STDIN {
                    tracing::warn!("a client sent more standard input than is allowed");
                    break;
                }
                // The command may have stopped reading; the client is still read, to see it go.
                let _ = input.send(Input::Data(bytes));
            }
            Ok(Some(ClientFrame::StdinEnd)) => {
                let _ = input.send(Input::End);
            }
            Ok(Some(ClientFrame::Request(_))) => {
                tracing::warn!("a client sent a second request");
                break;
            }
            Ok(None) | Err(ProtocolError::Truncated | ProtocolError::Io(_)) => break,
            Err(e) => {
                tracing::warn!(reason = %adam_error::report(&e), "a client broke the protocol");
                break;
            }
        }
    }
    gone.cancel();
}
