//! JSON-RPC 2.0 over `Content-Length` frames: the client half of the language server protocol, over
//! the pipes of a child process.
//!
//! * A request has an id and a deadline of its own ([`Client::request`]); a late answer is dropped.
//! * Notifications go both ways ([`Client::notify`], [`Peer::notification`]).
//! * A request the server makes is answered by the [`Peer`], never left hanging.
//! * What the server sends is untrusted: a header block that is long, has no `Content-Length` or
//!   names more than [`MAX_MESSAGE_BYTES`], and a body that is not JSON, **close** the client with a
//!   reason (the caller stops the server), and every request that waits fails with it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use adam_error::{Classify, ErrorClass};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, oneshot};

/// The largest message (its `Content-Length`) the client reads: 16 MiB. A server that names more
/// is stopped, and the tool says so.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// The most header lines of one message, and the longest, in bytes.
const MAX_HEADER_LINES: usize = 32;
const MAX_HEADER_LINE_BYTES: usize = 1024;

/// How long a write to the server's stdin may take: a server that does not read its input is stuck.
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);

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

/// A client of one server.
pub struct Client {
    writer: tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, Pending>>,
    closed: Mutex<Option<String>>,
    closed_signal: Notify,
}

impl Client {
    /// A client over `reader` (the server's stdout) and `writer` (its stdin); `peer` answers what
    /// the server sends. The reader runs in a task of its own until the stream ends or is refused.
    pub fn start<R, W>(reader: R, writer: W, peer: Arc<dyn Peer>) -> Arc<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let client = Arc::new(Self {
            writer: tokio::sync::Mutex::new(Box::new(writer)),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            closed: Mutex::new(None),
            closed_signal: Notify::new(),
        });
        tokio::spawn(read_loop(client.clone(), BufReader::new(reader), peer));
        client
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
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => answer.map_err(|e| match e {
                RpcError::Server { code, message, .. } => RpcError::Server {
                    method: method.to_owned(),
                    code,
                    message,
                },
                other => other,
            }),
            Ok(Err(_)) => {
                Err(RpcError::Closed(self.closed().unwrap_or_else(|| {
                    "the server's answer was lost".to_owned()
                })))
            }
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                // Best effort: a server that is stuck does not read it.
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    self.send(&json!({
                        "jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": id}
                    })),
                )
                .await;
                Err(RpcError::Timeout {
                    method: method.to_owned(),
                    secs: timeout.as_secs().max(1),
                })
            }
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

    async fn respond(&self, id: &Value, answer: Result<Value, (i64, String)>) {
        let body = match answer {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        let _ = self.send(&body).await;
    }

    async fn send(&self, body: &Value) -> Result<(), RpcError> {
        let bytes = serde_json::to_vec(body)
            .map_err(|e| RpcError::Closed(format!("cannot write a message: {e}")))?;
        let mut framed = format!("Content-Length: {}\r\n\r\n", bytes.len()).into_bytes();
        framed.extend_from_slice(&bytes);
        let mut writer = self.writer.lock().await;
        let wrote = tokio::time::timeout(WRITE_TIMEOUT, async {
            writer.write_all(&framed).await?;
            writer.flush().await
        })
        .await;
        match wrote {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                let reason = format!("the server does not take its input: {e}");
                self.close(reason.clone());
                Err(RpcError::Closed(reason))
            }
            Err(_) => {
                let reason = format!(
                    "the server did not read its input for {} s",
                    WRITE_TIMEOUT.as_secs()
                );
                self.close(reason.clone());
                Err(RpcError::Closed(reason))
            }
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
    let mut body = vec![0_u8; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| FrameError::Io(format!("it ended inside a message: {e}")))?;
    Ok(body)
}

async fn read_loop<R: AsyncRead + Send + Unpin>(
    client: Arc<Client>,
    mut reader: BufReader<R>,
    peer: Arc<dyn Peer>,
) {
    loop {
        let body = match read_frame(&mut reader).await {
            Ok(body) => body,
            Err(e) => {
                client.close(e.to_string());
                return;
            }
        };
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
                let (client, id) = (client.clone(), id.clone());
                tokio::spawn(async move { client.respond(&id, answer).await });
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
        drop(their_write);
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
}
