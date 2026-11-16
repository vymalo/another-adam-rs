//! Commands that keep running: `start_command`, `stop_command`, `list_commands` (ADR 0044 slice 16).
//!
//! `run` and `run_command` wait for their command. A development server or a watcher does not end,
//! and the model needs it while it works: a check against `localhost`, a test that talks to the
//! server. `start_command` starts such a command, reads **its output as it arrives** into the same
//! kept output `read_output` pages (the id is the call's output id), and returns after a short wait.
//! `stop_command` stops it and `list_commands` says what runs. Nothing here outlives its run.
//!
//! ```mermaid
//! sequenceDiagram
//!     participant M as model
//!     participant T as start_command
//!     participant C as Commands
//!     participant P as the process
//!     participant F as outputs/RUN/ID.txt
//!     M->>T: start_command {command, wait_for}
//!     T->>C: reserve the id (at most N at once)
//!     T->>P: spawn (a process group of its own)
//!     P-->>F: pumps: scrub, fold into lines, append
//!     T-->>M: the id and the first page [running: n lines so far; next page: offset=n+1]
//!     M->>F: read_output {id, offset}
//!     M->>C: stop_command {id}
//!     C->>P: kill the group, the environment's `kill` of the exec id
//!     P-->>F: the footer, the record says how it ended
//! ```
//!
//! # What is kept, and where
//!
//! * **The output** is `<root>/outputs/<root run>/<id>.txt`, as for every kept output, appended
//!   line by line while the process runs. It is scrubbed **before** it is written
//!   ([`StreamScrubber`](adam_llm_agent::StreamScrubber): a secret split between two reads is never
//!   on the volume) and folded into lines ([`LineFolder`](adam_llm_agent::LineFolder)); only complete
//!   lines are written, so a page of lines already there is the same bytes on every read, and only
//!   the page's last line, `[running: n lines so far; next page: offset=n+1]`, moves. The file
//!   stops at [`MAX_KEPT_BYTES`](adam_llm_agent::MAX_KEPT_BYTES) and at the run's room, with the
//!   line `read_output` shows for every cut output; after that the last 64 KiB are kept in memory
//!   for `stop_command` and `list_commands`.
//! * **The record** is `<id>.cmd` beside it: JSON, private, with the command (scrubbed, 1 KiB), the
//!   slot, the exec id, the **owner** (a token this process made at start), the pid with its start
//!   time, and how it ended. A worker that starts, stops or lists reads the records of the run: a
//!   command whose owner is another process is not running here, so its process is killed (by the
//!   exec id, and by pid when the start time still matches) and the record says `gone`.
//!
//! # Replay
//!
//! A start step that runs again (its result was never journaled) finds its command: this process
//! owns it and it is in the registry, so the call returns its view and starts nothing; otherwise
//! the record of the earlier try is stale, its process is killed and the command starts again,
//! with its output truncated. At most one process lives for an id.
//!
//! # When a command stops
//!
//! `stop_command`; its lifetime (`BACKGROUND_COMMAND_MAX_SECS`); the **root run is no longer open**
//! (at once, in the agent's step, and by [`Commands::sweep`] every `BACKGROUND_SWEEP_SECS`, because a
//! cancel never calls the agent); a **park on the person** (`ask_user`); the environment is made
//! again; the janitor's release. A process group is killed, and what a pod or a devcontainer holds is
//! stopped through [`EnvSession::kill`] by exec id.

use std::fmt::Write as _;
use std::hash::{BuildHasher as _, Hasher as _};
use std::io::Write as _;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use adam::prelude::*;
use adam_core::{DynStore, Store};
use adam_llm_agent::{LineFolder, StreamScrubber, is_output_id, output_id_of};
use adam_workspace::{EnvSession, ExecId, Slot};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::inspect::Snapshot;
use super::sandbox::Meter;
use super::shell::{KeptNote, ShellOutcome, kill_group, resolve_cwd, shell_spec, spawn_prepared};
use super::{Outcome, ToolEnv, cancelled, non_empty, output, run_error};
use crate::kept::{FileKept, RUN_KEPT_MAX};
use crate::redact::Redactor;

/// The tool that starts a command that keeps running.
pub const START_COMMAND: &str = "start_command";
/// The tool that stops one.
pub const STOP_COMMAND: &str = "stop_command";
/// The tool that lists them.
pub const LIST_COMMANDS: &str = "list_commands";
/// The three tools, in the order they are offered.
pub const BACKGROUND_TOOLS: [&str; 3] = [START_COMMAND, STOP_COMMAND, LIST_COMMANDS];

/// Seconds `start_command` waits when the call names none.
const DEFAULT_WAIT_SECS: i64 = 5;
/// The most seconds a call may ask `start_command` to wait.
pub const MAX_WAIT_SECS: i64 = 120;
/// The longest `wait_for`, in characters.
const MAX_WAIT_FOR_CHARS: usize = 200;
/// What `stop_command` shows of the end of an output.
const STOP_VIEW_BYTES: usize = 4096;
/// The command line a record keeps.
const MAX_COMMAND_BYTES: usize = 1024;
/// What is kept in memory of an output: its last bytes (and its first quarter of a view).
const MEMORY_KEPT: usize = 64 * 1024;
/// A run must have this much room left of its 64 MiB to start a command.
const MIN_ROOM: u64 = 1024 * 1024;
/// How often the run's room is looked at while a command writes: every this many bytes.
const ROOM_CHECK_EVERY: u64 = 1024 * 1024;
/// How often a waiting call looks at the command.
const POLL: Duration = Duration::from_millis(40);
/// The longest `stop_command` waits for a process to be gone.
const END_WAIT: Duration = Duration::from_secs(20);
/// The most commands a run's registry keeps once they ended (their records stay on the volume).
const KEEP_ENDED: usize = 16;
/// How long the pipes are waited for after the process is gone.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// How a command stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CommandState {
    /// Made, not started yet.
    Starting,
    /// Running.
    Running,
    /// Ended by itself; `None` is a signal.
    Exited {
        /// The exit code.
        code: Option<i32>,
    },
    /// Stopped: by `stop_command`, or by the reaper.
    Stopped {
        /// Why.
        why: String,
    },
    /// Not running and not known to have ended: the worker that ran it is gone.
    Gone {
        /// Why.
        why: String,
    },
}

