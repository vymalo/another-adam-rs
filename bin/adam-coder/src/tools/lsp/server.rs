//! The language servers this worker holds: a process per (run, slot, language, project root), kept
//! between calls, started on the first need and stopped at the points that stop a command that
//! keeps running.
//!
//! ```mermaid
//! stateDiagram-v2
//!     [*] --> Probing
//!     Probing --> Missing: not on PATH
//!     Probing --> Waiting: found
//!     Waiting --> Refused: the bound is reached and every server is working
//!     Waiting --> Starting: a place under the bound
//!     Starting --> Initializing: spawned, piped
//!     Starting --> Failed: its run was stopped meanwhile
//!     Initializing --> Indexing: initialize answered
//!     Initializing --> Failed: no answer in 60 s, exited, or its call was dropped
//!     Indexing --> Ready: no progress open, quiet for 300 ms
//!     Ready --> Indexing: progress begins
//!     Ready --> Stopped: idle, park, run over, the environment made again, release, worker stops, room for another
//!     Indexing --> Stopped: idle, park, run over, the environment made again, release, worker stops, room for another
//!     Ready --> Stopped: crash, output that is not messages, no answer, wrote where it must not
//!     Indexing --> Stopped: crash, output that is not messages, no answer, wrote where it must not
//!     Missing --> [*]
//!     Refused --> [*]
//!     Failed --> [*]
//!     Stopped --> [*]
//! ```
//!
//! A stopped server is replaced by the next call. A server counts toward the bound from the moment its
//! start is decided until its process is gone (one being started or stopped included), and room is
//! made one call at a time ([`Servers::get_or_start`]).
//!
//! A server is **never** trusted to be one thing: what it sends is bounded (`rpc`), what it asks is
//! answered by `Shared` (`workspace/applyEdit` is refused: the client never declares the
//! capability, and a server that asks anyway is answered `applied: false`), and every process of
//! it is killed by group and by the environment's own `kill` at each stop point. The processes of
//! a run end with the run: [`Servers::stop_run`] is called wherever
//! [`Commands::stop_run`](super::super::background::Commands::stop_run) is.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use adam_code_tools::governor::Governor;
use adam_workspace::{EnvError, EnvKind, EnvSession, ExecId, SlotDir};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;
use tokio::process::Child;
use tokio::sync::{Notify, watch};

use super::position::{Encoding, file_uri, path_of_uri};
use super::rpc::{Client, Peer, RpcError};
use super::table::{Language, Mode, ServerSpec, Settings};
use crate::redact::Redactor;
use crate::tools::inspect::Snapshot;
use crate::tools::shell::{Hold, RunError, seat_of, spawn_seated};

/// How long `initialize` may take.
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
/// What of the server's stderr is shown: its last bytes. What is held in memory is that and the
/// redactor's margin before it (see [`Want::redactor`]), so that a secret cut by the start of what is
/// held is scrubbed whole or dropped, and the 16 KiB shown never start with a part of one.
pub const STDERR_KEPT: usize = 16 * 1024;
/// How long a server is quiet (no progress begins or ends) before it counts as ready.
pub const SETTLE: Duration = Duration::from_millis(300);
/// How long a call waits for a first `publishDiagnostics` of a file from a server that is not
/// indexing, before it answers that none was received ([`Diagnostics::Nothing`]: not a clean file).
pub const FIRST_PUBLISH_GRACE: Duration = Duration::from_secs(3);
/// How long no new `publishDiagnostics` of a file must be seen before the set is taken as complete.
pub const PUBLISH_SETTLE: Duration = Duration::from_millis(300);
/// The files a server holds open at once: the least recently opened is closed for the next.
pub const MAX_OPEN_FILES: usize = 200;
/// The most files looked at for changes under a project root.
pub const MAX_WATCHED_FILES: usize = 20_000;
/// How often the root is walked for `didChangeWatchedFiles`: a call inside the interval reuses the
/// last walk unless the coder wrote since, so a turn of tool calls does not walk the project once
/// per call, and a change the coder made is still reported at its next call.
pub const WATCHED_TTL: Duration = Duration::from_secs(1);
/// The largest file sent to a server.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// How long a stop waits for the process to be gone.
pub const STOP_WAIT: Duration = Duration::from_secs(12);
/// The least time a request is given, whatever is left of the call's `wait`.
pub const REQUEST_MIN: Duration = Duration::from_secs(2);
/// How long the probe of a program takes at most.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Names of directories that are not looked into for changes: dependencies and build output.
const SKIPPED_DIRS: [&str; 11] = [
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    "dist",
    "build",
    ".mypy_cache",
    ".dart_tool",
];

/// What identifies a server.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    /// The root run.
    pub run: String,
    /// The slot's directory name.
    pub slot: String,
    /// The language.
    pub language: Language,
    /// The project root, absolute.
    pub root: PathBuf,
    /// Safe or full ([`Mode`]): a server is never shared between the two.
    pub mode: Mode,
}

/// What the server is told that does not change: answers to `workspace/configuration` and the
/// workspace folder.
struct Fixed {
    settings: Settings,
    folders: Value,
}

#[derive(Default)]
struct Live {
    /// The tokens of the progress that is open, oldest first, at most [`MAX_PROGRESS`].
    progress: Vec<String>,
    quiescent: Option<bool>,
    last_event: Option<Instant>,
    /// What the server published last, by the file's normal address: one entry per file, at most
    /// [`MAX_OPEN_FILES`] (the oldest publication is dropped for the next).
    published: HashMap<String, Published>,
    /// The publications so far, in all: each is numbered by it, so a number never goes back.
    counter: u64,
}

struct Published {
    items: Vec<Value>,
    seq: u64,
}

/// The most progress tokens held open at once: a server that opens more has the oldest forgotten.
const MAX_PROGRESS: usize = 64;

/// The address of a file as the coder spells it, whatever the server's spelling (`%2d` for `-`, a
/// lower-case escape, `localhost`): the key everything published is held and found by.
fn normal_uri(uri: &str) -> String {
    path_of_uri(uri).map_or_else(|| uri.to_owned(), |path| file_uri(&path))
}

/// The part of a server that its messages change: shared with the reader task.
pub(super) struct Shared {
    fixed: Fixed,
    live: Mutex<Live>,
    changed: watch::Sender<u64>,
    started: Instant,
    /// The server said at `initialize` that it tells when it is quiet (`experimental/serverStatus`,
    /// rust-analyzer's): it is then not ready before it says `quiescent: true`, however quiet it
    /// has been.
    reports_status: AtomicBool,
}

impl Shared {
    fn new(settings: Settings, folders: Value) -> Arc<Self> {
        Arc::new(Self {
            fixed: Fixed { settings, folders },
            live: Mutex::new(Live::default()),
            changed: watch::channel(0).0,
            started: Instant::now(),
            reports_status: AtomicBool::new(false),
        })
    }

    fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n += 1);
    }

    /// Whether the server is working through its index: a progress is open, or it said it is not
    /// quiescent, or it says when it is quiet and has not yet.
    fn working(&self, live: &Live) -> bool {
        !live.progress.is_empty()
            || live.quiescent == Some(false)
            || (self.reports_status.load(Ordering::Acquire) && live.quiescent != Some(true))
    }

    /// Whether the server is indexing now.
    fn indexing(&self) -> bool {
        let live = self.live();
        self.working(&live)
    }

    /// Wait until the server is not indexing and has been quiet for [`SETTLE`], at most until
    /// `deadline`. `true` when it is ready; `false` when it is still indexing at the deadline.
    async fn ready(&self, deadline: Instant) -> bool {
        let mut rx = self.changed.subscribe();
        loop {
            let (busy, since) = {
                let live = self.live();
                (self.working(&live), live.last_event.unwrap_or(self.started))
            };
            if !busy && since.elapsed() >= SETTLE {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return !busy;
            }
            let until = if busy {
                deadline
            } else {
                deadline.min(since + SETTLE)
            };
            tokio::select! {
                _ = rx.changed() => {}
                () = tokio::time::sleep_until(until.into()) => {}
            }
        }
    }

    /// The number of the last `publishDiagnostics` of `uri` (0 for none), and what it said.
    fn published(&self, uri: &str) -> (u64, Option<Vec<Value>>) {
        self.live()
            .published
            .get(&normal_uri(uri))
            .map_or((0, None), |p| (p.seq, Some(p.items.clone())))
    }

    /// The number of the last `publishDiagnostics` of `uri`, without what it said.
    fn published_seq(&self, uri: &str) -> u64 {
        self.live()
            .published
            .get(&normal_uri(uri))
            .map_or(0, |p| p.seq)
    }

    /// The server was told the file is closed: what it said of it is not kept.
    fn forget(&self, uri: &str) {
        self.live().published.remove(&normal_uri(uri));
    }

    /// Wait for `publishDiagnostics` of `uri` after the count was `seen`: the first one within
    /// `first`, then until none comes for [`PUBLISH_SETTLE`]; all of it before `deadline`. What was
    /// published last is the result (`None` when nothing was).
    async fn wait_published(
        &self,
        uri: &str,
        seen: u64,
        first: Duration,
        deadline: Instant,
    ) -> Option<Vec<Value>> {
        let mut rx = self.changed.subscribe();
        let begun = Instant::now();
        let mut last_seq = seen;
        let mut last_at: Option<Instant> = None;
        loop {
            let seq = self.published_seq(uri);
            if seq > last_seq {
                last_seq = seq;
                last_at = Some(Instant::now());
            }
            let now = Instant::now();
            let give_up = match last_at {
                // Something came: done when it was quiet for the settle time.
                Some(at) => at + PUBLISH_SETTLE,
                None => (begun + first).min(deadline),
            };
            let give_up = give_up.min(deadline);
            if now >= give_up {
                return if seq > seen {
                    self.published(uri).1
                } else {
                    None
                };
            }
            tokio::select! {
                _ = rx.changed() => {}
                () = tokio::time::sleep_until(give_up.into()) => {}
            }
        }
    }
    /// Wait, while the server is still indexing, for a publication of `uri` newer than `seen`: the
    /// newer one when it comes, the last one it made when indexing ends first (that word is then
    /// final), and that same last word at `deadline`. A publication that arrived while the server
    /// was indexing is not final — rust-analyzer publishes an empty set before it has looked at the
    /// file, and taking it then reports a broken file as clean.
    async fn wait_past_indexing(
        &self,
        uri: &str,
        seen: u64,
        deadline: Instant,
    ) -> Option<Vec<Value>> {
        let mut rx = self.changed.subscribe();
        loop {
            if self.published_seq(uri) > seen || !self.indexing() {
                return self.published(uri).1;
            }
            if Instant::now() >= deadline {
                return self.published(uri).1;
            }
            tokio::select! {
                _ = rx.changed() => {}
                () = tokio::time::sleep_until(deadline.into()) => {}
            }
        }
    }
}

impl Peer for Shared {
    fn notification(&self, method: &str, params: Value) {
        match method {
            "$/progress" => {
                let token = match params.get("token") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => return,
                };
                let kind = params
                    .pointer("/value/kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mut live = self.live();
                match kind {
                    "begin" => {
                        if !live.progress.contains(&token) {
                            live.progress.push(token);
                            if live.progress.len() > MAX_PROGRESS {
                                live.progress.remove(0);
                            }
                        }
                    }
                    "end" => live.progress.retain(|open| *open != token),
                    _ => {}
                }
                live.last_event = Some(Instant::now());
            }
            "textDocument/publishDiagnostics" => {
                let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                    return;
                };
                let items = params
                    .get("diagnostics")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut live = self.live();
                live.counter += 1;
                let seq = live.counter;
                live.published
                    .insert(normal_uri(uri), Published { items, seq });
                while live.published.len() > MAX_OPEN_FILES {
                    let Some(oldest) = live
                        .published
                        .iter()
                        .min_by_key(|(_, p)| p.seq)
                        .map(|(key, _)| key.clone())
                    else {
                        break;
                    };
                    live.published.remove(&oldest);
                }
            }
            // rust-analyzer's own: `quiescent` is false while it works.
            "experimental/serverStatus" => {
                let mut live = self.live();
                live.quiescent = params.get("quiescent").and_then(Value::as_bool);
                live.last_event = Some(Instant::now());
            }
            _ => return,
        }
        self.bump();
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, (i64, String)> {
        match method {
            "workspace/configuration" => {
                let items = params
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                Ok(Value::Array(
                    items
                        .iter()
                        .map(|item| {
                            item.get("section")
                                .and_then(Value::as_str)
                                .map_or(Value::Null, |s| self.fixed.settings.section(s))
                        })
                        .collect(),
                ))
            }
            // Never: the tools do not write, and the capability is not declared.
            "workspace/applyEdit" => Ok(json!({
                "applied": false,
                "failureReason": "this client only reads; it applies no edit",
            })),
            "workspace/workspaceFolders" => Ok(self.fixed.folders.clone()),
            "window/showDocument" => Ok(json!({"success": false})),
            "window/workDoneProgress/create"
            | "window/showMessageRequest"
            | "client/registerCapability"
            | "client/unregisterCapability"
            | "workspace/semanticTokens/refresh"
            | "workspace/inlayHint/refresh"
            | "workspace/codeLens/refresh"
            | "workspace/diagnostic/refresh"
            | "workspace/inlineValue/refresh" => Ok(Value::Null),
            other => Err((-32601, format!("`{other}` is not supported by this client"))),
        }
    }
}

/// The last bytes of a server's stderr.
#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
    total: u64,
}

/// The last [`STDERR_KEPT`] bytes of what a server said on stderr, scrubbed: what is held is
/// scrubbed as a whole, its start (where a secret may be cut) is dropped when it is not the start of
/// the output, and then the last bytes are taken, from a character's start.
fn tail_text(stderr: &Mutex<Tail>, redactor: &Redactor) -> String {
    let (bytes, cut) = {
        let tail = lock(stderr);
        (tail.bytes.clone(), tail.total > tail.bytes.len() as u64)
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let scrubbed = if cut {
        adam_llm_agent::scrub_open_start(
            &text,
            |t| redactor.scrub(t).into_owned(),
            redactor.margin(),
        )
    } else {
        redactor.scrub(&text).into_owned()
    };
    let scrubbed = scrubbed.trim();
    let mut from = scrubbed.len().saturating_sub(STDERR_KEPT);
    while !scrubbed.is_char_boundary(from) {
        from += 1;
    }
    scrubbed[from..].to_owned()
}

/// What a start can fail with.
#[derive(Debug)]
pub enum StartError {
    /// The environment would not prepare the command.
    Prepare(EnvError),
    /// The process could not be spawned.
    Spawn(std::io::Error),
    /// It started and did not get through `initialize`: why, with the end of its stderr.
    Failed(String),
    /// `LANGUAGE_SERVERS_MAX` servers are held and every one is working.
    Busy(usize),
    /// No seat of the process pool (`MAX_PROCESSES`) freed in time: the call is told to try again.
    Seat(adam_budget::Refused),
    /// The run's servers were stopped (it parked, ended or remade its environment) while this one
    /// was starting: it was stopped too.
    Stopped,
}

/// Kills a server that is not held yet, when the call that starts it is dropped (a time limit, a
/// cancel): its process group here, and what the environment holds for its exec id (a run pod
/// keeps the process and its descendants beyond this machine's reach).
struct StartGuard {
    pid: Option<u32>,
    session: Arc<dyn EnvSession>,
    exec: ExecId,
    kill_wait: Duration,
    armed: bool,
}

impl StartGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(pid) = self.pid {
            crate::tools::background::kill_group_blocking(pid);
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (session, exec, wait) = (self.session.clone(), self.exec.clone(), self.kill_wait);
            runtime.spawn(async move {
                let _ = tokio::time::timeout(wait, session.kill(&exec)).await;
            });
        }
    }
}

/// A file a server holds open: what it was sent last.
struct Opened {
    rel: PathBuf,
    version: i64,
    hash: u64,
}

/// What the calls change, one at a time ([`Server::lock`]).
#[derive(Default)]
struct Work {
    opened: HashMap<PathBuf, Opened>,
    order: Vec<PathBuf>,
    /// The files under the root, as the server was last told: size and modification time.
    watched: Option<HashMap<PathBuf, (u64, u128)>>,
    /// When `watched` was taken: the walk is repeated at most once per [`WATCHED_TTL`].
    walked_at: Option<Instant>,
    /// A reference's state when the server started: what it must leave as it found it.
    reference: Option<Snapshot>,
}

impl Work {
    /// Whether this call walks the root again: the first call, a write the coder made since the
    /// last walk, or a set older than [`WATCHED_TTL`]. A call inside the interval reuses the set.
    fn should_walk(&self, now: Instant, written: bool) -> bool {
        self.watched.is_none()
            || written
            || self
                .walked_at
                .is_none_or(|at| now.saturating_duration_since(at) >= WATCHED_TTL)
    }
}

