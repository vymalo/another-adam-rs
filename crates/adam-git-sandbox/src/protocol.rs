//! The wire protocol between a client (the coder, or the `adam-git` relay) and the server.
//!
//! One connection carries one command. Both directions are a sequence of **frames**:
//!
//! ```text
//! +------+--------------------+----------------+
//! | kind | length (u32, BE)   | payload        |
//! | 1 B  | 4 B                | length bytes   |
//! +------+--------------------+----------------+
//! ```
//!
//! | Kind | Direction | Payload |
//! |---|---|---|
//! | `0x01` request | client to server | JSON [`Request`]; exactly one, and the first frame |
//! | `0x02` stdin | client to server | bytes for the command's standard input |
//! | `0x03` stdin end | client to server | empty: the standard input is closed |
//! | `0x81` stdout | server to client | bytes the command wrote to its standard output |
//! | `0x82` stderr | server to client | bytes the command wrote to its standard error |
//! | `0x83` exit | server to client | JSON [`Exit`]; the last frame of a command that ran |
//! | `0x84` refused | server to client | JSON [`Refusal`]; the last frame of a command that was not started |
//!
//! A payload is at most [`MAX_PAYLOAD`] bytes; the endpoints cut data into frames of at most
//! [`DATA_CHUNK`]. A client stays connected until it has read the last frame: a connection that
//! ends earlier is how it says it has gone, and the server kills the command's process group.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// The version of the protocol a [`Request`] names; a server speaks this one only.
pub const PROTOCOL_VERSION: u32 = 1;

/// The most bytes a frame's payload may have (4 MiB: a request with a long argument list).
pub const MAX_PAYLOAD: usize = 4 * 1024 * 1024;

/// The size the endpoints cut a command's input and output into.
pub const DATA_CHUNK: usize = 32 * 1024;

const KIND_REQUEST: u8 = 0x01;
const KIND_STDIN: u8 = 0x02;
const KIND_STDIN_END: u8 = 0x03;
const KIND_STDOUT: u8 = 0x81;
const KIND_STDERR: u8 = 0x82;
const KIND_EXIT: u8 = 0x83;
const KIND_REFUSED: u8 = 0x84;

/// What went wrong reading or writing a frame.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// The transport failed.
    #[error("the connection failed")]
    Io(#[source] io::Error),
    /// The peer's connection ended in the middle of a frame.
    #[error("the connection ended in the middle of a frame")]
    Truncated,
    /// A frame announces a payload larger than [`MAX_PAYLOAD`].
    #[error("a frame announces {0} bytes, more than the {MAX_PAYLOAD} a frame may hold")]
    TooLarge(usize),
    /// A frame of a kind this direction does not have.
    #[error("unexpected frame kind {0:#04x}")]
    UnexpectedKind(u8),
    /// A payload that is not what its kind says.
    #[error("malformed {0} frame")]
    Malformed(&'static str),
}

impl adam_error::Classify for ProtocolError {
    fn class(&self) -> adam_error::ErrorClass {
        match self {
            Self::Io(_) | Self::Truncated => adam_error::ErrorClass::Transient,
            Self::TooLarge(_) | Self::UnexpectedKind(_) | Self::Malformed(_) => {
                adam_error::ErrorClass::Corrupt
            }
        }
    }
}

/// An argument, a path or a value of the environment: text when it is UTF-8, else its bytes in
/// base64 (`{"b64": "..."}`), so that whatever the operating system holds crosses the wire intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireStr(OsString);

impl WireStr {
    /// The string as the operating system has it.
    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }

    /// Take the string.
    pub fn into_os_string(self) -> OsString {
        self.0
    }
}

