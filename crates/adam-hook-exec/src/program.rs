//! The child process: start it, say hello, ask one question at a time, and kill and restart it when
//! it stops answering (ADR 0050, decision 15, "Fail closed").
//!
//! ```mermaid
//! stateDiagram-v2
//!     [*] --> Starting: spawn, write hello
//!     Starting --> Ready: the program answers adam-hooks/1
//!     Starting --> [*]: no hello, or another protocol (exit 78 in a binary)
//!     Ready --> Asking: one event, one deadline
//!     Asking --> Ready: a reply in time
//!     Asking --> Missed: no reply, a broken line, or a dead child
//!     Missed --> Ready: fewer than three in a row
//!     Missed --> Down: three in a row, or the child died: killed, then restarted with backoff
//!     Down --> Starting: the backoff elapsed
//!     Ready --> [*]: shutdown
//! ```

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::{debug, warn};

use crate::config::{HELLO_DEADLINE_MS, SAFE_VARS, Scrub};
use crate::error::Error;
use crate::protocol::{Hello, LINE_MAX, Mode, Outgoing, PROTOCOL, Reply, VERSION};

/// How many misses in a row kill the program (ADR 0050, decision 15).
const MISSES_MAX: u32 = 3;

/// The first wait before a killed program is restarted; it doubles up to [`BACKOFF_MAX`].
const BACKOFF_START: Duration = Duration::from_secs(1);

/// The longest wait between restarts.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

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

/// The state the reader thread shares with the adapter.
struct Shared {
    program: String,
    /// The requests waiting for a reply, by id.
    pending: Mutex<HashMap<u64, SyncSender<Answer>>>,
    /// The hello's channel, taken once.
    hello: Mutex<Option<SyncSender<Answer>>>,
}

impl Shared {
    fn new(program: String, hello: Option<SyncSender<Answer>>) -> Self {
        Shared {
            program,
            pending: Mutex::new(HashMap::new()),
            hello: Mutex::new(hello),
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
}

/// Read the program's lines and hand each to the request that waits for it.
fn read_lines(stdout: ChildStdout, shared: Arc<Shared>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() > LINE_MAX {
            shared.fail_all(Fault::Malformed(format!(
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
            shared.fail_all(Fault::Malformed("a line that is not JSON".to_owned()));
            return;
        };
        // The kind is copied out first: the arms move `value` into `from_value`.
        let kind = value.get("type").and_then(Value::as_str).map(str::to_owned);
        match kind.as_deref() {
            Some("hello") => match serde_json::from_value::<Hello>(value) {
                Ok(hello) => shared.hello(hello),
                Err(e) => {
                    shared.fail_all(Fault::Malformed(format!(
                        "a hello this build cannot read: {e}"
                    )));
                    return;
                }
            },
            Some("reply") => match serde_json::from_value::<Reply>(value) {
                Ok(reply) => shared.reply(Box::new(reply)),
                Err(e) => {
                    shared.fail_all(Fault::Malformed(format!(
                        "a reply this build cannot read: {e}"
                    )));
                    return;
                }
            },
            // An unknown message is ignored: a program may speak more than this build knows.
            _ => debug!(program = %shared.program, "a line of an unknown kind is ignored"),
        }
    }
    shared.fail_all(Fault::Closed("the program closed its output".to_owned()));
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
    state: Mutex<State>,
}

struct State {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    shared: Arc<Shared>,
    next_id: u64,
    misses: u32,
    backoff: Duration,
    down_until: Option<Instant>,
}

impl Program {
    /// Start the program and say hello.
    ///
    /// # Errors
    ///
    /// [`Error::Spawn`] when it cannot start, [`Error::Hello`] when it does not answer in time, and
    /// [`Error::Protocol`] when it names another protocol.
    pub(crate) fn start(spec: Arc<Spec>, deadline: Duration) -> Result<Self, Error> {
        let (child, stdin, shared) = launch(&spec)?;
        Ok(Program {
            spec,
            deadline,
            state: Mutex::new(State {
                child: Some(child),
                stdin: Some(stdin),
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
    /// [`Error::Deadline`] when it does not answer in time, [`Error::Down`] when it is restarting,
    /// [`Error::Malformed`] when its answer is not the protocol's.
    pub(crate) fn request(&self, mode: Mode, point: &str, payload: Value) -> Result<Reply, Error> {
        let deadline = self.deadline;
        let (rx, id) = {
            let mut state = self.lock();
            state.ensure(&self.spec)?;
            let id = state.next_id;
            state.next_id += 1;
            let (tx, rx) = sync_channel(1);
            Shared::guard(&state.shared.pending).insert(id, tx);
            let line = Outgoing::Event {
                id: id.to_string(),
                mode,
                key: format!("{}/{}/{}", self.spec.name, point, id),
                point: point.to_owned(),
                deadline_ms: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
                payload,
            };
            if let Err(message) = state.write(&line) {
                Shared::guard(&state.shared.pending).remove(&id);
                state.broken(&self.spec, &message);
                return Err(Error::Down {
                    program: self.spec.name.clone(),
                    message,
                });
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
        let mut state = self.lock();
        if let Some(mut stdin) = state.stdin.take() {
            if let Ok(text) = serde_json::to_string(&Outgoing::Shutdown) {
                let _ = writeln!(stdin, "{text}");
            }
            let _ = stdin.flush();
            // Closing stdin tells a well-behaved program to exit.
            drop(stdin);
        }
        if let Some(mut child) = state.child.take() {
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
            kill(&mut child);
        }
    }
}

impl State {
    /// Make sure a child is running: restart it when its backoff elapsed, and fail transiently
    /// while it is down.
    fn ensure(&mut self, spec: &Spec) -> Result<(), Error> {
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
        match launch(spec) {
            Ok((child, stdin, shared)) => {
                self.child = Some(child);
                self.stdin = Some(stdin);
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

    fn write(&mut self, line: &Outgoing) -> Result<(), String> {
        let Some(stdin) = &mut self.stdin else {
            return Err("the program has no stdin".to_owned());
        };
        let text = serde_json::to_string(line).map_err(|e| e.to_string())?;
        let mut write = |bytes: &[u8]| {
            stdin
                .write_all(bytes)
                .map_err(|e| format!("the program's stdin refused a line: {e}"))
        };
        write(text.as_bytes())?;
        write(b"\n")?;
        stdin
            .flush()
            .map_err(|e| format!("the program's stdin refused a line: {e}"))
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
    /// (ADR 0050, decision 15). Its deciding points are `Transient` until it is back.
    fn broken(&mut self, spec: &Spec, reason: &str) {
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
        self.stdin = None;
        self.shared
            .fail_all(Fault::Closed("the program was stopped".to_owned()));
    }
}

/// Start the program, say hello, and check the answer.
fn launch(spec: &Spec) -> Result<(Child, ChildStdin, Arc<Shared>), Error> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
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
    let shared = Arc::new(Shared::new(spec.name.clone(), Some(hello_tx)));
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

/// SIGKILL the process group led by the child, then the child itself.
///
/// The child was started with `process_group(0)`, so its group id is its pid and a grandchild it
/// started dies with it (ADR 0050, decision 15). `kill(1)` is used rather than `libc`: this crate
/// forbids `unsafe`, and the child's own binary is enough for a shell to be there.
fn kill(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        let _ = Command::new("sh")
            .arg("-c")
            .arg(format!("kill -s KILL -- -{pid}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}