/// One server.
pub struct Server {
    key: Key,
    spec_program: String,
    pid: Option<u32>,
    exec: ExecId,
    session: Arc<dyn EnvSession>,
    described: String,
    client: Arc<Client>,
    shared: Arc<Shared>,
    stderr: Arc<Mutex<Tail>>,
    work: tokio::sync::Mutex<Work>,
    busy: AtomicUsize,
    /// A tool of the coder wrote to the worktree since the last walk of the root: the next call
    /// walks it, whatever [`WATCHED_TTL`] says.
    written: AtomicBool,
    /// The directory the server was told to build in (a safe rust-analyzer's), removed when it ends.
    scratch: Option<String>,
    last_used: Mutex<Instant>,
    started: Instant,
    stop: Notify,
    stop_why: Mutex<Option<String>>,
    exited: watch::Sender<bool>,
    end: Mutex<Option<String>>,
    encoding: Encoding,
    pull: bool,
    idle: Duration,
    kill_wait: Duration,
    extensions: &'static [&'static str],
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Server {
    /// What this server is.
    pub fn key(&self) -> &Key {
        &self.key
    }

    /// The program it runs (as the table names it).
    pub fn program(&self) -> &str {
        &self.spec_program
    }

    /// Safe or full.
    pub fn mode(&self) -> Mode {
        self.key.mode
    }

    /// Note that the run wrote to its worktree: the next call walks the root, so a file the server
    /// does not hold open is reported without waiting for [`WATCHED_TTL`].
    fn wrote(&self) {
        self.written.store(true, Ordering::Release);
    }

    /// The process id, while it is known.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// How the server counts columns.
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Whether it answers `textDocument/diagnostic`.
    pub fn pulls(&self) -> bool {
        self.pull
    }

    /// How long it has been up.
    pub fn up(&self) -> Duration {
        self.started.elapsed()
    }

    /// Whether the process is there and its pipes are open.
    pub fn is_alive(&self) -> bool {
        !*self.exited.borrow() && self.client.closed().is_none() && lock(&self.stop_why).is_none()
    }

    /// Whether the process is gone (and what it held is cleaned up).
    pub fn has_exited(&self) -> bool {
        *self.exited.borrow()
    }

    /// Whether the server is indexing now.
    pub fn indexing(&self) -> bool {
        self.shared.indexing()
    }

    /// Wait until it is ready, at most until `deadline`: `false` when it is still indexing then.
    pub async fn ready(&self, deadline: Instant) -> bool {
        self.shared.ready(deadline).await
    }

    fn touch(&self) {
        *lock(&self.last_used) = Instant::now();
    }

    fn idle_due(&self) -> bool {
        self.busy.load(Ordering::Acquire) == 0 && lock(&self.last_used).elapsed() >= self.idle
    }

    /// Ask the supervisor to stop it, saying why. The first reason stays.
    pub fn request_stop(&self, why: &str) {
        lock(&self.stop_why).get_or_insert_with(|| why.to_owned());
        self.stop.notify_one();
    }

    /// Why it was stopped, when it was.
    pub fn stopped_because(&self) -> Option<String> {
        lock(&self.stop_why).clone()
    }

    /// Stop it and wait until the process is gone (at most [`STOP_WAIT`]).
    pub async fn stop(&self, why: &str) {
        self.request_stop(why);
        self.wait_exited(STOP_WAIT).await;
    }

    async fn wait_exited(&self, limit: Duration) {
        let mut rx = self.exited.subscribe();
        let _ = tokio::time::timeout(limit, async {
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
    }

    /// How the process ended, when it has.
    pub fn end_status(&self) -> Option<String> {
        lock(&self.end).clone()
    }

    /// What the client was closed with, when it was.
    pub fn closed_because(&self) -> Option<String> {
        self.client.closed()
    }

    /// The last bytes of the server's stderr, scrubbed (an empty string for none).
    pub fn stderr_tail(&self, redactor: &Redactor) -> String {
        tail_text(&self.stderr, redactor)
    }

    /// Take the server for one call, waiting at most `within` for the call before it. The server is
    /// busy (never stopped as idle, never the one stopped for room) until the gate is dropped.
    pub async fn lock(&self, within: Duration) -> Option<Gate<'_>> {
        // Taken before the wait and given back whatever ends it: the time, a cancel, the gate.
        let busy = BusyGuard::new(self);
        match tokio::time::timeout(within, self.work.lock()).await {
            Ok(work) => Some(Gate {
                server: self,
                work,
                _busy: busy,
            }),
            Err(_) => None,
        }
    }

    /// Send a request, and say what came of it: a result, "still indexing" (the request is slow and
    /// the server is working; it stays), or a server that is stopped (it did not answer while idle,
    /// or it is gone).
    pub async fn ask(&self, method: &str, params: Value, deadline: Instant) -> Reply {
        let room = deadline
            .saturating_duration_since(Instant::now())
            .max(REQUEST_MIN);
        match self.client.request(method, params, room).await {
            Ok(value) => Reply::Done(value),
            Err(RpcError::Timeout { .. }) if self.indexing() => Reply::Indexing,
            Err(RpcError::Timeout { secs, .. }) => {
                let why = format!("it did not answer `{method}` within {secs} s");
                self.stop(&why).await;
                Reply::Hung(why)
            }
            Err(RpcError::Closed(why)) => {
                self.stop("it ended").await;
                Reply::Died(why)
            }
            Err(RpcError::Server { code, message, .. }) => Reply::Refused { code, message },
        }
    }
}

/// What a request came to.
#[derive(Debug)]
pub enum Reply {
    /// The result.
    Done(Value),
    /// No answer yet, and the server is working through its index: it stays.
    Indexing,
    /// No answer while the server was not working: it is stopped.
    Hung(String),
    /// The server ended, or sent what cannot be read: it is stopped.
    Died(String),
    /// The server answered with an error.
    Refused {
        /// The JSON-RPC code.
        code: i64,
        /// Its message, not yet scrubbed.
        message: String,
    },
}

/// A call that has the server or waits for it: the server is not idle, and not the one stopped for
/// room, while one exists. Given back when it is dropped, however the call ends.
struct BusyGuard<'a>(&'a Server);

impl<'a> BusyGuard<'a> {
    fn new(server: &'a Server) -> Self {
        server.busy.fetch_add(1, Ordering::AcqRel);
        Self(server)
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.touch();
        self.0.busy.fetch_sub(1, Ordering::AcqRel);
    }
}

/// One call's hold on a server.
pub struct Gate<'a> {
    server: &'a Server,
    work: tokio::sync::MutexGuard<'a, Work>,
    _busy: BusyGuard<'a>,
}

/// What a call knows of the file it is about.
pub struct Target<'a> {
    /// The path relative to the slot.
    pub rel: &'a Path,
    /// The path in the environment (the slot's, then `rel`).
    pub abs: PathBuf,
    /// The text on the disk now.
    pub text: &'a str,
    /// What `didOpen` says it is.
    pub language_id: &'static str,
}

/// What a sync sent.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Synced {
    /// The target file was opened or changed for the server.
    pub target_changed: bool,
    /// Anything at all was sent.
    pub anything: bool,
}