impl<T: Into<OsString>> From<T> for WireStr {
    fn from(value: T) -> Self {
        Self(value.into())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum WireRepr {
    Text(String),
    Bytes {
        #[serde(rename = "b64")]
        bytes: String,
    },
}

impl From<WireStr> for WireRepr {
    fn from(value: WireStr) -> Self {
        match value.0.into_string() {
            Ok(text) => Self::Text(text),
            Err(raw) => Self::Bytes {
                bytes: STANDARD.encode(raw.as_bytes()),
            },
        }
    }
}

impl TryFrom<WireRepr> for WireStr {
    type Error = String;

    fn try_from(value: WireRepr) -> Result<Self, Self::Error> {
        match value {
            WireRepr::Text(text) => Ok(Self(OsString::from(text))),
            WireRepr::Bytes { bytes } => STANDARD
                .decode(bytes)
                .map(|raw| Self(OsString::from_vec(raw)))
                .map_err(|_| "not base64".to_owned()),
        }
    }
}

impl Serialize for WireStr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WireRepr::from(self.clone()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WireStr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        WireRepr::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

/// The command to run: what follows `git` on the command line, the directory to run it in and the
/// environment it starts with. The server runs **only its own `git`**, whatever the request says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The protocol version, [`PROTOCOL_VERSION`].
    #[serde(rename = "v")]
    pub version: u32,
    /// The arguments after `git` (not `git` itself).
    pub argv: Vec<WireStr>,
    /// The directory to run in: absolute, and inside the server's root.
    pub cwd: WireStr,
    /// The environment the client offers; the server keeps only the names it allows
    /// ([`crate::environment`]) and adds its own `PATH`.
    pub env: Vec<EnvPair>,
}

/// One variable of a request's environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvPair {
    /// The name.
    pub name: String,
    /// The value.
    pub value: WireStr,
}

impl Request {
    /// A request for `git <argv>` in `cwd` with `env`.
    pub fn new<A, S, E, N, V>(argv: A, cwd: impl Into<OsString>, env: E) -> Self
    where
        A: IntoIterator<Item = S>,
        S: Into<OsString>,
        E: IntoIterator<Item = (N, V)>,
        N: Into<String>,
        V: Into<OsString>,
    {
        Self {
            version: PROTOCOL_VERSION,
            argv: argv.into_iter().map(|a| WireStr::from(a.into())).collect(),
            cwd: WireStr::from(cwd.into()),
            env: env
                .into_iter()
                .map(|(name, value)| EnvPair {
                    name: name.into(),
                    value: WireStr::from(value.into()),
                })
                .collect(),
        }
    }
}

/// How a command that ran ended: its exit code, or the signal that killed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exit {
    /// The exit code, when the command exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    /// The signal that killed it, when it did not exit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

impl Exit {
    /// Whether the command exited with code 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// The code a shell would report: the exit code, or 128 plus the signal. `1` for a status that
    /// says neither.
    pub fn shell_code(&self) -> u8 {
        match (self.code, self.signal) {
            (Some(code), _) => u8::try_from(code & 0xff).unwrap_or(1),
            (None, Some(signal)) => u8::try_from((128 + signal) & 0xff).unwrap_or(1),
            (None, None) => 1,
        }
    }
}

/// Why the server did not start a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RefusalReason {
    /// The first frame was not a request, or the request is not well formed.
    BadRequest,
    /// The request names a protocol version the server does not speak.
    UnsupportedVersion,
    /// The request has no arguments (`git` alone).
    NoArguments,
    /// The directory is not inside the server's root, or it is not a plain directory (a link on
    /// the way, or at its end).
    BadDirectory,
    /// The server's `git` could not be started.
    SpawnFailed,
    /// The server is shutting down.
    ShuttingDown,
}

/// The server's answer to a request it did not run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refusal {
    /// Why, as a code.
    pub reason: RefusalReason,
    /// Why, in a sentence. It names no argument and no value of the environment.
    pub message: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({:?})", self.message, self.reason)
    }
}

/// A frame a client sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientFrame {
    /// The command to run. The first frame, once.
    Request(Request),
    /// Bytes for the command's standard input.
    Stdin(Vec<u8>),
    /// The command's standard input ends.
    StdinEnd,
}

