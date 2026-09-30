//! Type erasure for the agent registry (the composition-root exception of
//! ADR 0009): the runtime stores agents of different `State` types side by
//! side and moves their state around as JSON.

use adam_core::RunId;
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::agent::{Agent, AgentError, AgentStarter, Inbound, Transition};
use crate::ctx::Ctx;

/// What starting a run needs: a name and the initial state.
pub(crate) trait ErasedStarter: Send + Sync {
    fn name(&self) -> &str;
    fn init(&self, input: Inbound) -> Result<Value, AgentError>;
    /// The initial state of a run that continues `prior_run`, whose stored agent state is
    /// `prior`. A `prior` that does not decode as this agent's state is not an error: the run
    /// starts fresh, as [`init`](Self::init) says.
    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Value,
        prior_run: RunId,
    ) -> Result<Value, AgentError>;
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

/// The prior run's state as `S`, or `None` (logged) when it does not decode.
///
/// A prior state that cannot be read, written by another version of the agent, or by an agent
/// that shares the name but not the state type, must not stop a new task from starting: it starts
/// as if nothing came before. Only the kind of decoding failure is logged, never the
/// serde message, because that can quote a value of the prior conversation.
fn decode_prior<S: DeserializeOwned>(agent: &str, prior_run: RunId, prior: &Value) -> Option<S> {
    match S::deserialize(prior) {
        Ok(state) => Some(state),
        Err(e) => {
            tracing::warn!(
                agent,
                %prior_run,
                category = ?e.classify(),
                "the prior run's state does not decode as this agent's state; starting without it"
            );
            None
        }
    }
}