impl CommandState {
    /// Whether the command still runs (or is about to).
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

/// The file `<id>.cmd`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    command: String,
    slot: String,
    cwd: String,
    exec: String,
    owner: String,
    pid: Option<u32>,
    start: Option<u64>,
    started_at: u64,
    #[serde(flatten)]
    state: CommandState,
    ran_secs: Option<u64>,
    lines: Option<u64>,
    git: Option<String>,
}

/// A process that ended: how, and how long it ran.
#[derive(Debug, Clone)]
struct Ended {
    state: CommandState,
    ran: Duration,
}

/// Git as it was when the command started, for the report at its end.
struct GitWatch {
    slot: Slot,
    before: Snapshot,
}

/// What a command's output has come to: written as it arrives.
struct Sink {
    scrubber: StreamScrubber,
    folder: LineFolder,
    redactor: Redactor,
    kept: Arc<FileKept>,
    run: String,
    id: String,
    file: Option<std::fs::File>,
    /// Writing the file stopped (the bound, the run's room, the volume).
    full: bool,
    /// Bytes written to the file.
    written: u64,
    /// Lines written to the file.
    lines: u64,
    /// The output is longer than what was kept.
    cut: bool,
    /// Why nothing more is kept, when the volume or the run's room said no.
    refused: Option<String>,
    cap: u64,
    /// The first bytes of the scrubbed output, and the last.
    head: String,
    tail: Vec<u8>,
    /// What the process printed, and what is left of it once scrubbed.
    raw_total: u64,
    scrubbed_total: u64,
    next_check: u64,
    /// The text `start_command` waits for, and whether a line has had it.
    watch: Option<String>,
    matched: bool,
}

impl Sink {
    fn feed(&mut self, bytes: &[u8]) {
        self.raw_total += bytes.len() as u64;
        let redactor = self.redactor.clone();
        let scrub = |text: &str| redactor.scrub(text).into_owned();
        let mut text = self.scrubber.push(bytes, &scrub);
        text.push_str(
            &self
                .scrubber
                .release(&scrub, &|text| redactor.partial_secret_len(text)),
        );
        let lines = self.folder.push(&text);
        self.accept(&lines);
    }

    /// The output has ended: what was held back, and the line that was never ended.
    fn finish(&mut self) {
        let redactor = self.redactor.clone();
        let scrub = |text: &str| redactor.scrub(text).into_owned();
        let rest = self.scrubber.finish(&scrub);
        let mut lines = self.folder.push(&rest);
        lines.push_str(&self.folder.finish());
        self.accept(&lines);
        if self.cut
            && let Some(file) = self.file.as_mut()
        {
            let mut footer = String::new();
            match &self.refused {
                Some(why) => {
                    let _ = write!(footer, "(output cut: {why})");
                }
                None => {
                    let _ = write!(
                        footer,
                        "(output cut: only the first {} of {} bytes are kept; the first number counts the scrubbed text, the second the output)",
                        self.written, self.raw_total
                    );
                }
            }
            footer.push('\n');
            if file.write_all(footer.as_bytes()).is_ok() {
                self.lines += 1;
            }
        }
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
        }
        self.file = None;
    }

    /// Complete (folded) lines of scrubbed output: to the head and tail, and to the file.
    fn accept(&mut self, lines: &str) {
        if lines.is_empty() {
            return;
        }
        self.scrubbed_total += lines.len() as u64;
        if self.head.len() < MEMORY_KEPT / 4 {
            let mut take = (MEMORY_KEPT / 4 - self.head.len()).min(lines.len());
            while !lines.is_char_boundary(take) {
                take -= 1;
            }
            self.head.push_str(&lines[..take]);
        }
        self.tail.extend_from_slice(lines.as_bytes());
        if self.tail.len() > 2 * MEMORY_KEPT {
            let drop = self.tail.len() - MEMORY_KEPT;
            self.tail.drain(..drop);
        }
        if let Some(watch) = &self.watch
            && lines.contains(watch.as_str())
        {
            self.matched = true;
        }
        if self.full {
            self.cut = true;
            return;
        }
        let Some(file) = self.file.as_mut() else {
            self.cut = true;
            self.full = true;
            return;
        };
        let room = self.cap.saturating_sub(self.written);
        let mut text = lines;
        if text.len() as u64 > room {
            // Only whole lines: cut back to the last line that fits.
            let mut end = usize::try_from(room).unwrap_or(usize::MAX).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            end = text[..end].rfind('\n').map_or(0, |at| at + 1);
            text = &text[..end];
            self.cut = true;
            self.full = true;
        }
        if let Err(e) = file.write_all(text.as_bytes()) {
            self.refused = Some(format!("the volume refused it ({})", e.kind()));
            self.cut = true;
            self.full = true;
            return;
        }
        self.written += text.len() as u64;
        self.lines += text.bytes().filter(|&b| b == b'\n').count() as u64;
        // A line that never ended is the last, and is a line.
        if !text.is_empty() && !text.ends_with('\n') {
            self.lines += 1;
        }
        if self.written >= self.next_check {
            self.next_check = self.written + ROOM_CHECK_EVERY;
            match self.kept.run_used(&self.run, &self.id) {
                Ok(used) if used + self.written > RUN_KEPT_MAX => {
                    self.refused = Some(format!(
                        "a run keeps at most {} MiB of output",
                        RUN_KEPT_MAX / (1024 * 1024)
                    ));
                    self.cut = true;
                    self.full = true;
                }
                Ok(_) | Err(_) => {}
            }
        }
    }

    /// The last `bytes` of the output, started at the beginning of a line when there is one.
    fn tail_text(&self, bytes: usize) -> String {
        let text = String::from_utf8_lossy(&self.tail);
        output::tail_of(&text, bytes).to_owned()
    }
}

