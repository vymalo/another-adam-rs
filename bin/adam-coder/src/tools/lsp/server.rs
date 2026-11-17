//! The language servers this worker holds: a process per (run, slot, language, project root), kept
//! between calls, started on the first need and stopped at the points that stop a command that
//! keeps running.
//!
//! ```mermaid
//! stateDiagram-v2
//!     [*] --> Probing
//!     Probing --> Missing: not on PATH
//!     Probing --> Starting: found
//!     Starting --> Initializing: spawned, piped
//!     Initializing --> Indexing: initialize answered
//!     Initializing --> Dead: no answer in 60 s
//!     Indexing --> Ready: no progress open, quiet for 300 ms
//!     Ready --> Indexing: progress begins
//!     Ready --> Stopped: idle, park, run over, rebuild, release, worker stops
//!     Indexing --> Stopped: idle, park, run over, rebuild, release, worker stops
//!     Indexing --> Dead: crash, bad framing, no answer, wrote to a reference
//!     Ready --> Dead: crash, bad framing, no answer, wrote to a reference
//!     Dead --> Starting: the next call
//!     Missing --> [*]
//!     Stopped --> [*]
//! ```
//!
//! A server is **never** trusted to be one thing: what it sends is bounded (`rpc`), what it asks is
//! answered by [`Shared`] (`workspace/applyEdit` is refused: the client never declares the
//! capability, and a server that asks anyway is answered `applied: false`), and every process of
//! it is killed by group and by the environment's own `kill` at each stop point. The processes of
//! a run end with the run: [`Servers::stop_run`] is called wherever
//! [`Commands::stop_run`](super::super::background::Commands::stop_run) is.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use adam_workspace::{EnvError, EnvSession, ExecId, SlotDir};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;
use tokio::process::Child;
use tokio::sync::{Notify, watch};

use super::position::{Encoding, file_uri};
use super::rpc::{Client, Peer, RpcError};
use super::table::{Language, ServerSpec, Settings};
use crate::redact::Redactor;
use crate::tools::inspect::Snapshot;

/// How long `initialize` may take.
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
/// What of the server's stderr is kept: its last bytes.
pub const STDERR_KEPT: usize = 16 * 1024;
/// How long a server is quiet (no progress begins or ends) before it counts as ready.
pub const SETTLE: Duration = Duration::from_millis(300);
/// How long a call waits for a first `publishDiagnostics` of a file from a server that is not
/// indexing, before it says the server sent none.
pub const FIRST_PUBLISH_GRACE: Duration = Duration::from_secs(3);
/// How long no new `publishDiagnostics` of a file must be seen before the set is taken as complete.
pub const PUBLISH_SETTLE: Duration = Duration::from_millis(300);
/// The files a server holds open at once: the least recently opened is closed for the next.
pub const MAX_OPEN_FILES: usize = 200;
/// The most files looked at for changes under a project root.
pub const MAX_WATCHED_FILES: usize = 20_000;
/// The largest file sent to a server.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// How long a stop waits for the process to be gone.
pub const STOP_WAIT: Duration = Duration::from_secs(12);
/// The least time a request is given, whatever is left of the call's `wait`.
pub const REQUEST_MIN: Duration = Duration::from_secs(2);
/// How long the probe of a program takes at most.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Names of directories that are not looked into for changes: dependencies and build output.
const SKIPPED_DIRS: [&str; 10] = [
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
}

/// What the server is told that does not change: answers to `workspace/configuration` and the
/// workspace folder.
struct Fixed {
    settings: Settings,
    folders: Value,
}

#[derive(Default)]
struct Live {
    progress: HashSet<String>,
    quiescent: Option<bool>,
    last_event: Option<Instant>,
    published: HashMap<String, Published>,
}

struct Published {
    items: Vec<Value>,
    seq: u64,
}

/// The part of a server that its messages change: shared with the reader task.
pub(super) struct Shared {
    fixed: Fixed,
    live: Mutex<Live>,
    changed: watch::Sender<u64>,
    started: Instant,
}

impl Shared {
    fn new(settings: Settings, folders: Value) -> Arc<Self> {
        Arc::new(Self {
            fixed: Fixed { settings, folders },
            live: Mutex::new(Live::default()),
            changed: watch::channel(0).0,
            started: Instant::now(),
        })
    }

    fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n += 1);
    }

    /// Whether the server is indexing: a progress is open, or it said it is not quiescent.
    fn indexing(&self) -> bool {
        let live = self.live();
        !live.progress.is_empty() || live.quiescent == Some(false)
    }

    /// Wait until the server is not indexing and has been quiet for [`SETTLE`], at most until
    /// `deadline`. `true` when it is ready; `false` when it is still indexing at the deadline.
    async fn ready(&self, deadline: Instant) -> bool {
        let mut rx = self.changed.subscribe();
        loop {
            let (busy, since) = {
                let live = self.live();
                (
                    !live.progress.is_empty() || live.quiescent == Some(false),
                    live.last_event.unwrap_or(self.started),
                )
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

    /// The count of `publishDiagnostics` of `uri` so far, and what the last said.
    fn published(&self, uri: &str) -> (u64, Option<Vec<Value>>) {
        self.live()
            .published
            .get(uri)
            .map_or((0, None), |p| (p.seq, Some(p.items.clone())))
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
            let (seq, items) = self.published(uri);
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
                return if seq > seen { items } else { None };
            }
            tokio::select! {
                _ = rx.changed() => {}
                () = tokio::time::sleep_until(give_up.into()) => {}
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
                        live.progress.insert(token);
                    }
                    "end" => {
                        live.progress.remove(&token);
                    }
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
                let seq = live.published.get(uri).map_or(0, |p| p.seq) + 1;
                live.published
                    .insert(uri.to_owned(), Published { items, seq });
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
}

/// Kills the process group of a server that is not held yet, when the call that starts it is
/// dropped (a time limit, a cancel).
struct StartGuard(Option<u32>);

impl StartGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            crate::tools::background::kill_group_blocking(pid);
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
    /// A reference's state when the server started: what it must leave as it found it.
    reference: Option<Snapshot>,
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
    last_used: Mutex<Instant>,
    started: Instant,
    stop: Notify,
    stop_why: Mutex<Option<String>>,
    exited: watch::Sender<bool>,
    end: Mutex<Option<String>>,
    encoding: Encoding,
    pull: bool,
    reference: bool,
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
        let (bytes, cut) = {
            let tail = lock(&self.stderr);
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
        scrubbed.trim().to_owned()
    }

    /// Take the server for one call, waiting at most `within` for the call before it. The server is
    /// busy (never stopped as idle, never the one stopped for room) until the gate is dropped.
    pub async fn lock(&self, within: Duration) -> Option<Gate<'_>> {
        self.busy.fetch_add(1, Ordering::AcqRel);
        match tokio::time::timeout(within, self.work.lock()).await {
            Ok(work) => Some(Gate { server: self, work }),
            Err(_) => {
                self.busy.fetch_sub(1, Ordering::AcqRel);
                None
            }
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

/// One call's hold on a server.
pub struct Gate<'a> {
    server: &'a Server,
    work: tokio::sync::MutexGuard<'a, Work>,
}

impl Drop for Gate<'_> {
    fn drop(&mut self) {
        self.server.touch();
        self.server.busy.fetch_sub(1, Ordering::AcqRel);
    }
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
    let mut found = HashMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                let skipped = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| SKIPPED_DIRS.contains(&n));
                if !skipped {
                    stack.push(path);
                }
            } else if kind.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| extensions.contains(&e))
                && let Ok(meta) = entry.metadata()
            {
                if found.len() >= MAX_WATCHED_FILES {
                    return found;
                }
                found.insert(path, (meta.len(), mtime_ns(&meta)));
            }
        }
    }
    found
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
        // The files the server did not open: created, changed or deleted.
        let (key_root, extensions) = (server.key.root.clone(), server.extensions);
        let now = tokio::task::spawn_blocking(move || walk(&key_root, extensions))
            .await
            .unwrap_or_default();
        if let Some(before) = &self.work.watched {
            let mut changes = Vec::new();
            for (path, stamp) in &now {
                if self.work.opened.contains_key(path) {
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
                if !now.contains_key(path) && !self.work.opened.contains_key(path) {
                    changes.push(json!({"uri": file_uri(path), "type": 3}));
                }
            }
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
        self.work.watched = Some(now);
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
    /// has one, else what it pushed for the file, waited for until `deadline`.
    pub async fn diagnostics(
        &self,
        target_uri: &str,
        before_seq: u64,
        sent: Synced,
        deadline: Instant,
    ) -> Result<Vec<Value>, Reply> {
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
                Reply::Done(value) => Ok(value
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()),
                other => Err(other),
            };
        }
        let indexing = server.indexing();
        // A file that changed (or was never published) is waited for; one that is as it was has
        // what the server said last.
        let (seq, stored) = server.shared.published(target_uri);
        if !sent.target_changed && seq > 0 {
            return Ok(stored.unwrap_or_default());
        }
        let first = if indexing {
            deadline.saturating_duration_since(Instant::now())
        } else {
            FIRST_PUBLISH_GRACE
        };
        Ok(server
            .shared
            .wait_published(target_uri, before_seq, first, deadline)
            .await
            .unwrap_or_default())
    }

    /// How many times the server has published for `uri`, for [`Gate::diagnostics`].
    pub fn published_seq(&self, uri: &str) -> u64 {
        self.server.shared.published(uri).0
    }
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
}

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
    /// A reference: read only.
    pub reference: bool,
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
}