/// A frame the server sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerFrame {
    /// Bytes the command wrote to its standard output.
    Stdout(Vec<u8>),
    /// Bytes the command wrote to its standard error.
    Stderr(Vec<u8>),
    /// The command ended. The last frame.
    Exit(Exit),
    /// The command was not started. The last frame.
    Refused(Refusal),
}

fn json<T: Serialize>(value: &T) -> Vec<u8> {
    // These types hold strings and numbers only: serialising them cannot fail.
    serde_json::to_vec(value).unwrap_or_default()
}

fn parse<T: for<'de> Deserialize<'de>>(
    payload: &[u8],
    what: &'static str,
) -> Result<T, ProtocolError> {
    serde_json::from_slice(payload).map_err(|_| ProtocolError::Malformed(what))
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: u8,
    payload: &[u8],
) -> Result<(), ProtocolError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(ProtocolError::TooLarge(payload.len()));
    }
    // Checked above: it fits.
    let length =
        u32::try_from(payload.len()).map_err(|_| ProtocolError::TooLarge(payload.len()))?;
    let mut head = [0u8; 5];
    head[0] = kind;
    head[1..].copy_from_slice(&length.to_be_bytes());
    writer.write_all(&head).await.map_err(ProtocolError::Io)?;
    if !payload.is_empty() {
        writer.write_all(payload).await.map_err(ProtocolError::Io)?;
    }
    writer.flush().await.map_err(ProtocolError::Io)
}

/// The next frame: its kind and payload, `None` when the peer closed the connection between
/// two frames.
async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<(u8, Vec<u8>)>, ProtocolError> {
    let mut head = [0u8; 5];
    let mut filled = 0;
    while filled < head.len() {
        match reader.read(&mut head[filled..]).await {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(ProtocolError::Truncated),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(ProtocolError::Io(e)),
        }
    }
    let length = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if length > MAX_PAYLOAD {
        return Err(ProtocolError::TooLarge(length));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).await.map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            ProtocolError::Truncated
        } else {
            ProtocolError::Io(e)
        }
    })?;
    Ok(Some((head[0], payload)))
}

/// Send a frame to the server.
///
/// # Errors
///
/// [`ProtocolError::Io`] when the connection fails, [`ProtocolError::TooLarge`] for a payload over
/// [`MAX_PAYLOAD`].
pub async fn write_client_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &ClientFrame,
) -> Result<(), ProtocolError> {
    match frame {
        ClientFrame::Request(request) => write_frame(writer, KIND_REQUEST, &json(request)).await,
        ClientFrame::Stdin(bytes) => write_frame(writer, KIND_STDIN, bytes).await,
        ClientFrame::StdinEnd => write_frame(writer, KIND_STDIN_END, &[]).await,
    }
}

/// Read the next frame a client sent; `None` when it closed the connection between two frames.
///
/// # Errors
///
/// [`ProtocolError`] for a connection that fails or ends in a frame, a frame that is too large,
/// of a kind a client does not send, or whose payload is not what its kind says.
pub async fn read_client_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<ClientFrame>, ProtocolError> {
    let Some((kind, payload)) = read_frame(reader).await? else {
        return Ok(None);
    };
    match kind {
        KIND_REQUEST => Ok(Some(ClientFrame::Request(parse(&payload, "request")?))),
        KIND_STDIN => Ok(Some(ClientFrame::Stdin(payload))),
        KIND_STDIN_END if payload.is_empty() => Ok(Some(ClientFrame::StdinEnd)),
        KIND_STDIN_END => Err(ProtocolError::Malformed("stdin end")),
        other => Err(ProtocolError::UnexpectedKind(other)),
    }
}

