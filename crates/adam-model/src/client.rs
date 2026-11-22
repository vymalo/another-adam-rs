use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::{InputModalities, ModelDelta, ModelError, ModelProfile, ModelRequest, ModelResponse};

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

    /// The deployment's profile of the model the alias `model` names, when it declared one
    /// (`MODEL_ALIASES_FILE`, ADR 0063 and ADR 0064): its reasoning levels and how they are sent,
    /// its output cap, its extra body. `None` (the default) when nobody declared one, and then
    /// nothing of a profile is applied.
    ///
    /// Hosts will read it to resolve a call's settings — the cap resolved per call once the model
    /// is chosen at run time (ADR 0064 slice 5, not built; the cap itself is built per agent, slice
    /// 2) — and the OpenAI-compatible client reads it to send a request's reasoning level through
    /// the model's wire.
    fn profile(&self, model: &str) -> Option<&ModelProfile> {
        let _ = model;
        None
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

    fn profile(&self, model: &str) -> Option<&ModelProfile> {
        (**self).profile(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ImageLimits, ImageOrigin, MockModel, ReasoningEffort, ReasoningWire};

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

    /// A client that answers a profile for one alias, to test the forward.
    struct Profiled(ModelProfile);

    #[async_trait]
    impl ModelClient for Profiled {
        async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse, ModelError> {
            Err(ModelError::invalid_request(
                "a test client completes nothing",
            ))
        }

        async fn stream(
            &self,
            _req: ModelRequest,
        ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
            Err(ModelError::invalid_request("a test client streams nothing"))
        }

        fn profile(&self, model: &str) -> Option<&ModelProfile> {
            (model == "m").then_some(&self.0)
        }
    }

    #[test]
    fn arc_forwards_the_profile() {
        let profile = ModelProfile {
            reasoning_efforts: Some(vec![ReasoningEffort::Low]),
            reasoning_wire: Some(ReasoningWire::ReasoningEffort),
            ..ModelProfile::default()
        };
        let shared: DynModel = Arc::new(Profiled(profile.clone()));
        assert_eq!(shared.profile("m"), Some(&profile));
        assert_eq!(shared.profile("other"), None);
        let twice: Arc<DynModel> = Arc::new(shared);
        assert_eq!(twice.profile("m"), Some(&profile));
        // A client that says nothing has no profile.
        let plain: DynModel = Arc::new(MockModel::new());
        assert_eq!(plain.profile("m"), None);
    }
}