fn hash_of(text: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn mtime_ns(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

/// The files under `root` with one of `extensions`: their size and modification time. Not
/// followed through links, not into dependency or build directories, and at most
/// [`MAX_WATCHED_FILES`].
fn walk(root: &Path, extensions: &[&str]) -> HashMap<PathBuf, (u64, u128)> {
    walk_capped(root, extensions, MAX_WATCHED_FILES)
}

/// The entries of `dir` by name, each with its kind: the order of every walk.
fn sorted_entries(dir: &Path) -> Vec<(std::ffi::OsString, std::fs::FileType, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<_> = entries
        .flatten()
        .filter_map(|e| Some((e.file_name(), e.file_type().ok()?, e.path())))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// [`walk`] with the most files given. The entries of a directory are visited by name and a
/// directory's own where it stands, so the files kept at the cap are the first in the order of their
/// paths, the same on every call: a file does not come and go with the order the file system lists.
fn walk_capped(root: &Path, extensions: &[&str], cap: usize) -> HashMap<PathBuf, (u64, u128)> {
    let mut found = HashMap::new();
    let mut stack = vec![sorted_entries(root).into_iter()];
    while let Some(top) = stack.last_mut() {
        let Some((name, kind, path)) = top.next() else {
            stack.pop();
            continue;
        };
        if kind.is_dir() {
            if !SKIPPED_DIRS.iter().any(|skipped| name == *skipped) {
                stack.push(sorted_entries(&path).into_iter());
            }
        } else if kind.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| extensions.contains(&e))
            && let Ok(meta) = std::fs::symlink_metadata(&path)
        {
            if found.len() >= cap {
                return found;
            }
            found.insert(path, (meta.len(), mtime_ns(&meta)));
        }
    }
    found
}

/// The `didChangeWatchedFiles` changes between two walks: what was created, changed or deleted,
/// leaving out the files the server holds open (it is told of those by their text).
fn watched_changes(
    before: &HashMap<PathBuf, (u64, u128)>,
    now: &HashMap<PathBuf, (u64, u128)>,
    cap: usize,
    is_open: impl Fn(&Path) -> bool,
) -> Vec<Value> {
    // At the cap only the files up to the last one kept are watched: a file that a new one pushed
    // past it is not deleted, it is out of sight.
    let last = if now.len() >= cap {
        now.keys().max()
    } else {
        None
    };
    let mut changes = Vec::new();
    for (path, stamp) in now {
        if is_open(path) {
            continue;
        }
        match before.get(path) {
            None => changes.push(json!({"uri": file_uri(path), "type": 1})),
            Some(old) if old != stamp => {
                changes.push(json!({"uri": file_uri(path), "type": 2}));
            }
            Some(_) => {}
        }
    }
    for path in before.keys() {
        if !now.contains_key(path) && !is_open(path) && last.is_none_or(|last| path <= last) {
            changes.push(json!({"uri": file_uri(path), "type": 3}));
        }
    }
    changes
}

/// Read the text of `rel` below `root`, through the held descriptor of the slot.
pub(super) fn read_text(root: &SlotDir, rel: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let shown = rel.display().to_string();
    let (file, meta) = crate::tools::files::open_for_read(root, &shown)?;
    if !meta.is_file() {
        return Err(format!("`{shown}` is not a regular file"));
    }
    if meta.size() > MAX_FILE_BYTES {
        return Err(format!(
            "`{shown}` is {} bytes, too big for a language server (the limit is {MAX_FILE_BYTES})",
            meta.size()
        ));
    }
    let mut text = String::new();
    std::io::BufReader::new(file)
        .take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("cannot read `{shown}` as text: {e}"))?;
    Ok(text)
}

impl Gate<'_> {
    /// The server this call holds.
    pub fn server(&self) -> &Server {
        self.server
    }

    /// Tell the server what is on the disk now: the target file (opened, or changed since it was
    /// sent), every other file it holds open that changed, and the files under the project root
    /// that were created, changed or deleted since the last call (`didChangeWatchedFiles`).
    ///
    /// # Errors
    ///
    /// What cannot be sent: the server is closed.
    pub async fn sync(
        &mut self,
        root: &Arc<SlotDir>,
        target: &Target<'_>,
    ) -> Result<Synced, RpcError> {
        let server = self.server;
        let mut synced = Synced::default();
        let uri = file_uri(&target.abs);
        let hash = hash_of(target.text);
        let existing = self
            .work
            .opened
            .get(&target.abs)
            .map(|o| (o.version, o.hash));
        match existing {
            None => {
                self.make_room(&mut synced).await?;
                server
                    .client
                    .notify(
                        "textDocument/didOpen",
                        json!({"textDocument": {
                            "uri": uri, "languageId": target.language_id,
                            "version": 1, "text": target.text,
                        }}),
                    )
                    .await?;
                self.saved(&uri).await?;
                self.work.opened.insert(
                    target.abs.clone(),
                    Opened {
                        rel: target.rel.to_path_buf(),
                        version: 1,
                        hash,
                    },
                );
                self.work.order.push(target.abs.clone());
                synced.target_changed = true;
                synced.anything = true;
            }
            Some((version, old)) if old != hash => {
                self.change(&target.abs, &uri, version + 1, hash, target.text)
                    .await?;
                synced.target_changed = true;
                synced.anything = true;
            }
            Some(_) => {}
        }
        // The others the server holds open: what the disk says now.
        let others: Vec<(PathBuf, PathBuf, i64, u64)> = self
            .work
            .opened
            .iter()
            .filter(|(abs, _)| **abs != target.abs)
            .map(|(abs, o)| (abs.clone(), o.rel.clone(), o.version, o.hash))
            .collect();
        for (abs, rel, version, old) in others {
            let read = {
                let root = root.clone();
                tokio::task::spawn_blocking(move || read_text(&root, &rel))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
            };
            match read {
                Ok(text) if hash_of(&text) != old => {
                    let uri = file_uri(&abs);
                    self.change(&abs, &uri, version + 1, hash_of(&text), &text)
                        .await?;
                    synced.anything = true;
                }
                Ok(_) => {}
                // Gone or unreadable: it is not open any more.
                Err(_) => {
                    self.close(&abs).await?;
                    synced.anything = true;
                }
            }
        }
        // The files the server did not open: created, changed or deleted. The walk is the expensive
        // part (up to MAX_WATCHED_FILES of the project) and a turn asks the same question call after
        // call, so it is made at most once per WATCHED_TTL, or at once when the coder wrote since:
        // a server that caches file contents still learns of every change.
        let written = server.written.swap(false, Ordering::AcqRel);
        if self.work.should_walk(Instant::now(), written) {
            let (key_root, extensions) = (server.key.root.clone(), server.extensions);
            let walked = tokio::task::spawn_blocking(move || walk(&key_root, extensions))
                .await
                .unwrap_or_default();
            if let Some(before) = &self.work.watched {
                let changes = watched_changes(before, &walked, MAX_WATCHED_FILES, |path| {
                    self.work.opened.contains_key(path)
                });
                if !changes.is_empty() {
                    server
                        .client
                        .notify(
                            "workspace/didChangeWatchedFiles",
                            json!({ "changes": changes }),
                        )
                        .await?;
                    synced.anything = true;
                }
            }
            self.work.watched = Some(walked);
            self.work.walked_at = Some(Instant::now());
        }
        Ok(synced)
    }

    async fn saved(&self, uri: &str) -> Result<(), RpcError> {
        self.server
            .client
            .notify(
                "textDocument/didSave",
                json!({"textDocument": {"uri": uri}}),
            )
            .await
    }

    async fn change(
        &mut self,
        abs: &Path,
        uri: &str,
        version: i64,
        hash: u64,
        text: &str,
    ) -> Result<(), RpcError> {
        self.server
            .client
            .notify(
                "textDocument/didChange",
                json!({
                    "textDocument": {"uri": uri, "version": version},
                    "contentChanges": [{"text": text}],
                }),
            )
            .await?;
        self.saved(uri).await?;
        if let Some(opened) = self.work.opened.get_mut(abs) {
            opened.version = version;
            opened.hash = hash;
        }
        Ok(())
    }

    async fn close(&mut self, abs: &Path) -> Result<(), RpcError> {
        self.work.opened.remove(abs);
        self.work.order.retain(|p| p != abs);
        self.server.shared.forget(&file_uri(abs));
        self.server
            .client
            .notify(
                "textDocument/didClose",
                json!({"textDocument": {"uri": file_uri(abs)}}),
            )
            .await
    }

    /// Close the file opened longest ago when [`MAX_OPEN_FILES`] are open.
    async fn make_room(&mut self, synced: &mut Synced) -> Result<(), RpcError> {
        while self.work.opened.len() >= MAX_OPEN_FILES {
            let Some(oldest) = self.work.order.first().cloned() else {
                break;
            };
            self.close(&oldest).await?;
            synced.anything = true;
        }
        Ok(())
    }

    /// Keep the state a reference was in when the server started: taken now, and compared at each
    /// later call.
    pub(super) fn remember_reference(&mut self, state: Snapshot) {
        self.work.reference = Some(state);
    }

    /// What a reference must be, when this server runs in one.
    pub(super) fn reference(&self) -> Option<&Snapshot> {
        self.work.reference.as_ref()
    }

    /// The diagnostics of `target`, errors and warnings: asked for with a pull when the server
    /// has one, else what it pushed for the file, waited for until `deadline`. A server that
    /// published nothing for the file is [`Diagnostics::Nothing`], which is not a clean file.
    pub async fn diagnostics(
        &self,
        target_uri: &str,
        before_seq: u64,
        sent: Synced,
        deadline: Instant,
    ) -> Result<Diagnostics, Reply> {
        let server = self.server;
        if server.pull {
            let reply = server
                .ask(
                    "textDocument/diagnostic",
                    json!({"textDocument": {"uri": target_uri}}),
                    deadline,
                )
                .await;
            return match reply {
                Reply::Done(value) => Ok(Diagnostics::Items(
                    value
                        .get("items")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                )),
                // `ServerCancelled` and `ContentModified`: it is working and asked again later.
                Reply::Refused { code, .. } if (-32802..=-32801).contains(&code) => {
                    Err(Reply::Indexing)
                }
                other => Err(other),
            };
        }
        let indexing = server.indexing();
        // A file that changed (or was never published) is waited for; one that is as it was has
        // what the server said last. **Unless the server is still indexing**: a publication that
        // arrived while it was (an empty one above all) is not final — rust-analyzer publishes an
        // empty set before it has looked at the file, and taking it then reports a broken file as
        // clean. Wait for a newer one, up to the deadline.
        let (seq, stored) = server.shared.published(target_uri);
        if !sent.target_changed && seq > 0 {
            if !indexing {
                return Ok(Diagnostics::Items(stored.unwrap_or_default()));
            }
            // Still indexing: what it published so far (an empty set, above all) is not final.
            let waited = deadline.saturating_duration_since(Instant::now());
            return Ok(
                match server
                    .shared
                    .wait_past_indexing(target_uri, seq, deadline)
                    .await
                {
                    Some(items) => Diagnostics::Items(items),
                    None => Diagnostics::Nothing(waited),
                },
            );
        }
        let first = if indexing {
            deadline.saturating_duration_since(Instant::now())
        } else {
            FIRST_PUBLISH_GRACE
        };
        let waited = first.min(deadline.saturating_duration_since(Instant::now()));
        Ok(
            match server
                .shared
                .wait_published(target_uri, before_seq, first, deadline)
                .await
            {
                Some(items) => Diagnostics::Items(items),
                None => Diagnostics::Nothing(waited),
            },
        )
    }

    /// The number of the server's last publication for `uri`, for [`Gate::diagnostics`].
    pub fn published_seq(&self, uri: &str) -> u64 {
        self.server.shared.published_seq(uri)
    }
}