/// A command this process runs, or ran.
pub(crate) struct Command {
    id: String,
    run: String,
    shown: String,
    slot: String,
    cwd: String,
    exec: Mutex<ExecId>,
    started: Instant,
    started_at: u64,
    pid: Mutex<Option<u32>>,
    start_ticks: Mutex<Option<u64>>,
    ended: Mutex<Option<Ended>>,
    stop: Notify,
    stop_why: Mutex<Option<String>>,
    sink: Mutex<Sink>,
    session: Arc<dyn EnvSession>,
    kept: Arc<FileKept>,
    owner: Arc<str>,
    metered: AtomicBool,
    git: Option<GitWatch>,
    git_note: Mutex<Option<String>>,
    /// What a view shows of the output at most.
    view_cap: usize,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Command {
    fn ended(&self) -> Option<Ended> {
        lock(&self.ended).clone()
    }

    fn state(&self) -> CommandState {
        match self.ended() {
            Some(ended) => ended.state,
            None if lock(&self.pid).is_some() => CommandState::Running,
            None => CommandState::Starting,
        }
    }

    fn running(&self) -> bool {
        self.ended().is_none()
    }

    fn seconds(&self) -> u64 {
        self.ended()
            .map_or_else(|| self.started.elapsed(), |e| e.ran)
            .as_secs()
    }

    fn record(&self) -> Record {
        let (lines, git) = {
            let sink = lock(&self.sink);
            (sink.lines, lock(&self.git_note).clone())
        };
        let ended = self.ended();
        let state = ended
            .as_ref()
            .map_or_else(|| self.state(), |e| e.state.clone());
        // Each lock is taken and let go on its own line: `state` takes the pid's.
        let exec = lock(&self.exec).to_string();
        let pid = *lock(&self.pid);
        let start = *lock(&self.start_ticks);
        Record {
            command: self.shown.clone(),
            slot: self.slot.clone(),
            cwd: self.cwd.clone(),
            exec,
            owner: self.owner.to_string(),
            pid,
            start,
            started_at: self.started_at,
            state,
            ran_secs: ended.as_ref().map(|e| e.ran.as_secs()),
            lines: Some(lines),
            git,
        }
    }

    /// Write the record, unless another worker has taken the command over (its record names
    /// another owner): that record is the truth, and this one must not overwrite it.
    fn write_record(&self) {
        let taken = format!("gone:{}", self.owner);
        if let Ok(Some(bytes)) = self.kept.get_record(&self.run, &self.id)
            && let Ok(current) = serde_json::from_slice::<Record>(&bytes)
            && current.owner != *self.owner
            && current.owner != taken
        {
            return;
        }
        self.write_record_over();
    }

    /// Write the record whatever is there: the command starts now, and what the volume holds for
    /// its id is an earlier try's.
    fn write_record_over(&self) {
        match serde_json::to_vec(&self.record()) {
            Ok(bytes) => {
                if let Err(e) = self.kept.put_record(&self.run, &self.id, &bytes) {
                    tracing::warn!(error = %e, "cannot write the record of a command");
                }
            }
            Err(e) => tracing::warn!(error = %e, "cannot make the record of a command"),
        }
    }

    /// Ask the supervisor to stop it, saying why. `false` when it had ended already.
    fn request_stop(&self, why: &str) -> bool {
        if !self.running() {
            return false;
        }
        lock(&self.stop_why).get_or_insert_with(|| why.to_owned());
        self.stop.notify_one();
        true
    }

    /// The output as a [`ShellOutcome`], for the view `run` gives.
    fn outcome(&self, exit: Option<i32>) -> ShellOutcome {
        let sink = lock(&self.sink);
        let cap = self.view_cap;
        let truncated = sink.scrubbed_total > cap as u64;
        let mut outcome = ShellOutcome {
            exit_code: exit,
            total: sink.raw_total,
            truncated,
            ..ShellOutcome::default()
        };
        if !truncated {
            outcome.tail = sink.tail_text(cap.max(1));
            return outcome;
        }
        let head_cap = cap / 4;
        let tail_cap = cap - head_cap;
        outcome.head = output::head_of(&sink.head, head_cap).to_owned();
        outcome.tail = sink.tail_text(tail_cap);
        let shown = (outcome.head.len() + outcome.tail.len()) as u64;
        outcome.kept = Some(KeptNote {
            id: self.id.clone(),
            lines: sink.lines,
            stored: sink.written,
            cut: sink.cut,
            refused: None,
            hidden: sink.scrubbed_total.saturating_sub(shown),
            hidden_about: false,
        });
        outcome
    }
}

/// The processes this worker runs for the runs it serves.
///
/// Held by [`ToolEnv`]; clones share it. The tools hold nothing of their own: what a command is
/// lives here and in its record on the volume.
#[derive(Clone)]
pub struct Commands {
    owner: Arc<str>,
    entries: Arc<Mutex<Vec<Arc<Command>>>>,
}

impl std::fmt::Debug for Commands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Commands")
            .field("running", &self.running_count_all())
            .finish_non_exhaustive()
    }
}

impl Default for Commands {
    fn default() -> Self {
        Self::new()
    }
}

