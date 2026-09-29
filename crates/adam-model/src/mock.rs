use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};

use crate::{
    ContentPart, DynModel, Message, ModelClient, ModelDelta, ModelError, ModelRequest,
    ModelResponse, ToolCall,
};

/// One call a [`MockModel`] received.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedCall {
    /// The request as the caller passed it.
    pub request: ModelRequest,
    /// `true` for [`ModelClient::stream`], `false` for [`ModelClient::complete`].
    pub streaming: bool,
}

#[derive(Default)]
struct Inner {
    script: VecDeque<Result<ModelResponse, ModelError>>,
    calls: Vec<RecordedCall>,
}

/// A scripted [`ModelClient`] for tests.
///
/// Queue outcomes with the `push_*` methods; each call to
/// [`complete`](ModelClient::complete) or [`stream`](ModelClient::stream) pops
/// the next one and records the request it received. When the queue is empty
/// the call fails with [`ModelError::InvalidRequest`] (a script that is too
/// short is a bug in the test, and this keeps it non-retryable).
///
/// `stream` replays the queued response as deltas: one [`ModelDelta::Text`]
/// (if there is any text), one [`ModelDelta::ToolCallStarted`] per tool call,
/// then [`ModelDelta::Finished`]. A queued error fails the call before the
/// stream starts.
///
/// It is always compiled, so any crate can use it from its tests through a
/// normal dependency on `adam-model`. All methods take `&self`, so a mock can
/// be scripted after it has been shared as a [`DynModel`]:
///
/// ```
/// use std::sync::Arc;
/// use adam_model::{Classify, MockModel, ModelClient, ModelError, ModelRequest};
///
/// # futures::executor::block_on(async {
/// let mock = Arc::new(MockModel::new());
/// mock.push_error(ModelError::transient("blip"));
/// mock.push_text("done");
///
/// let model: Arc<dyn ModelClient> = mock.clone();
/// assert!(model.complete(ModelRequest::new("m")).await.unwrap_err().is_retryable());
/// assert_eq!(model.complete(ModelRequest::new("m")).await.unwrap().message.text(), "done");
/// assert_eq!(mock.requests().len(), 2);
/// # });
/// ```
#[derive(Default)]
pub struct MockModel {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for MockModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("MockModel")
            .field("remaining", &inner.script.len())
            .field("calls", &inner.calls.len())
            .finish()
    }
}

impl MockModel {
    /// An empty mock; script it with the `push_*` methods.
    pub fn new() -> Self {
        Self::default()
    }

    /// A mock preloaded with `script`.
    pub fn scripted(script: impl IntoIterator<Item = Result<ModelResponse, ModelError>>) -> Self {
        let mock = Self::new();
        mock.lock().script.extend(script);
        mock
    }

    /// Wrap into a [`DynModel`].
    pub fn into_dyn(self) -> DynModel {
        Arc::new(self)
    }

    /// Queue a response.
    pub fn push_response(&self, response: ModelResponse) -> &Self {
        self.lock().script.push_back(Ok(response));
        self
    }

    /// Queue a text-only response ([`ModelResponse::text`]).
    pub fn push_text(&self, text: impl Into<String>) -> &Self {
        self.push_response(ModelResponse::text(text))
    }

    /// Queue a response asking for tool calls ([`ModelResponse::tool_calls`]).
    pub fn push_tool_calls(&self, calls: Vec<ToolCall>) -> &Self {
        self.push_response(ModelResponse::tool_calls(calls))
    }

    /// Queue a failure.
    pub fn push_error(&self, error: ModelError) -> &Self {
        self.lock().script.push_back(Err(error));
        self
    }

    /// Every request received so far, in order (both `complete` and `stream`).
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.lock()
            .calls
            .iter()
            .map(|c| c.request.clone())
            .collect()
    }

    /// Every call received so far, with whether it was streaming.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.lock().calls.clone()
    }

    /// The most recent request, if any.
    pub fn last_request(&self) -> Option<ModelRequest> {
        self.lock().calls.last().map(|c| c.request.clone())
    }

    /// How many scripted outcomes have not been consumed yet.
    pub fn remaining(&self) -> usize {
        self.lock().script.len()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panicking test thread must not hide the recorded calls.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next(&self, request: ModelRequest, streaming: bool) -> Result<ModelResponse, ModelError> {
        let mut inner = self.lock();
        inner.calls.push(RecordedCall { request, streaming });
        inner.script.pop_front().unwrap_or_else(|| {
            Err(ModelError::invalid_request(
                "MockModel script exhausted: no queued response",
            ))
        })
    }
}

