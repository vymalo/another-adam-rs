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
//!     T-->>L: ToolOutput (text of the content, scrubbed of expanded values, cut at 64 KiB; is_error kept)
//!     Note over L: the result is journaled; a replay returns it without calling again
//! ```

use std::sync::Arc;
use std::time::Duration;

use adam_llm_agent::{StepStyle, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::ToolSpec;
use async_trait::async_trait;
use rmcp::ServiceError;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ResourceContents,
};
use serde_json::{Map, Value};

use crate::connection::Connection;
use crate::redact::Redactor;
use crate::text::{MAX_RESULT_BYTES, cap_text};

/// A tool of an MCP server. The model sees it as `<server>__<tool>`.
///
/// A call is **not** retried by this crate and never becomes a [`ToolError::Transient`]: an MCP
/// call has no idempotency key, so a call that failed on the way may or may not have run, and the
/// model is told so in an error result. `adam-llm-agent` journals the result under the step
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
    connection: Arc<Connection>,
    call_timeout: Duration,
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
        connection: Arc<Connection>,
        call_timeout: Duration,
    ) -> Self {
        Self {
            spec,
            server,
            remote,
            title,
            connection,
            call_timeout,
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

    /// What the server answered (or did not), as the result of the call. A broken session is
    /// dropped, so the next call reconnects.
    async fn finish(
        &self,
        generation: u64,
        outcome: Result<CallToolResponse, ServiceError>,
    ) -> ToolOutput {
        match outcome {
            Ok(CallToolResponse::Complete(result)) => {
                map_result(&result, self.connection.redactor())
            }
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
                self.connection.scrub(&error.message)
            )),
            Err(ServiceError::Timeout { .. }) => self.lost(
                "the connection gave up waiting for the answer (the call may still be running \
                 on the server)",
            ),
            Err(error) => {
                self.connection.mark_broken(generation).await;
                self.lost(&format!(
                    "the connection to the server failed ({})",
                    self.connection.scrub(&adam_error::report(&error))
                ))
            }
        }
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
        let params = CallToolRequestParams::new(self.remote.clone()).with_arguments(arguments);
        let cancelled = || {
            ToolOutput::error(format!(
                "the run was cancelled while `{}` was called; it may or may not have run",
                self.name()
            ))
        };

        let (peer, generation) = tokio::select! {
            () = ctx.cancelled() => return Ok(cancelled()),
            got = self.connection.peer() => match got {
                Ok(pair) => pair,
                Err(text) => return Ok(ToolOutput::error(text)),
            },
        };
        let outcome = tokio::select! {
            () = ctx.cancelled() => return Ok(cancelled()),
            waited = tokio::time::timeout(self.call_timeout, peer.call_tool_once(params)) => waited,
        };
        Ok(match outcome {
            Ok(outcome) => self.finish(generation, outcome).await,
            Err(_) => self.lost(&format!(
                "no answer within {} ms (the call may still be running on the server)",
                self.call_timeout.as_millis()
            )),
        })
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
pub(crate) fn map_result(result: &CallToolResult, redactor: &Redactor) -> ToolOutput {
    let blocks: Vec<String> = result.content.iter().map(block_text).collect();
    let text = if !blocks.is_empty() {
        blocks.join("\n")
    } else if let Some(structured) = &result.structured_content {
        structured.to_string()
    } else {
        "(the tool returned no content)".to_owned()
    };
    let text = cap_text(redactor.scrub(&text), MAX_RESULT_BYTES);
    if result.is_error == Some(true) {
        ToolOutput::error(text)
    } else {
        ToolOutput::text(text)
    }
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
        let output = map_result(&result, &Redactor::default());
        assert!(!output.is_error);
        assert_eq!(
            output.content,
            "first\n[image not included: image/png]\n[audio not included: audio/wav]\ninside\n\
             [binary resource not included: file:///b.bin (application/octet-stream)]\n\
             [resource link: file:///c.txt]"
        );
        // No bytes of a blob or an image reach the model.
        assert!(!output.content.contains("aGVsbG8") && !output.content.contains("AAAA"));
    }

    #[test]
    fn nothing_and_structured_content_have_their_own_text() {
        let empty = map_result(&CallToolResult::success(vec![]), &Redactor::default());
        assert_eq!(empty.content, "(the tool returned no content)");
        let structured = map_result(
            &CallToolResult::structured(serde_json::json!({"n": 3})),
            &Redactor::default(),
        );
        assert!(!structured.is_error);
        assert!(structured.content.contains("\"n\":3") || structured.content.contains("\"n\": 3"));
    }

    #[test]
    fn is_error_is_an_error_result() {
        let output = map_result(
            &CallToolResult::error(vec![ContentBlock::text("no such issue")]),
            &Redactor::default(),
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
        );
        assert_eq!(
            ok.content,
            "the token is [REDACTED], really\nand [REDACTED] again"
        );
        let failed = map_result(
            &CallToolResult::error(vec![ContentBlock::text("denied for tok-6d2f-secret")]),
            &redactor,
        );
        assert!(failed.is_error);
        assert_eq!(failed.content, "denied for [REDACTED]");
        let structured = map_result(
            &CallToolResult::structured(serde_json::json!({"t": "tok-6d2f-secret"})),
            &redactor,
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
        );
        assert!(!straddling.content.contains("tok-6"), "cut before scrubbed");
        assert!(straddling.content.contains("[REDA"), "scrubbed, then cut");
    }
}
