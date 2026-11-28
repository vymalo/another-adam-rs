//! One training sample per assistant turn (ADR 0087, slice E5): each is the request the model was
//! sent at that call, as far as the state still holds it, and its answer as the last message.
//!
//! The stored history is the run's **current** one. A compaction replaced the turns before it with
//! a marker, so only the answers of the calls made since the last compaction have their prefix in
//! the store; the earlier turns are counted ([`turns_lost_to_compaction`]), never rebuilt. A
//! shortening rewrites old tool results in place, so a sample made before it is marked not exact.
//!
//! **Which assistant message is which call.** Every model call (`Conversation::turns` counts them)
//! puts one assistant message in the history, so the answers are placed **from the end**: the
//! newest is the last call's, and so on back, which also skips the answers a continued run carries
//! from the run before it. The exceptions the state shows are handled: the assistant message the
//! loop writes for the reports of sessions (calls `report_*` of `subagent_read`, no model) is never
//! an answer; a turn cut while thinking whose message left the history is found by the
//! instruction the loop wrote after it; the closing turn's instruction, sent and not kept, is put
//! back in front of its answer; the read the loop added to an answer (`wait_<turn>`) marks it not
//! exact. An answer dropped because it predated a message (a lost commit, then a message) cannot be
//! seen: when the count does not add up, every sample of the run is marked not exact.

use adam_llm_agent::{
    CLOSING_TURN, Conversation, SUBAGENT_READ, THINKING_CUT_TURN, interface_language,
};
use adam_model::{ContentPart, Message};

use crate::convert::{chat_message, head_of, shape_for, tools_of};
use crate::{
    ChatMessage, ExportOptions, Record, RequestShape, RunMeta, Sample, ToolsSource, TrainOn,
};

/// How many records an export writes for a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Samples {
    /// One record per run, every assistant message a target (`train_on: "assistant"`).
    #[default]
    Run,
    /// One record per assistant turn since the last compaction, its answer the last message
    /// (`train_on: "last"`), with a [`Sample`].
    Turn,
}

/// The system prompt and tools a run recorded for the calls from `turn` on
/// (`Conversation::requests`, read from the store).
#[derive(Debug, Clone, PartialEq)]
pub struct TurnRequest {
    /// The first call that was sent them.
    pub turn: u32,
    /// What it was sent.
    pub shape: RequestShape,
}

/// The turns of `conversation` before its last compaction: their answers have no prefix left in the
/// store, so they have no sample. What a continued run carried from the runs before it is not
/// counted (it is theirs).
pub fn turns_lost_to_compaction(conversation: &Conversation) -> u32 {
    last_compaction(conversation)
}

/// The first call of the current prefix: the turn of the run's own last compaction, else 0.
fn last_compaction(conversation: &Conversation) -> u32 {
    conversation
        .compactions
        .iter()
        .rev()
        .find(|compaction| !compaction.carried)
        .map_or(0, |compaction| compaction.turn)
}

/// The estimated tokens of `record`: the characters of its system prompt, messages (text, tool
/// calls' names and arguments, reasoning) and tools (names, descriptions and parameter schemas as
/// JSON), divided by four and rounded up, the rule adam's own history estimate uses. An estimate:
/// a trainer's tokenizer counts otherwise.
pub fn estimated_tokens(record: &Record) -> u64 {
    let count = |text: &str| text.chars().count() as u64;
    let messages: u64 = record
        .messages
        .iter()
        .map(|message| match message {
            ChatMessage::System { content } | ChatMessage::User { content } => count(content),
            ChatMessage::Assistant {
                content,
                tool_calls,
                reasoning_content,
            } => {
                count(content)
                    + reasoning_content.as_deref().map_or(0, count)
                    + tool_calls
                        .iter()
                        .map(|call| count(&call.function.name) + count(&call.function.arguments))
                        .sum::<u64>()
            }
            ChatMessage::Tool { content, .. } => count(content),
        })
        .sum();
    let tools: u64 = record
        .tools
        .iter()
        .map(|tool| {
            count(&tool.function.name)
                + count(&tool.function.description)
                + count(&tool.function.parameters.to_string())
        })
        .sum();
    (messages + tools).div_ceil(4)
}

