//! Fitting the history into the model's context budget.

use adam_model::Message;

/// Prefix of the marker appended to a truncated tool output. Public through
/// the crate root so callers and tests can recognise truncation.
pub const TRUNCATION_MARKER_PREFIX: &str = "[truncated:";

fn marker(removed: usize) -> String {
    format!(
        "\n{TRUNCATION_MARKER_PREFIX} {removed} chars of tool output omitted to fit the history limit]"
    )
}

/// Characters a message contributes to the estimate.
fn message_chars(m: &Message) -> usize {
    match m {
        Message::User { content } => content.iter().map(|p| p.as_text().chars().count()).sum(),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            content
                .iter()
                .map(|p| p.as_text().chars().count())
                .sum::<usize>()
                + tool_calls
                    .iter()
                    .map(|c| c.name.chars().count() + c.arguments.to_string().chars().count())
                    .sum::<usize>()
        }
        Message::Tool { content, .. } => content.chars().count(),
    }
}

#[cfg(test)]
/// Estimated tokens of `messages`: total characters divided by 4, rounded up.
fn estimate_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(message_chars)
        .sum::<usize>()
        .div_ceil(4)
}

/// A copy of `messages` that fits `max_tokens` (estimated as chars / 4), by
/// truncating old tool outputs; `0` means no limit.
///
/// * Only tool outputs are shortened, oldest first, and only as much as
///   needed. Each keeps its head and gets an explicit marker
///   ([`TRUNCATION_MARKER_PREFIX`]): truncated, never dropped, so the call/result
///   pairing the model API requires stays intact.
/// * The tool outputs after the last assistant message (the results the model
///   has not seen yet) are never touched.
/// * If that is not enough, the rest is sent as is: better an over-long
///   prompt (which the model may reject, failing the run with a clear error)
///   than silently losing conversation.
pub(crate) fn fit_history(messages: &[Message], max_tokens: u32) -> Vec<Message> {
    let mut out = messages.to_vec();
    if max_tokens == 0 {
        return out;
    }
    let budget = usize::try_from(max_tokens)
        .unwrap_or(usize::MAX)
        .saturating_mul(4);
    let mut total: usize = out.iter().map(message_chars).sum();
    if total <= budget {
        return out;
    }
    let protected_from = out
        .iter()
        .rposition(|m| matches!(m, Message::Assistant { .. }))
        .map_or(0, |i| i + 1);
    for m in &mut out[..protected_from] {
        if total <= budget {
            break;
        }
        let Message::Tool { content, .. } = m else {
            continue;
        };
        let len = content.chars().count();
        // The marker is at most this long for any smaller `removed`.
        let marker_max = marker(len).chars().count();
        if len <= marker_max {
            continue; // would not get shorter
        }
        let excess = total - budget;
        let removed = len.min(excess + marker_max);
        let mut shortened: String = content.chars().take(len - removed).collect();
        shortened.push_str(&marker(removed));
        total = total - len + shortened.chars().count();
        *content = shortened;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_model::ToolCall;
    use serde_json::json;

    fn call(id: &str) -> Message {
        Message::Assistant {
            content: vec![],
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "t".into(),
                arguments: json!({}),
            }],
        }
    }

    fn big(id: &str, n: usize, ch: char) -> Message {
        Message::tool_result(id, ch.to_string().repeat(n))
    }

    fn convo() -> Vec<Message> {
        vec![
            Message::user_text("go"),
            call("c1"),
            big("c1", 4000, 'a'),
            call("c2"),
            big("c2", 4000, 'b'),
            call("c3"),
            big("c3", 4000, 'c'),
        ]
    }

    #[test]
    fn under_budget_or_unlimited_is_untouched() {
        let m = convo();
        assert_eq!(fit_history(&m, 0), m);
        assert_eq!(fit_history(&m, 10_000), m);
    }

    #[test]
    fn truncates_oldest_first_keeps_newest_and_marks() {
        let m = convo();
        // ~3000 tokens of content, budget 2500 tokens = 10000 chars.
        let fit = fit_history(&m, 2500);
        assert_eq!(fit.len(), m.len(), "nothing is dropped");
        assert!(estimate_tokens(&fit) <= 2500);
        // Newest results intact.
        assert_eq!(fit[6], m[6]);
        assert_eq!(fit[4], m[4]);
        // The oldest one carries the cut, with a marker.
        let oldest = fit[2].text();
        assert!(oldest.len() < m[2].text().len());
        assert!(oldest.contains(TRUNCATION_MARKER_PREFIX));
        assert!(oldest.starts_with("aaaa"));
        // Non-tool messages and call ids are preserved.
        assert_eq!(fit[0], m[0]);
        assert_eq!(fit[1], m[1]);
        assert!(
            matches!(&fit[2], Message::Tool { call_id, is_error: false, .. } if call_id == "c1")
        );
    }

    #[test]
    fn truncates_further_back_only_as_needed() {
        let m = convo();
        // Budget forces both older outputs down but never the newest.
        let fit = fit_history(&m, 1300);
        assert_eq!(fit[6], m[6]);
        assert!(fit[2].text().contains(TRUNCATION_MARKER_PREFIX));
        assert!(fit[4].text().contains(TRUNCATION_MARKER_PREFIX));
        assert!(estimate_tokens(&fit) <= 1300 + 10);
    }

    #[test]
    fn newest_batch_is_never_truncated_even_if_over_budget() {
        let m = convo();
        let fit = fit_history(&m, 10);
        assert_eq!(fit[6], m[6], "latest output survives an impossible budget");
        assert_eq!(fit.len(), m.len());
    }

    #[test]
    fn error_flag_survives_and_multibyte_is_safe() {
        let m = vec![
            Message::user_text("go"),
            call("c1"),
            Message::tool_error("c1", "é".repeat(2000)),
            call("c2"),
            big("c2", 10, 'x'),
        ];
        let fit = fit_history(&m, 200);
        assert!(matches!(&fit[2], Message::Tool { is_error: true, .. }));
        assert!(fit[2].text().contains(TRUNCATION_MARKER_PREFIX));
    }

    #[test]
    fn estimate_counts_calls_and_text() {
        let m = vec![Message::user_text("abcd"), call("c1")];
        // "abcd" + name "t" + "{}" = 4 + 1 + 2 = 7 chars -> 2 tokens.
        assert_eq!(estimate_tokens(&m), 2);
    }

    mod prop {
        use proptest::collection::vec;
        use proptest::prelude::*;

        use super::*;

        fn arb_message() -> impl Strategy<Value = Message> {
            let text = "\\PC{0,120}";
            prop_oneof![
                text.prop_map(Message::user_text),
                text.prop_map(Message::assistant_text),
                ("[a-z]{1,3}", 0usize..3, text).prop_map(|(id, n, args)| Message::Assistant {
                    content: vec![],
                    tool_calls: (0..n)
                        .map(|i| ToolCall {
                            id: format!("{id}{i}"),
                            name: "tool".into(),
                            arguments: json!({ "x": args }),
                        })
                        .collect(),
                }),
                ("[a-z]{1,3}", "\\PC{0,700}", any::<bool>()).prop_map(|(id, out, err)| {
                    if err {
                        Message::tool_error(id, out)
                    } else {
                        Message::tool_result(id, out)
                    }
                }),
            ]
        }

        fn total(messages: &[Message]) -> usize {
            messages.iter().map(message_chars).sum()
        }

        proptest! {
            /// Fitting never drops, reorders or rewrites anything but tool
            /// outputs; a rewritten output keeps its head and says so; the
            /// results after the last assistant message are untouched; the
            /// result is never longer than the input, and fits the budget
            /// unless nothing shortenable is left.
            #[test]
            fn prop_fit_history_keeps_count_order_and_budget(
                messages in vec(arb_message(), 0..14),
                max_tokens in 0u32..1200,
            ) {
                let fitted = fit_history(&messages, max_tokens);
                prop_assert_eq!(fitted.len(), messages.len());
                prop_assert!(total(&fitted) <= total(&messages));

                let protected_from = messages
                    .iter()
                    .rposition(|m| matches!(m, Message::Assistant { .. }))
                    .map_or(0, |i| i + 1);
                for (i, (before, after)) in messages.iter().zip(&fitted).enumerate() {
                    match (before, after) {
                        (
                            Message::Tool { call_id: a, content: old, is_error: ea },
                            Message::Tool { call_id: b, content: new, is_error: eb },
                        ) => {
                            prop_assert_eq!(a, b, "tool call ids are kept");
                            prop_assert_eq!(ea, eb);
                            if old != new {
                                prop_assert!(i < protected_from, "unseen result touched");
                                prop_assert!(max_tokens > 0);
                                prop_assert!(new.contains(TRUNCATION_MARKER_PREFIX));
                                let head = new.split(&format!("\n{TRUNCATION_MARKER_PREFIX}")).next().unwrap();
                                prop_assert!(old.starts_with(head), "the head is kept");
                            }
                        }
                        _ => prop_assert_eq!(before, after, "only tool outputs may change"),
                    }
                }

                if max_tokens == 0 {
                    prop_assert_eq!(&fitted, &messages);
                } else if total(&fitted) > usize::try_from(max_tokens).unwrap() * 4 {
                    // Over budget: every candidate was already as short as it
                    // can get (rewritten, or not longer than its own marker).
                    for (i, (before, after)) in messages.iter().zip(&fitted).enumerate() {
                        if let (
                            Message::Tool { content: old, .. },
                            Message::Tool { content: new, .. },
                        ) = (before, after)
                            && i < protected_from
                            && old == new
                        {
                            let len = old.chars().count();
                            prop_assert!(len <= marker(len).chars().count());
                        }
                    }
                }
            }
        }
    }
}
