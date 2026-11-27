//! `uploads/v1`, the agent's side of the outgoing direction: ask the thread tools for a slot with
//! `request_upload`, `PUT` the bytes to the presigned URL, keep the `{uploadId, expiresAt}`.
//!
//! ```mermaid
//! sequenceDiagram
//!     participant C as caller (a tool that holds a file over its inline cap)
//!     participant U as Uploader
//!     participant T as thread tools (Endpoint)
//!     participant B as bucket (public files host)
//!     C->>U: upload(endpoint, request, bytes)
//!     U->>T: tools/call request_upload {filename, mediaType, size, sha256}
//!     T-->>U: {uploadId, url, method PUT, headers, expiresAt}, or isError with a code
//!     U->>B: PUT url, exactly the signed headers, the bytes
//!     B-->>U: 2xx (a refusal is an error, never a retry elsewhere)
//!     U-->>C: Uploaded {upload_id, expires_at}
//! ```
//!
//! The signed URL and the signed headers are the only credential the bucket takes: they live in
//! an [`UploadSlot`] whose `Debug` shows neither, are never logged, and are scrubbed from every
//! error this module returns. This module opens no connection but the one `PUT`, with no store
//! credential. A slot the orchestrator refuses is an [`UploadError`] with its code: the caller
//! decides what to do, nothing here falls back to sending the bytes inline.

use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::once::{Endpoint, EndpointError, RemoteResult};
use crate::policy::McpPolicy;
use crate::redact::Redactor;

/// The tool's name on the thread-tools endpoint.
pub const REQUEST_UPLOAD: &str = "request_upload";

/// How long the `PUT` is given by default. The slot's own life bounds the start of the request,
/// not its length, and a file over the inline cap can be large.
pub const DEFAULT_PUT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The longest `uploadId` the contract allows, in bytes.
const MAX_UPLOAD_ID_BYTES: usize = 1024;

/// What the agent asks a slot for: the four members of `request_upload`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRequest {
    filename: String,
    media_type: String,
    size: u64,
    sha256: String,
}

impl UploadRequest {
    /// A request for `bytes`: its size and its SHA-256 (64 lower-case hexadecimal characters) are
    /// computed here, so the key the orchestrator signs is the content's own.
    pub fn for_bytes(
        filename: impl Into<String>,
        media_type: impl Into<String>,
        bytes: &[u8],
    ) -> Self {
        let digest = Sha256::digest(bytes);
        let mut sha256 = String::with_capacity(64);
        for byte in digest {
            sha256.push_str(&format!("{byte:02x}"));
        }
        Self {
            filename: filename.into(),
            media_type: media_type.into(),
            size: bytes.len() as u64,
            sha256,
        }
    }

    /// The file's name as it will be asked.
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// The declared media type.
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// The size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The content's SHA-256, lower-case hexadecimal.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    fn arguments(&self) -> Map<String, Value> {
        let Value::Object(map) = json!({
            "filename": self.filename,
            "mediaType": self.media_type,
            "size": self.size,
            "sha256": self.sha256,
        }) else {
            unreachable!("a JSON object literal")
        };
        map
    }
}

/// What the orchestrator kept of a successful upload: the id the artifact names, and when the
/// slot stopped working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uploaded {
    /// The MAC the orchestrator issued; the artifact of part 2 names it. Not a secret of the
    /// bucket, but only this thread and job can use it.
    pub upload_id: String,
    /// The slot's expiry, RFC 3339, as the orchestrator wrote it.
    pub expires_at: String,
}

/// A slot: the URL, the method and the signed headers. Holds the bucket's only credential, so its
/// `Debug` shows none of it.
pub struct UploadSlot {
    upload_id: String,
    expires_at: String,
    url: String,
    headers: Vec<(String, String)>,
}

impl std::fmt::Debug for UploadSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadSlot")
            .field("expires_at", &self.expires_at)
            .field("headers", &self.headers.len())
            .finish_non_exhaustive()
    }
}