/// An assistant message the model wrote: not the one the loop writes for the reports of sessions
/// (only `report_*` calls of `subagent_read`, no words, no provider).
fn is_answer(message: &Message) -> bool {
    match message {
        Message::Assistant {
            content,
            tool_calls,
            provider,
            ..
        } => {
            let reports = !tool_calls.is_empty()
                && provider.is_none()
                && content.iter().all(|part| part.as_text().is_empty())
                && tool_calls
                    .iter()
                    .all(|call| call.name == SUBAGENT_READ && call.id.starts_with("report_"));
            !reports
        }
        _ => false,
    }
}

/// Whether `message` is the instruction the loop writes after a turn cut while thinking.
fn is_thinking_cut(message: &Message) -> bool {
    matches!(message, Message::User { content }
        if matches!(content.as_slice(), [ContentPart::Text { text }] if text == THINKING_CUT_TURN))
}

/// The read the loop added to the answer of `turn` (`wait_<turn>`), when it did: `Some(true)`
/// when its turn is `turn`, `Some(false)` when it names another (the placement is wrong).
fn added_wait(message: &Message, turn: u32) -> Option<bool> {
    message
        .tool_calls()
        .iter()
        .filter(|call| call.name == SUBAGENT_READ)
        .find_map(|call| call.id.strip_prefix("wait_"))
        .map(|n| n == turn.to_string())
}

/// One answer of the current prefix: its index in the history and its call.
struct Target {
    at: usize,
    turn: u32,
}

/// The answers of the calls made since the last compaction, oldest first, and whether the count
/// adds up (see the module's documentation).
fn targets(conversation: &Conversation) -> (Vec<Target>, bool) {
    let messages = &conversation.messages;
    let first = last_compaction(conversation);
    let mut turn = conversation.turns;
    let mut found = Vec::new();
    let mut at = messages.len();
    while turn > first && at > 0 {
        at -= 1;
        let message = &messages[at];
        if is_answer(message) {
            turn -= 1;
            found.push(Target { at, turn });
        } else if is_thinking_cut(message)
            && !matches!(
                at.checked_sub(1).map(|i| &messages[i]),
                Some(Message::Assistant { .. })
            )
        {
            // The cut turn's message left the history: the call was made, and has no answer.
            turn -= 1;
        }
    }
    let mut consistent = turn <= first;
    // A run that began from nothing and was never compacted: every answer is one of its calls.
    let from_nothing = conversation.continued_from.is_none()
        && conversation.omitted_turns == 0
        && conversation.compactions.is_empty();
    if from_nothing && messages[..at].iter().any(is_answer) {
        consistent = false;
    }
    found.reverse();
    (found, consistent)
}

/// The request recorded for `turn`: the entry with the latest `turn` at or before it, and whether it
/// is surely that call's. Before the first entry, the first one, not surely.
fn request_for(recorded: &[TurnRequest], turn: u32) -> Option<(&RequestShape, bool)> {
    match recorded.iter().rev().find(|entry| entry.turn <= turn) {
        Some(entry) => Some((&entry.shape, true)),
        None => recorded.first().map(|entry| (&entry.shape, false)),
    }
}

