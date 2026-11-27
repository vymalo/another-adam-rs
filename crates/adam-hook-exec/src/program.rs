//! The child process: start it, say hello, ask it one question per call, and kill and restart it
//! when it stops answering (ADR 0050, decision 15, "Fail closed").
//!
//! ```mermaid
//! stateDiagram-v2
//!     [*] --> Starting: spawn in a private directory, write hello
//!     Starting --> Ready: the program answers adam-hooks/1
//!     Starting --> [*]: no hello, or another protocol (exit 78 in a binary)
//!     Ready --> Asking: one event queued to the writer, one deadline
//!     Asking --> Ready: a reply in time
//!     Asking --> Missed: no reply in time
//!     Missed --> Ready: fewer than three in a row
//!     Ready --> Down: a broken line, a closed output, or a dead child
//!     Missed --> Down: three in a row
//!     Down --> Starting: the backoff elapsed
//!     Ready --> [*]: shutdown
//! ```
//!
//! **No I/O happens under the state lock.** A writer thread owns the child's stdin and takes the
//! lines the adapter queues; a reader thread owns its stdout and hands each reply to the request
//! that waits for it. A program that stops reading its input fills the queue and is treated as
//! broken, so a slow program cannot hold the loop.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::{debug, warn};

use crate::config::{HELLO_DEADLINE_MS, SAFE_VARS, Scrub};
use crate::error::Error;
use crate::protocol::{Hello, LINE_MAX, Mode, Outgoing, PROTOCOL, Reply, VERSION};

/// How many deadlines in a row kill the program (ADR 0050, decision 15).
const MISSES_MAX: u32 = 3;

/// How many lines may wait for the writer thread. A program that lets this fill is not reading,
/// and is treated as broken instead of growing this process's memory.
const WRITE_QUEUE: usize = 64;

/// The first wait before a killed program is restarted; it doubles up to [`BACKOFF_MAX`].
const BACKOFF_START: Duration = Duration::from_secs(1);

/// The longest wait between restarts.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// How long a killed program is given to exit on its own before its group is killed.
const EXIT_GRACE: Duration = Duration::from_secs(1);

/// What one program is: fixed for the life of the adapter.
pub(crate) struct Spec {
    pub(crate) name: String,
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) points: crate::protocol::WirePoints,
    pub(crate) scrub: Arc<Scrub>,
}

/// A line the program wrote.
enum Incoming {
    Hello(Hello),
    /// Boxed: a reply is much larger than a hello, and the channel carries both.
    Reply(Box<Reply>),
}

/// Why a request got no answer.
enum Fault {
    /// The program wrote something the protocol does not allow: `Invalid`, the call is refused.
    Malformed(String),
    /// The program is gone: `Transient`, the call is retried and the program restarted.
    Closed(String),
}

/// What a waiting request receives: the answer, or why there is none.
type Answer = Result<Incoming, Fault>;

/// The state the two threads share with the adapter.
struct Shared {
    program: String,
    scrub: Arc<Scrub>,
    /// The requests waiting for a reply, by id.
    pending: Mutex<HashMap<u64, SyncSender<Answer>>>,
    /// The hello's channel, taken once.
    hello: Mutex<Option<SyncSender<Answer>>>,
    /// The program closed its output: nothing more will be read from it.
    closed: AtomicBool,
}

impl Shared {
    fn new(program: String, scrub: Arc<Scrub>, hello: Option<SyncSender<Answer>>) -> Self {
        Shared {
            program,
            scrub,
            pending: Mutex::new(HashMap::new()),
            hello: Mutex::new(hello),
            closed: AtomicBool::new(false),
        }
    }

    fn guard<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        // A panic while the map was held leaves it usable: the entries are plain channels.
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn hello(&self, hello: Hello) {
        let taken = Self::guard(&self.hello).take();
        if let Some(tx) = taken {
            let _ = tx.send(Ok(Incoming::Hello(hello)));
        }
    }

    fn reply(&self, reply: Box<Reply>) {
        let Ok(id) = reply.id.parse::<u64>() else {
            warn!(program = %self.program, id = %reply.id, "a reply names no request");
            return;
        };
        let waiting = Self::guard(&self.pending).remove(&id);
        match waiting {
            Some(tx) => {
                let _ = tx.send(Ok(Incoming::Reply(reply)));
            }
            // A reply that arrives after its deadline is dropped: the request already failed.
            None => debug!(program = %self.program, id, "a late reply is dropped"),
        }
    }