/// A token this process made at start: 128 random bits, from the standard library's hasher keys.
fn random_token() -> String {
    let state = std::collections::hash_map::RandomState::new();
    let mut token = String::with_capacity(32);
    for round in 0u8..2 {
        let mut hasher = state.build_hasher();
        hasher.write_u8(round);
        hasher.write_u32(std::process::id());
        let _ = write!(token, "{:016x}", hasher.finish());
    }
    token
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Commands {
    /// A registry for this process, with an owner token of its own.
    pub fn new() -> Self {
        Self {
            owner: random_token().into(),
            entries: Arc::default(),
        }
    }

    /// The owner token (for tests of a worker that takes a run over).
    pub fn owner(&self) -> &str {
        &self.owner
    }

    fn all(&self) -> Vec<Arc<Command>> {
        lock(&self.entries).clone()
    }

    fn of_run(&self, run: &str) -> Vec<Arc<Command>> {
        lock(&self.entries)
            .iter()
            .filter(|c| c.run == run)
            .cloned()
            .collect()
    }

    fn get(&self, run: &str, id: &str) -> Option<Arc<Command>> {
        lock(&self.entries)
            .iter()
            .find(|c| c.id == id && c.run == run)
            .cloned()
    }

    /// The commands of `run` this process holds, in the order they started, and how each stands.
    pub fn states(&self, run: &str) -> Vec<(String, CommandState)> {
        self.of_run(run)
            .iter()
            .map(|c| (c.id.clone(), c.state()))
            .collect()
    }

    fn running_count_all(&self) -> usize {
        lock(&self.entries).iter().filter(|c| c.running()).count()
    }

    /// How many commands of `run` run now.
    pub fn running_count(&self, run: &str) -> usize {
        lock(&self.entries)
            .iter()
            .filter(|c| c.run == run && c.running())
            .count()
    }

    /// The commands running in the slot `slot` of `run`: their ids and command lines, for a refusal
    /// that says what else writes there.
    pub fn running_in(&self, run: &str, slot: &str) -> Vec<(String, String)> {
        lock(&self.entries)
            .iter()
            .filter(|c| c.run == run && c.slot == slot && c.running())
            .map(|c| (c.id.clone(), c.shown.clone()))
            .collect()
    }

    /// Add `command`, when its run has fewer than `max` running. Checked and added under one lock, so
    /// two calls of a turn cannot both take the last place.
    fn reserve(&self, command: Arc<Command>, max: usize) -> Result<(), usize> {
        let mut entries = lock(&self.entries);
        let running = entries
            .iter()
            .filter(|c| c.run == command.run && c.running())
            .count();
        if running >= max {
            return Err(running);
        }
        // What ended and is not wanted any more (the records stay on the volume).
        let mut ended: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, c)| c.run == command.run && !c.running())
            .map(|(at, _)| at)
            .collect();
        while ended.len() >= KEEP_ENDED {
            entries.remove(ended.remove(0));
            for at in &mut ended {
                *at -= 1;
            }
        }
        entries.push(command);
        Ok(())
    }

    fn remove(&self, run: &str, id: &str) {
        lock(&self.entries).retain(|c| !(c.run == run && c.id == id));
    }

    /// Forget the commands of `run` that ended (its workspace is gone).
    pub fn forget_run(&self, run: &str) {
        lock(&self.entries).retain(|c| c.run != run || c.running());
    }

    /// Stop every command of `run`, saying why, and wait until they are gone (at most 10 s). Never
    /// fails; what is already over is left alone. For the end of the run, a park on the person and
    /// the janitor.
    pub async fn stop_run(&self, run: &str, why: &str) {
        let mine: Vec<Arc<Command>> = self
            .of_run(run)
            .into_iter()
            .filter(|c| c.request_stop(why))
            .collect();
        wait_all_ended(&mine, Duration::from_secs(10)).await;
    }

    /// Stop every command of every run: this process is stopping.
    pub async fn stop_all(&self, why: &str) {
        let mine: Vec<Arc<Command>> = self
            .all()
            .into_iter()
            .filter(|c| c.request_stop(why))
            .collect();
        wait_all_ended(&mine, Duration::from_secs(10)).await;
    }

    /// One look at the runs that have commands: a run that is over (finished, failed, cancelled or
    /// unknown to the store) has them stopped, and a command whose record names another owner (a
    /// worker took the run over) is stopped too. Returns how many it stopped.
    pub async fn sweep(&self, store: &dyn Store) -> usize {
        let mut stopped = 0;
        let mut runs: Vec<String> = Vec::new();
        for command in self.all().iter().filter(|c| c.running()) {
            if !runs.contains(&command.run) {
                runs.push(command.run.clone());
            }
        }
        for run in runs {
            match crate::janitor::run_is_over(store, &run).await {
                Ok(true) => {
                    let before = self.running_count(&run);
                    self.stop_run(&run, "the run ended").await;
                    stopped += before;
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(%run, error = %adam_error::report(&e), "cannot ask the store about a run with commands; they stay");
                }
            }
            // Another worker took the run over: its record names it, and this process must not go
            // on running what is no longer its own.
            for command in self.of_run(&run).into_iter().filter(|c| c.running()) {
                if let Ok(Some(bytes)) = command.kept.get_record(&command.run, &command.id)
                    && let Ok(record) = serde_json::from_slice::<Record>(&bytes)
                    && record.owner != *self.owner
                    && command.request_stop("another worker took the run over")
                {
                    stopped += 1;
                }
            }
        }
        stopped
    }

    /// [`sweep`](Self::sweep) every `every` until `stop` fires, and then stop everything: the
    /// worker's component.
    ///
    /// # Errors
    ///
    /// Never; the type is the component's.
    pub async fn sweep_loop(
        self,
        store: DynStore,
        stop: CancellationToken,
        every: Duration,
    ) -> Result<(), adam_error::BoxError> {
        loop {
            tokio::select! {
                () = stop.cancelled() => {
                    self.stop_all("the worker is stopping").await;
                    return Ok(());
                }
                () = tokio::time::sleep(every) => {}
            }
            let stopped = self.sweep(store.as_ref()).await;
            if stopped > 0 {
                tracing::info!(stopped, "stopped background commands of runs that are over");
            }
        }
    }

    /// Whether the volume holds a record of a command of `run` that says it runs and that this
    /// process does not hold: a worker that is gone left it, and [`settle_records`](Self::settle_records)
    /// has something to do.
    fn has_stale(&self, kept: &FileKept, run: &str) -> bool {
        let Ok(records) = kept.records(run) else {
            return false;
        };
        records.iter().any(|(id, bytes)| {
            self.get(run, id).is_none()
                && serde_json::from_slice::<Record>(bytes).is_ok_and(|r| r.state.is_running())
        })
    }

    /// Settle what an earlier owner left for `run`: a record that says a command runs, and that this
    /// process does not hold, is of a worker that is gone (or that lost the run). Its process is
    /// killed, by its exec id and, when the pid still has the start time the record names, by pid;
    /// the record then says `gone`.
    async fn settle_records(&self, kept: &FileKept, run: &str, session: &dyn EnvSession) {
        let records = match kept.records(run) {
            Ok(records) => records,
            Err(e) => {
                tracing::warn!(%run, error = %e, "cannot read the records of the commands of a run");
                return;
            }
        };
        for (id, bytes) in records {
            if self.get(run, &id).is_some() {
                continue;
            }
            let Ok(mut record) = serde_json::from_slice::<Record>(&bytes) else {
                continue;
            };
            if !record.state.is_running() {
                continue;
            }
            kill_recorded(&record, session).await;
            // The record is this process's now: the process that wrote it, if it is still alive
            // somewhere, no longer writes over it.
            record.owner = format!("gone:{}", self.owner);
            record.state = CommandState::Gone {
                why: "the worker that ran it restarted; start it again".to_owned(),
            };
            if let Ok(bytes) = serde_json::to_vec(&record)
                && let Err(e) = kept.put_record(run, &id, &bytes)
            {
                tracing::warn!(%run, error = %e, "cannot write the record of a command");
            }
        }
    }
}

async fn wait_all_ended(commands: &[Arc<Command>], limit: Duration) {
    let deadline = Instant::now() + limit;
    while commands.iter().any(|c| c.running()) && Instant::now() < deadline {
        tokio::time::sleep(POLL).await;
    }
}

