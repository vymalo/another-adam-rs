//! [`McpTool`]: one tool of one MCP server, as an `adam_llm_agent::Tool`, and the mapping of a
//! server's answer to text.
//!
//! ```mermaid
//! sequenceDiagram
//!     participant L as LlmAgent (step tool:CALL_ID)
//!     participant T as McpTool
//!     participant C as Connection
//!     participant S as MCP server
//!     L->>T: call(ctx, args)
//!     T->>C: peer()
//!     Note over C: session gone? one reconnect from the same recipe
//!     C-->>T: peer, generation
//!     T->>S: tools/call (call_tool_once), within call_timeout and ctx.cancelled()
//!     S-->>T: result
//!     T-->>L: ToolOutput (text of the content, scrubbed of expanded values, cut at 64 KiB; is_error kept;<br/>with files: true, its images, audio and blobs as file artifacts and a line each)
//!     Note over L: the result is journaled; a replay returns it without calling again
//! ```

use std::sync::Arc;
use std::time::Duration;

use adam_llm_agent::{Artifact, StepStyle, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::ToolSpec;
use adam_runtime::{MAX_ARTIFACT_FILE_BYTES, checked_media_type, extension_of};
use async_trait::async_trait;
use base64::Engine as _;
use rmcp::ServiceError;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ResourceContents,
};
use serde_json::{Map, Value};

use crate::bearer::{CLOSE_GRACE, PerCall, UnusableBearer};
use crate::connection::Connection;
use crate::redact::Redactor;
use crate::text::{MAX_MESSAGE_BYTES, MAX_RESULT_BYTES, cap_text};

/// A tool of an MCP server. The model sees it as `<server>__<tool>`.
///
/// A call is **not** retried by this crate and never becomes a [`ToolError::Transient`] because of
/// what the server did: an MCP call has no idempotency key, so a call that failed on the way may or
/// may not have run, and the model is told so in an error result. (The one `Transient` there is
/// comes from the deployment's [`CallBearer`](crate::CallBearer) of a server bound with
/// [`McpPolicy::bearer_per_call`](crate::McpPolicy::bearer_per_call): it is returned before
/// anything is sent, so a retry cannot repeat a call.) `adam-llm-agent` journals the result under the step
/// `tool:CALL_ID`, so a replay of a committed call returns the recorded result and does not call
/// the server again; a transition that fails *before* its commit (a crash, a lost lease, a later
/// tool of the same turn returning `Transient`) runs the call again, so an MCP tool is
/// **at-least-once**.
pub(crate) struct McpTool {
    spec: ToolSpec,
    server: String,
    /// The tool's name on the server.
    remote: String,
    /// The tool's title, when the server gave one: the label of its step.
    title: Option<String>,
    target: Target,
    call_timeout: Duration,
    /// `files: true` on its server: the files of a result are shared, not described.
    files: bool,
}

/// How a tool reaches its server.
#[derive(Clone)]
pub(crate) enum Target {
    /// One connection, kept and redialled when it breaks.
    Kept(Arc<Connection>),
    /// A connection of its own for every call, with the bearer the deployment gives that call.
    PerCall(Arc<PerCall>),
}

impl Target {
    /// Close what is open, and make every later call an error result.
    pub(crate) async fn close(&self) {
        match self {
            Self::Kept(connection) => connection.close().await,
            Self::PerCall(per_call) => per_call.close(),
        }
    }
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool")
            .field("name", &self.spec.name)
            .finish_non_exhaustive()
    }
}

impl McpTool {
    pub(crate) fn new(
        spec: ToolSpec,
        server: String,
        remote: String,
        title: Option<String>,
        target: Target,
        call_timeout: Duration,
        files: bool,
    ) -> Self {
        Self {
            spec,
            server,
            remote,
            title,
            target,
            call_timeout,
            files,
        }
    }

    fn name(&self) -> &str {
        &self.spec.name
    }

    fn lost(&self, what: &str) -> ToolOutput {
        ToolOutput::error(format!(
            "the call to `{}` did not finish: {what}. It may or may not have run on the MCP \
             server `{}`: check before you repeat it",
            self.name(),
            self.server
        ))
    }