    /// Every waiting request gets `fault`: the program is gone or wrote something impossible.
    fn fail_all(&self, fault: Fault) {
        let waiting: Vec<_> = Self::guard(&self.pending)
            .drain()
            .map(|(_, tx)| tx)
            .collect();
        for tx in waiting {
            let _ = tx.send(Err(match &fault {
                Fault::Malformed(reason) => Fault::Malformed(reason.clone()),
                Fault::Closed(reason) => Fault::Closed(reason.clone()),
            }));
        }
        let hello = Self::guard(&self.hello).take();
        if let Some(tx) = hello {
            let _ = tx.send(Err(fault));
        }
    }

    /// Nothing more will be read: wake the waiters and remember it, so the next call restarts.
    fn close(&self, fault: Fault) {
        self.closed.store(true, Ordering::SeqCst);
        self.fail_all(fault);
    }
}

/// Read the program's lines and hand each to the request that waits for it.
fn read_lines(stdout: ChildStdout, shared: Arc<Shared>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        // A line longer than the protocol's bound is refused without growing this vector: the
        // reader is capped, so a program cannot exhaust this process's memory with one line.
        match reader
            .by_ref()
            .take(LINE_MAX as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let content = if line.last() == Some(&b'\n') {
            line.len() - 1
        } else {
            line.len()
        };
        if content > LINE_MAX {
            shared.close(Fault::Malformed(format!(
                "a line longer than {LINE_MAX} bytes"
            )));
            return;
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            shared.close(Fault::Malformed("a line that is not JSON".to_owned()));
            return;
        };
        // The kind is copied out first: the arms move `value` into `from_value`.
        let kind = value.get("type").and_then(Value::as_str).map(str::to_owned);
        match kind.as_deref() {
            Some("hello") => match serde_json::from_value::<Hello>(value) {
                Ok(hello) => shared.hello(hello),
                Err(e) => {
                    // The detail is logged, not carried: a serde error quotes the line, and a
                    // hook's reason is recorded and read by the model.
                    warn!(program = %shared.program, "{}", (shared.scrub.as_ref())(&e.to_string()));
                    shared.close(Fault::Malformed(
                        "a hello this build cannot read".to_owned(),
                    ));
                    return;
                }
            },
            Some("reply") => match serde_json::from_value::<Reply>(value) {
                Ok(reply) => shared.reply(Box::new(reply)),
                Err(e) => {
                    warn!(program = %shared.program, "{}", (shared.scrub.as_ref())(&e.to_string()));
                    shared.close(Fault::Malformed(
                        "a reply this build cannot read".to_owned(),
                    ));
                    return;
                }
            },
            // An unknown message is ignored: a program may speak more than this build knows.
            _ => debug!(program = %shared.program, "a line of an unknown kind is ignored"),
        }
    }
    shared.close(Fault::Closed("the program closed its output".to_owned()));
}

/// Write the lines the adapter queues, and close the program when its stdin refuses one.
fn write_lines(mut stdin: ChildStdin, lines: Receiver<Vec<u8>>, shared: Arc<Shared>) {
    for line in lines {
        if let Err(e) = stdin.write_all(&line).and_then(|()| stdin.flush()) {
            shared.close(Fault::Closed(format!(
                "the program's stdin refused a line: {e}"
            )));
            return;
        }
    }
}

/// Log the program's stderr, one scrubbed line at a time.
fn read_stderr(stderr: ChildStderr, program: String, scrub: Arc<Scrub>) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        debug!(program = %program, "{}", scrub.as_ref()(text.trim_end()));
    }
}

/// One running program, with its state across restarts.
pub(crate) struct Program {
    spec: Arc<Spec>,
    deadline: Duration,
    /// The empty private directory the program runs in.
    cwd: PathBuf,
    /// Whether this adapter created `cwd` (and so removes it when it goes).
    own_cwd: bool,
    state: Mutex<State>,
}

struct State {
    child: Option<Child>,
    /// The queue the writer thread takes lines from.
    writer: Option<SyncSender<Vec<u8>>>,
    shared: Arc<Shared>,
    next_id: u64,
    misses: u32,
    backoff: Duration,
    down_until: Option<Instant>,
}

