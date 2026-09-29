//! Type erasure for the agent registry (the composition-root exception of
//! ADR 0009): the runtime stores agents of different `State` types side by
//! side and moves their state around as JSON.

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::{Agent, AgentError, Inbound, Transition};
use crate::ctx::Ctx;

#[async_trait]
pub(crate) trait ErasedAgent: Send + Sync {
    fn name(&self) -> &str;
    fn init(&self, input: Inbound) -> Result<Value, AgentError>;
    async fn step(&self, ctx: &mut Ctx, state: Value) -> Result<Transition<Value>, AgentError>;
}

pub(crate) struct Erased<A>(pub A);

fn encode<S: serde::Serialize>(state: &S) -> Result<Value, AgentError> {
    serde_json::to_value(state)
        .map_err(|e| AgentError::permanent("agent state is not serializable").with_source(e))
}

#[async_trait]
impl<A: Agent> ErasedAgent for Erased<A> {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        encode(&self.0.init(input)?)
    }

    async fn step(&self, ctx: &mut Ctx, state: Value) -> Result<Transition<Value>, AgentError> {
        let state: A::State = serde_json::from_value(state).map_err(|e| {
            AgentError::permanent("stored agent state does not decode").with_source(e)
        })?;
        Ok(match self.0.step(ctx, state).await? {
            Transition::Continue(s) => Transition::Continue(encode(&s)?),
            Transition::Park { state, wake_at } => Transition::Park {
                state: encode(&state)?,
                wake_at,
            },
            Transition::Done { state, output } => Transition::Done {
                state: encode(&state)?,
                output,
            },
            Transition::Fail { state, error } => Transition::Fail {
                state: encode(&state)?,
                error,
            },
        })
    }
}
