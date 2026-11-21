//! A run parked on the model ([ADR 0067] decision 4): what its task shows, and the resume that
//! wakes it. The section "A run parked on the model" of `docs/reference/a2a-server.md` is the
//! contract.
//!
//! [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md

use a2a::Message;
use adam_core::RunStatus;
use adam_runtime::{Inbound, RunView};
use serde_json::{Value, json};

pub(crate) use adam_a2a::is_resume;

/// The reason a resume to a task that is not parked on the model is refused with.
pub const NOT_PARKED: &str = "the task is not parked on the model";

/// The inbound kind a resume is delivered as: `adam_llm_agent::MODEL_RESUME_KIND`, which the agent
/// loop reads as a wake-up and drops (this crate does not depend on the loop, so the word is
/// repeated; a test holds the two equal).
pub(crate) const RESUME_KIND: &str = "adam.resume";

/// What a resume delivers: a wake-up with no text, under the message's id.
pub(crate) fn resume_inbound(message: &Message) -> Inbound {
    Inbound::new(RESUME_KIND, json!({})).with_id(message.message_id.clone())
}

/// The model wait the run's state holds (`/model_wait`, what `adam_llm_agent::Conversation`
/// writes), when it has one.
fn model_wait(view: &RunView) -> Option<&Value> {
    view.state.get("model_wait").filter(|w| w.is_object())
}

/// Whether the run is parked on the model: not finished, and its state holds a model wait. A run
/// woken a moment ago and not yet stepped still is (it makes the call again when it steps).
pub(crate) fn parked_on_model(view: &RunView) -> bool {
    !view.status.is_terminal() && model_wait(view).is_some()
}

/// Whether the run waits on the model, parked: what its task's state and message come from.
fn parked_now(view: &RunView) -> Option<&Value> {
    (view.status == RunStatus::Parked)
        .then(|| model_wait(view))
        .flatten()
}

/// Whether the run is parked on a refusal of status 401 or 403: its task is `auth-required`.
pub(crate) fn needs_auth(view: &RunView) -> bool {
    parked_now(view)
        .and_then(|w| w.get("status"))
        .and_then(Value::as_u64)
        .is_some_and(|s| matches!(s, 401 | 403))
}

/// The reason of a run parked on the model, as its task's status message.
pub(crate) fn reason(view: &RunView) -> Option<String> {
    parked_now(view)?
        .get("reason")?
        .as_str()
        .filter(|r| !r.trim().is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kind_is_the_one_the_agent_loop_reads() {
        assert_eq!(RESUME_KIND, adam_llm_agent::MODEL_RESUME_KIND);
    }

    #[test]
    fn only_true_makes_a_resume() {
        let with = |v: Value| {
            let mut m = Message::new(a2a::Role::User, vec![]);
            m.metadata = Some(
                [(adam_a2a::RESUME_METADATA_KEY.to_owned(), v)]
                    .into_iter()
                    .collect(),
            );
            m
        };
        assert!(is_resume(&with(json!(true))));
        for v in [json!(false), json!("true"), json!(1), Value::Null] {
            assert!(!is_resume(&with(v)));
        }
        assert!(!is_resume(&Message::new(a2a::Role::User, vec![])));
    }
}
