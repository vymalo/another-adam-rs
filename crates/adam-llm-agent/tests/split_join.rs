//! The `Conversation` half of `Agent::split`/`join`: the head keeps everything but the messages,
//! the rows are the messages, and the round trip re-encodes to the same bytes.
//!
//! The shapes are the ones the agent can produce: empty, one message, a long history, a compacted
//! history, a pending wait, and an artifact (with the plan and the announcement the head carries).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use adam_llm_agent::{
    ArtifactRef, Conversation, LlmStarter, PendingQuestion, PendingWait, Plan, PlanItem,
    PlanStatus, TRUNCATION_MARKER_PREFIX,
};
use adam_model::{Message, ToolCall, Usage};
use adam_runtime::AgentStarter;
use serde_json::json;

/// `join(split(s))`, and the invariant that it re-encodes to `s` byte for byte.
fn assert_round_trips(state: &Conversation) {
    let (head, rows) = state.split();
    let joined = Conversation::join(head, rows).expect("joins");
    assert_eq!(joined, *state, "join(split(s)) == s");
    assert_eq!(
        serde_json::to_vec(&joined).expect("serializes"),
        serde_json::to_vec(state).expect("serializes"),
        "the round trip re-encodes to the same bytes"
    );
}

#[test]
fn an_empty_conversation_splits_into_no_rows() {
    let state = Conversation::default();
    let (head, rows) = state.split();
    assert!(rows.is_empty(), "no messages, no rows");
    assert!(head.messages.is_empty());
    assert_eq!(head, state, "the head is the whole state");
    assert_round_trips(&state);
}

#[test]
fn one_message_round_trips() {
    let state = Conversation::new("hello");
    let (head, rows) = state.split();
    assert_eq!(rows.len(), 1);
    assert!(head.messages.is_empty(), "the head keeps no message");
    assert_eq!(Conversation::join(head, rows).unwrap(), state);
    assert_round_trips(&state);
}

#[test]
fn a_long_history_round_trips_in_order() {
    let mut state = Conversation::default();
    for turn in 0..40 {
        state
            .messages
            .push(Message::user_text(format!("ask {turn}")));
        state
            .messages
            .push(Message::assistant_text(format!("answer {turn}")));
    }
    let (head, rows) = state.split();
    assert_eq!(rows.len(), 80);
    assert!(head.messages.is_empty());
    let joined = Conversation::join(head, rows).unwrap();
    assert_eq!(joined.messages, state.messages, "oldest first, unchanged");
    assert_round_trips(&state);
}

#[test]
fn a_compacted_history_keeps_its_shortenings_in_the_head() {
    let mut state = Conversation::default();
    state.messages.push(Message::user_text("read the file"));
    state.messages.push(Message::Tool {
        call_id: "call-1".into(),
        content: format!("{TRUNCATION_MARKER_PREFIX} the output was long"),
        is_error: false,
        images: Vec::new(),
    });
    state.messages.push(Message::assistant_text("done"));
    state.shortenings = vec![2, 5];
    state.turns = 6;
    let (head, rows) = state.split();
    assert_eq!(head.shortenings, vec![2, 5], "a shortening is head data");
    assert_eq!(head.turns, 6);
    assert_eq!(rows.len(), 3);
    let joined = Conversation::join(head, rows).unwrap();
    assert_eq!(joined, state);
    assert_round_trips(&state);
}

#[test]
fn a_pending_wait_round_trips_in_the_head() {
    let mut state = Conversation::default();
    state.messages.push(Message::user_text("which one?"));
    state.pending_wait = Some(PendingWait::Question(PendingQuestion {
        call_id: "call-7".into(),
        tool: "ask_user".into(),
        question: "Which repository?".into(),
        ui: Some(json!([{"type": "text", "text": "Which repository?"}])),
        stream: None,
    }));
    state.pending_calls = vec![ToolCall {
        id: "call-7".into(),
        name: "ask_user".into(),
        arguments: json!({"question": "Which repository?"}),
        invalid_arguments: None,
    }];
    state.ordered_calls = vec!["call-7".into()];
    let (head, rows) = state.split();
    assert_eq!(head.pending_wait, state.pending_wait);
    assert_eq!(head.pending_calls, state.pending_calls);
    assert_eq!(head.ordered_calls, state.ordered_calls);
    assert_eq!(Conversation::join(head, rows).unwrap(), state);
    assert_round_trips(&state);
}