/// One record per answer of the calls made since the last compaction, oldest first: the system
/// prompt and tools recorded for that call ([`TurnRequest`], else the agent's
/// [current](ExportOptions::current) ones), the history before the answer, and the answer as the
/// last message (`train_on: "last"`). The run's facets, outcome, models and counts are on each.
///
/// Every sample is returned: [`ExportOptions::max_tokens`] is applied by
/// [`export_runs`](crate::export_runs) (with [`estimated_tokens`]). A run with no call since its
/// last compaction has none.
pub fn samples_of(
    run: &RunMeta,
    conversation: &Conversation,
    recorded: &[TurnRequest],
    options: &ExportOptions,
) -> Vec<Record> {
    let (targets, counted) = targets(conversation);
    if targets.is_empty() {
        return Vec::new();
    }
    // A read the loop added names the turn of its answer: one that names another turn says the
    // answers were placed wrong.
    let consistent = counted
        && !targets.iter().any(|target| {
            added_wait(&conversation.messages[target.at], target.turn) == Some(false)
        });
    let head = head_of(run, conversation, options);
    let history: Vec<ChatMessage> = conversation
        .messages
        .iter()
        .map(|message| chat_message(message, options))
        .collect();
    // The interface language is a turn of the request that the history does not keep, and it is
    // part of the context, which is never exported: the requests had it, the samples do not.
    let language = interface_language(&conversation.context).is_some();
    // The last entry may be stale for the calls after it when the last call recorded nothing.
    let last_unrecorded = conversation.request.is_none();
    let last_entry = recorded.last().map(|entry| entry.turn);
    let closing = closing_answers(conversation, &targets);
    let of = u32::try_from(targets.len()).unwrap_or(u32::MAX);
    targets
        .iter()
        .enumerate()
        .map(|(index, target)| {
            let message = &conversation.messages[target.at];
            let (shape, surely) = match request_for(recorded, target.turn) {
                Some((shape, surely)) => (Some(shape), surely),
                None => (None, false),
            };
            let (tools_source, shape) = shape_for(run, shape, options);
            let fresh = !(last_unrecorded && last_entry.is_some_and(|last| target.turn > last));
            let shortened_after = conversation
                .shortenings
                .iter()
                .any(|&turn| turn > target.turn);
            let wait = added_wait(message, target.turn);
            let (closing_turn, closing_sure) = closing(target.at);
            let exact = consistent
                && tools_source == ToolsSource::Recorded
                && surely
                && fresh
                && !shortened_after
                && !language
                && wait.is_none()
                && closing_sure;
            let mut messages = Vec::with_capacity(target.at + 3);
            if let Some(system) = shape.and_then(|shape| shape.system.as_deref()) {
                messages.push(ChatMessage::System {
                    content: options.clean(system),
                });
            }
            messages.extend_from_slice(&history[..target.at]);
            if closing_turn {
                messages.push(ChatMessage::User {
                    content: CLOSING_TURN.to_owned(),
                });
            }
            messages.push(history[target.at].clone());
            Record {
                tools_source,
                tools: tools_of(shape, options),
                train_on: TrainOn::Last,
                sample: Some(Sample {
                    turn: target.turn,
                    index: u32::try_from(index).unwrap_or(u32::MAX),
                    of,
                    exact,
                }),
                messages,
                ..head.clone()
            }
        })
        .collect()
}

/// Which answers were the closing turn's (asked with [`CLOSING_TURN`], which the history does not
/// keep): `(closing, sure)` for the answer at an index. With a limit hit, the closing answers are
/// among the answers without calls after the last tool result: the newest of them is the closing
/// turn's once the limit says it answered; the others may be (a run reopened after its closing
/// answer) and are not sure.
fn closing_answers(
    conversation: &Conversation,
    targets: &[Target],
) -> Box<dyn Fn(usize) -> (bool, bool)> {
    let Some(hit) = &conversation.limit else {
        return Box::new(|_| (false, true));
    };
    let after = conversation
        .messages
        .iter()
        .rposition(|message| matches!(message, Message::Tool { .. }))
        .map_or(0, |at| at + 1);
    let candidates: Vec<usize> = targets
        .iter()
        .map(|target| target.at)
        .filter(|&at| at >= after && conversation.messages[at].tool_calls().is_empty())
        .collect();
    let last = candidates.last().copied().filter(|_| hit.answered);
    Box::new(move |at| {
        if Some(at) == last {
            (true, true)
        } else {
            (false, !candidates.contains(&at))
        }
    })
}

#[cfg(test)]
mod tests {
    use adam_core::RunId;
    use adam_core::store::RunStatus;
    use adam_core::time::Timestamp;
    use adam_llm_agent::{Compaction, CompactionReason, LimitHit, LimitKind};
    use adam_model::{ProviderTurn, ToolCall, ToolSpec};
    use serde_json::json;

    use super::*;
    use crate::Pseudonyms;

