//! The process's model-call pool, wrapped around the model client an agent was given.
//!
//! [ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
//! puts a semaphore around the model calls in flight and a token bucket on the model's tokens per
//! minute. Both are the [`Admission`] of the process, and this is where a call asks it: one seat
//! per `complete` or `stream`, held until the answer or the stream ends, so a cancelled call
//! gives its seat back.
//!
//! The conversation a seat counts against is the root run the request names
//! ([`ModelRequest::run`], which the loop always sets), so every turn, compaction and child call of
//! one conversation counts together. A request that names no run is counted under the agent's
//! name.

use std::sync::Arc;
use std::time::Duration;

use adam_budget::{Admission, Ask, ConversationId, DynAdmission, Permit, Pool, Refused};
use adam_model::{
    DynModel, InputModalities, ModelClient, ModelDelta, ModelError, ModelProfile, ModelRequest,
    ModelResponse,
};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

/// How long one model call waits for a seat before the run parks on the refusal (ADR 0065,
/// decision 4: a short queue, then a park at `retry_after`).
pub(crate) const WAIT: Duration = Duration::from_secs(5);

/// A model client that takes a seat of the process's model-call pool for every call.
pub(crate) struct Admitted {
    inner: DynModel,
    admission: DynAdmission,
    /// The conversation key of a request that names no run.
    agent: String,
}

impl Admitted {
    pub(crate) fn new(inner: DynModel, admission: DynAdmission, agent: impl Into<String>) -> Self {
        Self {
            inner,
            admission,
            agent: agent.into(),
        }
    }

    /// The seat for `req`, or the refusal as the model error whose class and `retry_after` the
    /// loop already parks on.
    async fn seat(&self, req: &ModelRequest) -> Result<Permit, ModelError> {
        let conversation = match &req.run {
            Some(run) => ConversationId::new(run.as_str()),
            None => ConversationId::new(self.agent.as_str()),
        };
        let ask = Ask::new(Pool::ModelCalls, conversation)
            .tokens(tokens_of(req))
            .wait(WAIT);
        self.admission
            .admit(ask)
            .await
            .map_err(|refused| refusal_as_model_error(&refused))
    }
}

/// A refusal as the model error the loop reacts to: a full pool or an empty bucket is
/// `RateLimited` with its wait, so the run waits and asks again, and a request that can never fit
/// is `Invalid`, naming the limit.
fn refusal_as_model_error(refused: &Refused) -> ModelError {
    match refused {
        Refused::Busy { retry_after, .. } => ModelError::RateLimited {
            retry_after: *retry_after,
        },
        Refused::TokensPerMinute { retry_after } => ModelError::RateLimited {
            retry_after: Some(*retry_after),
        },
        Refused::TooLarge { asked, limit, .. } => ModelError::InvalidRequest {
            message: format!(
                "the model call asks for {asked} tokens, over the process's limit of {limit}"
            ),
            source: None,
        },
        Refused::DiskFull { used, limit } => ModelError::InvalidRequest {
            message: format!("the disk budget is full: {used} of {limit} bytes used"),
            source: None,
        },
        // `Refused` is non-exhaustive: a refusal a later slice adds is a request that cannot be
        // made as it is, never a failed run.
        other => ModelError::InvalidRequest {
            message: other.to_string(),
            source: None,
        },
    }
}

/// What a call is estimated to use: the request's text at four characters a token, plus the tokens
/// the answer may take. An estimate, because only the provider knows the count before the answer,
/// and the bucket is a rate, not a bill.
fn tokens_of(req: &ModelRequest) -> u32 {
    let mut chars = req.system.as_deref().map_or(0, |s| s.chars().count());
    for message in &req.messages {
        chars = chars.saturating_add(message.text().chars().count());
    }
    let asked = chars / 4 + usize::try_from(req.max_output_tokens.unwrap_or(0)).unwrap_or(0);
    u32::try_from(asked).unwrap_or(u32::MAX)
}

#[async_trait]
impl ModelClient for Admitted {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        let _seat = self.seat(&req).await?;
        self.inner.complete(req).await
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        let seat = self.seat(&req).await?;
        let stream = self.inner.stream(req).await?;
        // The seat rides with the stream: released when the stream ends, or is dropped by a
        // cancelled run.
        Ok(stream
            .map(move |item| {
                let _ = &seat;
                item
            })
            .boxed())
    }

    fn provider(&self) -> Option<&str> {
        self.inner.provider()
    }

    fn context_window(&self, model: &str) -> Option<u64> {
        self.inner.context_window(model)
    }

    fn input_modalities(&self, model: &str) -> InputModalities {
        self.inner.input_modalities(model)
    }

    fn profile(&self, model: &str) -> Option<&ModelProfile> {
        self.inner.profile(model)
    }
}

/// Wrap `model` in the process's admission when there is one.
pub(crate) fn admitted(model: DynModel, admission: Option<&DynAdmission>, agent: &str) -> DynModel {
    match admission {
        Some(admission) => Arc::new(Admitted::new(
            model,
            Arc::clone(admission),
            agent.to_owned(),
        )),
        None => model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saturated_bucket_is_rate_limited_with_its_wait() {
        let error = refusal_as_model_error(&Refused::TokensPerMinute {
            retry_after: Duration::from_secs(2),
        });
        assert!(matches!(
            error,
            ModelError::RateLimited {
                retry_after: Some(wait)
            } if wait == Duration::from_secs(2)
        ));
        let error = refusal_as_model_error(&Refused::Busy {
            pool: Pool::ModelCalls,
            retry_after: Some(Duration::from_millis(250)),
        });
        assert!(matches!(error, ModelError::RateLimited { .. }));
        let error = refusal_as_model_error(&Refused::TooLarge {
            what: "a call",
            asked: 9,
            limit: 8,
        });
        assert!(matches!(error, ModelError::InvalidRequest { .. }));
    }

    #[test]
    fn the_token_estimate_is_four_characters_a_token_plus_the_answer() {
        let mut req = ModelRequest::new("m");
        req.system = Some("aaaa".to_owned());
        req.max_output_tokens = Some(10);
        assert_eq!(tokens_of(&req), 11);
    }
}