    /// What the server answered (or did not), as the result of the call, scrubbed of what
    /// `redactor` knows; and whether the transport failed, so that a kept session is dropped and
    /// the next call reconnects.
    fn describe(
        &self,
        redactor: &Redactor,
        outcome: Result<CallToolResponse, ServiceError>,
    ) -> (ToolOutput, bool) {
        let scrub = |text: &str| cap_text(redactor.scrub(text), MAX_MESSAGE_BYTES);
        let output = match outcome {
            Ok(CallToolResponse::Complete(result)) => map_result(
                &result,
                redactor,
                self.files.then_some(self.remote.as_str()),
            ),
            Ok(CallToolResponse::InputRequired(_)) => ToolOutput::error(format!(
                "the MCP server `{}` needs more input for `{}` (input_required), which this \
                 client cannot give: nobody is there to answer",
                self.server,
                self.name()
            )),
            Ok(CallToolResponse::Task(_)) => ToolOutput::error(format!(
                "the MCP server `{}` answered `{}` with a task to poll, which this client does \
                 not support",
                self.server,
                self.name()
            )),
            Ok(_) => ToolOutput::error(format!(
                "the MCP server `{}` answered `{}` in a way this client does not understand",
                self.server,
                self.name()
            )),
            Err(ServiceError::McpError(error)) => ToolOutput::error(format!(
                "the MCP server `{}` refused the call to `{}`: {}",
                self.server,
                self.name(),
                scrub(&error.message)
            )),
            Err(ServiceError::Timeout { .. }) => self.lost(
                "the connection gave up waiting for the answer (the call may still be running \
                 on the server)",
            ),
            Err(error) => {
                return (
                    self.lost(&format!(
                        "the connection to the server failed ({})",
                        scrub(&adam_error::report(&error))
                    )),
                    true,
                );
            }
        };
        (output, false)
    }

    fn cancelled(&self) -> ToolOutput {
        ToolOutput::error(format!(
            "the run was cancelled while `{}` was called; it may or may not have run",
            self.name()
        ))
    }

    fn no_answer(&self) -> ToolOutput {
        self.lost(&format!(
            "no answer within {} ms (the call may still be running on the server)",
            self.call_timeout.as_millis()
        ))
    }

    /// A call on the kept connection.
    async fn call_kept(
        &self,
        ctx: &ToolCtx,
        connection: &Connection,
        params: CallToolRequestParams,
    ) -> ToolOutput {
        let (peer, generation) = tokio::select! {
            () = ctx.cancelled() => return self.cancelled(),
            got = connection.peer() => match got {
                Ok(pair) => pair,
                Err(text) => return ToolOutput::error(text),
            },
        };
        let outcome = tokio::select! {
            () = ctx.cancelled() => return self.cancelled(),
            waited = tokio::time::timeout(self.call_timeout, peer.call_tool_once(params)) => waited,
        };
        match outcome {
            Ok(outcome) => {
                let (output, broken) = self.describe(connection.redactor(), outcome);
                if broken {
                    // A broken session is dropped, so the next call reconnects.
                    connection.mark_broken(generation).await;
                }
                output
            }
            Err(_) => self.no_answer(),
        }
    }

