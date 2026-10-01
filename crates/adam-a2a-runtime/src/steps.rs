//! A run's steps ([`RunEvent::Step`](adam_runtime::RunEvent::Step)) as A2A status messages.
//!
//! A step is reported as a `working` status update whose message has one text part, a plain line
//! for anyone who ignores the extension, and, **for a client whose request activated `steps/v1`**
//! (`Caller::extensions`), the step itself in the message's `metadata` under the extension's URI:
//!
//! ```json
//! {"https://agents.vymalo.com/a2a/extensions/steps/v1": {
//!   "id": "acp:c2:1", "parentId": "tool:c2", "kind": "command", "label": "npm test",
//!   "state": "failed", "icon": "execute", "detail": "1 failed"}}
//! ```
//!
//! (the contract is the orchestration layer's, `docs/api/steps-v1.md` of
//! `vymalo/another-agentic-system`). Without the activation the message is the line alone, which
//! is what a client that knows nothing of steps reads.

use std::collections::HashMap;

use a2a::{Message, Part, Role};
use adam_a2a::STEPS_EXTENSION;
use adam_runtime::{StepEvent, StepState};
use serde_json::{Map, Value};

/// The status message for `step`: its [`plain_text`], and, when `activated`, the step in the
/// message's metadata (and the extension named in `extensions`).
pub(crate) fn step_message(step: &StepEvent, activated: bool) -> Message {
    let mut message = Message::new(Role::Agent, vec![Part::text(plain_text(step))]);
    if activated {
        message.metadata = Some(HashMap::from([(
            STEPS_EXTENSION.to_owned(),
            step_metadata(step),
        )]));
        message.extensions = Some(vec![STEPS_EXTENSION.to_owned()]);
    }
    message
}

/// What `steps/v1` carries: the members of the contract, the optional ones only when there are.
pub(crate) fn step_metadata(step: &StepEvent) -> Value {
    let mut report = Map::new();
    report.insert("id".into(), step.id.clone().into());
    if let Some(parent) = &step.parent {
        report.insert("parentId".into(), parent.clone().into());
    }
    report.insert("kind".into(), step.kind.as_str().into());
    report.insert("label".into(), step.label.clone().into());
    report.insert("state".into(), step.state.as_str().into());
    if let Some(icon) = step.icon {
        report.insert("icon".into(), icon.as_str().into());
    }
    if let Some(detail) = &step.detail {
        report.insert("detail".into(), detail.clone().into());
    }
    Value::Object(report)
}

/// The line a client that ignores steps reads (and what `steps/v1` asks for as the message's one
/// text part):
///
/// * a step that starts or moves: its label; with a detail, the detail on a step at the top (the
///   progress line of a tool call, as `emit_progress` always was) and `label: detail` on a step
///   under another;
/// * a step that ends: `label: done`, `label: failed` or `label: canceled`, and the detail after it.
pub(crate) fn plain_text(step: &StepEvent) -> String {
    let end = |word: &str| match &step.detail {
        Some(detail) => format!("{}: {word}: {detail}", step.label),
        None => format!("{}: {word}", step.label),
    };
    match step.state {
        StepState::Completed => end("done"),
        StepState::Failed => end("failed"),
        StepState::Canceled => end("canceled"),
        _ => match (&step.detail, &step.parent) {
            (None, _) => step.label.clone(),
            (Some(detail), None) => detail.clone(),
            (Some(detail), Some(_)) => format!("{}: {detail}", step.label),
        },
    }
}

#[cfg(test)]
mod tests {
    use adam_runtime::{StepIcon, StepKind};
    use serde_json::json;

    use super::*;

    fn step(state: StepState) -> StepEvent {
        StepEvent::new("acp:c2:1", StepKind::Command, "npm test", state)
    }

    #[test]
    fn the_metadata_is_the_contracts_report() {
        let full = step(StepState::Failed)
            .under("tool:c2")
            .with_icon(StepIcon::Execute)
            .with_detail("1 failed");
        assert_eq!(
            step_metadata(&full),
            json!({"id": "acp:c2:1", "parentId": "tool:c2", "kind": "command", "label": "npm test",
                   "state": "failed", "icon": "execute", "detail": "1 failed"})
        );
        assert_eq!(
            step_metadata(&step(StepState::Running)),
            json!({"id": "acp:c2:1", "kind": "command", "label": "npm test", "state": "running"}),
            "no parent, icon or detail: no member"
        );
    }

    #[test]
    fn a_client_that_activated_steps_gets_the_report_beside_the_line() {
        let report = step(StepState::Running).under("tool:c2");
        let message = step_message(&report, true);
        assert_eq!(message.role, Role::Agent);
        assert_eq!(message.text(), Some("npm test"));
        assert_eq!(message.parts.len(), 1);
        let metadata = message.metadata.expect("the step is in the metadata");
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[STEPS_EXTENSION], step_metadata(&report));
        assert_eq!(message.extensions, Some(vec![STEPS_EXTENSION.to_owned()]));
    }

    #[test]
    fn a_client_that_did_not_gets_the_line_alone() {
        let message = step_message(&step(StepState::Running), false);
        assert_eq!(message.text(), Some("npm test"));
        assert_eq!(message.metadata, None);
        assert_eq!(message.extensions, None);
    }

    #[test]
    fn the_line_says_what_the_step_is_doing_and_how_it_ended() {
        let at_the_top = |state| StepEvent::new("tool:c1", StepKind::Tool, "run_checks", state);
        // A start is the label; a progress line of a call at the top is the line itself, as
        // `emit_progress` has always been.
        assert_eq!(plain_text(&at_the_top(StepState::Running)), "run_checks");
        assert_eq!(
            plain_text(&at_the_top(StepState::Running).with_detail("running checks: just check")),
            "running checks: just check"
        );
        // Under another step the label says whose line it is.
        assert_eq!(
            plain_text(
                &step(StepState::Running)
                    .under("tool:c2")
                    .with_detail("12 passed")
            ),
            "npm test: 12 passed"
        );
        assert_eq!(plain_text(&step(StepState::Waiting)), "npm test");
        // An end says how it ended, and why when it says.
        assert_eq!(
            plain_text(&at_the_top(StepState::Completed)),
            "run_checks: done"
        );
        assert_eq!(
            plain_text(&at_the_top(StepState::Canceled)),
            "run_checks: canceled"
        );
        assert_eq!(
            plain_text(&step(StepState::Failed).with_detail("1 failed")),
            "npm test: failed: 1 failed"
        );
    }
}