/// The start time of a process (field 22 of `/proc/<pid>/stat`), when there is such a process.
fn proc_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parentheses: cut after its last `) `.
    let rest = stat.rsplit_once(") ")?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// The pids of the processes that are members of the process group `group`.
fn group_members(group: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(rest) = stat.rsplit_once(") ").map(|(_, rest)| rest) else {
            continue;
        };
        if rest
            .split_whitespace()
            .nth(2)
            .and_then(|g| g.parse::<u32>().ok())
            == Some(group)
            && pid != std::process::id()
        {
            found.push(pid);
        }
    }
    found
}

/// SIGKILL these processes (the shell's `kill`: this crate forbids `unsafe`).
async fn kill_pids(pids: &[u32]) {
    if pids.is_empty() {
        return;
    }
    let list: Vec<String> = pids.iter().map(u32::to_string).collect();
    let _ = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("kill -s KILL -- {}", list.join(" ")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

/// What a command's process group left after the leader ended: killed (they are ours: a group id
/// is not reused while a member lives).
async fn kill_leftovers(group: u32) {
    kill_pids(&group_members(group)).await;
}

/// Kill what a record says runs: through the environment, by exec id, and the process this
/// machine holds, by pid when it is still the process the record names (the same start time).
async fn kill_recorded(record: &Record, session: &dyn EnvSession) {
    session.kill(&ExecId::new(record.exec.clone())).await;
    if let (Some(pid), Some(start)) = (record.pid, record.start)
        && proc_start(pid) == Some(start)
    {
        kill_group(Some(pid)).await;
        kill_leftovers(pid).await;
    }
}

async fn pump(mut reader: impl AsyncRead + Unpin, command: Arc<Command>) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => lock(&command.sink).feed(&chunk[..n]),
        }
    }
}

/// What ended the process.
enum Ending {
    Exit(std::io::Result<std::process::ExitStatus>),
    Stop(String),
}

/// Own the process until it ends: by itself, by a stop, or at its lifetime. Then settle the output,
/// look at git, write the record and say it ended.
async fn supervise(
    command: Arc<Command>,
    mut child: tokio::process::Child,
    readers: Vec<tokio::task::JoinHandle<()>>,
    lifetime: Duration,
) {
    let pid = child.id();
    let ending = tokio::select! {
        biased;
        () = command.stop.notified() => {
            Ending::Stop(lock(&command.stop_why).clone().unwrap_or_else(|| "stopped".to_owned()))
        }
        waited = child.wait() => Ending::Exit(waited),
        () = tokio::time::sleep(lifetime) => Ending::Stop(format!(
            "its lifetime of {} s ended (BACKGROUND_COMMAND_MAX_SECS)",
            lifetime.as_secs()
        )),
    };
    let state = match ending {
        Ending::Exit(waited) => {
            // What the leader left in its group is the command's too.
            if let Some(group) = pid {
                kill_leftovers(group).await;
            }
            CommandState::Exited {
                code: waited.ok().and_then(|status| status.code()),
            }
        }
        Ending::Stop(why) => {
            kill_group(pid).await;
            let _ = child.kill().await;
            let exec = lock(&command.exec).clone();
            command.session.kill(&exec).await;
            if let Some(group) = pid {
                kill_leftovers(group).await;
            }
            CommandState::Stopped { why }
        }
    };
    let ran = command.started.elapsed();
    // The pipes: a grandchild that outlives the process may hold them open.
    let drained = tokio::time::timeout(PIPE_GRACE, async {
        for reader in &readers {
            while !reader.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    })
    .await;
    if drained.is_err() {
        for reader in &readers {
            reader.abort();
        }
    }
    lock(&command.sink).finish();
    command.kept.end_live(&command.run, &command.id);
    if let Some(watch) = &command.git {
        let after = Snapshot::of_slot(&watch.slot).await;
        let same = after
            .as_ref()
            .is_some_and(|after| watch.before.same_git_as(after));
        if !same {
            *lock(&command.git_note) = Some(
                "git was changed while it ran (HEAD, a ref, `.git` or the git configuration differ \
                 from when it started); it is not undone here, because the run's own commits move \
                 them too: look with `run_command git status`"
                    .to_owned(),
            );
        }
    }
    *lock(&command.ended) = Some(Ended { state, ran });
    command.write_record();
}

/// `text` cut to `max` bytes on a character boundary, with a marker when it was.
fn cut_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} …", &text[..end])
}

/// How the state reads in a line of `list_commands` and in a view.
fn how(state: &CommandState, secs: Option<u64>) -> String {
    let after = secs.map_or_else(String::new, |s| format!(" after {s} s"));
    match state {
        CommandState::Starting => "starting".to_owned(),
        CommandState::Running => format!("running {} s", secs.unwrap_or(0)),
        CommandState::Exited { code: Some(code) } => format!("exited {code}{after}"),
        CommandState::Exited { code: None } => format!("ended by a signal{after}"),
        CommandState::Stopped { why } => format!("stopped: {why}{after}"),
        CommandState::Gone { why } => format!("gone: {why}"),
    }
}

/// What a finished command says first, for the view of a result (`run` says `exit code 0`).
fn verdict(state: &CommandState) -> String {
    match state {
        CommandState::Exited { code: Some(code) } => format!("exit code {code}"),
        CommandState::Exited { code: None } => "killed by a signal".to_owned(),
        CommandState::Stopped { why } => format!("stopped: {why}"),
        CommandState::Gone { why } => format!("gone: {why}"),
        CommandState::Starting | CommandState::Running => "running".to_owned(),
    }
}

/// The result of a command that ended: `run`'s view of it (the exit, the head and the tail of the
/// output, and how to read the rest), and what happened to git.
fn finished_view(command: &Command, ended: &Ended) -> String {
    let code = match &ended.state {
        CommandState::Exited { code } => *code,
        _ => None,
    };
    let outcome = command.outcome(code);
    let mut text = format!("$ {}\n{}\n", command.shown, verdict(&ended.state));
    text.push_str(&output::section(&outcome));
    if outcome.kept.is_none() {
        // Short output is shown whole and still kept: say where, so that list_commands is not a
        // surprise.
        let lines = lock(&command.sink).lines;
        let _ = writeln!(
            text,
            "[{lines} lines are kept as {}; read_output {{\"id\":\"{}\"}} reads them]",
            command.id, command.id
        );
    }
    if let Some(note) = lock(&command.git_note).as_ref() {
        let _ = writeln!(text, "Note: {note}.");
    }
    text
}