/// What the server said of a file.
#[derive(Debug)]
pub enum Diagnostics {
    /// The diagnostics it gave (none: the file is clean as far as it can tell).
    Items(Vec<Value>),
    /// It published nothing for the file in the time given: the file may not have been looked at.
    Nothing(Duration),
}

/// What the registry says about a server it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// The root run.
    pub run: String,
    /// The slot.
    pub slot: String,
    /// The language.
    pub language: Language,
    /// The project root.
    pub root: PathBuf,
    /// Its process id.
    pub pid: Option<u32>,
    /// Whether it is working through its index now: a `$/progress` is open, it said it is not
    /// quiescent, or it said it reports its status and has not said it is quiescent yet. A test
    /// that must observe a server *while* it indexes waits for this instead of sleeping.
    pub indexing: bool,
}

/// What the probe of a program found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// It is there.
    Found,
    /// It is not, or it does not run: what to tell the model.
    Missing(String),
}

/// What a probe was of: the run, the slot and the language.
type ProbeKey = (String, String, Language);

struct Inner {
    list: Mutex<Vec<Arc<Server>>>,
    probes: Mutex<HashMap<ProbeKey, (String, Probe)>>,
    starting: Mutex<HashMap<Key, Arc<tokio::sync::Mutex<()>>>>,
    /// Held while room is made for a server, so that the bound is one decision at a time.
    room: tokio::sync::Mutex<()>,
    /// The servers that are being started: they count toward the bound before they are in the list.
    reserved: AtomicUsize,
    /// Told when a start ends, for a call that waits for room.
    freed: Notify,
    /// Per run, the number of the last time its servers were stopped, and the same for all of them
    /// (the worker stopping): a start that began before is not registered after.
    stopped_at: Mutex<HashMap<String, u64>>,
    stopped_all: AtomicU64,
    clock: AtomicU64,
}

/// How many runs' stop numbers are remembered before they are all counted as stopped at once.
const MAX_REMEMBERED_RUNS: usize = 4096;

impl Drop for Inner {
    /// The last holder is gone (a worker that stops, a test that ends): no server is left behind.
    fn drop(&mut self) {
        let list = self.list.get_mut().unwrap_or_else(PoisonError::into_inner);
        for server in list.iter().filter(|s| s.is_alive()) {
            if let Some(pid) = server.pid {
                crate::tools::background::kill_group_blocking(pid);
            }
        }
    }
}

/// The language servers of this worker. Clones share them.
#[derive(Clone)]
pub struct Servers {
    inner: Arc<Inner>,
}

impl Default for Servers {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Servers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Servers")
            .field("held", &self.count())
            .finish_non_exhaustive()
    }
}

/// What a call asks of [`Servers::get_or_start`].
pub struct Want<'a> {
    /// Which server.
    pub key: Key,
    /// How it is started.
    pub spec: &'a ServerSpec,
    /// The run's environment.
    pub session: Arc<dyn EnvSession>,
    /// Safe (read only, and the slot held to what it was) or full.
    pub mode: Mode,
    /// The idle time after which it stops.
    pub idle: Duration,
    /// The most servers this worker holds.
    pub max: usize,
    /// How long the environment's `kill` is waited for.
    pub kill_wait: Duration,
    /// The variables that are the process's secrets, hidden from the server.
    pub hide: Vec<String>,
    /// How long `initialize` may take ([`INITIALIZE_TIMEOUT`] for the tools).
    pub init_timeout: Duration,
    /// What the server's stderr is scrubbed with; its [`margin`](Redactor::margin) is how much of
    /// it is held beyond [`STDERR_KEPT`].
    pub redactor: Redactor,
    /// The process pool a server takes a seat of for as long as it lives (ADR 0065): `None` takes
    /// none. A server that runs in a pod is never counted, whatever this says.
    pub governor: Option<Governor>,
}

