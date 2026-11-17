//! JSON-RPC 2.0 over `Content-Length` frames: the client half of the language server protocol, over
//! the pipes of a child process.
//!
//! * A request has an id and a deadline of its own ([`Client::request`]); a late answer is dropped.
//! * Notifications go both ways ([`Client::notify`], [`Peer::notification`]).
//! * A request the server makes is answered by the [`Peer`], never left hanging.
//! * What the server sends is untrusted: a header block that is long, has no `Content-Length`, has two
//!   or names more than [`MAX_MESSAGE_BYTES`], and a body that is not JSON, **close** the client with
//!   a reason (the caller stops the server), and every request that waits fails with it. Before the
//!   first message only, lines that are not headers are skipped (at most [`MAX_PRELUDE_BYTES`]): a
//!   login shell's profile may print before the server starts.
//! * One task writes: a message is queued whole, and written whole whatever happens to the call that
//!   queued it, so a call that is dropped (a time limit, a cancel) never leaves half a message in the
//!   server's input. A write that takes more than [`WRITE_TIMEOUT`] closes the client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use adam_error::{Classify, ErrorClass};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc, oneshot};

/// The largest message (its `Content-Length`) the client reads: 16 MiB. A server that names more
/// is stopped, and the tool says so.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// The most header lines of one message, and the longest, in bytes.
const MAX_HEADER_LINES: usize = 32;
const MAX_HEADER_LINE_BYTES: usize = 1024;

/// How long a write to the server's stdin may take: a server that does not read its input is stuck.
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);

/// The messages that may wait to be written. A server that asks for more than this while it reads
/// nothing is closed.
const WRITE_QUEUE: usize = 256;

/// How much of what is not a message is skipped before the first message: 64 KiB.
pub const MAX_PRELUDE_BYTES: usize = 64 * 1024;

/// Why a request got no result.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RpcError {
    /// The server did not answer in time. The request is cancelled.
    #[error("the server did not answer `{method}` within {secs} s")]
    Timeout {
        /// The method.
        method: String,
        /// The deadline, in seconds.
        secs: u64,
    },
    /// The client is closed: the server ended, or sent what cannot be read.
    #[error("{0}")]
    Closed(String),
    /// The server answered with an error.
    #[error("the server answered `{method}` with an error ({code}): {message}")]
    Server {
        /// The method.
        method: String,
        /// The JSON-RPC error code.
        code: i64,
        /// Its message (not yet scrubbed).
        message: String,
    },
}

impl Classify for RpcError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Timeout { .. } | Self::Closed(_) => ErrorClass::Transient,
            Self::Server { .. } => ErrorClass::Invalid,
        }
    }
}

/// What answers the server's own messages.
pub trait Peer: Send + Sync + 'static {
    /// A notification of the server's.
    fn notification(&self, method: &str, params: Value);

    /// A request of the server's: the result, or a JSON-RPC error `(code, message)`. It answers at
    /// once and never waits.
    ///
    /// # Errors
    ///
    /// The error the server is sent.
    fn request(&self, method: &str, params: Value) -> Result<Value, (i64, String)>;
}

type Pending = oneshot::Sender<Result<Value, RpcError>>;

/// One message for the writer task: the frame, whole, and who is told it was written.
struct Outgoing {
    frame: Vec<u8>,
    done: Option<oneshot::Sender<Result<(), String>>>,
}

/// A client of one server.
pub struct Client {
    out: mpsc::Sender<Outgoing>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, Pending>>,
    closed: Mutex<Option<String>>,
    closed_signal: Notify,
}

