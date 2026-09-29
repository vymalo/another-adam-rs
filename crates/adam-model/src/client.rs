use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::{ModelDelta, ModelError, ModelRequest, ModelResponse};

/// A language model behind some API.
///
/// Implementations do not retry; they report failures as [`ModelError`] and
/// the runtime decides (see [`ModelError::is_retryable`]).
#[async_trait]
pub trait ModelClient: Send + Sync + 'static {
    /// One non-streaming completion.
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError>;

    /// Streaming completion. The final item is always [`ModelDelta::Finished`]
    /// carrying the fully assembled message (text + tool calls) and usage.
    ///
    /// The outer `Result` covers failures before the first byte (connection,
    /// HTTP status); failures after that arrive as an `Err` item, after which
    /// the stream ends without a `Finished`.
    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError>;
}

/// A shared, type-erased [`ModelClient`].
pub type DynModel = Arc<dyn ModelClient>;

#[async_trait]
impl<T: ModelClient + ?Sized> ModelClient for Arc<T> {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        (**self).complete(req).await
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        (**self).stream(req).await
    }
}
