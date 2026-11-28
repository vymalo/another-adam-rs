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
    /// The journal epoch this starter writes into a run it starts ([ADR 0076]).
    ///
    /// [ADR 0076]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0076-a-run-records-the-journal-epoch-it-started-on.md
    fn replay_epoch(&self) -> u32;
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
    /// The whole state a head and its rows reconstruct
    /// ([`AgentStarter::join`](crate::AgentStarter::join)).
    fn join(&self, head: Value, rows: Vec<Value>) -> Result<Value, AgentError>;

    /// The transcript of `state`, or `None` when the agent does not split it. The runtime compares
    /// it with the rows it is about to write, to tell an append from a compaction without reading
    /// the store, and chains it to sign and check a run whose state is not split (ADR 0068
    /// decision 4).
    fn transcript(&self, state: Value) -> Result<Option<Vec<Value>>, AgentError>;
}

/// What stepping a run needs on top of starting it.
#[async_trait]
pub(crate) trait ErasedAgent: ErasedStarter {
    /// The oldest journal epoch this agent can still replay ([ADR 0076]); a run below it ends
    /// `Failed` naming both epochs.
    ///
    /// [ADR 0076]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0076-a-run-records-the-journal-epoch-it-started-on.md
    fn min_replay_epoch(&self) -> u32;
    /// Step `state` and, when the agent splits it, return the head and the transcript it would
    /// keep beside it. `state` stays the whole state: the read path still serves the v4 format in
    /// the expand release, so a step never receives a head with empty rows.
    async fn step(&self, ctx: &mut Ctx, state: Value) -> Result<Stepped, AgentError>;

    /// The whole state a head and its rows reconstruct ([`Agent::join`](crate::Agent::join)).
    fn join(&self, head: Value, rows: Vec<Value>) -> Result<Value, AgentError>;

    async fn on_cancelled(&self, run: RunId, state: Value);
}

/// The outcome of one erased step: the transition the runtime commits, and the split of the state
/// it carries, if the agent declares one.
pub(crate) struct Stepped {
    pub transition: Transition<Value>,
    /// `(head, rows)` for [`Agent::split`](crate::Agent::split); `None` keeps the whole state.
    pub split: Option<(Value, Vec<Value>)>,
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

    fn replay_epoch(&self) -> u32 {
        self.0.replay_epoch()
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

    fn join(&self, head: Value, rows: Vec<Value>) -> Result<Value, AgentError> {
        ErasedAgent::join(self, head, rows)
    }

    fn transcript(&self, state: Value) -> Result<Option<Vec<Value>>, AgentError> {
        let state: A::State = serde_json::from_value(state).map_err(|e| {
            AgentError::permanent(format!(
                "a run's previous state does not decode as {:?}'s state",
                self.0.name()
            ))
            .with_source(e)
        })?;
        Ok(self.0.split(&state).map(|(_, rows)| rows))
    }
}

/// A start-only registration: no `step`.
pub(crate) struct StarterOnly<S>(pub S);

impl<S: AgentStarter> ErasedStarter for StarterOnly<S> {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn replay_epoch(&self) -> u32 {
        self.0.replay_epoch()
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

    fn join(&self, head: Value, rows: Vec<Value>) -> Result<Value, AgentError> {
        let head: S::State = serde_json::from_value(head).map_err(|e| {
            AgentError::permanent(format!(
                "a run's head does not decode as {:?}'s state",
                self.0.name()
            ))
            .with_source(e)
        })?;
        encode(&self.0.join(head, rows)?)
    }

    fn transcript(&self, state: Value) -> Result<Option<Vec<Value>>, AgentError> {
        let state: S::State = serde_json::from_value(state).map_err(|e| {
            AgentError::permanent(format!(
                "a run's state does not decode as {:?}'s state",
                self.0.name()
            ))
            .with_source(e)
        })?;
        Ok(self.0.split(&state).map(|(_, rows)| rows))
    }
}

#[async_trait]
impl<A: Agent> ErasedAgent for Erased<A> {
    fn min_replay_epoch(&self) -> u32 {
        self.0.min_replay_epoch()
    }

    async fn step(&self, ctx: &mut Ctx, state: Value) -> Result<Stepped, AgentError> {
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
        // The split is taken from the typed state, inside the one place that already has it: the
        // state is deserialized once, and the rows the head would keep are the agent's own.
        let stepped = self.0.step(ctx, state).await?;
        Ok(match stepped {
            Transition::Continue(s) => Stepped {
                split: self.split(&s)?,
                transition: Transition::Continue(encode(&s)?),
            },
            Transition::Park { state, wake_at } => Stepped {
                split: self.split(&state)?,
                transition: Transition::Park {
                    state: encode(&state)?,
                    wake_at,
                },
            },
            Transition::Done { state, output } => Stepped {
                split: self.split(&state)?,
                transition: Transition::Done {
                    state: encode(&state)?,
                    output,
                },
            },
            Transition::Fail { state, error } => Stepped {
                split: self.split(&state)?,
                transition: Transition::Fail {
                    state: encode(&state)?,
                    error,
                },
            },
        })
    }

    fn join(&self, head: Value, rows: Vec<Value>) -> Result<Value, AgentError> {
        let head: A::State = serde_json::from_value(head).map_err(|e| {
            AgentError::permanent(format!(
                "a run's head does not decode as {:?}'s state",
                self.0.name()
            ))
            .with_source(e)
        })?;
        encode(&self.0.join(head, rows)?)
    }

    async fn on_cancelled(&self, run: RunId, state: Value) {
        self.0.on_cancelled(run, state).await;
    }
}

impl<A: Agent> Erased<A> {
    /// `state` split as the agent declares it, with the head encoded for the head document.
    fn split(&self, state: &A::State) -> Result<Option<(Value, Vec<Value>)>, AgentError> {
        match self.0.split(state) {
            None => Ok(None),
            Some((head, rows)) => Ok(Some((encode(&head)?, rows))),
        }
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