    fn meta() -> RunMeta {
        let at = Timestamp::from_second(1_800_000_000).expect("an instant");
        RunMeta {
            id: RunId::new(),
            agent: "coder".to_owned(),
            conversation_id: Some("conv".to_owned()),
            parent_id: None,
            status: RunStatus::Done,
            created_at: at,
            updated_at: at,
            error: None,
            artifacts: Vec::new(),
        }
    }

    fn options() -> ExportOptions {
        ExportOptions::new(Pseudonyms::new(b"key"))
    }

    fn answer(text: &str, calls: Vec<ToolCall>) -> Message {
        Message::Assistant {
            content: vec![ContentPart::text(text)],
            tool_calls: calls,
            reasoning: None,
            provider: Some(ProviderTurn::new("main", "endpoint")),
            extra_content: None,
        }
    }

    fn read(id: &str) -> ToolCall {
        ToolCall::new(id, "read_file", json!({"path": format!("{id}.rs")}))
    }

    fn shape(system: &str, tools: &[&str]) -> RequestShape {
        RequestShape {
            system: Some(system.to_owned()),
            tools: tools
                .iter()
                .map(|name| ToolSpec {
                    name: (*name).to_owned(),
                    description: format!("the {name} tool"),
                    parameters: json!({"type": "object"}),
                })
                .collect(),
        }
    }

    /// Three calls: two that read a file each, and the answer.
    fn three_turns() -> Conversation {
        Conversation {
            messages: vec![
                Message::user_text("fix the bug"),
                answer("", vec![read("a")]),
                Message::tool_result("a", "fn a() {}"),
                answer("and b", vec![read("b")]),
                Message::tool_result("b", "fn b() {}"),
                answer("fixed", Vec::new()),
            ],
            turns: 3,
            tool_calls: 2,
            request: Some(adam_llm_agent::RecordedRequest {
                turn: 2,
                system: None,
                tools: adam_runtime::blob_id(b"[]"),
            }),
            ..Conversation::default()
        }
    }

    fn recorded() -> Vec<TurnRequest> {
        vec![
            TurnRequest {
                turn: 0,
                shape: shape("be brief", &["read_file"]),
            },
            TurnRequest {
                turn: 2,
                shape: shape("be brief", &["read_file", "late"]),
            },
        ]
    }