impl<A: Agent> ErasedStarter for Erased<A> {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        encode(&self.0.init(input)?)
    }

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Value,
        prior_run: RunId,
    ) -> Result<Value, AgentError> {
        match decode_prior::<A::State>(self.0.name(), prior_run, prior) {
            Some(state) => encode(&self.0.init_continuing(input, &state, prior_run)?),
            None => self.init(input),
        }
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

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Value,
        prior_run: RunId,
    ) -> Result<Value, AgentError> {
        match decode_prior::<S::State>(self.0.name(), prior_run, prior) {
            Some(state) => encode(&self.0.init_continuing(input, &state, prior_run)?),
            None => self.init(input),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    /// An agent whose state counts how many tasks came before, so a continuation is visible.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Chain {
        text: String,
        before: Vec<String>,
    }

    fn start(input: &Inbound) -> Chain {
        Chain {
            text: input.payload["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            before: Vec::new(),
        }
    }

    fn carry(input: &Inbound, prior: &Chain, prior_run: RunId) -> Chain {
        let mut before = prior.before.clone();
        before.push(format!("{}@{prior_run}", prior.text));
        Chain {
            text: input.payload["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            before,
        }
    }

    fn input(text: &str) -> Inbound {
        Inbound::new("message", json!({ "text": text }))
    }

    /// Overrides `init_continuing`.
    struct Carrying;

    #[async_trait]
    impl Agent for Carrying {
        type State = Chain;
        fn name(&self) -> &str {
            "carrying"
        }
        fn init(&self, input: Inbound) -> Result<Chain, AgentError> {
            Ok(start(&input))
        }
        fn init_continuing(
            &self,
            input: Inbound,
            prior: &Chain,
            prior_run: RunId,
        ) -> Result<Chain, AgentError> {
            Ok(carry(&input, prior, prior_run))
        }
        async fn step(&self, _: &mut Ctx, s: Chain) -> Result<Transition<Chain>, AgentError> {
            Ok(Transition::Continue(s))
        }
    }

    /// Does not override it.
    struct Plain;

    #[async_trait]
    impl Agent for Plain {
        type State = Chain;
        fn name(&self) -> &str {
            "plain"
        }
        fn init(&self, input: Inbound) -> Result<Chain, AgentError> {
            Ok(start(&input))
        }
        async fn step(&self, _: &mut Ctx, s: Chain) -> Result<Transition<Chain>, AgentError> {
            Ok(Transition::Continue(s))
        }
    }

    struct CarryingStarter;

    impl AgentStarter for CarryingStarter {
        type State = Chain;
        fn name(&self) -> &str {
            "carrying"
        }
        fn init(&self, input: Inbound) -> Result<Chain, AgentError> {
            Ok(start(&input))
        }
        fn init_continuing(
            &self,
            input: Inbound,
            prior: &Chain,
            prior_run: RunId,
        ) -> Result<Chain, AgentError> {
            Ok(carry(&input, prior, prior_run))
        }
    }

    struct PlainStarter;

    impl AgentStarter for PlainStarter {
        type State = Chain;
        fn name(&self) -> &str {
            "plain"
        }
        fn init(&self, input: Inbound) -> Result<Chain, AgentError> {
            Ok(start(&input))
        }
    }

    fn prior_state() -> Value {
        json!({"text": "first", "before": []})
    }

    fn decoded(v: Value) -> Chain {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_continuation_reaches_the_override_of_an_agent_and_of_a_starter() {
        let run = RunId::new();
        let want = Chain {
            text: "second".into(),
            before: vec![format!("first@{run}")],
        };
        let by_agent = Erased(Carrying)
            .init_continuing(input("second"), &prior_state(), run)
            .unwrap();
        assert_eq!(decoded(by_agent), want);
        let by_starter = StarterOnly(CarryingStarter)
            .init_continuing(input("second"), &prior_state(), run)
            .unwrap();
        assert_eq!(decoded(by_starter), want);
    }

    #[test]
    fn without_an_override_a_continuation_is_a_fresh_start() {
        let run = RunId::new();
        let fresh = Chain {
            text: "second".into(),
            before: Vec::new(),
        };
        let by_agent = Erased(Plain)
            .init_continuing(input("second"), &prior_state(), run)
            .unwrap();
        assert_eq!(decoded(by_agent), fresh);
        let by_starter = StarterOnly(PlainStarter)
            .init_continuing(input("second"), &prior_state(), run)
            .unwrap();
        assert_eq!(decoded(by_starter), fresh);
        // And it is exactly what `init` gives.
        assert_eq!(
            Erased(Plain).init(input("second")).unwrap(),
            Erased(Plain)
                .init_continuing(input("second"), &prior_state(), run)
                .unwrap()
        );
    }

    #[test]
    fn a_prior_state_that_does_not_decode_falls_back_to_init() {
        let run = RunId::new();
        let fresh = Chain {
            text: "second".into(),
            before: Vec::new(),
        };
        // A different shape, a different type, and no state at all.
        for not_a_chain in [
            json!({"text": 7}),
            json!("a string"),
            json!(null),
            json!([1]),
        ] {
            let by_agent = Erased(Carrying)
                .init_continuing(input("second"), &not_a_chain, run)
                .unwrap();
            assert_eq!(decoded(by_agent), fresh, "{not_a_chain}");
            let by_starter = StarterOnly(CarryingStarter)
                .init_continuing(input("second"), &not_a_chain, run)
                .unwrap();
            assert_eq!(decoded(by_starter), fresh, "{not_a_chain}");
        }
    }

    #[test]
    fn the_fallback_still_reports_what_init_refuses() {
        struct Picky;
        #[async_trait]
        impl Agent for Picky {
            type State = Chain;
            fn name(&self) -> &str {
                "picky"
            }
            fn init(&self, _: Inbound) -> Result<Chain, AgentError> {
                Err(AgentError::permanent("unusable start message: nope"))
            }
            async fn step(&self, _: &mut Ctx, s: Chain) -> Result<Transition<Chain>, AgentError> {
                Ok(Transition::Continue(s))
            }
        }
        let err = Erased(Picky)
            .init_continuing(input("x"), &json!(null), RunId::new())
            .unwrap_err();
        assert!(matches!(err, AgentError::Permanent { .. }), "{err:?}");
        // With a prior that decodes the default goes through `init` too, and fails the same.
        let err = Erased(Picky)
            .init_continuing(input("x"), &prior_state(), RunId::new())
            .unwrap_err();
        assert!(matches!(err, AgentError::Permanent { .. }), "{err:?}");
    }
}