impl UploadSlot {
    /// What is kept of the slot once the bytes are in.
    pub fn uploaded(&self) -> Uploaded {
        Uploaded {
            upload_id: self.upload_id.clone(),
            expires_at: self.expires_at.clone(),
        }
    }
}

/// Why an upload did not happen. No text of it carries the URL, a signed header or the token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum UploadError {
    /// The orchestrator refused the slot (a tool error). `code` is the contract's:
    /// `not_activated`, `too_large`, `over_limit`, `unsupported`, `bad_request`,
    /// `caller_not_allowed`, `task_over`, `unavailable`, or `unknown` when the text names none.
    #[error("the orchestrator refused the upload slot ({code}): {message}")]
    Refused {
        /// The contract's code.
        code: String,
        /// The orchestrator's own text, scrubbed.
        message: String,
    },
    /// The thread-tools endpoint could not be called.
    #[error("the thread tools could not give an upload slot: {0}")]
    Endpoint(#[from] EndpointError),
    /// The answer is not the shape the contract defines (or asks for something other than a
    /// `PUT`, or a URL this client's policy refuses).
    #[error("the upload slot is not usable: {0}")]
    BadSlot(String),
    /// The bytes handed over are not the ones the request describes.
    #[error("the bytes are not the ones the upload was requested for: {0}")]
    BytesMismatch(&'static str),
    /// The bucket answered the `PUT` with this status (an expired URL, a signature or checksum
    /// that does not match). Nothing is retried.
    #[error("the bucket refused the upload with status {0}")]
    PutRejected(u16),
    /// The `PUT` could not be made or did not finish (connection, time).
    #[error("the upload did not complete: {0}")]
    PutFailed(String),
}

/// The client of the outgoing direction. Cheap to make; holds nothing between uploads.
#[derive(Debug, Clone)]
pub struct Uploader {
    allow_insecure: bool,
    put_timeout: Duration,
}

impl Uploader {
    /// An uploader under `policy`: the URL of a slot follows its rules (https, or plain `http` to
    /// this machine unless the deployment allows more).
    pub fn new(policy: &McpPolicy) -> Self {
        Self {
            allow_insecure: policy.insecure_allowed(),
            put_timeout: DEFAULT_PUT_TIMEOUT,
        }
    }

    /// Give the `PUT` this long instead of [`DEFAULT_PUT_TIMEOUT`].
    #[must_use]
    pub fn put_timeout(mut self, timeout: Duration) -> Self {
        self.put_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// Ask the thread tools for a slot. Nothing is sent to the bucket.
    ///
    /// # Errors
    ///
    /// [`UploadError::Refused`] when the orchestrator says no (size over `maxBytes`, an
    /// over-limit job, ...), [`UploadError::Endpoint`] when the call fails, and
    /// [`UploadError::BadSlot`] when the answer is not a usable slot.
    pub async fn request(
        &self,
        endpoint: &Endpoint,
        request: &UploadRequest,
    ) -> Result<UploadSlot, UploadError> {
        let result = endpoint
            .call_tool(REQUEST_UPLOAD, request.arguments())
            .await?;
        if result.is_error {
            return Err(refusal(&result.text));
        }
        let slot = self.slot_of(&result)?;
        Ok(slot)
    }

    /// `PUT` `bytes` to `slot`, with exactly its method, URL and signed headers.
    ///
    /// # Errors
    ///
    /// [`UploadError::BytesMismatch`] when `bytes` are not the size and hash of `request`;
    /// [`UploadError::PutRejected`] for a non-2xx answer; [`UploadError::PutFailed`] otherwise.
    pub async fn put(
        &self,
        slot: &UploadSlot,
        request: &UploadRequest,
        bytes: Vec<u8>,
    ) -> Result<Uploaded, UploadError> {
        if bytes.len() as u64 != request.size {
            return Err(UploadError::BytesMismatch("the size differs"));
        }
        if UploadRequest::for_bytes("", "", &bytes).sha256 != request.sha256 {
            return Err(UploadError::BytesMismatch("the SHA-256 differs"));
        }
        let mut redactor = Redactor::default();
        redactor.add(&slot.url);
        for (_, value) in &slot.headers {
            redactor.add(value);
        }
        let fail = |e: reqwest::Error| {
            // reqwest's text names the URL: drop it, then scrub what is left.
            UploadError::PutFailed(redactor.scrub(&e.without_url().to_string()))
        };
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(self.put_timeout)
            .build()
            .map_err(fail)?;
        let mut put = client.put(&slot.url);
        for (name, value) in &slot.headers {
            // Validated when the slot was read.
            put = put.header(name.as_str(), value.as_str());
        }
        let response = put.body(bytes).send().await.map_err(fail)?;
        let status = response.status();
        if !status.is_success() {
            return Err(UploadError::PutRejected(status.as_u16()));
        }
        Ok(slot.uploaded())
    }

    /// Ask for a slot, `PUT` the bytes, and keep the `{uploadId, expiresAt}`.
    ///
    /// # Errors
    ///
    /// Any [`UploadError`] of [`request`](Self::request) or [`put`](Self::put); the first failure
    /// ends it and the bytes are never sent anywhere else.
    pub async fn upload(
        &self,
        endpoint: &Endpoint,
        request: &UploadRequest,
        bytes: Vec<u8>,
    ) -> Result<Uploaded, UploadError> {
        let slot = self.request(endpoint, request).await?;
        self.put(&slot, request, bytes).await
    }

    fn slot_of(&self, result: &RemoteResult) -> Result<UploadSlot, UploadError> {
        let bad = |why: &str| UploadError::BadSlot(why.to_owned());
        let body = match &result.structured {
            Some(v) => v.clone(),
            None => serde_json::from_str(&result.text)
                .map_err(|_| bad("the answer is not a JSON object"))?,
        };
        let text = |key: &str| {
            body.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        let upload_id = text("uploadId").ok_or_else(|| bad("no uploadId"))?;
        if upload_id.len() > MAX_UPLOAD_ID_BYTES {
            return Err(bad("the uploadId is too long"));
        }
        let expires_at = text("expiresAt").ok_or_else(|| bad("no expiresAt"))?;
        if text("method") != Some("PUT") {
            return Err(bad("the method is not PUT"));
        }
        let url = text("url").ok_or_else(|| bad("no url"))?;
        crate::url::check("upload", url, self.allow_insecure, &Redactor::default())
            .map_err(|_| bad("the url is refused by the policy (https, no credentials)"))?;
        let Some(Value::Object(headers)) = body.get("headers") else {
            return Err(bad("no headers"));
        };
        let mut signed = Vec::with_capacity(headers.len());
        for (name, value) in headers {
            let Some(value) = value.as_str() else {
                return Err(bad("a header value is not text"));
            };
            if HeaderName::from_bytes(name.as_bytes()).is_err()
                || HeaderValue::from_str(value).is_err()
            {
                return Err(bad("a header is not a valid HTTP header"));
            }
            signed.push((name.clone(), value.to_owned()));
        }
        Ok(UploadSlot {
            upload_id: upload_id.to_owned(),
            expires_at: expires_at.to_owned(),
            url: url.to_owned(),
            headers: signed,
        })
    }
}

/// The contract's code in the text of a tool error: the first word of it that is one.
fn refusal(text: &str) -> UploadError {
    const CODES: [&str; 8] = [
        "not_activated",
        "too_large",
        "over_limit",
        "unsupported",
        "bad_request",
        "caller_not_allowed",
        "task_over",
        "unavailable",
    ];
    let code = CODES
        .iter()
        .find(|code| {
            text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .any(|word| word == **code)
        })
        .copied()
        .unwrap_or("unknown");
    UploadError::Refused {
        code: code.to_owned(),
        message: text.chars().take(512).collect(),
    }
}