impl Client {
    /// A client over `reader` (the server's stdout) and `writer` (its stdin); `peer` answers what
    /// the server sends. The reader and the writer run in tasks of their own until the stream ends or
    /// is refused.
    pub fn start<R, W>(reader: R, writer: W, peer: Arc<dyn Peer>) -> Arc<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (out, queue) = mpsc::channel(WRITE_QUEUE);
        let client = Arc::new(Self {
            out,
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            closed: Mutex::new(None),
            closed_signal: Notify::new(),
        });
        tokio::spawn(write_loop(Arc::downgrade(&client), queue, writer));
        tokio::spawn(read_loop(client.clone(), BufReader::new(reader), peer));
        client
    }

    /// How many requests wait for an answer (a test reads it).
    #[doc(hidden)]
    pub fn pending_len(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Why the client is closed, or `None` while it is open.
    pub fn closed(&self) -> Option<String> {
        self.closed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Wait until the client is closed.
    pub async fn wait_closed(&self) {
        loop {
            let signalled = self.closed_signal.notified();
            tokio::pin!(signalled);
            // Registered before the check: a close in between is not missed.
            signalled.as_mut().enable();
            if self.closed().is_some() {
                return;
            }
            signalled.await;
        }
    }

    /// Close the client with `reason` (the first reason stays), and fail every waiting request.
    pub fn close(&self, reason: impl Into<String>) {
        let reason = reason.into();
        {
            let mut closed = self.closed.lock().unwrap_or_else(PoisonError::into_inner);
            if closed.is_some() {
                return;
            }
            *closed = Some(reason.clone());
        }
        let waiting: Vec<Pending> = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, tx)| tx)
            .collect();
        for tx in waiting {
            let _ = tx.send(Err(RpcError::Closed(reason.clone())));
        }
        self.closed_signal.notify_waiters();
    }

    /// Send a request and wait for its answer, at most `timeout`.
    ///
    /// # Errors
    ///
    /// [`RpcError`]: no answer in time (the request is then cancelled), the client is closed, or
    /// the server answered with an error.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        if let Some(reason) = self.closed() {
            return Err(RpcError::Closed(reason));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, tx);
        // However this ends (an answer, the time, the caller dropped), the wait is forgotten and a
        // request that nobody waits for any more is cancelled.
        let mut waiting = Waiting {
            client: self,
            id,
            armed: true,
        };
        let sent_and_answered = tokio::time::timeout(timeout, async {
            self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .await?;
            Ok::<_, RpcError>(rx.await)
        })
        .await;
        match sent_and_answered {
            Ok(Ok(Ok(answer))) => {
                waiting.armed = false;
                answer.map_err(|e| match e {
                    RpcError::Server { code, message, .. } => RpcError::Server {
                        method: method.to_owned(),
                        code,
                        message,
                    },
                    other => other,
                })
            }
            Ok(Ok(Err(_))) => {
                waiting.armed = false;
                Err(RpcError::Closed(self.closed().unwrap_or_else(|| {
                    "the server's answer was lost".to_owned()
                })))
            }
            Ok(Err(closed)) => {
                waiting.armed = false;
                Err(closed)
            }
            // `waiting` drops here: the wait is forgotten and `$/cancelRequest` is queued.
            Err(_) => Err(RpcError::Timeout {
                method: method.to_owned(),
                secs: timeout.as_secs().max(1),
            }),
        }
    }

    /// Send a notification.
    ///
    /// # Errors
    ///
    /// [`RpcError::Closed`] when the client is closed or the write fails.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        if let Some(reason) = self.closed() {
            return Err(RpcError::Closed(reason));
        }
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// Queue the answer to a request of the server's. It never waits: a server that is not reading
    /// and keeps asking is closed.
    fn respond(&self, id: &Value, answer: Result<Value, (i64, String)>) {
        let body = match answer {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        let Some(frame) = frame_of(&body) else { return };
        match self.out.try_send(Outgoing { frame, done: None }) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.close("the server asks more than it reads the answers to");
            }
        }
    }

    /// Queue `body` and wait until it was written. Dropping this call after the message is queued
    /// does not stop its write, and before that nothing was written: never half a message.
    async fn send(&self, body: &Value) -> Result<(), RpcError> {
        let frame =
            frame_of(body).ok_or_else(|| RpcError::Closed("cannot write a message".to_owned()))?;
        let (done, written) = oneshot::channel();
        let gone = || {
            RpcError::Closed(
                self.closed()
                    .unwrap_or_else(|| "the server's input is closed".to_owned()),
            )
        };
        self.out
            .send(Outgoing {
                frame,
                done: Some(done),
            })
            .await
            .map_err(|_| gone())?;
        match written.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(reason)) => Err(RpcError::Closed(reason)),
            Err(_) => Err(gone()),
        }
    }

    fn answer(&self, id: i64, answer: Result<Value, RpcError>) {
        let waiting = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        if let Some(tx) = waiting {
            let _ = tx.send(answer);
        }
    }
}

/// `body` with its `Content-Length` header.
fn frame_of(body: &Value) -> Option<Vec<u8>> {
    let bytes = serde_json::to_vec(body).ok()?;
    let mut framed = format!("Content-Length: {}\r\n\r\n", bytes.len()).into_bytes();
    framed.extend_from_slice(&bytes);
    Some(framed)
}

