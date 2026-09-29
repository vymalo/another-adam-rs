//! Type erasure for the agent registry (the composition-root exception of
//! ADR 0009): the runtime stores agents of different `State` types side by
//! side and moves their state around as JSON.

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::{Agent, AgentError, AgentStarter, Inbound, Transition};
use crate::ctx::Ctx;

/// What starting a run needs: a name and the initial state.
pub(crate) trait ErasedStarter: Send + Sync {
    fn name(&self) -> &str;
    fn init(&self, input: Inbound) -> Result<Value, AgentError>;
}

/// What stepping a run needs on top of starting it.
#[async_trait]
pub(crate) trait ErasedAgent: ErasedStarter {
    async fn step(&self, ctx: &mut Ctx, state: Value) -> Result<Transition<Value>, AgentError>;
}

pub(crate) struct Erased<A>(pub A);

fn encode<S: serde::Serialize>(state: &S) -> Result<Value, AgentError> {
    serde_json::to_value(state)
        .map_err(|e| AgentError::permanent("agent state is not serializable").with_source(e))
}

impl<A: Agent> ErasedStarter for Erased<A> {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        encode(&self.0.init(input)?)
    }
}

/// A start-only registration: no `step`.
pub(crate) struct StarterOnly<S>(pub S);

impl<S: AgentStarter> ErasedStarter for StarterOnly<S> {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        encode(&self.0.init(input)?)
    }
}

#[async_trait]
impl<A: Agent> ErasedAgent for Erased<A> {
    async fn step(&self, ctx: &mut Ctx, state: Value) -> Result<Transition<Value>, AgentError> {
        let state: A::State = serde_json::from_value(state).map_err(|e| {
            // With a start-only front, a mismatch between the starter's and this agent's
            // `State` for the same name surfaces here, on the worker; say so.
            AgentError::permanent(format!(
                "stored agent state does not decode as {:?}'s state (if another process \
                 started this run with an AgentStarter, its State must be this agent's State)",
                self.0.name()
            ))
            .with_source(e)
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