impl Servers {
    /// No servers.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                list: Mutex::new(Vec::new()),
                probes: Mutex::new(HashMap::new()),
                starting: Mutex::new(HashMap::new()),
                room: tokio::sync::Mutex::new(()),
                reserved: AtomicUsize::new(0),
                freed: Notify::new(),
                stopped_at: Mutex::new(HashMap::new()),
                stopped_all: AtomicU64::new(0),
                clock: AtomicU64::new(0),
            }),
        }
    }

    /// How many per-key start gates are held (a test reads it: none is kept once a start is over).
    #[doc(hidden)]
    pub fn starting_held(&self) -> usize {
        lock(&self.inner.starting).len()
    }

    fn list(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Server>>> {
        lock(&self.inner.list)
    }

    /// How many servers this worker holds that are alive.
    pub fn count(&self) -> usize {
        self.list().iter().filter(|s| s.is_alive()).count()
    }

    /// The servers of `run` that are alive.
    pub fn running(&self, run: &str) -> Vec<ServerInfo> {
        self.list()
            .iter()
            .filter(|s| s.key.run == run && s.is_alive())
            .map(|s| ServerInfo {
                run: s.key.run.clone(),
                slot: s.key.slot.clone(),
                language: s.key.language,
                root: s.key.root.clone(),
                pid: s.pid,
                indexing: s.shared.indexing(),
            })
            .collect()
    }

    /// The runs that have a server here.
    pub fn runs(&self) -> Vec<String> {
        let mut runs: Vec<String> = self
            .list()
            .iter()
            .filter(|s| s.is_alive())
            .map(|s| s.key.run.clone())
            .collect();
        runs.sort();
        runs.dedup();
        runs
    }

    /// What the probe of the program found for this run, slot and language, while the environment is
    /// the same one (it is described the same way).
    pub fn probed(
        &self,
        run: &str,
        slot: &str,
        language: Language,
        described: &str,
    ) -> Option<Probe> {
        lock(&self.inner.probes)
            .get(&(run.to_owned(), slot.to_owned(), language))
            .filter(|(was, _)| was == described)
            .map(|(_, probe)| probe.clone())
    }

    /// Remember what the probe found.
    pub fn remember_probe(
        &self,
        run: &str,
        slot: &str,
        language: Language,
        described: &str,
        probe: Probe,
    ) {
        lock(&self.inner.probes).insert(
            (run.to_owned(), slot.to_owned(), language),
            (described.to_owned(), probe),
        );
    }

    /// Note that `run` wrote to its worktree (a tool of the coder, not a server): the next call of
    /// each of its servers walks the root, whatever the walk interval says.
    pub fn wrote(&self, run: &str) {
        for server in self.list().iter().filter(|s| s.key.run == run) {
            server.wrote();
        }
    }

    /// Stop every server of `run`, saying why, and forget what was probed for it. Waits until the
    /// processes are gone (at most [`STOP_WAIT`] each, together).
    pub async fn stop_run(&self, run: &str, why: &str) {
        // Before anything is stopped: a server that is starting now will not be registered.
        self.mark_stopped(run);
        lock(&self.inner.probes).retain(|(r, _, _), _| r != run);
        let mine: Vec<Arc<Server>> = self
            .list()
            .iter()
            .filter(|s| s.key.run == run)
            .cloned()
            .collect();
        Self::stop_all_of(&mine, why).await;
        self.prune();
    }

    /// Stop every server: this worker is stopping.
    pub async fn stop_all(&self) {
        self.inner.stopped_all.store(self.tick(), Ordering::Release);
        let all: Vec<Arc<Server>> = self.list().clone();
        Self::stop_all_of(&all, "the worker stopped").await;
        lock(&self.inner.probes).clear();
        self.prune();
    }

    async fn stop_all_of(servers: &[Arc<Server>], why: &str) {
        for server in servers {
            server.request_stop(why);
        }
        for server in servers {
            server.wait_exited(STOP_WAIT).await;
        }
    }

    /// Forget what is known of `run` that no stop forgets (it is over, and its workspace goes).
    pub fn forget_run(&self, run: &str) {
        self.mark_stopped(run);
        lock(&self.inner.probes).retain(|(r, _, _), _| r != run);
        self.prune();
    }

    /// Servers that are gone are forgotten; one that is being stopped stays until its process is.
    fn prune(&self) {
        self.list().retain(|s| !s.has_exited());
    }

    fn tick(&self) -> u64 {
        self.inner.clock.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn mark_stopped(&self, run: &str) {
        let at = self.tick();
        let mut stopped = lock(&self.inner.stopped_at);
        if stopped.len() >= MAX_REMEMBERED_RUNS {
            stopped.clear();
            self.inner.stopped_all.store(at, Ordering::Release);
        }
        stopped.insert(run.to_owned(), at);
    }

    /// What a start of a server of `run` that begins now must be compared with when it ends.
    fn stop_mark(&self, run: &str) -> (u64, u64) {
        (
            lock(&self.inner.stopped_at).get(run).copied().unwrap_or(0),
            self.inner.stopped_all.load(Ordering::Acquire),
        )
    }

    /// The server for `want`, started when there is none: one held and alive is the one, unless the
    /// environment is not the one it was started in (it was made again).
    ///
    /// # Errors
    ///
    /// [`StartError`] when it cannot be started.
    pub async fn get_or_start(&self, want: Want<'_>) -> Result<Arc<Server>, StartError> {
        if let Some(held) = self.held(&want).await {
            return Ok(held);
        }
        let gate = StartGate::new(self, &want.key);
        let _starting = gate.gate.lock().await;
        if let Some(held) = self.held(&want).await {
            return Ok(held);
        }
        let marked = self.stop_mark(&want.key.run);
        let mut reservation = self.make_room(want.max).await?;
        let server = start(&want).await?;
        let mut list = self.list();
        if self.stop_mark(&want.key.run) != marked {
            // The run's servers were stopped while this one started: it is stopped too, and never held.
            drop(list);
            reservation.release();
            server
                .stop("its run's servers were stopped while it started")
                .await;
            return Err(StartError::Stopped);
        }
        // In the list and no longer reserved at once: the count never dips.
        list.push(server.clone());
        reservation.release();
        drop(list);
        Ok(server)
    }

    /// The live server for the key, stopped first when its environment is not the current one.
    async fn held(&self, want: &Want<'_>) -> Option<Arc<Server>> {
        let found = self
            .list()
            .iter()
            .find(|s| s.key == want.key && s.is_alive())
            .cloned()?;
        if found.described != want.session.describe().summary {
            found.stop("its environment was made again").await;
            self.prune();
            return None;
        }
        found.touch();
        Some(found)
    }

    /// Make room for one more server, one decision at a time. The servers that count are those in
    /// the list whose process is not gone (one that is being stopped counts until it is) and those
    /// being started. At `max`, the least recently used server that is not working is stopped; a
    /// server on its way out or being started is waited for; when every one is working the call is
    /// told so.
    async fn make_room(&self, max: usize) -> Result<Reservation, StartError> {
        enum Next {
            Reserved,
            Victim(Arc<Server>),
            Stopping(Arc<Server>),
            Starting,
            Full,
        }
        let _room = self.inner.room.lock().await;
        loop {
            let next = {
                let mut list = self.list();
                list.retain(|s| !s.has_exited());
                let reserved = self.inner.reserved.load(Ordering::Acquire);
                if list.len() + reserved < max {
                    self.inner.reserved.fetch_add(1, Ordering::AcqRel);
                    Next::Reserved
                } else if let Some(victim) = list
                    .iter()
                    .filter(|s| s.is_alive() && s.busy.load(Ordering::Acquire) == 0)
                    .min_by_key(|s| *lock(&s.last_used))
                {
                    Next::Victim(victim.clone())
                } else if let Some(leaving) = list.iter().find(|s| !s.is_alive()) {
                    Next::Stopping(leaving.clone())
                } else if reserved > 0 {
                    Next::Starting
                } else {
                    Next::Full
                }
            };
            match next {
                Next::Reserved => {
                    return Ok(Reservation {
                        inner: self.inner.clone(),
                        live: true,
                    });
                }
                Next::Victim(victim) => {
                    victim
                        .stop("it was the least recently used, and the worker holds its maximum")
                        .await;
                }
                Next::Stopping(leaving) => {
                    leaving.wait_exited(STOP_WAIT).await;
                    if !leaving.has_exited() {
                        return Err(StartError::Busy(max));
                    }
                }
                Next::Starting => {
                    let _ = tokio::time::timeout(
                        Duration::from_millis(200),
                        self.inner.freed.notified(),
                    )
                    .await;
                }
                Next::Full => return Err(StartError::Busy(max)),
            }
        }
    }
}

/// A place in the bound for a server that is being started.
struct Reservation {
    inner: Arc<Inner>,
    live: bool,
}

impl Reservation {
    fn release(&mut self) {
        if self.live {
            self.live = false;
            self.inner.reserved.fetch_sub(1, Ordering::AcqRel);
            self.inner.freed.notify_waiters();
        }
    }
}

impl Drop for Reservation {
    /// A start that ends any way (an error, its call dropped) gives its place back.
    fn drop(&mut self) {
        self.release();
    }
}

/// The gate of one key's start, and its removal from the map when the start is over, whether it
/// ended or its call was dropped.
struct StartGate<'a> {
    servers: &'a Servers,
    key: Key,
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl<'a> StartGate<'a> {
    fn new(servers: &'a Servers, key: &Key) -> Self {
        let gate = lock(&servers.inner.starting)
            .entry(key.clone())
            .or_default()
            .clone();
        Self {
            servers,
            key: key.clone(),
            gate,
        }
    }
}

impl Drop for StartGate<'_> {
    fn drop(&mut self) {
        let mut map = lock(&self.servers.inner.starting);
        // The map's and this one's: nobody else is waiting for it.
        if map
            .get(&self.key)
            .is_some_and(|held| Arc::strong_count(held) <= 2)
        {
            map.remove(&self.key);
        }
    }
}