#[test]
fn an_artifact_plan_and_announcement_stay_in_the_head() {
    let mut state = Conversation::default();
    state.messages.push(Message::user_text("make a plan"));
    state.artifacts = vec![ArtifactRef {
        id: Some("plan".into()),
        name: "plan".into(),
        mime_type: Some("application/json".into()),
        bytes: None,
    }];
    state.plan = Some(Plan {
        title: Some("Ship it".into()),
        items: vec![PlanItem {
            id: 1,
            text: "write the test".into(),
            status: PlanStatus::Done,
            note: None,
        }],
    });
    state.announced = Some("the plan is ready".into());
    state.announced_final = true;
    state.usage = Usage::new(10, 4);
    let (head, rows) = state.split();
    assert_eq!(head.artifacts, state.artifacts);
    assert_eq!(head.plan, state.plan);
    assert_eq!(head.announced, state.announced);
    assert_eq!(head.usage, state.usage);
    assert_eq!(Conversation::join(head, rows).unwrap(), state);
    assert_round_trips(&state);
}

#[test]
fn join_replaces_the_head_messages_instead_of_appending_to_them() {
    let mut head = Conversation::default();
    // A head with a message in it is not what the runtime writes, but join must not depend on
    // that: the rows are the transcript, and they replace whatever the head held.
    head.messages.push(Message::user_text("stale"));
    let rows = vec![
        serde_json::to_value(Message::user_text("first")).unwrap(),
        serde_json::to_value(Message::assistant_text("second")).unwrap(),
    ];
    let joined = Conversation::join(head, rows).unwrap();
    assert_eq!(
        joined.messages,
        vec![
            Message::user_text("first"),
            Message::assistant_text("second")
        ],
        "the rows are the transcript, not an addition to it"
    );
}

#[test]
fn join_keeps_every_member_of_the_head_and_takes_the_rows_as_messages() {
    let head = Conversation {
        shortenings: vec![3],
        turns: 9,
        tool_calls: 4,
        usage: Usage::new(100, 20),
        ..Conversation::default()
    };
    let rows = vec![
        serde_json::to_value(Message::user_text("one")).unwrap(),
        serde_json::to_value(Message::assistant_text("two")).unwrap(),
    ];
    let joined = Conversation::join(head, rows).unwrap();
    assert_eq!(joined.shortenings, vec![3]);
    assert_eq!((joined.turns, joined.tool_calls), (9, 4));
    assert_eq!(joined.usage.input_tokens, 100);
    assert_eq!(joined.messages.len(), 2);
}

#[test]
fn the_round_trip_is_the_identity_over_every_shape() {
    // The same invariant, stated once over all the shapes above: an agent's split is only useful
    // if the runtime's reconstruction check can trust it for any state the agent commits.
    let compacted = Conversation {
        messages: vec![
            Message::user_text("go"),
            Message::Tool {
                call_id: "c".into(),
                content: format!("{TRUNCATION_MARKER_PREFIX} long output"),
                is_error: false,
                images: Vec::new(),
            },
        ],
        shortenings: vec![1],
        ..Conversation::default()
    };
    let waiting = Conversation {
        pending_wait: Some(PendingWait::Question(PendingQuestion {
            call_id: "q".into(),
            tool: "ask_user".into(),
            question: "which?".into(),
            ui: None,
            stream: None,
        })),
        ..Conversation::default()
    };
    let artifact = Conversation {
        artifacts: vec![ArtifactRef {
            id: None,
            name: "report".into(),
            mime_type: None,
            bytes: Some(12),
        }],
        ..Conversation::default()
    };
    for state in [
        Conversation::default(),
        Conversation::new("one"),
        compacted,
        waiting,
        artifact,
    ] {
        assert_round_trips(&state);
    }
}

#[test]
fn the_starter_delegates_to_the_conversation() {
    // A wrapper (or a start-only front) that forgets these methods leaves its runs in the v4
    // format, so the starter must split exactly as the agent does.
    let starter = LlmStarter::new("a");
    let state = Conversation::new("hello");
    let (head, rows) = AgentStarter::split(&starter, &state).expect("the starter splits");
    assert_eq!(
        Conversation::join(head, rows).unwrap(),
        state,
        "the starter's join is the conversation's"
    );
}

#[test]
fn the_thinking_cut_count_survives_the_round_trip() {
    let mut state = Conversation::new("hello");
    state.thinking_cuts = 2;
    let (head, rows) = state.split();
    assert_eq!(head.thinking_cuts, 2, "the head keeps the count");
    let joined = Conversation::join(head, rows).unwrap();
    assert_eq!(joined.thinking_cuts, 2);
    assert_round_trips(&state);
}
