use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::{InputModalities, ModelDelta, ModelError, ModelRequest, ModelResponse};

/// A language model behind some API.
///
/// Implementations do not retry; they report failures as [`ModelError`] and
/// the runtime decides (see [`Classify::is_retryable`](adam_error::Classify::is_retryable)).
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

    /// The provider this client talks to, as a lower-case label for reports (`openai` for an
    /// OpenAI-compatible endpoint): what the `usage/v1` extension calls `provider`. `None` (the
    /// default) when the client does not say.
    fn provider(&self) -> Option<&str> {
        None
    }

    /// The context window, in tokens, of the model the alias `model` names, as the deployment
    /// configured it. `None` (the default) when nobody said: a report then carries no window.
    fn context_window(&self, model: &str) -> Option<u64> {
        let _ = model;
        None
    }

    /// What the model the alias `model` names can read besides text, as the deployment declared
    /// it. Text only (the default) unless somebody said otherwise: it is never guessed from the
    /// name of a model, and an alias nobody declared stays text only.
    fn input_modalities(&self, model: &str) -> InputModalities {
        let _ = model;
        InputModalities::text_only()
    }
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

    fn provider(&self) -> Option<&str> {
        (**self).provider()
    }

    fn context_window(&self, model: &str) -> Option<u64> {
        (**self).context_window(model)
    }

    fn input_modalities(&self, model: &str) -> InputModalities {
        (**self).input_modalities(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ImageLimits, ImageOrigin, MockModel};

    #[test]
    fn arc_forwards_input_modalities() {
        let limits = ImageLimits {
            max_bytes: 10,
            max_count: 2,
            max_side: 64,
            sources: [ImageOrigin::Person].into(),
        };
        let seeing = Arc::new(MockModel::new().seeing_images(limits.clone()));
        assert_eq!(seeing.input_modalities("m").images, Some(limits));
        let shared: DynModel = seeing.clone();
        let twice: Arc<DynModel> = Arc::new(shared);
        assert!(twice.input_modalities("m").images.is_some());
        // A client that says nothing is text only.
        let blind: DynModel = Arc::new(MockModel::new());
        assert_eq!(blind.input_modalities("m"), InputModalities::text_only());
    }
}