/// What `start_command` returns while the command still runs: its id and how to use it, how long it
/// has run, and the first page of its output so far.
async fn running_view(env: &ToolEnv, command: &Command) -> String {
    let id = &command.id;
    let (lines, matched) = {
        let sink = lock(&command.sink);
        (sink.lines, sink.matched)
    };
    let mut text = format!(
        "$ {}\n[running as {id}; read_output {{\"id\":\"{id}\"}} reads its output, stop_command {{\"id\":\"{id}\"}} stops it]\n",
        command.shown
    );
    let _ = writeln!(
        text,
        "running for {} s, {lines} lines so far{}",
        command.seconds(),
        if matched {
            " (a line had the text waited for)"
        } else {
            ""
        }
    );
    text.push_str("--- output so far ---\n");
    match adam_llm_agent::KeptOutputs::page(
        &*env.kept,
        &command.run,
        id,
        1,
        adam_llm_agent::paging::PAGE_LINES,
    )
    .await
    {
        Ok(page) => text.push_str(&page),
        Err(e) => {
            let _ = write!(text, "(the output cannot be read now: {e})");
        }
    }
    text
}

/// Say what the time a command ran was, once: the sandbox's meter reports it at the call that sees
/// the end (a stop by the reaper is not metered).
fn meter_once(ctx: &ToolCtx, command: &Command) {
    let Some(ended) = command.ended() else { return };
    if command.metered.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(meter) = Meter::of(&*command.session) {
        meter.report_command(ctx, ended.ran);
    }
}