impl Servers {
    /// No servers.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                list: Mutex::new(Vec::new()),
                probes: Mutex::new(HashMap::new()),
                starting: Mutex::new(HashMap::new()),
            }),
        }
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

    /// Stop every server of `run`, saying why, and forget what was probed for it. Waits until the
    /// processes are gone (at most [`STOP_WAIT`] each, together).
    pub async fn stop_run(&self, run: &str, why: &str) {
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
        lock(&self.inner.probes).retain(|(r, _, _), _| r != run);
        self.prune();
    }

    fn prune(&self) {
        self.list().retain(|s| s.is_alive());
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
        let gate = lock(&self.inner.starting)
            .entry(want.key.clone())
            .or_default()
            .clone();
        let _starting = gate.lock().await;
        if let Some(held) = self.held(&want).await {
            return Ok(held);
        }
        self.make_room(want.max).await?;
        let server = start(&want).await?;
        self.list().push(server.clone());
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

    /// Make room for one more server: when `max` are alive, the least recently used one that is not
    /// working is stopped; when all are working, the call is told so.
    async fn make_room(&self, max: usize) -> Result<(), StartError> {
        loop {
            let list = self.list().clone();
            let alive: Vec<&Arc<Server>> = list.iter().filter(|s| s.is_alive()).collect();
            if alive.len() < max {
                return Ok(());
            }
            let victim = alive
                .into_iter()
                .filter(|s| s.busy.load(Ordering::Acquire) == 0)
                .min_by_key(|s| *lock(&s.last_used))
                .cloned();
            let Some(victim) = victim else {
                return Err(StartError::Busy(max));
            };
            victim
                .stop("it was the least recently used, and the worker holds its maximum")
                .await;
            self.prune();
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
    let mut cmd = prepared.command();
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = cmd.spawn().map_err(StartError::Spawn)?;
    let pid = child.id();
    let mut guard = StartGuard(pid);

    let stderr = Arc::new(Mutex::new(Tail::default()));
    if let Some(mut err) = child.stderr.take() {
        let sink = stderr.clone();
        tokio::spawn(async move {
            let mut buf = [0_u8; 4096];
            while let Ok(n) = err.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut tail = lock(&sink);
                tail.total += n as u64;
                tail.bytes.extend_from_slice(&buf[..n]);
                let over = tail.bytes.len().saturating_sub(STDERR_KEPT);
                if over > 0 {
                    tail.bytes.drain(..over);
                }
            }
        });
    }
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(StartError::Failed("the process has no pipes".to_owned()));
    };

    let language = key.language;
    let settings = language.settings(want.reference);
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
                "processId": std::process::id(),
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
            let tail = {
                let tail = lock(&stderr);
                String::from_utf8_lossy(&tail.bytes).into_owned()
            };
            return Err(StartError::Failed(format!(
                "{e}{exit}{}",
                if tail.trim().is_empty() {
                    String::new()
                } else {
                    format!("\nits stderr ended with:\n{}", tail.trim())
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
        last_used: Mutex::new(Instant::now()),
        started: Instant::now(),
        stop: Notify::new(),
        stop_why: Mutex::new(None),
        exited: watch::channel(false).0,
        end: Mutex::new(None),
        encoding,
        pull,
        reference: want.reference,
        idle: want.idle,
        kill_wait: want.kill_wait,
        extensions: language.extensions(),
    });
    tokio::spawn(supervise(server.clone(), child));
    guard.disarm();
    Ok(server)
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

/// Own the process until it ends: by itself, or stopped (idle, or asked to).
async fn supervise(server: Arc<Server>, mut child: Child) {
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
    *lock(&server.end) = Some(how.clone());
    server.client.close(how);
    let _ = server.exited.send(true);
}

impl Server {
    /// Whether this server runs in a reference.
    pub fn in_reference(&self) -> bool {
        self.reference
    }
}