    fn roles(record: &Record) -> Vec<&'static str> {
        record
            .messages
            .iter()
            .map(|message| match message {
                ChatMessage::System { .. } => "system",
                ChatMessage::User { .. } => "user",
                ChatMessage::Assistant { .. } => "assistant",
                ChatMessage::Tool { .. } => "tool",
            })
            .collect()
    }

    /// Every call the prefix makes has its result in the prefix, and the target is the last message.
    fn pairs_hold(record: &Record) {
        let (target, prefix) = record.messages.split_last().expect("messages");
        assert!(
            matches!(target, ChatMessage::Assistant { .. }),
            "{target:?}"
        );
        for (i, message) in prefix.iter().enumerate() {
            if let ChatMessage::Assistant { tool_calls, .. } = message {
                for call in tool_calls {
                    assert!(
                        prefix[i + 1..].iter().any(|m| matches!(
                            m,
                            ChatMessage::Tool { tool_call_id, .. } if *tool_call_id == call.id
                        )),
                        "the call {} has no result in the prefix",
                        call.id
                    );
                }
            }
        }
    }

    #[test]
    fn each_answer_is_a_sample_with_its_prefix_and_its_turns_tools() {
        let samples = samples_of(&meta(), &three_turns(), &recorded(), &options());
        assert_eq!(samples.len(), 3);
        let expected = [
            vec!["system", "user", "assistant"],
            vec!["system", "user", "assistant", "tool", "assistant"],
            vec![
                "system",
                "user",
                "assistant",
                "tool",
                "assistant",
                "tool",
                "assistant",
            ],
        ];
        for (n, (sample, roles_of)) in samples.iter().zip(expected).enumerate() {
            assert_eq!(roles(sample), roles_of);
            pairs_hold(sample);
            assert_eq!(sample.train_on, TrainOn::Last);
            assert_eq!(sample.tools_source, ToolsSource::Recorded);
            let info = sample.sample.expect("a sample");
            assert_eq!((info.turn, info.index, info.of), (n as u32, n as u32, 3));
            assert!(info.exact, "sample {n} is exact");
            assert_eq!(sample.turns, 3, "the run's own counts");
        }
        assert!(matches!(
            samples[2].messages.last(),
            Some(ChatMessage::Assistant { content, .. }) if content == "fixed"
        ));
        // The tools of turns 0 and 1 are the first entry's; turn 2 is sent `late` too.
        let names = |record: &Record| -> Vec<String> {
            record
                .tools
                .iter()
                .map(|t| t.function.name.clone())
                .collect()
        };
        assert_eq!(names(&samples[0]), ["read_file"]);
        assert_eq!(names(&samples[1]), ["read_file"]);
        assert_eq!(names(&samples[2]), ["read_file", "late"]);
        assert!(matches!(
            &samples[0].messages[0],
            ChatMessage::System { content } if content == "be brief"
        ));
    }

    #[test]
    fn without_a_recorded_request_no_sample_is_exact() {
        let samples = samples_of(&meta(), &three_turns(), &[], &options());
        assert_eq!(samples.len(), 3);
        for sample in &samples {
            assert_eq!(sample.tools_source, ToolsSource::None);
            assert!(sample.tools.is_empty());
            assert!(!sample.sample.expect("a sample").exact);
        }
        let current = samples_of(
            &meta(),
            &three_turns(),
            &[],
            &options().current("coder", shape("now", &["read_file"])),
        );
        assert_eq!(current[0].tools_source, ToolsSource::Current);
        assert!(!current[0].sample.expect("a sample").exact);
    }

    #[test]
    fn a_compacted_run_starts_at_the_marker_and_counts_what_it_lost() {
        // Five calls; the compaction before call 3 kept the answer of call 2 verbatim.
        let marker = Message::User {
            content: vec![
                ContentPart::text("fix the bug"),
                ContentPart::text("[earlier turns summarised]\nread a and b"),
            ],
        };
        let conversation = Conversation {
            messages: vec![
                marker,
                answer("", vec![read("c")]),
                Message::tool_result("c", "fn c() {}"),
                answer("", vec![read("d")]),
                Message::tool_result("d", "fn d() {}"),
                answer("fixed", Vec::new()),
            ],
            turns: 5,
            compactions: vec![Compaction {
                turn: 3,
                dropped_turns: 2,
                reason: CompactionReason::Budget,
                summarised: true,
                sessions: false,
                rejected: None,
                skills: Vec::new(),
                pointers: Vec::new(),
                pinned: Vec::new(),
                carried: false,
            }],
            omitted_turns: 2,
            summary: Some("read a and b".to_owned()),
            ..three_turns()
        };
        assert_eq!(turns_lost_to_compaction(&conversation), 3);
        let samples = samples_of(&meta(), &conversation, &recorded(), &options());
        let turns: Vec<u32> = samples
            .iter()
            .map(|s| s.sample.expect("a sample").turn)
            .collect();
        assert_eq!(turns, [3, 4], "only the calls since the compaction");
        for sample in &samples {
            assert!(sample.sample.expect("a sample").exact);
            assert!(sample.compacted, "the run's flag is kept");
            pairs_hold(sample);
            assert!(matches!(
                &sample.messages[1],
                ChatMessage::User { content } if content.contains("[earlier turns summarised]")
            ));
        }
        // The answer kept from call 2 is context, not a target.
        assert_eq!(
            roles(&samples[0]),
            ["system", "user", "assistant", "tool", "assistant"]
        );
    }

    #[test]
    fn a_later_shortening_makes_the_earlier_samples_inexact() {
        let conversation = Conversation {
            shortenings: vec![2],
            ..three_turns()
        };
        let exact: Vec<bool> = samples_of(&meta(), &conversation, &recorded(), &options())
            .iter()
            .map(|s| s.sample.expect("a sample").exact)
            .collect();
        assert_eq!(
            exact,
            [false, false, true],
            "call 2 saw the shortened history"
        );
    }

    #[test]
    fn the_reports_message_is_context_and_a_cut_turn_still_counts() {
        let reports = Message::Assistant {
            content: Vec::new(),
            tool_calls: vec![ToolCall::new(
                "report_0123",
                SUBAGENT_READ,
                json!({"session": "s"}),
            )],
            reasoning: None,
            provider: None,
            extra_content: None,
        };
        let conversation = Conversation {
            messages: vec![
                Message::user_text("fix the bug"),
                answer("", vec![read("a")]),
                Message::tool_result("a", "fn a() {}"),
                // Call 1 was cut while thinking and left nothing; the loop asked again.
                Message::user_text(THINKING_CUT_TURN),
                reports,
                Message::tool_result("report_0123", "the session's report"),
                answer("fixed", Vec::new()),
            ],
            turns: 3,
            ..three_turns()
        };
        let samples = samples_of(&meta(), &conversation, &recorded(), &options());
        let turns: Vec<u32> = samples
            .iter()
            .map(|s| s.sample.expect("a sample").turn)
            .collect();
        assert_eq!(turns, [0, 2]);
        assert!(samples.iter().all(|s| s.sample.expect("a sample").exact));
        pairs_hold(&samples[1]);
        // An answer too many for the calls: the placement is a guess.
        let too_many = Conversation {
            turns: 2,
            ..three_turns()
        };
        let samples = samples_of(&meta(), &too_many, &recorded(), &options());
        assert_eq!(samples.len(), 2);
        assert!(samples.iter().all(|s| !s.sample.expect("a sample").exact));
    }

    #[test]
    fn the_closing_answer_is_asked_with_the_closing_instruction() {
        let conversation = Conversation {
            limit: Some(LimitHit {
                limit: LimitKind::MaxTurns,
                answered: true,
                answer: None,
            }),
            ..three_turns()
        };
        let samples = samples_of(&meta(), &conversation, &recorded(), &options());
        let last = samples.last().expect("samples");
        let [.., closing, _answer] = last.messages.as_slice() else {
            panic!("messages: {:?}", last.messages);
        };
        assert_eq!(
            closing,
            &ChatMessage::User {
                content: CLOSING_TURN.to_owned()
            }
        );
        assert!(last.sample.expect("a sample").exact);
        assert!(
            !samples[1]
                .messages
                .iter()
                .any(|m| matches!(m, ChatMessage::User { content } if content == CLOSING_TURN))
        );
    }

    #[test]
    fn a_read_the_loop_added_marks_its_answer_or_the_run_inexact() {
        let wait =
            |id: &str| ToolCall::new(id, SUBAGENT_READ, json!({"session": "s", "wait": true}));
        let with_wait = |id: &str| {
            let mut conversation = three_turns();
            conversation.messages[3] = answer("and b", vec![read("b"), wait(id)]);
            conversation
                .messages
                .insert(5, Message::tool_result(id, "the session's report"));
            conversation
        };
        // The read names the answer's own turn: that sample only is not exact.
        let exact: Vec<bool> = samples_of(&meta(), &with_wait("wait_1"), &recorded(), &options())
            .iter()
            .map(|s| s.sample.expect("a sample").exact)
            .collect();
        assert_eq!(exact, [true, false, true]);
        // It names another turn: the answers were placed wrong, and no sample is exact.
        let samples = samples_of(&meta(), &with_wait("wait_7"), &recorded(), &options());
        assert!(samples.iter().all(|s| !s.sample.expect("a sample").exact));
    }

    #[test]
    fn the_estimate_is_a_quarter_of_the_characters() {
        let samples = samples_of(&meta(), &three_turns(), &recorded(), &options());
        let first = &samples[0];
        let chars = "be brief".len()
            + "fix the bug".len()
            + "read_file".len()
            + r#"{"path":"a.rs"}"#.len()
            + "read_file".len()
            + "the read_file tool".len()
            + r#"{"type":"object"}"#.len();
        assert_eq!(estimated_tokens(first), (chars as u64).div_ceil(4));
        assert!(estimated_tokens(&samples[2]) > estimated_tokens(first));
    }
}