/// Wait for the command up to `wait`, for a line with `wait_for`, or for its end; then the view.
async fn view_after(
    env: &ToolEnv,
    ctx: &ToolCtx,
    command: &Arc<Command>,
    wait: Duration,
) -> Outcome {
    let deadline = Instant::now() + wait;
    loop {
        if !command.running() || lock(&command.sink).matched {
            break;
        }
        if ctx.is_cancelled() {
            return Err(cancelled("the command was started; it stops with the run"));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        tokio::time::sleep(POLL.min(left)).await;
    }
    match command.ended() {
        Some(ended) => {
            meter_once(ctx, command);
            let mut output = ToolOutput::text(finished_view(command, &ended));
            if let CommandState::Exited { code } = &ended.state
                && *code != Some(0)
            {
                output = output.with_repeat_key(match code {
                    Some(code) => format!("exit:{code}"),
                    None => "signal".to_owned(),
                });
            }
            Ok(output)
        }
        None => Ok(ToolOutput::text(running_view(env, command).await)),
    }
}

/// Start a command that keeps running after this call: a development server, a watcher, a build
/// you look at later. It runs in the workspace's environment like `run`, and the files it writes
/// stay. You get its id, the output so far and, if it ends within the wait, its exit code and
/// output. Read more of its output with read_output (its last line says where the next page starts:
/// `[running: N lines so far; next page: offset=N+1]`), list what runs with list_commands and stop it
/// with stop_command. The call waits `wait` seconds (default 5, at most 120), or until a line of
/// the output has the text `wait_for`, or until the command ends. A command ends when you stop it,
/// when the task ends, when you ask the person a question, after the deployment's time limit, and when
/// the run's pod is made again for a new repository; the answer names the limit of how many run at
/// once. A port it opens is reachable from the run's other commands on localhost, never from
/// outside the run; the person cannot reach it, and nothing is published. Without a pod, all runs of
/// the worker share one network, so pick an unusual port. For a command that ends by itself use run;
/// for the project's checks, run_checks. Example: {"command":"npm run dev","wait_for":"ready"}
#[tool(label = "Start a command that keeps running", icon = "execute")]
pub async fn start_command(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Shell command, run with `bash -lc` (`sh -lc` without bash) in the worktree, in the workspace's environment
    command: String,
    /// Optional sub-directory of the worktree to run in (relative, inside the worktree)
    cwd: Option<String>,
    /// The slot to run in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
    /// Seconds to wait before the call returns, 1 to 120 (default 5)
    wait: Option<i64>,
    /// Return as soon as a line of the output has this text (one line, at most 200 characters)
    wait_for: Option<String>,
) -> Outcome {
    let Some(command) = non_empty(&command) else {
        return Ok(ToolOutput::error("command is required"));
    };
    let wait = match wait {
        None => DEFAULT_WAIT_SECS,
        Some(secs) if (1..=MAX_WAIT_SECS).contains(&secs) => secs,
        Some(secs) => {
            return Ok(ToolOutput::error(format!(
                "wait is {secs}: it is from 1 to {MAX_WAIT_SECS} seconds; leave it out for {DEFAULT_WAIT_SECS}"
            )));
        }
    };
    let wait = Duration::from_secs(wait.unsigned_abs());
    let wait_for = match wait_for.as_deref().and_then(non_empty) {
        Some(text) if text.contains('\n') || text.chars().count() > MAX_WAIT_FOR_CHARS => {
            return Ok(ToolOutput::error(format!(
                "wait_for is one line of at most {MAX_WAIT_FOR_CHARS} characters"
            )));
        }
        other => other.map(str::to_owned),
    };
    let id = output_id_of(ctx);
    let run = ctx.root_run_id().to_string();

    // A step that runs again finds its command: this process owns it, and nothing is started twice.
    if let Some(existing) = env.commands.get(&run, &id) {
        if let Some(text) = &wait_for {
            lock(&existing.sink).watch = Some(text.clone());
        }
        return view_after(&env, ctx, &existing, wait).await;
    }

    // A slot the run may write: a command changes files, and never in a reference.
    let slot = match env.slot_to_write(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    let dir = match resolve_cwd(slot.path(), cwd.as_deref().and_then(non_empty)) {
        Ok(dir) => dir,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    let environment = env.session(ctx).await?;
    // What an earlier try of this call, or a worker that is gone, left running under any id.
    env.commands
        .settle_records(&env.kept, &run, &*environment)
        .await;
    if let Ok(Some(bytes)) = env.kept.get_record(&run, &id)
        && let Ok(record) = serde_json::from_slice::<Record>(&bytes)
    {
        // The earlier try of this very call: its process must not live beside the new one.
        kill_recorded(&record, &*environment).await;
    }

    let Some(before) = Snapshot::of_slot(&slot).await else {
        return Ok(ToolOutput::error(
            "Cannot read the state of the worktree, so nothing was started. Try again; if it persists, tell the person.",
        ));
    };
    if let Some(refusal) = before.refusal() {
        return Ok(ToolOutput::error(refusal));
    }

    let redactor = &env.redactor;
    let shown = cut_text(&redactor.scrub(command), MAX_COMMAND_BYTES);
    let file = match env.kept.open_live(&run, &id, MIN_ROOM) {
        Ok(file) => file,
        Err(e) => {
            return Ok(ToolOutput::error(format!(
                "Nothing was started: its output cannot be kept ({e})."
            )));
        }
    };
    let spec = shell_spec(&dir, command).background();
    let prepared = match environment.prepare(&spec) {
        Ok(prepared) => prepared,
        Err(e) => {
            env.kept.end_live(&run, &id);
            return Err(run_error(redactor, &super::shell::RunError::Prepare(e)));
        }
    };
    let view_cap = env.settings.check_output_tail.clamp(256, MEMORY_KEPT);
    let entry = Arc::new(Command {
        id: id.clone(),
        run: run.clone(),
        shown: shown.clone(),
        slot: slot.dir().to_owned(),
        cwd: cwd
            .as_deref()
            .and_then(non_empty)
            .unwrap_or_default()
            .to_owned(),
        exec: Mutex::new(prepared.exec.clone()),
        started: Instant::now(),
        started_at: now_unix(),
        pid: Mutex::new(None),
        start_ticks: Mutex::new(None),
        ended: Mutex::new(None),
        stop: Notify::new(),
        stop_why: Mutex::new(None),
        sink: Mutex::new(Sink {
            scrubber: StreamScrubber::new(redactor.margin()),
            folder: LineFolder::new(),
            redactor: redactor.clone(),
            kept: env.kept.clone(),
            run: run.clone(),
            id: id.clone(),
            file: Some(file),
            full: false,
            written: 0,
            lines: 0,
            cut: false,
            refused: None,
            cap: env.settings.background_output_max,
            head: String::new(),
            tail: Vec::new(),
            raw_total: 0,
            scrubbed_total: 0,
            next_check: ROOM_CHECK_EVERY,
            watch: wait_for,
            matched: false,
        }),
        session: environment.clone(),
        kept: env.kept.clone(),
        owner: env.commands.owner.clone(),
        metered: AtomicBool::new(false),
        git: Some(GitWatch {
            slot: slot.clone(),
            before,
        }),
        git_note: Mutex::new(None),
        view_cap,
    });
    let max = env.settings.background_commands_max;
    if let Err(running) = env.commands.reserve(entry.clone(), max) {
        env.kept.end_live(&run, &id);
        return Ok(ToolOutput::error(format!(
            "{running} commands of this run are running, and at most {max} may run at once: stop one \
             with stop_command (list_commands says which) and start this one again."
        )));
    }

    ctx.emit_progress(format!("starting: {shown}")).await;
    // Written before the process exists: a worker that dies in between leaves the exec id to kill.
    entry.write_record_over();
    let mut child = match spawn_prepared(&prepared) {
        Ok(child) => child,
        Err(e) => {
            env.commands.remove(&run, &id);
            env.kept.end_live(&run, &id);
            return Err(run_error(redactor, &super::shell::RunError::Spawn(e)));
        }
    };
    let pid = child.id();
    *lock(&entry.pid) = pid;
    *lock(&entry.start_ticks) = pid.and_then(proc_start);
    entry.write_record_over();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(tokio::spawn(pump(out, entry.clone())));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(tokio::spawn(pump(err, entry.clone())));
    }
    tokio::spawn(supervise(
        entry.clone(),
        child,
        readers,
        env.settings.background_command_max,
    ));
    view_after(&env, ctx, &entry, wait).await
}

/// Stop a command started with start_command: its whole process group is killed. You get how it
/// ended and the last 4 KiB of its output, and the id still reads the rest with read_output. It is
/// safe to call again: for a command that has ended it says how it ended. Example:
/// {"id":"o-3f9c2a17b0d4e8a1"}
#[tool(label = "Stop a command", icon = "execute")]
pub async fn stop_command(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// The command's id, from start_command or list_commands
    id: String,
) -> Outcome {
    let id = id.trim();
    if !is_output_id(id) {
        return Ok(ToolOutput::error(format!(
            "`{id}` is not a command id (it is o- and 16 hex characters, in the result of start_command; list_commands lists them)"
        )));
    }
    let run = ctx.root_run_id().to_string();
    let Some(command) = env.commands.get(&run, id) else {
        // Not held here: a record of a worker that is gone, or nothing.
        if env.commands.has_stale(&env.kept, &run) {
            let session = env.session(ctx).await?;
            env.commands
                .settle_records(&env.kept, &run, &*session)
                .await;
        }
        return match env.kept.get_record(&run, id) {
            Ok(Some(bytes)) => match serde_json::from_slice::<Record>(&bytes) {
                Ok(record) => Ok(ToolOutput::text(record_view(&record, id))),
                Err(_) => Ok(ToolOutput::error(format!(
                    "the record of `{id}` cannot be read"
                ))),
            },
            _ => Ok(ToolOutput::error(format!(
                "no command `{id}` was started in this run (list_commands lists them)"
            ))),
        };
    };
    command.request_stop("stop_command");
    let deadline = Instant::now() + END_WAIT;
    while command.running() && Instant::now() < deadline {
        if ctx.is_cancelled() {
            return Err(cancelled("the command is being stopped"));
        }
        tokio::time::sleep(POLL).await;
    }
    let Some(ended) = command.ended() else {
        return Ok(ToolOutput::error(format!(
            "`{id}` did not end within {} s of being stopped; look at list_commands, and try again",
            END_WAIT.as_secs()
        )));
    };
    meter_once(ctx, &command);
    let mut text = format!(
        "$ {}\n{} (it ran {} s)\n",
        command.shown,
        verdict(&ended.state),
        ended.ran.as_secs()
    );
    let (tail, lines, cut) = {
        let sink = lock(&command.sink);
        (sink.tail_text(STOP_VIEW_BYTES), sink.lines, sink.cut)
    };
    let _ = writeln!(text, "--- the last output ---");
    text.push_str(&output::escaped(&tail));
    if !tail.ends_with('\n') && !tail.is_empty() {
        text.push('\n');
    }
    if let Some(note) = lock(&command.git_note).as_ref() {
        let _ = writeln!(text, "Note: {note}.");
    }
    let _ = writeln!(
        text,
        "[more: {lines} lines are kept as {id}{}; read_output {{\"id\":\"{id}\"}} reads them page by page]",
        if cut { " (the output was cut)" } else { "" }
    );
    Ok(ToolOutput::text(text))
}

/// The view of a command this process does not hold: from its record.
fn record_view(record: &Record, id: &str) -> String {
    let mut text = format!(
        "$ {}\n{}\n",
        record.command,
        how(&record.state, record.ran_secs)
    );
    if let Some(note) = &record.git {
        let _ = writeln!(text, "Note: {note}.");
    }
    let _ = writeln!(
        text,
        "[more: {} lines are kept as {id}; read_output {{\"id\":\"{id}\"}} reads them page by page]",
        record.lines.unwrap_or(0)
    );
    text
}

/// List the commands this task started with start_command, in the order they started: each one's
/// id, whether it runs or how it ended, the lines of output it has, its slot and the command. Read
/// the output of one with read_output.
#[tool(label = "List the commands", icon = "execute")]
pub async fn list_commands(env: State<ToolEnv>, ctx: &ToolCtx) -> Outcome {
    let run = ctx.root_run_id().to_string();
    // The environment is asked for only when there is something to settle in it.
    if env.commands.has_stale(&env.kept, &run) {
        let session = env.session(ctx).await?;
        env.commands
            .settle_records(&env.kept, &run, &*session)
            .await;
    }
    let mut rows: Vec<(u64, String, String)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for command in env.commands.of_run(&run) {
        meter_once(ctx, &command);
        let secs = command.seconds();
        let state = command.state();
        let lines = lock(&command.sink).lines;
        let mut line = format!(
            "{} {}, {lines} lines, slot {}",
            command.id,
            how(&state, Some(secs)),
            command.slot
        );
        if !command.cwd.is_empty() {
            let _ = write!(line, ", cwd {}", command.cwd);
        }
        let _ = write!(line, ": {}", command.shown);
        if lock(&command.git_note).is_some() {
            line.push_str(" (git changed while it ran)");
        }
        seen.push(command.id.clone());
        rows.push((command.started_at, command.id.clone(), line));
    }
    if let Ok(records) = env.kept.records(&run) {
        for (id, bytes) in records {
            if seen.contains(&id) {
                continue;
            }
            let Ok(record) = serde_json::from_slice::<Record>(&bytes) else {
                continue;
            };
            let mut line = format!(
                "{id} {}, {} lines, slot {}",
                how(&record.state, record.ran_secs),
                record.lines.unwrap_or(0),
                record.slot
            );
            if !record.cwd.is_empty() {
                let _ = write!(line, ", cwd {}", record.cwd);
            }
            let _ = write!(line, ": {}", record.command);
            rows.push((record.started_at, id, line));
        }
    }
    rows.sort();
    let running = env.commands.running_count(&run);
    let max = env.settings.background_commands_max;
    if rows.is_empty() {
        return Ok(ToolOutput::text(format!(
            "No command was started in this run (at most {max} may run at once)."
        )));
    }
    let mut text = format!("Commands of this run ({running} running, at most {max} at once):\n");
    for (_, _, line) in rows {
        text.push_str(&line);
        text.push('\n');
    }
    Ok(ToolOutput::text(text))
}

/// A line for a refusal in a slot that a background command may be writing: the commands running
/// there, or nothing.
pub(crate) fn slot_note(commands: &Commands, run: &str, slot: &str) -> String {
    let running = commands.running_in(run, slot);
    if running.is_empty() {
        return String::new();
    }
    let mut note = String::from(
        "\nCommands that keep running are writing in this slot, and what they write is not undone: ",
    );
    let listed: Vec<String> = running
        .iter()
        .map(|(id, command)| format!("{id} ({command})"))
        .collect();
    note.push_str(&listed.join(", "));
    note.push_str(". Stop one with stop_command when it is the cause.");
    note
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_start_time_is_read_for_a_live_process_and_not_for_a_dead_one() {
        let me = std::process::id();
        if cfg!(target_os = "linux") {
            assert!(proc_start(me).is_some());
            assert_eq!(proc_start(me), proc_start(me));
            assert!(group_members(me).is_empty() || group_members(me).iter().all(|p| *p != me));
        }
        assert_eq!(proc_start(u32::MAX - 1), None);
    }

    #[test]
    fn states_read_as_the_list_says_them() {
        assert_eq!(how(&CommandState::Running, Some(312)), "running 312 s");
        assert_eq!(
            how(&CommandState::Exited { code: Some(1) }, Some(40)),
            "exited 1 after 40 s"
        );
        assert_eq!(
            how(
                &CommandState::Stopped {
                    why: "stop_command".into()
                },
                Some(3)
            ),
            "stopped: stop_command after 3 s"
        );
        assert_eq!(
            how(
                &CommandState::Gone {
                    why: "the worker that ran it restarted; start it again".into()
                },
                None
            ),
            "gone: the worker that ran it restarted; start it again"
        );
    }

    #[test]
    fn a_record_round_trips_and_names_its_state_beside_the_rest() {
        let record = Record {
            command: "npm run dev".into(),
            slot: "app".into(),
            cwd: "web".into(),
            exec: "bg-kp-1-1".into(),
            owner: "o".into(),
            pid: Some(7),
            start: Some(9),
            started_at: 1,
            state: CommandState::Exited { code: Some(2) },
            ran_secs: Some(3),
            lines: Some(4),
            git: None,
        };
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["state"], "exited");
        assert_eq!(json["code"], 2);
        let back: Record = serde_json::from_value(json).unwrap();
        assert_eq!(back.state, record.state);
    }

    #[test]
    fn a_long_command_is_cut_for_the_record_on_a_character_boundary() {
        let cut = cut_text(&"é".repeat(2000), MAX_COMMAND_BYTES);
        assert!(cut.len() <= MAX_COMMAND_BYTES + 4 && cut.ends_with('…'));
        assert_eq!(cut_text("short", MAX_COMMAND_BYTES), "short");
    }

    #[test]
    fn the_owner_tokens_of_two_registries_differ() {
        assert_ne!(Commands::new().owner(), Commands::new().owner());
        assert_eq!(Commands::new().owner().len(), 32);
    }
}