    /// A call on a connection of its own, with the bearer the deployment gives it. What the
    /// deployment refuses is answered before anything is sent.
    async fn call_per_call(
        &self,
        ctx: &ToolCtx,
        per_call: &PerCall,
        arguments: Map<String, Value>,
    ) -> Result<ToolOutput, ToolError> {
        if per_call.is_closed() {
            return Ok(ToolOutput::error(format!(
                "the connection to the MCP server `{}` was shut down",
                self.server
            )));
        }
        let token = tokio::select! {
            () = ctx.cancelled() => return Ok(self.cancelled()),
            got = per_call.bearer.for_call(&self.remote, &arguments) => got,
        };
        let token = match token {
            Ok(token) => token,
            Err(ToolError::Permanent(why)) => {
                return Ok(ToolOutput::error(format!(
                    "the call to `{}` was not sent: {}",
                    self.name(),
                    per_call.scrub(&why)
                )));
            }
            Err(ToolError::Transient(why)) => {
                return Err(ToolError::Transient(per_call.scrub(&why)));
            }
            Err(other) => return Err(other),
        };
        let recipe = match per_call.recipe(&token) {
            Ok(recipe) => recipe,
            Err(UnusableBearer) => {
                return Ok(ToolOutput::error(format!(
                    "the call to `{}` was not sent: the bearer for it is empty or not a valid \
                     header value",
                    self.name()
                )));
            }
        };
        let params = CallToolRequestParams::new(self.remote.clone()).with_arguments(arguments);
        let mut service = tokio::select! {
            () = ctx.cancelled() => return Ok(self.cancelled()),
            dialled = recipe.dial() => match dialled {
                Ok(service) => service,
                // Failed before the call was made: the error is scrubbed of the token.
                Err(error) => {
                    return Ok(ToolOutput::error(format!(
                        "the call to `{}` was not made: {error}",
                        self.name()
                    )));
                }
            },
        };
        let outcome = tokio::select! {
            () = ctx.cancelled() => return Ok(self.cancelled()),
            waited = tokio::time::timeout(self.call_timeout, service.peer().call_tool_once(params)) => waited,
        };
        let output = match outcome {
            Ok(outcome) => self.describe(&recipe.redactor, outcome).0,
            Err(_) => self.no_answer(),
        };
        let _ = service.close_with_timeout(CLOSE_GRACE).await;
        Ok(output)
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    /// The step of a call is labelled with the tool's `title` when its server gave one (`Search the
    /// web`), and with `<server>__<tool>` otherwise, as the model knows it.
    fn step_style(&self) -> StepStyle {
        match &self.title {
            Some(title) => StepStyle::default().with_label(title.clone()),
            None => StepStyle::default(),
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let arguments: Map<String, Value> = match args {
            Value::Null => Map::new(),
            Value::Object(map) => map,
            other => {
                return Ok(ToolOutput::error(format!(
                    "`{}` takes a JSON object of arguments, but got {}",
                    self.name(),
                    kind(&other)
                )));
            }
        };
        match &self.target {
            Target::Kept(connection) => {
                let params =
                    CallToolRequestParams::new(self.remote.clone()).with_arguments(arguments);
                Ok(self.call_kept(ctx, connection, params).await)
            }
            Target::PerCall(per_call) => self.call_per_call(ctx, per_call, arguments).await,
        }
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The most files one result shares. A file block after them is not shared, and the result says so.
pub(crate) const MAX_FILES_PER_RESULT: usize = 16;

/// A server's answer as text for the model: the blocks joined by newlines, scrubbed of what
/// `redactor` knows (a server that echoes a credential it was given must not hand it to the model
/// and the journal), then cut at [`MAX_RESULT_BYTES`]; an error result when the server says
/// `isError`. Scrubbing comes first, so that a cut cannot leave the first half of a value that
/// the redactor would no longer recognise.
///
/// With `files` (the tool's name on a server with `files: true`), each image, audio clip and blob
/// becomes a file artifact of the result and its block one line, [`Artifact::shared_line`] (see
/// [`Sharing`]); a file that cannot be shared is a line that says why, and makes the result an
/// error result. Without it, they are described and no byte of them is kept.
pub(crate) fn map_result(
    result: &CallToolResult,
    redactor: &Redactor,
    files: Option<&str>,
) -> ToolOutput {
    let mut sharing = files.map(|stem| Sharing::new(stem, redactor));
    let mut blocks: Vec<String> = Vec::with_capacity(result.content.len());
    for block in &result.content {
        blocks.push(match sharing.as_mut() {
            Some(sharing) => sharing.block(block),
            None => block_text(block),
        });
    }
    let text = if !blocks.is_empty() {
        blocks.join("\n")
    } else if let Some(structured) = &result.structured_content {
        structured.to_string()
    } else {
        "(the tool returned no content)".to_owned()
    };
    let text = cap_text(redactor.scrub(&text), MAX_RESULT_BYTES);
    let (artifacts, refused) = sharing.map_or((Vec::new(), false), |s| (s.artifacts, s.refused));
    let mut output = if result.is_error == Some(true) || refused {
        ToolOutput::error(text)
    } else {
        ToolOutput::text(text)
    };
    output.artifacts = artifacts;
    output
}

/// One content block as text. What a model cannot read (an image, a blob) is described, not
/// included: no bytes of it reach the context window or the journal.
fn block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(image) => format!("[image not included: {}]", image.mime_type),
        ContentBlock::Audio(audio) => format!("[audio not included: {}]", audio.mime_type),
        ContentBlock::Resource(embedded) => match &embedded.resource {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            ResourceContents::BlobResourceContents { uri, mime_type, .. } => format!(
                "[binary resource not included: {uri} ({})]",
                mime_type.as_deref().unwrap_or("unknown type")
            ),
            _ => "[unsupported content not included]".to_owned(),
        },
        ContentBlock::ResourceLink(link) => format!("[resource link: {}]", link.uri),
        _ => "[unsupported content not included]".to_owned(),
    }
}

/// The files of one result of a server with `files: true`, shared as they are met
/// ([ADR 0033](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0033-files-from-mcp-results-are-shared-files.md)).
///
/// * A file is an image, an audio clip or an embedded blob resource; its bytes are the block's
///   base64. Text, text resources and resource links stay text.
/// * Its name is the tool's name on the server, the file's place among the files of the result and the
///   extension of its media type: `browser_screenshot-1.png`.
/// * Its media type is the one the server declared, checked against the bytes
///   ([`checked_media_type`]): a "PNG" that is not one is `application/octet-stream`.
/// * A file that is text has the values the redactor knows taken out, like the text of the result.
/// * Not shared, with a line that says why: a file over [`MAX_ARTIFACT_FILE_BYTES`], one that is not
///   base64, and every file after the first [`MAX_FILES_PER_RESULT`]. The agent loop then keeps the
///   run's files within `MAX_RUN_FILE_BYTES` (`adam-llm-agent`).
struct Sharing<'a> {
    /// The tool's name on the server: the stem of each file's name.
    stem: &'a str,
    redactor: &'a Redactor,
    /// The file blocks met so far: the counter in a file's name.
    met: usize,
    artifacts: Vec<Artifact>,
    /// A file was not shared: the result is an error result.
    refused: bool,
}

impl<'a> Sharing<'a> {
    fn new(stem: &'a str, redactor: &'a Redactor) -> Self {
        Self {
            stem,
            redactor,
            met: 0,
            artifacts: Vec::new(),
            refused: false,
        }
    }