impl Program {
    /// Start the program in `cwd` (a private directory this adapter makes when it is not given)
    /// and say hello.
    ///
    /// # Errors
    ///
    /// [`Error::Spawn`] when it cannot start or the directory cannot be made, [`Error::Hello`] when
    /// it does not answer in time, and [`Error::Protocol`] when it names another protocol.
    pub(crate) fn start(
        spec: Arc<Spec>,
        deadline: Duration,
        cwd: Option<PathBuf>,
    ) -> Result<Self, Error> {
        let (cwd, own_cwd) = match cwd {
            Some(dir) => (dir, false),
            None => (private_dir(&spec.name)?, true),
        };
        let (child, stdin, shared) = launch(&spec, &cwd)?;
        let writer = start_writer(stdin, &shared);
        Ok(Program {
            spec,
            deadline,
            cwd,
            own_cwd,
            state: Mutex::new(State {
                child: Some(child),
                writer: Some(writer),
                shared,
                next_id: 0,
                misses: 0,
                backoff: BACKOFF_START,
                down_until: None,
            }),
        })
    }

    /// Ask the program one question and wait for its answer, for at most the configured deadline.
    ///
    /// # Errors
    ///
    /// [`Error::Deadline`] when it does not answer in time, [`Error::Down`] when it is restarting or
    /// not reading, [`Error::Malformed`] when the request or its answer is not the protocol's.
    pub(crate) fn request(
        &self,
        mode: Mode,
        key: Option<&str>,
        point: &str,
        payload: Value,
    ) -> Result<Reply, Error> {
        let deadline = self.deadline;
        let (rx, id) = {
            let mut state = self.lock();
            state.ensure(&self.spec, &self.cwd)?;
            let id = state.next_id;
            state.next_id += 1;
            let (tx, rx) = sync_channel(1);
            Shared::guard(&state.shared.pending).insert(id, tx);
            let line = Outgoing::Event {
                id: id.to_string(),
                mode,
                // An observer's key is the event's own, stable across replays; a deciding hook has
                // no key of the loop's to carry, so it is unique to the request.
                key: key.map_or_else(
                    || format!("{}/{}/{}", self.spec.name, point, id),
                    str::to_owned,
                ),
                point: point.to_owned(),
                deadline_ms: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
                payload,
            };
            let bytes = match serde_json::to_vec(&line) {
                Ok(bytes) => bytes,
                Err(e) => {
                    Shared::guard(&state.shared.pending).remove(&id);
                    return Err(Error::Malformed {
                        program: self.spec.name.clone(),
                        expected: "a request".to_owned(),
                        what: e.to_string(),
                    });
                }
            };
            if bytes.len() > LINE_MAX {
                Shared::guard(&state.shared.pending).remove(&id);
                return Err(Error::Malformed {
                    program: self.spec.name.clone(),
                    expected: format!("a request of at most {LINE_MAX} bytes"),
                    what: format!("a request of {} bytes", bytes.len()),
                });
            }
            let queued = match &state.writer {
                Some(writer) => writer.try_send(bytes),
                None => {
                    Shared::guard(&state.shared.pending).remove(&id);
                    return Err(Error::Down {
                        program: self.spec.name.clone(),
                        message: "the program has no writer".to_owned(),
                    });
                }
            };
            match queued {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    Shared::guard(&state.shared.pending).remove(&id);
                    state.broken(&self.spec, "the program is not reading its input");
                    return Err(Error::Down {
                        program: self.spec.name.clone(),
                        message: "the program is not reading its input".to_owned(),
                    });
                }
                Err(TrySendError::Disconnected(_)) => {
                    Shared::guard(&state.shared.pending).remove(&id);
                    state.broken(&self.spec, "the writer stopped");
                    return Err(Error::Down {
                        program: self.spec.name.clone(),
                        message: "the writer stopped".to_owned(),
                    });
                }
            }
            (rx, id)
        };
        match rx.recv_timeout(deadline) {
            Ok(Ok(Incoming::Reply(reply))) => {
                self.lock().success();
                Ok(*reply)
            }
            Ok(Ok(Incoming::Hello(_))) => {
                let message = "a hello where a reply was expected".to_owned();
                self.lock().broken(&self.spec, &message);
                Err(Error::Malformed {
                    program: self.spec.name.clone(),
                    expected: "a reply".to_owned(),
                    what: message,
                })
            }
            Ok(Err(Fault::Malformed(message))) => {
                self.lock().broken(&self.spec, &message);
                Err(Error::Malformed {
                    program: self.spec.name.clone(),
                    expected: "a JSON line of the protocol".to_owned(),
                    what: message,
                })
            }
            Ok(Err(Fault::Closed(message))) => {
                self.lock().broken(&self.spec, &message);
                Err(Error::Down {
                    program: self.spec.name.clone(),
                    message,
                })
            }
            Err(RecvTimeoutError::Timeout) => {
                let mut state = self.lock();
                Shared::guard(&state.shared.pending).remove(&id);
                state.miss_deadline(&self.spec);
                Err(Error::Deadline {
                    program: self.spec.name.clone(),
                    millis: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
                })
            }
            Err(RecvTimeoutError::Disconnected) => {
                let message = "the program went away".to_owned();
                self.lock().broken(&self.spec, &message);
                Err(Error::Down {
                    program: self.spec.name.clone(),
                    message,
                })
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for Program {
    fn drop(&mut self) {
        // Nothing else runs by now: take the child and the queue out, and let the program go
        // without holding the lock while it is waited for.
        let (child, writer) = {
            let mut state = self.lock();
            (state.child.take(), state.writer.take())
        };
        if let Some(writer) = writer {
            if let Ok(bytes) = serde_json::to_vec(&Outgoing::Shutdown) {
                let _ = writer.try_send(bytes);
            }
            // Closing the queue tells the writer thread to finish and close stdin.
            drop(writer);
        }
        if let Some(mut child) = child {
            let deadline = Instant::now() + EXIT_GRACE;
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
            kill(&mut child);
        }
        if self.own_cwd {
            let _ = std::fs::remove_dir_all(&self.cwd);
        }
    }
}

impl State {
    /// Make sure a child is running: restart it when its backoff elapsed, and fail transiently
    /// while it is down.
    fn ensure(&mut self, spec: &Spec, cwd: &Path) -> Result<(), Error> {
        if self.shared.closed.load(Ordering::SeqCst) {
            self.broken(spec, "the program closed its output");
        }
        let exited = match &mut self.child {
            None => None,
            Some(child) => match child.try_wait() {
                Ok(None) => return Ok(()),
                Ok(Some(status)) => Some(format!("the program exited ({status})")),
                Err(e) => Some(format!("the program could not be checked: {e}")),
            },
        };
        if let Some(message) = exited {
            self.broken(spec, &message);
        }
        if let Some(until) = self.down_until {
            let now = Instant::now();
            if now < until {
                return Err(Error::Down {
                    program: spec.name.clone(),
                    message: format!(
                        "it is restarting in {} ms",
                        until.duration_since(now).as_millis()
                    ),
                });
            }
        }
        match launch(spec, cwd) {
            Ok((child, stdin, shared)) => {
                self.child = Some(child);
                self.writer = Some(start_writer(stdin, &shared));
                self.shared = shared;
                self.down_until = None;
                Ok(())
            }
            Err(e) => {
                // A restart that fails is transient: the transition is tried again.
                self.stop();
                self.down_until = Some(Instant::now() + self.backoff);
                Err(Error::Down {
                    program: spec.name.clone(),
                    message: e.to_string(),
                })
            }
        }
    }

    /// A deadline missed: the request was `Transient`, and [`MISSES_MAX`] of them in a row kill the
    /// program (ADR 0050, decision 15).
    fn miss_deadline(&mut self, spec: &Spec) {
        self.misses = self.misses.saturating_add(1);
        if self.misses < MISSES_MAX {
            warn!(program = %spec.name, misses = self.misses, "the program missed a deadline");
            return;
        }
        self.broken(spec, "three deadlines in a row");
    }

    /// The program is gone or wrote what it must not: kill it, and keep it down for the backoff
    /// (ADR 0050, decision 15). Its deciding points are `Transient` until it is back. A program
    /// already down is left alone: one kill advances the backoff once, however many calls saw it.
    fn broken(&mut self, spec: &Spec, reason: &str) {
        if self.child.is_none() && self.down_until.is_some() {
            return;
        }
        warn!(program = %spec.name, reason, "the program is killed and restarted");
        self.stop();
        self.down_until = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
        self.misses = 0;
    }

    /// A reply arrived: the program is healthy again.
    fn success(&mut self) {
        self.misses = 0;
        self.backoff = BACKOFF_START;
        self.down_until = None;
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill(&mut child);
        }
        // Dropping the queue ends the writer thread, which closes the child's stdin.
        self.writer = None;
        self.shared
            .close(Fault::Closed("the program was stopped".to_owned()));
    }
}

/// The writer thread: it owns the child's stdin from here on.
fn start_writer(stdin: ChildStdin, shared: &Arc<Shared>) -> SyncSender<Vec<u8>> {
    let (tx, rx) = sync_channel(WRITE_QUEUE);
    let shared = Arc::clone(shared);
    std::thread::spawn(move || write_lines(stdin, rx, shared));
    tx
}

/// Start the program, say hello, and check the answer.
fn launch(spec: &Spec, cwd: &Path) -> Result<(Child, ChildStdin, Arc<Shared>), Error> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for name in SAFE_VARS {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    for (name, value) in &spec.env {
        cmd.env(name, value);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| Error::Spawn {
        program: spec.name.clone(),
        message: e.to_string(),
    })?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (Some(mut stdin), Some(stdout)) = (stdin, stdout) else {
        kill(&mut child);
        return Err(Error::Spawn {
            program: spec.name.clone(),
            message: "the program has no pipes".to_owned(),
        });
    };
    if let Some(stderr) = stderr {
        let program = spec.name.clone();
        let scrub = Arc::clone(&spec.scrub);
        std::thread::spawn(move || read_stderr(stderr, program, scrub));
    }
    let (hello_tx, hello_rx) = sync_channel(1);
    let shared = Arc::new(Shared::new(
        spec.name.clone(),
        Arc::clone(&spec.scrub),
        Some(hello_tx),
    ));
    {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || read_lines(stdout, shared));
    }
    let hello = Outgoing::Hello {
        protocol: PROTOCOL.to_owned(),
        name: spec.name.clone(),
        version: VERSION,
        points: spec.points.clone(),
    };
    let text = serde_json::to_string(&hello).map_err(|e| Error::Spawn {
        program: spec.name.clone(),
        message: e.to_string(),
    })?;
    let mut written = stdin.write_all(text.as_bytes());
    if written.is_ok() {
        written = stdin.write_all(b"\n");
    }
    if written.is_ok() {
        written = stdin.flush();
    }
    if let Err(e) = written {
        kill(&mut child);
        return Err(Error::Spawn {
            program: spec.name.clone(),
            message: format!("the program's stdin refused the hello: {e}"),
        });
    }
    let answer = hello_rx.recv_timeout(Duration::from_millis(HELLO_DEADLINE_MS));
    let result = match answer {
        Ok(Ok(Incoming::Hello(hello))) if hello.protocol == PROTOCOL => Ok(()),
        Ok(Ok(Incoming::Hello(hello))) => Err(Error::Protocol {
            program: spec.name.clone(),
            expected: PROTOCOL.to_owned(),
            found: hello.protocol,
        }),
        Ok(Ok(Incoming::Reply(reply))) => Err(Error::Malformed {
            program: spec.name.clone(),
            expected: "a hello".to_owned(),
            what: format!("a reply ({:?})", reply.verdict),
        }),
        Ok(Err(Fault::Malformed(reason))) => Err(Error::Malformed {
            program: spec.name.clone(),
            expected: "a hello".to_owned(),
            what: reason,
        }),
        Ok(Err(Fault::Closed(_))) => Err(Error::Hello {
            program: spec.name.clone(),
            millis: HELLO_DEADLINE_MS,
        }),
        Err(RecvTimeoutError::Timeout) => Err(Error::Hello {
            program: spec.name.clone(),
            millis: HELLO_DEADLINE_MS,
        }),
        Err(RecvTimeoutError::Disconnected) => Err(Error::Hello {
            program: spec.name.clone(),
            millis: HELLO_DEADLINE_MS,
        }),
    };
    match result {
        Ok(()) => Ok((child, stdin, shared)),
        Err(e) => {
            kill(&mut child);
            Err(e)
        }
    }
}

/// An empty private directory for one program: the adapter's own when the configuration names none.
fn private_dir(name: &str) -> Result<PathBuf, Error> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let dir = std::env::temp_dir().join(format!(
        "adam-hook-exec-{safe}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| Error::Spawn {
        program: name.to_owned(),
        message: format!("no private directory for the program: {e}"),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

/// SIGKILL the process group led by the child, then the child itself.
///
/// The child was started with `process_group(0)`, so its group id is its pid and a grandchild it
/// started dies with it (ADR 0050, decision 15). `kill(1)` is used rather than `libc`: this crate
/// forbids `unsafe`, and the child's own binary is enough for a shell to be there.
fn kill(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        let killed = Command::new("sh")
            .arg("-c")
            .arg(format!("kill -s KILL -- -{pid}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !matches!(killed, Ok(status) if status.success()) {
            warn!(
                pid,
                "could not kill the process group; killing the child only"
            );
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