/// Send a frame to the client.
///
/// # Errors
///
/// As [`write_client_frame`].
pub async fn write_server_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &ServerFrame,
) -> Result<(), ProtocolError> {
    match frame {
        ServerFrame::Stdout(bytes) => write_frame(writer, KIND_STDOUT, bytes).await,
        ServerFrame::Stderr(bytes) => write_frame(writer, KIND_STDERR, bytes).await,
        ServerFrame::Exit(exit) => write_frame(writer, KIND_EXIT, &json(exit)).await,
        ServerFrame::Refused(refusal) => write_frame(writer, KIND_REFUSED, &json(refusal)).await,
    }
}

/// Read the next frame the server sent; `None` when it closed the connection between two frames.
///
/// # Errors
///
/// As [`read_client_frame`], for the frames a server sends.
pub async fn read_server_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<ServerFrame>, ProtocolError> {
    let Some((kind, payload)) = read_frame(reader).await? else {
        return Ok(None);
    };
    match kind {
        KIND_STDOUT => Ok(Some(ServerFrame::Stdout(payload))),
        KIND_STDERR => Ok(Some(ServerFrame::Stderr(payload))),
        KIND_EXIT => Ok(Some(ServerFrame::Exit(parse(&payload, "exit")?))),
        KIND_REFUSED => Ok(Some(ServerFrame::Refused(parse(&payload, "refused")?))),
        other => Err(ProtocolError::UnexpectedKind(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Request {
        Request::new(
            ["status", "--porcelain"],
            "/work/workspaces/run/slot",
            [("GIT_DIR", "/work/git/x.git"), ("LC_ALL", "C")],
        )
    }

    #[tokio::test]
    async fn every_client_frame_round_trips() {
        let frames = [
            ClientFrame::Request(sample()),
            ClientFrame::Stdin(b"patch\n".to_vec()),
            ClientFrame::Stdin(Vec::new()),
            ClientFrame::StdinEnd,
        ];
        let mut wire = Vec::new();
        for frame in &frames {
            write_client_frame(&mut wire, frame).await.unwrap();
        }
        let mut reader = wire.as_slice();
        for frame in &frames {
            assert_eq!(
                read_client_frame(&mut reader).await.unwrap().as_ref(),
                Some(frame)
            );
        }
        assert!(read_client_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn every_server_frame_round_trips() {
        let frames = [
            ServerFrame::Stdout(vec![0, 1, 2, 255]),
            ServerFrame::Stderr(b"fatal: no\n".to_vec()),
            ServerFrame::Exit(Exit {
                code: Some(128),
                signal: None,
            }),
            ServerFrame::Exit(Exit {
                code: None,
                signal: Some(9),
            }),
            ServerFrame::Refused(Refusal {
                reason: RefusalReason::BadDirectory,
                message: "outside the root".to_owned(),
            }),
        ];
        let mut wire = Vec::new();
        for frame in &frames {
            write_server_frame(&mut wire, frame).await.unwrap();
        }
        let mut reader = wire.as_slice();
        for frame in &frames {
            assert_eq!(
                read_server_frame(&mut reader).await.unwrap().as_ref(),
                Some(frame)
            );
        }
    }

    #[test]
    fn the_request_is_json_with_a_short_version_key() {
        let json = serde_json::to_value(sample()).unwrap();
        assert_eq!(json["v"], 1);
        assert_eq!(json["argv"][0], "status");
        assert_eq!(json["env"][0]["name"], "GIT_DIR");
        assert_eq!(json["env"][0]["value"], "/work/git/x.git");
    }

    #[test]
    fn what_is_not_utf8_crosses_as_base64_and_comes_back_the_same() {
        let raw = OsString::from_vec(vec![b'a', 0xff, 0xfe, b'z']);
        let request = Request::new([raw.clone()], "/work", Vec::<(String, OsString)>::new());
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(
            json["argv"][0]["b64"],
            STANDARD.encode([b'a', 0xff, 0xfe, b'z'])
        );
        let back: Request = serde_json::from_value(json).unwrap();
        assert_eq!(back.argv[0].as_os_str(), raw.as_os_str());
    }

    #[test]
    fn a_request_with_a_field_it_does_not_know_is_not_a_request() {
        let text = r#"{"v":1,"argv":[],"cwd":"/work","env":[],"shell":true}"#;
        assert!(serde_json::from_str::<Request>(text).is_err());
        let text = r#"{"v":1,"argv":[{"b64":"!!"}],"cwd":"/work","env":[]}"#;
        assert!(serde_json::from_str::<Request>(text).is_err(), "bad base64");
    }

    #[tokio::test]
    async fn a_frame_over_the_limit_is_refused_before_it_is_read() {
        let mut wire = vec![KIND_STDIN];
        wire.extend_from_slice(&u32::try_from(MAX_PAYLOAD + 1).unwrap().to_be_bytes());
        // No payload follows: the length alone is the verdict.
        let error = read_client_frame(&mut wire.as_slice()).await.unwrap_err();
        assert!(matches!(error, ProtocolError::TooLarge(n) if n == MAX_PAYLOAD + 1));

        let error = write_client_frame(
            &mut Vec::new(),
            &ClientFrame::Stdin(vec![0; MAX_PAYLOAD + 1]),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ProtocolError::TooLarge(_)));
    }

    #[tokio::test]
    async fn a_frame_cut_short_is_not_a_clean_end() {
        // Two bytes of a header.
        let error = read_client_frame(&mut [KIND_STDIN, 0].as_slice())
            .await
            .unwrap_err();
        assert!(matches!(error, ProtocolError::Truncated), "{error}");
        // A header that promises more than follows.
        let mut wire = vec![KIND_STDIN];
        wire.extend_from_slice(&10u32.to_be_bytes());
        wire.extend_from_slice(b"abc");
        let error = read_client_frame(&mut wire.as_slice()).await.unwrap_err();
        assert!(matches!(error, ProtocolError::Truncated), "{error}");
    }

    #[tokio::test]
    async fn a_kind_from_the_other_direction_is_refused() {
        let mut wire = Vec::new();
        write_server_frame(&mut wire, &ServerFrame::Stdout(vec![1]))
            .await
            .unwrap();
        let error = read_client_frame(&mut wire.as_slice()).await.unwrap_err();
        assert!(matches!(error, ProtocolError::UnexpectedKind(KIND_STDOUT)));

        let mut wire = Vec::new();
        write_client_frame(&mut wire, &ClientFrame::StdinEnd)
            .await
            .unwrap();
        let error = read_server_frame(&mut wire.as_slice()).await.unwrap_err();
        assert!(matches!(
            error,
            ProtocolError::UnexpectedKind(KIND_STDIN_END)
        ));
    }

    #[tokio::test]
    async fn a_payload_that_is_not_what_its_kind_says_is_malformed() {
        let mut wire = Vec::new();
        write_frame(&mut wire, KIND_REQUEST, b"not json")
            .await
            .unwrap();
        let error = read_client_frame(&mut wire.as_slice()).await.unwrap_err();
        assert!(matches!(error, ProtocolError::Malformed("request")));

        let mut wire = Vec::new();
        write_frame(&mut wire, KIND_STDIN_END, b"x").await.unwrap();
        let error = read_client_frame(&mut wire.as_slice()).await.unwrap_err();
        assert!(matches!(error, ProtocolError::Malformed("stdin end")));
    }

    #[test]
    fn the_shell_code_of_an_exit() {
        let code = |code, signal| Exit { code, signal }.shell_code();
        assert_eq!(code(Some(0), None), 0);
        assert_eq!(code(Some(128), None), 128);
        assert_eq!(code(None, Some(9)), 137);
        assert_eq!(code(None, Some(15)), 143);
        assert_eq!(code(None, None), 1);
        assert!(
            Exit {
                code: Some(0),
                signal: None
            }
            .success()
        );
        assert!(
            !Exit {
                code: None,
                signal: Some(9)
            }
            .success()
        );
    }
}