/// A request that is waited for: when it is dropped before its answer (the time, the caller), the
/// wait is forgotten and the server is told to stop working on it.
struct Waiting<'a> {
    client: &'a Client,
    id: i64,
    armed: bool,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.client
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
        if self.client.closed().is_some() {
            return;
        }
        // Best effort, and never waited for: a server that is stuck does not read it.
        if let Some(frame) = frame_of(&json!({
            "jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": self.id}
        })) {
            let _ = self.client.out.try_send(Outgoing { frame, done: None });
        }
    }
}

/// The one task that writes to the server: a message at a time, whole.
async fn write_loop<W: AsyncWrite + Unpin>(
    client: Weak<Client>,
    mut queue: mpsc::Receiver<Outgoing>,
    mut writer: W,
) {
    while let Some(item) = queue.recv().await {
        let Some(live) = client.upgrade() else { return };
        if let Some(reason) = live.closed() {
            if let Some(done) = item.done {
                let _ = done.send(Err(reason));
            }
            continue;
        }
        let wrote = tokio::time::timeout(WRITE_TIMEOUT, async {
            writer.write_all(&item.frame).await?;
            writer.flush().await
        })
        .await;
        let failed = match wrote {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("the server does not take its input: {e}")),
            Err(_) => Some(format!(
                "the server did not read its input for {} s",
                WRITE_TIMEOUT.as_secs()
            )),
        };
        if let Some(done) = item.done {
            let _ = done.send(failed.clone().map_or(Ok(()), Err));
        }
        if let Some(reason) = failed {
            // What is half written is never followed by another message.
            live.close(reason);
            return;
        }
    }
}

/// Why a frame was not read.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The stream ended between messages.
    End,
    /// The stream failed or ended inside a message.
    Io(String),
    /// The header block is not one: no `Content-Length`, not a number, too long.
    Header(String),
    /// `Content-Length` names more than [`MAX_MESSAGE_BYTES`].
    TooLarge(usize),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::End => f.write_str("the server's output ended"),
            Self::Io(why) => write!(f, "the server's output failed: {why}"),
            Self::Header(why) => write!(f, "the server sent malformed framing: {why}"),
            Self::TooLarge(n) => write!(
                f,
                "the server announced a message of {n} bytes, over the limit of {MAX_MESSAGE_BYTES}"
            ),
        }
    }
}

/// Read one frame: the body of the next message.
///
/// # Errors
///
/// [`FrameError`] when the stream ends or the frame is not acceptable.
pub async fn read_frame<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Result<Vec<u8>, FrameError> {
    read_message(reader, None).await
}

/// [`read_frame`], and with `prelude` (the bytes of not-a-message that are still skipped): lines
/// before the first `Content-Length` are skipped, at most that many bytes in all.
async fn read_message<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    mut prelude: Option<&mut usize>,
) -> Result<Vec<u8>, FrameError> {
    let mut length: Option<usize> = None;
    let mut lines = 0;
    loop {
        let mut line = Vec::new();
        // A header line is short: read at most one more byte than the limit.
        let n = (&mut *reader)
            .take(MAX_HEADER_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
            .await
            .map_err(|e| FrameError::Io(e.to_string()))?;
        if n == 0 {
            return Err(if lines == 0 {
                FrameError::End
            } else {
                FrameError::Io("it ended inside a header block".to_owned())
            });
        }
        if length.is_none()
            && let Some(left) = prelude.as_deref_mut()
            && !starts_with_length(&line)
        {
            // Not the start of a message: what a profile printed. Skipped, within the budget.
            *left = left.saturating_sub(n);
            if *left == 0 {
                return Err(FrameError::Header(format!(
                    "{} KiB of output that is not a message before the first one",
                    MAX_PRELUDE_BYTES / 1024
                )));
            }
            continue;
        }
        if line.last() != Some(&b'\n') {
            return Err(FrameError::Header(if line.len() > MAX_HEADER_LINE_BYTES {
                format!("a header line over {MAX_HEADER_LINE_BYTES} bytes")
            } else {
                "a header line with no end".to_owned()
            }));
        }
        lines += 1;
        if lines > MAX_HEADER_LINES {
            return Err(FrameError::Header(format!(
                "more than {MAX_HEADER_LINES} header lines"
            )));
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            break;
        }
        let Some((name, value)) = text.split_once(':') else {
            return Err(FrameError::Header(format!(
                "a header line that is not `Name: value` ({} bytes)",
                text.len()
            )));
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(FrameError::Header(
                    "a second `Content-Length` in one header block".to_owned(),
                ));
            }
            length = Some(value.trim().parse::<usize>().map_err(|_| {
                FrameError::Header("a `Content-Length` that is not a number".to_owned())
            })?);
        }
    }
    let length = length
        .ok_or_else(|| FrameError::Header("a header block with no `Content-Length`".to_owned()))?;
    if length > MAX_MESSAGE_BYTES {
        return Err(FrameError::TooLarge(length));
    }
    // Read as it arrives: the space is not taken for a message that never comes.
    let mut body = Vec::new();
    (&mut *reader)
        .take(length as u64)
        .read_to_end(&mut body)
        .await
        .map_err(|e| FrameError::Io(format!("it ended inside a message: {e}")))?;
    if body.len() != length {
        return Err(FrameError::Io(
            "it ended inside a message: the body is shorter than announced".to_owned(),
        ));
    }
    Ok(body)
}