/// Start a server and get it through `initialize`.
async fn start(want: &Want<'_>) -> Result<Arc<Server>, StartError> {
    let key = &want.key;
    let mut spec = crate::tools::shell::shell_spec(&key.root, &want.spec.launch).background();
    for (name, value) in &want.spec.env {
        spec = spec.env(name, value);
    }
    spec = spec.hide(want.hide.iter().cloned());
    let prepared = want.session.prepare(&spec).map_err(StartError::Prepare)?;
    let governor = want
        .governor
        .as_ref()
        .filter(|_| matches!(want.session.describe().kind, EnvKind::Local));
    let seat = seat_of(governor, &key.run).await.map_err(|e| match e {
        RunError::Busy(refused) => StartError::Seat(refused),
        other => StartError::Failed(adam_error::report(&other)),
    })?;
    let (mut child, hold) =
        spawn_seated(&prepared, governor, seat, Stdio::piped()).map_err(|e| match e {
            RunError::Spawn(e) => StartError::Spawn(e),
            other => StartError::Failed(adam_error::report(&other)),
        })?;
    let pid = child.id();
    let mut guard = StartGuard {
        pid,
        session: want.session.clone(),
        exec: prepared.exec.clone(),
        kill_wait: want.kill_wait,
        armed: true,
    };

    let stderr = Arc::new(Mutex::new(Tail::default()));
    let mut stderr_task = None;
    // Told when stderr cannot be read to its end: a server whose pipe is not drained blocks.
    let stderr_failed = Arc::new(Notify::new());
    if let Some(err) = child.stderr.take() {
        let sink = stderr.clone();
        let held = STDERR_KEPT + want.redactor.margin();
        let failed = stderr_failed.clone();
        stderr_task = Some(tokio::spawn(async move {
            if !drain_stderr(err, &sink, held).await {
                failed.notify_one();
            }
        }));
    }
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(StartError::Failed("the process has no pipes".to_owned()));
    };

    let language = key.language;
    // A safe rust-analyzer builds in a directory of its own (never the slot, never one shared with
    // another run), which is removed when it stops.
    let scratch = (language == Language::Rust && want.mode == Mode::Safe).then(|| scratch_dir(key));
    let settings = language.settings(want.mode, scratch.as_deref().unwrap_or_default());
    let root_uri = file_uri(&key.root);
    let name = key.root.file_name().map_or_else(
        || "workspace".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let folders = json!([{"uri": root_uri, "name": name}]);
    let shared = Shared::new(settings.clone(), folders.clone());
    let client = Client::start(stdout, stdin, shared.clone());

    let initialized = client
        .request(
            "initialize",
            json!({
                // The worker's pid means nothing where the server runs (another PID namespace), and we stop
                // the server ourselves: some servers exit when the pid they were given is gone.
                "processId": null,
                "clientInfo": {"name": "adam-coder", "version": env!("CARGO_PKG_VERSION")},
                "locale": "en",
                "rootUri": root_uri,
                "workspaceFolders": folders,
                "initializationOptions": settings.options,
                "capabilities": {
                    "general": {"positionEncodings": ["utf-8", "utf-16"]},
                    "window": {"workDoneProgress": true},
                    "workspace": {
                        "workspaceFolders": true,
                        "configuration": true,
                        "didChangeWatchedFiles": {"dynamicRegistration": false},
                    },
                    "textDocument": {
                        "synchronization": {"didSave": true, "dynamicRegistration": false},
                        "publishDiagnostics": {"relatedInformation": false},
                        "diagnostic": {"dynamicRegistration": false, "relatedDocumentSupport": false},
                        "definition": {"linkSupport": true},
                        "references": {},
                    },
                    "experimental": {"serverStatusNotification": true},
                },
            }),
            want.init_timeout,
        )
        .await;
    let result = match initialized {
        Ok(result) => result,
        Err(e) => {
            // Not held yet: stop what was started, and say what its stderr said.
            let exit = stop_unheld(
                &mut child,
                pid,
                &*want.session,
                &prepared.exec,
                want.kill_wait,
            )
            .await;
            guard.disarm();
            client.close("it did not start");
            let tail = tail_text(&stderr, &want.redactor);
            return Err(StartError::Failed(format!(
                "{e}{exit}{}",
                if tail.is_empty() {
                    String::new()
                } else {
                    format!("\nits stderr ended with:\n{tail}")
                }
            )));
        }
    };
    let capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
    let encoding = capabilities
        .get("positionEncoding")
        .and_then(Value::as_str)
        .and_then(Encoding::from_lsp)
        .unwrap_or_default();
    let pull = capabilities
        .get("diagnosticProvider")
        .is_some_and(|v| !v.is_null() && *v != json!(false));
    shared.reports_status.store(
        capabilities
            .pointer("/experimental/serverStatusNotification")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        Ordering::Release,
    );
    if let Err(e) = client.notify("initialized", json!({})).await {
        let exit = stop_unheld(
            &mut child,
            pid,
            &*want.session,
            &prepared.exec,
            want.kill_wait,
        )
        .await;
        guard.disarm();
        return Err(StartError::Failed(format!("{e}{exit}")));
    }

    let server = Arc::new(Server {
        key: key.clone(),
        spec_program: want.spec.program.clone(),
        pid,
        exec: prepared.exec.clone(),
        session: want.session.clone(),
        described: want.session.describe().summary,
        client,
        shared,
        stderr,
        work: tokio::sync::Mutex::new(Work::default()),
        busy: AtomicUsize::new(0),
        written: AtomicBool::new(false),
        scratch,
        last_used: Mutex::new(Instant::now()),
        started: Instant::now(),
        stop: Notify::new(),
        stop_why: Mutex::new(None),
        exited: watch::channel(false).0,
        end: Mutex::new(None),
        encoding,
        pull,
        idle: want.idle,
        kill_wait: want.kill_wait,
        extensions: language.extensions(),
    });
    tokio::spawn(supervise(
        server.clone(),
        child,
        stderr_task,
        stderr_failed,
        hold,
    ));
    guard.disarm();
    Ok(server)
}

/// The directory a safe rust-analyzer builds in: one per run, slot and project root, under the
/// environment's `/tmp`.
fn scratch_dir(key: &Key) -> String {
    let id = hash_of(&format!(
        "{}\0{}\0{}",
        key.run,
        key.slot,
        key.root.display()
    ));
    format!("/tmp/adam-rust-analyzer/{id:016x}")
}

/// Read the server's stderr into `sink`, keeping its last `held` bytes, until it ends. A read that
/// fails is tried again, and `false` is the answer of a stream that keeps failing: it cannot be
/// drained, and the server is stopped (it would block on a full pipe).
async fn drain_stderr<R: tokio::io::AsyncRead + Unpin>(
    mut err: R,
    sink: &Mutex<Tail>,
    held: usize,
) -> bool {
    const FAILURES: u32 = 3;
    let mut buf = [0_u8; 4096];
    let mut failed = 0;
    loop {
        match err.read(&mut buf).await {
            Ok(0) => return true,
            Ok(n) => {
                failed = 0;
                let mut tail = lock(sink);
                tail.total += n as u64;
                tail.bytes.extend_from_slice(&buf[..n]);
                let over = tail.bytes.len().saturating_sub(held);
                if over > 0 {
                    tail.bytes.drain(..over);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                failed += 1;
                if failed >= FAILURES {
                    return false;
                }
            }
        }
    }
}

/// Stop a process that was started and is not held: its group, its exec id in the environment, and
/// what is left. How it ended, as a clause for the message.
async fn stop_unheld(
    child: &mut Child,
    pid: Option<u32>,
    session: &dyn EnvSession,
    exec: &ExecId,
    kill_wait: Duration,
) -> String {
    crate::tools::shell::kill_group(pid).await;
    let _ = child.kill().await;
    let _ = tokio::time::timeout(kill_wait, session.kill(exec)).await;
    if let Some(group) = pid {
        crate::tools::background::kill_leftovers(group).await;
    }
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) => format!(" (the process ended: {status})"),
        _ => String::new(),
    }
}

/// Remove what a server built in its scratch directory, where it ran.
async fn remove_scratch(server: &Server, dir: &str) {
    let command = format!("rm -rf -- {}", super::sh_quote(dir));
    let spec = crate::tools::shell::shell_spec(&server.key.root, &command);
    let Ok(prepared) = server.session.prepare(&spec) else {
        return;
    };
    let mut cmd = prepared.command();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if !matches!(
        tokio::time::timeout(Duration::from_secs(10), cmd.status()).await,
        Ok(Ok(status)) if status.success()
    ) {
        tracing::warn!("a language server's scratch directory could not be removed");
    }
}