    /// The text of `block`: a file's line once it is shared (or not), the usual text otherwise.
    fn block(&mut self, block: &ContentBlock) -> String {
        let (data, claimed) = match block {
            ContentBlock::Image(image) => (image.data.as_str(), Some(image.mime_type.as_str())),
            ContentBlock::Audio(audio) => (audio.data.as_str(), Some(audio.mime_type.as_str())),
            ContentBlock::Resource(embedded) => match &embedded.resource {
                ResourceContents::BlobResourceContents {
                    blob, mime_type, ..
                } => (blob.as_str(), mime_type.as_deref()),
                _ => return block_text(block),
            },
            _ => return block_text(block),
        };
        self.met += 1;
        match self.share(data, claimed) {
            Ok(artifact) => {
                let line = artifact.shared_line();
                self.artifacts.push(artifact);
                line
            }
            Err(why) => {
                self.refused = true;
                format!(
                    "Not shared: a file ({}) {why}.",
                    claimed.unwrap_or("of no declared type")
                )
            }
        }
    }

    /// The file of one block, or why it is not shared (the end of a sentence).
    fn share(&self, data: &str, claimed: Option<&str>) -> Result<Artifact, String> {
        if self.artifacts.len() >= MAX_FILES_PER_RESULT {
            return Err(format!(
                "is past the first {MAX_FILES_PER_RESULT} files of this result, and one result \
                 shares no more"
            ));
        }
        // Four characters of base64 are three bytes: a file far over the cap is refused unread.
        if data.len() / 4 * 3 > MAX_ARTIFACT_FILE_BYTES + 3 {
            return Err(too_large(data.len() / 4 * 3, true));
        }
        let mut bytes = decode(data).ok_or_else(|| "is not valid base64".to_owned())?;
        if bytes.len() > MAX_ARTIFACT_FILE_BYTES {
            return Err(too_large(bytes.len(), false));
        }
        if let Ok(text) = std::str::from_utf8(&bytes) {
            let scrubbed = self.redactor.scrub(text);
            if scrubbed != text {
                bytes = scrubbed.into_bytes();
            }
        }
        let media_type = checked_media_type(claimed, &bytes);
        let filename = format!("{}-{}.{}", self.stem, self.met, extension_of(&media_type));
        Artifact::file(filename.clone(), media_type, filename, bytes).map_err(|e| e.to_string())
    }
}

fn too_large(len: usize, about: bool) -> String {
    format!(
        "of {}{} bytes is over the limit of {MAX_ARTIFACT_FILE_BYTES} bytes (4 MiB) for one shared \
         file: ask the tool for a smaller one, or tell the person it is too big to share",
        if about { "about " } else { "" },
        len
    )
}

/// The bytes of a block's base64: the standard alphabet, padded or not, whitespace ignored.
fn decode(data: &str) -> Option<Vec<u8>> {
    const ENGINE: base64::engine::GeneralPurpose = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    let compact: String;
    let data = if data.bytes().any(|b| b.is_ascii_whitespace()) {
        compact = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        compact.as_str()
    } else {
        data
    };
    ENGINE.decode(data).ok()
}

#[cfg(test)]
mod tests {
    use rmcp::model::{Resource, ResourceContents};