#[async_trait]
impl ModelClient for MockModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.next(req, false)
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        let response = self.next(req, true)?;
        let mut deltas = Vec::new();
        if let Message::Assistant {
            content,
            tool_calls,
        } = &response.message
        {
            let text: String = content.iter().map(ContentPart::as_text).collect();
            if !text.is_empty() {
                deltas.push(ModelDelta::Text(text));
            }
            for call in tool_calls {
                deltas.push(ModelDelta::ToolCallStarted {
                    id: call.id.clone(),
                    name: call.name.clone(),
                });
            }
        }
        deltas.push(ModelDelta::Finished(response));
        Ok(stream::iter(deltas.into_iter().map(Ok)).boxed())
    }
}

#[cfg(test)]
mod tests {
    use futures::TryStreamExt;
    use serde_json::json;

    use super::*;
    use crate::FinishReason;
    use adam_error::Classify;

    #[tokio::test]
    async fn pops_in_order_and_records() {
        let mock = MockModel::scripted([
            Ok(ModelResponse::text("one")),
            Err(ModelError::Auth("nope".into())),
        ]);
        let mut req = ModelRequest::new("m");
        req.messages.push(Message::user_text("hi"));

        assert_eq!(
            mock.complete(req.clone()).await.unwrap().message.text(),
            "one"
        );
        assert!(matches!(
            mock.complete(req.clone()).await.unwrap_err(),
            ModelError::Auth(m) if m == "nope"
        ));
        assert_eq!(mock.requests(), vec![req.clone(), req.clone()]);
        assert_eq!(mock.last_request(), Some(req));
        assert_eq!(mock.remaining(), 0);
    }

    #[tokio::test]
    async fn exhausted_script_is_a_non_retryable_error() {
        let err = MockModel::new()
            .complete(ModelRequest::new("m"))
            .await
            .unwrap_err();
        assert!(matches!(err, ModelError::InvalidRequest { .. }));
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn stream_replays_text_and_tool_calls() {
        let mock = MockModel::new();
        let response = ModelResponse {
            message: Message::Assistant {
                content: vec![ContentPart::text("let me look")],
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "search".into(),
                    arguments: json!({"q": "x"}),
                }],
            },
            finish: FinishReason::ToolCalls,
            usage: Default::default(),
        };
        mock.push_response(response.clone());

        let deltas: Vec<_> = mock
            .stream(ModelRequest::new("m"))
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            deltas,
            vec![
                ModelDelta::Text("let me look".into()),
                ModelDelta::ToolCallStarted {
                    id: "c1".into(),
                    name: "search".into()
                },
                ModelDelta::Finished(response),
            ]
        );
        assert!(mock.calls()[0].streaming);
    }

    #[tokio::test]
    async fn stream_error_fails_before_the_stream_starts() {
        let mock = MockModel::new();
        mock.push_error(ModelError::RateLimited { retry_after: None });
        assert!(mock.stream(ModelRequest::new("m")).await.is_err());
    }

    #[tokio::test]
    async fn usable_as_dyn_model_and_through_arc() {
        let mock = Arc::new(MockModel::new());
        mock.push_text("a").push_text("b");
        let dyn_model: DynModel = mock.clone();
        assert_eq!(
            dyn_model
                .complete(ModelRequest::new("m"))
                .await
                .unwrap()
                .message
                .text(),
            "a"
        );
        // `Arc<dyn ModelClient>` is itself a `ModelClient`.
        fn takes_client(_: impl ModelClient) {}
        takes_client(dyn_model.clone());
        assert_eq!(
            dyn_model
                .complete(ModelRequest::new("m"))
                .await
                .unwrap()
                .message
                .text(),
            "b"
        );
    }
}