/// Own the process until it ends: by itself, or stopped (idle, or asked to).
async fn supervise(
    server: Arc<Server>,
    mut child: Child,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    stderr_failed: Arc<Notify>,
    _hold: Option<Hold>,
) {
    let pid = child.id();
    let ended = loop {
        // Asleep until the idle time could be over: a busy server is looked at again soon.
        let due_in = server
            .idle
            .saturating_sub(lock(&server.last_used).elapsed());
        tokio::select! {
            biased;
            () = server.stop.notified() => break None,
            status = child.wait() => break Some(status),
            // Its output is not messages any more (bad framing, a write that fails): the process
            // is of no use and is stopped now, not when it is next called or idle. A process that
            // ended by itself is given a moment to say so first.
            () = server.client.wait_closed() => {
                match tokio::time::timeout(Duration::from_millis(500), child.wait()).await {
                    Ok(status) => break Some(status),
                    Err(_) => {
                        let why = server.client.closed().unwrap_or_default();
                        server.request_stop(&format!("its output stopped being messages: {why}"));
                        break None;
                    }
                }
            }
            () = stderr_failed.notified() => {
                server.request_stop("its stderr could not be read");
                break None;
            }
            () = tokio::time::sleep(due_in.max(Duration::from_millis(100))) => {
                if server.idle_due() {
                    server.request_stop(&format!(
                        "it was idle for {} s (LANGUAGE_SERVER_IDLE_SECS)",
                        server.idle.as_secs()
                    ));
                }
            }
        }
    };
    let how = match ended {
        Some(status) => match status {
            Ok(status) => format!("the process ended: {status}"),
            Err(e) => format!("the process could not be waited for: {e}"),
        },
        None => {
            crate::tools::shell::kill_group(pid).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            format!(
                "the process was stopped: {}",
                server
                    .stopped_because()
                    .unwrap_or_else(|| "stopped".to_owned())
            )
        }
    };
    // What the leader left in its group is the server's too, and so is what the environment holds
    // for its exec id.
    if let Some(group) = pid {
        crate::tools::background::kill_leftovers(group).await;
    }
    if tokio::time::timeout(server.kill_wait, server.session.kill(&server.exec))
        .await
        .is_err()
    {
        tracing::warn!("the environment did not kill a language server in time");
    }
    // What it said on stderr before it ended is read to the end (a process that outlived its group
    // and holds the pipe is not waited for).
    if let Some(task) = stderr_task {
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }
    if let Some(scratch) = &server.scratch {
        remove_scratch(&server, scratch).await;
    }
    *lock(&server.end) = Some(how.clone());
    server.client.close(how);
    let _ = server.exited.send(true);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn shared() -> Arc<Shared> {
        Shared::new(Language::Rust.settings(Mode::Full, ""), json!([]))
    }

    fn publish(shared: &Shared, uri: &str, message: &str) {
        shared.notification(
            "textDocument/publishDiagnostics",
            json!({"uri": uri, "diagnostics": [{"message": message}]}),
        );
    }

    #[test]
    fn what_a_server_published_is_kept_per_file_and_bounded() {
        let shared = shared();
        for n in 0..1000 {
            publish(&shared, &format!("file:///w/f{n}.rs"), "x");
        }
        let held = shared.live().published.len();
        assert!(held <= MAX_OPEN_FILES, "{held} files are held");
        // The newest is there, and publishing again replaces a file's entry (one per path).
        assert!(shared.published("file:///w/f999.rs").1.is_some());
        publish(&shared, "file:///w/f999.rs", "y");
        assert_eq!(shared.live().published.len(), held);
        // The count never goes back, whatever was dropped.
        let (seq, _) = shared.published("file:///w/f999.rs");
        publish(&shared, "file:///w/f0.rs", "z");
        assert!(shared.published("file:///w/f0.rs").0 > seq);
    }

    #[test]
    fn a_file_is_found_under_whatever_spelling_the_server_gave_its_address() {
        let shared = shared();
        publish(&shared, "file:///w/a%2db.rs", "one");
        let canonical = file_uri(Path::new("/w/a-b.rs"));
        assert_eq!(
            shared.published(&canonical).1.unwrap()[0]["message"],
            "one",
            "{canonical}"
        );
        publish(&shared, "file:///w/a-b.rs", "two");
        assert_eq!(shared.published(&canonical).1.unwrap()[0]["message"], "two");
    }

    #[test]
    fn the_progress_a_server_has_open_is_bounded() {
        let shared = shared();
        for n in 0..1000 {
            shared.notification(
                "$/progress",
                json!({"token": format!("t{n}"), "value": {"kind": "begin"}}),
            );
        }
        let open = shared.live().progress.len();
        assert!(open <= 64, "{open} tokens are held");
        assert!(shared.indexing(), "what is held still counts as working");
    }

    /// A stream that fails once, then gives its text, then ends.
    struct Flaky {
        step: u8,
    }

    impl tokio::io::AsyncRead for Flaky {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.step += 1;
            match self.step {
                1 => std::task::Poll::Ready(Err(std::io::Error::other("a read failed"))),
                2 => {
                    buf.put_slice(b"said after the failure");
                    std::task::Poll::Ready(Ok(()))
                }
                _ => std::task::Poll::Ready(Ok(())),
            }
        }
    }

    #[tokio::test]
    async fn stderr_is_drained_to_its_end_after_a_read_error() {
        let sink = Mutex::new(Tail::default());
        drain_stderr(Flaky { step: 0 }, &sink, 1024).await;
        assert_eq!(lock(&sink).bytes, b"said after the failure");
    }

    #[test]
    fn a_secret_cut_by_the_start_of_the_held_stderr_is_never_shown() {
        let secret = "SECRETVALUE-0123456789-abcdefghijklmnop";
        let redactor = Redactor::new([secret]);
        // What is held starts inside the secret (the output went on after it was cut), so the
        // redactor cannot know the part is one.
        let held = format!("{} and then more output", &secret[20..]);
        let kept = Mutex::new(Tail {
            total: held.len() as u64 + 100,
            bytes: held.into_bytes(),
        });
        let shown = tail_text(&kept, &redactor);
        assert!(!shown.contains(&secret[20..]), "{shown}");
        // From the start of the output, the same text is shown whole.
        let whole = lock(&kept).bytes.len() as u64;
        lock(&kept).total = whole;
        assert!(tail_text(&kept, &redactor).contains("and then more output"));
    }

    #[tokio::test]
    async fn no_start_gate_is_kept_once_a_start_is_over() {
        let servers = Servers::new();
        let spec = ServerSpec {
            program: "sh".to_owned(),
            launch: "exit 3".to_owned(),
            env: Vec::new(),
            version_probe: false,
        };
        let root = tempfile::tempdir().unwrap();
        for n in 0..3 {
            let want = Want {
                key: Key {
                    run: format!("run-{n}"),
                    slot: "w".to_owned(),
                    language: Language::Rust,
                    root: root.path().to_path_buf(),
                    mode: Mode::Full,
                },
                spec: &spec,
                session: adam_workspace::LocalSession::shared(),
                mode: Mode::Full,
                idle: Duration::from_secs(600),
                max: 4,
                kill_wait: Duration::from_secs(2),
                hide: Vec::new(),
                init_timeout: Duration::from_secs(10),
                redactor: Redactor::default(),
                governor: None,
            };
            assert!(servers.get_or_start(want).await.is_err());
        }
        assert_eq!(servers.starting_held(), 0);
    }

    #[test]
    fn the_files_watched_are_the_first_in_a_fixed_order_and_a_file_moving_past_the_cap_is_no_change()
     {
        let dir = tempfile::tempdir().unwrap();
        for name in ["h", "e", "b", "g", "a", "d", "c", "f"] {
            std::fs::write(dir.path().join(format!("{name}.rs")), "x").unwrap();
        }
        let sorted = |m: &HashMap<PathBuf, (u64, u128)>| {
            let mut keys: Vec<String> = m
                .keys()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            keys.sort();
            keys
        };
        let before = walk_capped(dir.path(), &["rs"], 5);
        assert_eq!(sorted(&before), ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"]);
        // A file that sorts early appears: the window moves, `e.rs` leaves it. Only the creation
        // is a change; `e.rs` was not deleted.
        std::fs::write(dir.path().join("0.rs"), "x").unwrap();
        let now = walk_capped(dir.path(), &["rs"], 5);
        assert_eq!(sorted(&now), ["0.rs", "a.rs", "b.rs", "c.rs", "d.rs"]);
        let changes = watched_changes(&before, &now, 5, |_| false);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0]["type"], 1);
        assert!(changes[0]["uri"].as_str().unwrap().ends_with("/0.rs"));
        // A file that is really deleted inside the window is a delete.
        std::fs::remove_file(dir.path().join("b.rs")).unwrap();
        let later = walk_capped(dir.path(), &["rs"], 5);
        let changes = watched_changes(&now, &later, 5, |_| false);
        assert!(
            changes
                .iter()
                .any(|c| c["type"] == 3 && c["uri"].as_str().unwrap().ends_with("/b.rs")),
            "{changes:?}"
        );
    }

    /// A turn of tool calls asks the same question call after call: the root is walked once, and
    /// again after the interval or after a write of the coder.
    #[test]
    fn the_root_is_walked_once_per_interval_and_after_a_write_of_the_coder() {
        let mut work = Work::default();
        let t0 = Instant::now();
        assert!(work.should_walk(t0, false), "the first call has no set");
        work.watched = Some(HashMap::new());
        work.walked_at = Some(t0);
        for ms in [0, 100, 500, 999] {
            assert!(
                !work.should_walk(t0 + Duration::from_millis(ms), false),
                "{ms} ms is inside the interval"
            );
        }
        assert!(work.should_walk(t0 + WATCHED_TTL, false), "the set is old");
        assert!(
            work.should_walk(t0 + Duration::from_millis(1), true),
            "the coder wrote"
        );
    }
}