    use super::*;

    #[test]
    fn content_blocks_map_to_text() {
        let result = CallToolResult::success(vec![
            ContentBlock::text("first"),
            ContentBlock::image("aGVsbG8=", "image/png"),
            ContentBlock::audio("aGVsbG8=", "audio/wav"),
            ContentBlock::embedded_text("file:///a.txt", "inside"),
            ContentBlock::resource(ResourceContents::BlobResourceContents {
                uri: "file:///b.bin".into(),
                mime_type: Some("application/octet-stream".into()),
                blob: "AAAA".into(),
                meta: None,
            }),
            ContentBlock::resource_link(Resource::new("file:///c.txt", "c")),
        ]);
        let output = map_result(&result, &Redactor::default(), None);
        assert!(!output.is_error);
        assert_eq!(
            output.content,
            "first\n[image not included: image/png]\n[audio not included: audio/wav]\ninside\n\
             [binary resource not included: file:///b.bin (application/octet-stream)]\n\
             [resource link: file:///c.txt]"
        );
        // No bytes of a blob or an image reach the model, and none are kept.
        assert!(!output.content.contains("aGVsbG8") && !output.content.contains("AAAA"));
        assert!(output.artifacts.is_empty());
    }

    /// A 1x1 PNG.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89\0\0\0\nIDATx\x9cc\0\x01\0\0\x05\0\x01\r\n-\xb4\0\0\0\0IEND\xaeB`\x82";

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn blob(uri: &str, mime_type: Option<&str>, bytes: &[u8]) -> ContentBlock {
        ContentBlock::resource(ResourceContents::BlobResourceContents {
            uri: uri.into(),
            mime_type: mime_type.map(str::to_owned),
            blob: b64(bytes),
            meta: None,
        })
    }

