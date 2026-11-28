//! The host's model client with the engine's profiles behind it ([ADR 0088], slice W1 amended).
//!
//! [`EngineBuilder::models`](crate::EngineBuilder::models) gives the deployment's profiles to the
//! assembly, which resolves a folder's effort, preset and cap against them; the window of a model
//! is the client's answer ([`ModelClient::context_window`]). [`Profiled`] makes the two one source:
//! a client that has no profile for an alias answers with the engine's, through the same rule
//! ([`resolve_context_window`]), so the host does not give its client the profiles a second time.
//!
//! [ADR 0088]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0088-the-context-window-follows-the-model-and-the-history-budget-is-200k.md

use std::sync::Arc;

use adam_model::{
    DynModel, InputModalities, ModelClient, ModelDelta, ModelError, ModelProfile, ModelProfiles,
    ModelRequest, ModelResponse, resolve_context_window,
};
use async_trait::async_trait;
use futures::stream::BoxStream;

/// `model` with `profiles` behind it; `model` itself when there are no profiles.
pub(crate) fn profiled(model: DynModel, profiles: &ModelProfiles) -> DynModel {
    if profiles.profiles.is_empty() {
        model
    } else {
        Arc::new(Profiled {
            inner: model,
            profiles: profiles.clone(),
        })
    }
}

/// A model client whose profile of an alias is its own when it has one, else the engine's.
///
/// The client's own profile wins **whole** (the two are not merged): it is the one the client
/// sends a request by (its reasoning wire, its extra body), so the window it says is the one that
/// goes with it. Everything else is the client's.
struct Profiled {
    inner: DynModel,
    profiles: ModelProfiles,
}

#[async_trait]
impl ModelClient for Profiled {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.inner.complete(req).await
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        self.inner.stream(req).await
    }

    fn provider(&self) -> Option<&str> {
        self.inner.provider()
    }

    /// The client's window when it has a profile of `model`; else the engine's profile's
    /// `contextWindow`, over what the client says without a profile (the window it was configured
    /// with for that alias): [`resolve_context_window`], the one rule.
    fn context_window(&self, model: &str) -> Option<u64> {
        if self.inner.profile(model).is_some() {
            return self.inner.context_window(model);
        }
        resolve_context_window(
            self.profiles.profile(model),
            self.inner.context_window(model),
        )
    }

    fn input_modalities(&self, model: &str) -> InputModalities {
        self.inner.input_modalities(model)
    }

    fn profile(&self, model: &str) -> Option<&ModelProfile> {
        self.inner
            .profile(model)
            .or_else(|| self.profiles.profile(model))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeMap;

    use adam_model::MockModel;

    use super::*;

    fn windowed(window: Option<u64>) -> ModelProfile {
        ModelProfile {
            context_window: window,
            ..ModelProfile::default()
        }
    }

    fn of(entries: &[(&str, ModelProfile)]) -> ModelProfiles {
        ModelProfiles {
            profiles: entries
                .iter()
                .map(|(alias, profile)| ((*alias).to_owned(), profile.clone()))
                .collect::<BTreeMap<_, _>>(),
            presets: BTreeMap::new(),
        }
    }

    #[test]
    fn the_engines_profile_answers_for_a_client_without_one() {
        let engine = of(&[("m", windowed(Some(200_000)))]);
        let model = profiled(Arc::new(MockModel::new()), &engine);
        assert_eq!(model.context_window("m"), Some(200_000));
        assert_eq!(model.profile("m"), engine.profile("m"));
        assert_eq!(model.context_window("other"), None, "nobody said");
        assert!(model.profile("other").is_none());
    }

    #[test]
    fn the_profiles_window_wins_over_the_clients_configured_one() {
        let engine = of(&[("m", windowed(Some(200_000))), ("bare", windowed(None))]);
        let client = MockModel::new()
            .with_context_window("m", 8_192)
            .with_context_window("bare", 8_192)
            .with_context_window("plain", 4_096);
        let model = profiled(Arc::new(client), &engine);
        assert_eq!(model.context_window("m"), Some(200_000), "the profile's");
        assert_eq!(
            model.context_window("bare"),
            Some(8_192),
            "a profile without one falls through to the client's"
        );
        assert_eq!(model.context_window("plain"), Some(4_096), "no profile");
    }

    #[test]
    fn the_clients_own_profile_wins_whole() {
        let engine = of(&[
            ("m", windowed(Some(200_000))),
            ("n", windowed(Some(100_000))),
        ]);
        let own = of(&[("m", windowed(Some(400_000))), ("n", windowed(None))]);
        let client = MockModel::new()
            .with_profiles(own.clone())
            .with_context_window("n", 8_192);
        let model = profiled(Arc::new(client), &engine);
        assert_eq!(model.context_window("m"), Some(400_000), "the client's");
        assert_eq!(model.profile("m"), own.profile("m"));
        assert_eq!(
            model.context_window("n"),
            Some(8_192),
            "the client's profile is not merged with the engine's"
        );
    }

    #[test]
    fn no_profiles_leave_the_client_as_it_is() {
        let client: DynModel = Arc::new(MockModel::new().with_provider("mock"));
        let model = profiled(Arc::clone(&client), &ModelProfiles::default());
        assert!(Arc::ptr_eq(&client, &model));
        let model = profiled(client, &of(&[("m", windowed(Some(1)))]));
        assert_eq!(model.provider(), Some("mock"), "forwarded");
    }
}
