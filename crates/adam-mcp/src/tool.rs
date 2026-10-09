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

use adam_llm_agent::{StepStyle, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::ToolSpec;
use adam_runtime::ReceivedFiles;
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
        ctx: &ToolCtx,
        redactor: &Redactor,
        outcome: Result<CallToolResponse, ServiceError>,
    ) -> (ToolOutput, bool) {
        let scrub = |text: &str| cap_text(redactor.scrub(text), MAX_MESSAGE_BYTES);
        let output = match outcome {
            Ok(CallToolResponse::Complete(result)) => map_result(
                &result,
                redactor,
                self.files.then(|| ShareAs {
                    stem: &self.remote,
                    budget: ctx.files_left(),
                }),
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
                let (output, broken) = self.describe(ctx, connection.redactor(), outcome);
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
            Ok(outcome) => self.describe(ctx, &recipe.redactor, outcome).0,
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
    files: Option<ShareAs<'_>>,
) -> ToolOutput {
    let mut sharing = files.map(|share| Sharing::new(share, redactor));
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
    let (artifacts, refused) = sharing.map_or((Vec::new(), false), |s| s.files.into_parts());
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

/// How a result of a server with `files: true` shares its files: named after `stem` (the tool's
/// name on the server), within `budget` bytes in all (what the run may still share).
pub(crate) struct ShareAs<'a> {
    pub(crate) stem: &'a str,
    pub(crate) budget: u64,
}

/// The files of one result of a server with `files: true`, shared as they are met
/// ([ADR 0033](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0033-files-from-mcp-results-are-shared-files.md))
/// by [`ReceivedFiles`]: named `<tool>-<hash>.<ext>`, the media type the server declared checked
/// against the bytes, at most 4 MiB a file, 16 a result and what the run may still share.
///
/// * A file is an image, an audio clip or an embedded blob resource; its bytes are the block's
///   base64. Text, text resources and resource links stay text.
/// * A file that is text has the values the redactor knows taken out, like the text of the result.
/// * Every cap is checked on the length of the base64, before it is decoded; a text that does not
///   decode is refused.
struct Sharing<'a> {
    redactor: &'a Redactor,
    files: ReceivedFiles,
}

impl<'a> Sharing<'a> {
    fn new(share: ShareAs<'_>, redactor: &'a Redactor) -> Self {
        Self {
            redactor,
            files: ReceivedFiles::new(share.stem, share.budget),
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
        // Four characters of base64 are three bytes, less the padding: the file's length, checked
        // against every cap before anything is decoded.
        let chars = data.bytes().filter(|b| !b.is_ascii_whitespace()).count();
        let padding = data
            .bytes()
            .rev()
            .filter(|b| !b.is_ascii_whitespace())
            .take_while(|b| *b == b'=')
            .count();
        if let Err(line) = self.files.admit(claimed, (chars - padding) * 3 / 4) {
            return line;
        }
        let Some(mut bytes) = decode(data) else {
            return self.files.refuse(claimed, "is not valid base64");
        };
        if let Ok(text) = std::str::from_utf8(&bytes) {
            let scrubbed = self.redactor.scrub(text);
            if scrubbed != text {
                bytes = scrubbed.into_bytes();
            }
        }
        self.files.share(claimed, None, None, bytes)
    }
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
    use adam_llm_agent::Artifact;
    use adam_runtime::{MAX_ARTIFACT_FILE_BYTES, MAX_FILES_PER_RESULT, MAX_RUN_FILE_BYTES};
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

    /// Share as the tool `stem`, with the whole run's budget.
    fn share(stem: &str) -> Option<ShareAs<'_>> {
        Some(ShareAs {
            stem,
            budget: MAX_RUN_FILE_BYTES as u64,
        })
    }

    /// The file names of the artifacts, in order.
    fn names(output: &ToolOutput) -> Vec<String> {
        output
            .artifacts
            .iter()
            .map(|a| a.file.as_ref().unwrap().filename.clone())
            .collect()
    }

    /// `<stem>-<8 hex digits>.<ext>`.
    fn is_named(name: &str, stem: &str, ext: &str) -> bool {
        let Some(rest) = name.strip_prefix(&format!("{stem}-")) else {
            return false;
        };
        let Some(hash) = rest.strip_suffix(&format!(".{ext}")) else {
            return false;
        };
        hash.len() == 8 && hash.bytes().all(|b| b.is_ascii_hexdigit())
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
        let output = map_result(&result, &Redactor::default(), share("browser_screenshot"));
        assert!(!output.is_error, "{}", output.content);
        let names = names(&output);
        assert!(
            is_named(&names[0], "browser_screenshot", "png"),
            "{names:?}"
        );
        assert!(
            is_named(&names[1], "browser_screenshot", "pdf"),
            "{names:?}"
        );
        assert!(
            is_named(&names[2], "browser_screenshot", "wav"),
            "{names:?}"
        );
        assert_eq!(
            output.content,
            format!(
                "the page\nShared {} (67 bytes, image/png). To show it in your answer, write \
                 ![description]({}).\nShared {} (13 bytes, application/pdf).\n\
                 Shared {} (16 bytes, audio/wav).\nnotes\n[resource link: file:///c.txt]",
                names[0], names[0], names[1], names[2]
            )
        );
        let types: Vec<Option<&str>> = output
            .artifacts
            .iter()
            .map(|a| a.mime_type.as_deref())
            .collect();
        assert_eq!(
            types,
            [
                Some("image/png"),
                Some("application/pdf"),
                Some("audio/wav")
            ]
        );
        assert_eq!(output.artifacts[0].file.as_ref().unwrap().bytes, PNG);
        assert_eq!(
            output.artifacts[1].file.as_ref().unwrap().bytes,
            b"%PDF-1.7 page"
        );
        // The bytes are in the artifacts only, never in what the model reads.
        assert!(!output.content.contains(&b64(PNG)[..12]));
    }

    /// Two calls of a tool make two names: a screenshot is known by its content, so the inline
    /// image of the second never shows the first.
    #[test]
    fn two_screenshots_of_one_run_have_two_names() {
        let mut other = PNG.to_vec();
        other.extend_from_slice(b"another page");
        let shot = |bytes: &[u8]| {
            let result =
                CallToolResult::success(vec![ContentBlock::image(b64(bytes), "image/png")]);
            names(&map_result(
                &result,
                &Redactor::default(),
                share("browser_screenshot"),
            ))[0]
                .clone()
        };
        assert_ne!(shot(PNG), shot(&other));
        assert_eq!(shot(PNG), shot(PNG), "a replay names a file as before");
    }

    #[test]
    fn a_file_that_is_not_what_its_server_says_is_shared_as_bytes() {
        // `aGVsbG8=` is "hello": not a PNG.
        let result = CallToolResult::success(vec![
            ContentBlock::image("aGVsbG8=", "image/png"),
            blob("x://y", None, PNG),
            blob("x://z", Some("not a type"), b"\0\x01"),
        ]);
        let output = map_result(&result, &Redactor::default(), share("t"));
        assert!(!output.is_error, "{}", output.content);
        let names = names(&output);
        assert!(is_named(&names[0], "t", "bin"), "{names:?}");
        assert!(is_named(&names[1], "t", "png"), "{names:?}");
        assert!(is_named(&names[2], "t", "bin"), "{names:?}");
        let types: Vec<Option<&str>> = output
            .artifacts
            .iter()
            .map(|a| a.mime_type.as_deref())
            .collect();
        assert_eq!(
            types,
            [
                Some("application/octet-stream"),
                Some("image/png"),
                Some("application/octet-stream")
            ]
        );
    }

    #[test]
    fn a_file_over_a_cap_is_refused_before_it_is_decoded_and_the_result_is_an_error() {
        let over = {
            let mut bytes = PNG.to_vec();
            bytes.resize(MAX_ARTIFACT_FILE_BYTES + 1, 0);
            bytes
        };
        let result = CallToolResult::success(vec![
            ContentBlock::image(b64(&over), "image/png"),
            // Far over, and not even base64: refused on its length, never decoded.
            ContentBlock::image("!".repeat(MAX_ARTIFACT_FILE_BYTES * 2), "image/png"),
            ContentBlock::image(b64(PNG), "image/png"),
        ]);
        let output = map_result(&result, &Redactor::default(), share("shot"));
        assert!(
            output.is_error,
            "a file the person does not get is a failure"
        );
        let lines: Vec<&str> = output.content.lines().collect();
        assert_eq!(lines.len(), 3, "{}", output.content);
        assert!(
            lines[0].starts_with("Not shared: a file (image/png) of 4194305 bytes "),
            "{}",
            lines[0]
        );
        assert!(
            lines[0].ends_with(
                "is over the limit of 4194304 bytes (4 MiB) for one shared file: ask for a \
                 smaller one, or tell the person it is too big to share."
            ),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].starts_with("Not shared: a file (image/png) of 6291456 bytes is over"),
            "{}",
            lines[1]
        );
        assert!(lines[2].starts_with("Shared shot-"), "{}", lines[2]);
        assert_eq!(output.artifacts.len(), 1);
    }

    /// The whole result is journaled before the loop's run cap applies: what the run may still share
    /// (`ToolCtx::files_left`) bounds it, three files of about 4 MiB included.
    #[test]
    fn a_result_shares_no_more_than_the_run_may_still_share() {
        let big = |fill: u8| {
            let mut bytes = PNG.to_vec();
            bytes.resize(MAX_ARTIFACT_FILE_BYTES - 1024, fill);
            ContentBlock::image(b64(&bytes), "image/png")
        };
        let result = CallToolResult::success(vec![big(1), big(2), big(3)]);
        let output = map_result(&result, &Redactor::default(), share("shot"));
        assert!(output.is_error);
        assert_eq!(
            output.artifacts.len(),
            1,
            "4 MiB fits the run's 6, 8 do not"
        );
        let kept: usize = output.artifacts.iter().map(Artifact::file_len).sum();
        assert!(kept <= MAX_RUN_FILE_BYTES, "{kept}");
        let lines: Vec<&str> = output.content.lines().collect();
        assert!(
            lines[1].contains("is over what this run may still share"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].contains("is over what this run may still share"),
            "{}",
            lines[2]
        );
        // With 1 KiB left, nothing of it is shared.
        let tight = map_result(
            &result,
            &Redactor::default(),
            Some(ShareAs {
                stem: "shot",
                budget: 1024,
            }),
        );
        assert!(tight.artifacts.is_empty() && tight.is_error);
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
        let output = map_result(&result, &Redactor::default(), share("t"));
        assert!(output.is_error);
        assert_eq!(output.artifacts[1].file.as_ref().unwrap().bytes, b"hi!x");
        assert_eq!(output.artifacts[2].file.as_ref().unwrap().bytes, PNG);
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
            .map(|n| {
                ContentBlock::image(
                    b64(&[PNG, &[u8::try_from(n).unwrap()]].concat()),
                    "image/png",
                )
            })
            .collect();
        let output = map_result(
            &CallToolResult::success(blocks),
            &Redactor::default(),
            share("t"),
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
        let output = map_result(&result, &redactor, share("t"));
        assert!(output.is_error, "the server said isError");
        assert_eq!(
            output.artifacts[0].file.as_ref().unwrap().bytes,
            b"used [REDACTED]"
        );
        // A file that is not text is left as it is.
        assert_eq!(
            output.artifacts[1].file.as_ref().unwrap().bytes,
            b"\xfftok-6d2f-secret"
        );
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