    fn file<'a>(output: &'a ToolOutput, name: &str) -> &'a adam_runtime::ArtifactFile {
        output
            .artifacts
            .iter()
            .find(|a| a.name == name)
            .and_then(|a| a.file.as_ref())
            .unwrap_or_else(|| panic!("no file `{name}` in {:?}", output.artifacts))
    }

    #[test]
    fn with_files_images_audio_and_blobs_are_shared_and_the_rest_stays_text() {
        let result = CallToolResult::success(vec![
            ContentBlock::text("the page"),
            ContentBlock::image(b64(PNG), "image/png"),
            blob(
                "obscura://capture/current-page.pdf",
                Some("application/pdf"),
                b"%PDF-1.7 page",
            ),
            ContentBlock::audio(b64(b"RIFF....WAVEfmt "), "audio/wav"),
            ContentBlock::embedded_text("file:///notes.txt", "notes"),
            ContentBlock::resource_link(Resource::new("file:///c.txt", "c")),
        ]);
        let output = map_result(&result, &Redactor::default(), Some("browser_screenshot"));
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            output.content,
            "the page\nShared browser_screenshot-1.png (67 bytes, image/png). To show it in your answer, \
             write ![description](browser_screenshot-1.png).\n\
             Shared browser_screenshot-2.pdf (13 bytes, application/pdf).\n\
             Shared browser_screenshot-3.wav (16 bytes, audio/wav).\nnotes\n\
             [resource link: file:///c.txt]"
        );
        let names: Vec<(&str, Option<&str>)> = output
            .artifacts
            .iter()
            .map(|a| (a.name.as_str(), a.mime_type.as_deref()))
            .collect();
        assert_eq!(
            names,
            [
                ("browser_screenshot-1.png", Some("image/png")),
                ("browser_screenshot-2.pdf", Some("application/pdf")),
                ("browser_screenshot-3.wav", Some("audio/wav")),
            ]
        );
        let png = file(&output, "browser_screenshot-1.png");
        assert_eq!(
            (png.filename.as_str(), png.bytes.as_slice()),
            ("browser_screenshot-1.png", PNG)
        );
        assert_eq!(
            file(&output, "browser_screenshot-2.pdf").bytes,
            b"%PDF-1.7 page"
        );
        // The bytes are in the artifacts only, never in what the model reads.
        assert!(!output.content.contains(&b64(PNG)[..12]));
    }

    #[test]
    fn a_file_that_is_not_what_its_server_says_is_shared_as_bytes() {
        // `aGVsbG8=` is "hello": not a PNG.
        let result = CallToolResult::success(vec![
            ContentBlock::image("aGVsbG8=", "image/png"),
            blob("x://y", None, PNG),
            blob("x://z", Some("not a type"), b"\0\x01"),
        ]);
        let output = map_result(&result, &Redactor::default(), Some("t"));
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            output.content,
            "Shared t-1.bin (5 bytes, application/octet-stream).\n\
             Shared t-2.png (67 bytes, image/png). To show it in your answer, write \
             ![description](t-2.png).\n\
             Shared t-3.bin (2 bytes, application/octet-stream)."
        );
    }

    #[test]
    fn a_file_over_the_cap_is_not_shared_and_the_result_says_so_as_an_error() {
        let over = {
            let mut bytes = PNG.to_vec();
            bytes.resize(MAX_ARTIFACT_FILE_BYTES + 1, 0);
            bytes
        };
        let result = CallToolResult::success(vec![
            ContentBlock::image(b64(&over), "image/png"),
            // Far over: refused before it is decoded.
            ContentBlock::image("A".repeat(MAX_ARTIFACT_FILE_BYTES * 2), "image/png"),
            ContentBlock::image(b64(PNG), "image/png"),
        ]);
        let output = map_result(&result, &Redactor::default(), Some("shot"));
        assert!(
            output.is_error,
            "a file the person does not get is a failure"
        );
        let lines: Vec<&str> = output.content.lines().collect();
        assert_eq!(lines.len(), 3, "{}", output.content);
        assert_eq!(
            lines[0],
            "Not shared: a file (image/png) of 4194305 bytes is over the limit of 4194304 bytes \
             (4 MiB) for one shared file: ask the tool for a smaller one, or tell the person it is \
             too big to share."
        );
        assert!(
            lines[1].starts_with("Not shared: a file (image/png) of about 6291456 bytes"),
            "{}",
            lines[1]
        );
        // The counter is the file's place in the result, shared or not.
        assert_eq!(
            lines[2],
            "Shared shot-3.png (67 bytes, image/png). To show it in your answer, write \
             ![description](shot-3.png)."
        );
        assert_eq!(output.artifacts.len(), 1);
    }

    #[test]
    fn base64_may_lack_padding_or_wrap_and_garbage_is_not_shared() {
        let unpadded = b64(b"hi!x").trim_end_matches('=').to_owned();
        let wrapped = {
            let text = b64(PNG);
            format!("{}\n{}", &text[..40], &text[40..])
        };
        let result = CallToolResult::success(vec![
            blob("a://1", Some("text/plain"), b"x"),
            ContentBlock::resource(ResourceContents::BlobResourceContents {
                uri: "a://2".into(),
                mime_type: Some("text/plain".into()),
                blob: unpadded,
                meta: None,
            }),
            ContentBlock::image(wrapped, "image/png"),
            ContentBlock::image("not base64 at all!", "image/png"),
        ]);
        let output = map_result(&result, &Redactor::default(), Some("t"));
        assert!(output.is_error);
        assert_eq!(file(&output, "t-2.txt").bytes, b"hi!x");
        assert_eq!(file(&output, "t-3.png").bytes, PNG);
        assert!(
            output
                .content
                .ends_with("Not shared: a file (image/png) is not valid base64."),
            "{}",
            output.content
        );
    }

    #[test]
    fn one_result_shares_at_most_sixteen_files() {
        let blocks = (0..MAX_FILES_PER_RESULT + 2)
            .map(|_| ContentBlock::image(b64(PNG), "image/png"))
            .collect();
        let output = map_result(
            &CallToolResult::success(blocks),
            &Redactor::default(),
            Some("t"),
        );
        assert_eq!(output.artifacts.len(), MAX_FILES_PER_RESULT);
        assert!(output.is_error);
        assert!(
            output.content.ends_with(
                "Not shared: a file (image/png) is past the first 16 files of this result, and \
                 one result shares no more."
            ),
            "{}",
            output.content
        );
    }

    #[test]
    fn a_text_file_is_scrubbed_like_the_text_and_an_error_result_stays_one() {
        let mut redactor = Redactor::default();
        redactor.add("tok-6d2f-secret");
        let result = CallToolResult::error(vec![
            ContentBlock::text("failed, see the log"),
            blob("x://log", Some("text/plain"), b"used tok-6d2f-secret"),
            blob(
                "x://bin",
                Some("application/octet-stream"),
                b"\xfftok-6d2f-secret",
            ),
        ]);
        let output = map_result(&result, &redactor, Some("t"));
        assert!(output.is_error, "the server said isError");
        // The counter counts files only: the text before them is not one.
        assert_eq!(file(&output, "t-1.txt").bytes, b"used [REDACTED]");
        // A file that is not text is left as it is.
        assert_eq!(file(&output, "t-2.bin").bytes, b"\xfftok-6d2f-secret");
    }

    #[test]
    fn nothing_and_structured_content_have_their_own_text() {
        let empty = map_result(&CallToolResult::success(vec![]), &Redactor::default(), None);
        assert_eq!(empty.content, "(the tool returned no content)");
        let structured = map_result(
            &CallToolResult::structured(serde_json::json!({"n": 3})),
            &Redactor::default(),
            None,
        );
        assert!(!structured.is_error);
        assert!(structured.content.contains("\"n\":3") || structured.content.contains("\"n\": 3"));
    }

    #[test]
    fn is_error_is_an_error_result() {
        let output = map_result(
            &CallToolResult::error(vec![ContentBlock::text("no such issue")]),
            &Redactor::default(),
            None,
        );
        assert!(output.is_error);
        assert_eq!(output.content, "no such issue");
    }

    #[test]
    fn result_cut_at_64_kib() {
        let big = "x".repeat(MAX_RESULT_BYTES + 100);
        let output = map_result(
            &CallToolResult::success(vec![ContentBlock::text(big)]),
            &Redactor::default(),
            None,
        );
        assert!(!output.is_error);
        assert!(
            output.content.len() < MAX_RESULT_BYTES + 100,
            "{}",
            output.content.len()
        );
        assert!(
            output
                .content
                .contains("[cut here: 65536 of 65636 bytes shown]")
        );
    }

    #[test]
    fn results_are_scrubbed_of_registered_values_before_the_cut() {
        let mut redactor = Redactor::default();
        redactor.add("tok-6d2f-secret");
        // Success text, an error result's text, a resource's text and structured content.
        let ok = map_result(
            &CallToolResult::success(vec![
                ContentBlock::text("the token is tok-6d2f-secret, really"),
                ContentBlock::embedded_text("file:///a", "and tok-6d2f-secret again"),
            ]),
            &redactor,
            None,
        );
        assert_eq!(
            ok.content,
            "the token is [REDACTED], really\nand [REDACTED] again"
        );
        let failed = map_result(
            &CallToolResult::error(vec![ContentBlock::text("denied for tok-6d2f-secret")]),
            &redactor,
            None,
        );
        assert!(failed.is_error);
        assert_eq!(failed.content, "denied for [REDACTED]");
        let structured = map_result(
            &CallToolResult::structured(serde_json::json!({"t": "tok-6d2f-secret"})),
            &redactor,
            None,
        );
        assert!(
            !structured.content.contains("6d2f"),
            "{}",
            structured.content
        );
        // A value that straddles the 64 KiB cut is gone before the text is cut.
        let padding = "x".repeat(MAX_RESULT_BYTES - 5);
        let straddling = map_result(
            &CallToolResult::success(vec![ContentBlock::text(format!(
                "{padding}tok-6d2f-secret"
            ))]),
            &redactor,
            None,
        );
        assert!(!straddling.content.contains("tok-6"), "cut before scrubbed");
        assert!(straddling.content.contains("[REDA"), "scrubbed, then cut");
    }
}