/// Whether `line` starts a `Content-Length` header (any case).
fn starts_with_length(line: &[u8]) -> bool {
    const NAME: &[u8] = b"content-length:";
    line.len() >= NAME.len() && line[..NAME.len()].eq_ignore_ascii_case(NAME)
}

async fn read_loop<R: AsyncRead + Send + Unpin>(
    client: Arc<Client>,
    mut reader: BufReader<R>,
    peer: Arc<dyn Peer>,
) {
    // Until the first message, what is not one is skipped (a profile may print).
    let mut prelude = MAX_PRELUDE_BYTES;
    let mut first = true;
    loop {
        let read = read_message(&mut reader, first.then_some(&mut prelude)).await;
        let body = match read {
            Ok(body) => body,
            Err(e) => {
                client.close(e.to_string());
                return;
            }
        };
        first = false;
        let Ok(message) = serde_json::from_slice::<Value>(&body) else {
            client.close("the server sent a message that is not JSON");
            return;
        };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (Some(method), Some(id)) => {
                let answer = peer.request(method, params);
                client.respond(id, answer);
            }
            (Some(method), None) => peer.notification(method, params),
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                let answer = match message.get("error") {
                    Some(error) => Err(RpcError::Server {
                        method: String::new(),
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    }),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                client.answer(id, answer);
            }
            (None, None) => {}
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use tokio::io::duplex;

    use super::*;

    struct Echo;

    impl Peer for Echo {
        fn notification(&self, _method: &str, _params: Value) {}

        fn request(&self, method: &str, _params: Value) -> Result<Value, (i64, String)> {
            match method {
                "ok" => Ok(json!("fine")),
                _ => Err((-32601, "no such method".to_owned())),
            }
        }
    }

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    async fn read(bytes: &[u8]) -> Result<Vec<u8>, FrameError> {
        read_frame(&mut BufReader::new(bytes)).await
    }

    #[tokio::test]
    async fn a_frame_is_read_whatever_the_case_of_its_header() {
        let body = read(b"content-length: 2\r\nContent-Type: x\r\n\r\n{}").await;
        assert_eq!(body.unwrap(), b"{}");
    }

    #[tokio::test]
    async fn framing_that_is_not_framing_is_named() {
        for (bytes, expect) in [
            (&b"Content-Length: abc\r\n\r\n"[..], "not a number"),
            (b"X: 1\r\n\r\n", "no `Content-Length`"),
            (b"no colon here\r\n\r\n", "not `Name: value`"),
            (b"Content-Length: 5\r\n\r\nab", "inside a message"),
        ] {
            let e = read(bytes).await.unwrap_err();
            assert!(e.to_string().contains(expect), "{bytes:?}: {e}");
        }
        assert_eq!(read(b"").await.unwrap_err(), FrameError::End);
        let long = format!("X: {}\r\n\r\n", "a".repeat(MAX_HEADER_LINE_BYTES + 10));
        assert!(matches!(
            read(long.as_bytes()).await.unwrap_err(),
            FrameError::Header(_)
        ));
        let many = "X: 1\r\n".repeat(MAX_HEADER_LINES + 1);
        assert!(matches!(
            read(many.as_bytes()).await.unwrap_err(),
            FrameError::Header(_)
        ));
    }

    #[tokio::test]
    async fn a_message_over_the_limit_is_refused_before_it_is_read() {
        let header = format!("Content-Length: {}\r\n\r\n", MAX_MESSAGE_BYTES + 1);
        assert_eq!(
            read(header.as_bytes()).await.unwrap_err(),
            FrameError::TooLarge(MAX_MESSAGE_BYTES + 1)
        );
        // At the limit it is acceptable (the body is then missing here).
        let header = format!("Content-Length: {MAX_MESSAGE_BYTES}\r\n\r\n");
        assert!(matches!(
            read(header.as_bytes()).await.unwrap_err(),
            FrameError::Io(_)
        ));
    }

    #[tokio::test]
    async fn a_request_gets_its_answer_and_an_error_names_its_method() {
        let (ours, theirs) = duplex(64 * 1024);
        let (mut their_read, mut their_write) = tokio::io::split(theirs);
        let (our_read, our_write) = tokio::io::split(ours);
        let client = Client::start(our_read, our_write, Arc::new(Echo));
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(&mut their_read);
            for fail in [false, true] {
                let body = read_frame(&mut reader).await.unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let id = request["id"].clone();
                let reply = if fail {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32803, "message": "no"}})
                } else {
                    json!({"jsonrpc": "2.0", "id": id, "result": {"n": 1}})
                };
                their_write
                    .write_all(&frame(&reply.to_string()))
                    .await
                    .unwrap();
            }
        });
        let got = client
            .request("a", json!({}), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(got["n"], 1);
        let err = client
            .request("b", json!({}), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, RpcError::Server { method, code: -32803, .. } if method == "b"),
            "{err}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_request_that_is_not_answered_times_out_and_is_cancelled() {
        let (ours, theirs) = duplex(64 * 1024);
        let (mut their_read, _their_write) = tokio::io::split(theirs);
        let (our_read, our_write) = tokio::io::split(ours);
        let client = Client::start(our_read, our_write, Arc::new(Echo));
        let err = client
            .request("slow", json!({}), Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Timeout { .. }), "{err}");
        let mut reader = BufReader::new(&mut their_read);
        let first: Value = serde_json::from_slice(&read_frame(&mut reader).await.unwrap()).unwrap();
        assert_eq!(first["method"], "slow");
        let cancel: Value =
            serde_json::from_slice(&read_frame(&mut reader).await.unwrap()).unwrap();
        assert_eq!(cancel["method"], "$/cancelRequest");
        assert_eq!(cancel["params"]["id"], first["id"]);
        assert!(client.closed().is_none(), "a timeout does not close it");
    }

    #[tokio::test]
    async fn what_the_server_asks_is_answered_and_the_end_of_its_output_closes_the_client() {
        let (ours, theirs) = duplex(64 * 1024);
        let (mut their_read, mut their_write) = tokio::io::split(theirs);
        let (our_read, our_write) = tokio::io::split(ours);
        let client = Client::start(our_read, our_write, Arc::new(Echo));
        for (id, method) in [(7, "ok"), (8, "unknown")] {
            let ask = json!({"jsonrpc": "2.0", "id": id, "method": method}).to_string();
            their_write.write_all(&frame(&ask)).await.unwrap();
        }
        let mut reader = BufReader::new(&mut their_read);
        let mut seen = Vec::new();
        for _ in 0..2 {
            let v: Value = serde_json::from_slice(&read_frame(&mut reader).await.unwrap()).unwrap();
            seen.push(v);
        }
        drop(reader);
        seen.sort_by_key(|v| v["id"].as_i64());
        assert_eq!(seen[0]["result"], "fine");
        assert_eq!(seen[1]["error"]["code"], -32601);
        // The request that waits fails with the reason when the output ends.
        let waiting = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .request("x", json!({}), Duration::from_secs(30))
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The stream ends when both halves of the server's side are gone.
        drop(their_write);
        drop(their_read);
        let err = waiting.await.unwrap().unwrap_err();
        assert!(
            matches!(&err, RpcError::Closed(why) if why.contains("ended")),
            "{err}"
        );
        client.wait_closed().await;
        assert!(client.closed().is_some());
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_closes_the_client() {
        let (ours, theirs) = duplex(64 * 1024);
        let (_their_read, mut their_write) = tokio::io::split(theirs);
        let (our_read, our_write) = tokio::io::split(ours);
        let client = Client::start(our_read, our_write, Arc::new(Echo));
        their_write.write_all(&frame("not json")).await.unwrap();
        client.wait_closed().await;
        assert!(client.closed().unwrap().contains("not JSON"));
    }

    #[tokio::test]
    async fn a_write_that_is_dropped_midway_is_still_written_whole() {
        // A pipe that holds 1 KiB: a large message cannot be written until the server reads.
        let (ours, theirs) = duplex(1024);
        let (mut their_read, _their_write) = tokio::io::split(theirs);
        let (our_read, our_write) = tokio::io::split(ours);
        let client = Client::start(our_read, our_write, Arc::new(Echo));
        let big = "x".repeat(200 * 1024);
        let dropped = tokio::time::timeout(
            Duration::from_millis(100),
            client.notify("big", json!({"text": big})),
        )
        .await;
        assert!(
            dropped.is_err(),
            "the write is still going when it is dropped"
        );
        // The next message must not land in the middle of the first.
        let after = tokio::spawn({
            let client = client.clone();
            async move { client.notify("after", json!({})).await }
        });
        let mut reader = BufReader::new(&mut their_read);
        let mut methods = Vec::new();
        for _ in 0..2 {
            let body = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut reader))
                .await
                .expect("a whole frame arrives")
                .unwrap();
            let message: Value = serde_json::from_slice(&body).expect("a frame that is JSON");
            methods.push(message["method"].as_str().unwrap().to_owned());
        }
        assert_eq!(methods, ["big", "after"]);
        after.await.unwrap().unwrap();
        assert!(client.closed().is_none());
    }

    #[tokio::test]
    async fn a_request_that_is_dropped_forgets_its_wait_and_cancels_it() {
        let (ours, theirs) = duplex(64 * 1024);
        let (mut their_read, _their_write) = tokio::io::split(theirs);
        let (our_read, our_write) = tokio::io::split(ours);
        let client = Client::start(our_read, our_write, Arc::new(Echo));
        let waiting = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .request("slow", json!({}), Duration::from_secs(60))
                    .await
            }
        });
        let mut reader = BufReader::new(&mut their_read);
        let first: Value = serde_json::from_slice(&read_frame(&mut reader).await.unwrap()).unwrap();
        assert_eq!(client.pending_len(), 1);
        waiting.abort();
        let _ = waiting.await;
        let cancel: Value = serde_json::from_slice(
            &tokio::time::timeout(Duration::from_secs(5), read_frame(&mut reader))
                .await
                .expect("a $/cancelRequest is sent")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(cancel["method"], "$/cancelRequest");
        assert_eq!(cancel["params"]["id"], first["id"]);
        assert_eq!(client.pending_len(), 0);
    }

    #[tokio::test]
    async fn a_second_content_length_is_refused() {
        let e = read(b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}")
            .await
            .unwrap_err();
        assert!(
            matches!(&e, FrameError::Header(why) if why.contains("second")),
            "{e}"
        );
    }

    #[tokio::test]
    async fn a_profile_that_prints_before_the_first_message_is_skipped_but_nothing_later_is() {
        let (ours, theirs) = duplex(64 * 1024);
        let (mut their_read, mut their_write) = tokio::io::split(theirs);
        let (our_read, our_write) = tokio::io::split(ours);
        let client = Client::start(our_read, our_write, Arc::new(Echo));
        // What a login shell's profile prints (a banner, a version manager's notice), then the server.
        their_write
            .write_all(b"Welcome to a machine\nnvm: using v24\n\n")
            .await
            .unwrap();
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(&mut their_read);
            let body = read_frame(&mut reader).await.unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap();
            let reply = json!({"jsonrpc": "2.0", "id": request["id"], "result": 1});
            their_write
                .write_all(&frame(&reply.to_string()))
                .await
                .unwrap();
            // After the first message, anything that is not one closes the client.
            their_write
                .write_all(b"banner again\r\n\r\n")
                .await
                .unwrap();
            their_write
        });
        let got = client
            .request("a", json!({}), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(got, 1);
        let _keep = server.await.unwrap();
        client.wait_closed().await;
        assert!(client.closed().unwrap().contains("malformed framing"));
    }

    #[tokio::test]
    async fn output_that_never_becomes_a_message_closes_the_client_after_64_kib() {
        let (ours, theirs) = duplex(256 * 1024);
        let (_their_read, mut their_write) = tokio::io::split(theirs);
        let (our_read, our_write) = tokio::io::split(ours);
        let client = Client::start(our_read, our_write, Arc::new(Echo));
        let junk = "a line of a profile that never ends\n".repeat(3000);
        their_write.write_all(junk.as_bytes()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), client.wait_closed())
            .await
            .expect("closed");
        assert!(
            client.closed().unwrap().contains("64 KiB"),
            "{:?}",
            client.closed()
        );
    }
}
