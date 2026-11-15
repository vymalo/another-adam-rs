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

/// `content` shortened by at least `excess` characters, keeping its head and ending in the
/// truncation marker ([`TRUNCATION_MARKER_PREFIX`]); `None` when that would not make it shorter
/// (it is already about as short as the marker, or already nothing but a marker, whose count of
/// what was omitted is not to be overwritten by the marker's own length). Both [`record_shortenings`] and
/// the history a continued run carries shorten tool outputs this way.
pub(crate) fn shorten_output(content: &str, excess: usize) -> Option<String> {
    if content
        .strip_prefix('\n')
        .is_some_and(|rest| rest.starts_with(TRUNCATION_MARKER_PREFIX))
    {
        return None;
    }
    let len = content.chars().count();
    // The marker is at most this long for any smaller `removed`.
    let marker_max = marker(len).chars().count();
    if len <= marker_max {
        return None;
    }
    let removed = len.min(excess.saturating_add(marker_max));
    let mut shortened: String = content.chars().take(len - removed).collect();
    shortened.push_str(&marker(removed));
    Some(shortened)
}

/// Characters a message contributes to the estimate.
fn message_chars(m: &Message) -> usize {
    match m {
        Message::User { content } => content.iter().map(|p| p.as_text().chars().count()).sum(),
        Message::Assistant {
            content,
            tool_calls,
            reasoning,
        } => {
            // The reasoning a client echoes is sent, so it counts.
            reasoning.as_deref().map_or(0, |r| r.chars().count())
                + content
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

/// Shorten old tool outputs **in place** when the history is over `max_tokens` (estimated as
/// chars / 4), down to three quarters of the budget, and say whether anything was shortened; `0`
/// means no limit.
///
/// The history is what the next request starts with, so a shortening is written into the state
/// once (and the turn recorded in [`Conversation::shortenings`](crate::Conversation::shortenings)),
/// and the requests after it extend it unchanged: cutting only the copy that is sent would cut
/// again on every call and move the cached prefix each time. Going below the budget, not to it,
/// is what makes that rare.
///
/// * Only tool outputs are shortened, oldest first, and only as much as needed. Each keeps its
///   head and gets an explicit marker ([`TRUNCATION_MARKER_PREFIX`]): truncated, never dropped, so
///   the call/result pairing the model API requires stays intact.
/// * The tool outputs after the last assistant message (the results the model has not seen yet)
///   are never touched.
/// * If that is not enough, the rest is sent as is: better an over-long prompt (which the model
///   may reject, failing the run with a clear error) than silently losing conversation.
///
/// A pure function of `messages` and `max_tokens`, so a replay of a turn from the committed state
/// shortens the same way.
pub(crate) fn record_shortenings(messages: &mut [Message], max_tokens: u32) -> bool {
    if max_tokens == 0 {
        return false;
    }
    let budget = usize::try_from(max_tokens)
        .unwrap_or(usize::MAX)
        .saturating_mul(4);
    let mut total: usize = messages.iter().map(message_chars).sum();
    if total <= budget {
        return false;
    }
    let target = budget - budget / 4;
    let protected_from = messages
        .iter()
        .rposition(|m| matches!(m, Message::Assistant { .. }))
        .map_or(0, |i| i + 1);
    let mut shortened_any = false;
    for m in &mut messages[..protected_from] {
        if total <= target {
            break;
        }
        let Message::Tool { content, .. } = m else {
            continue;
        };
        let len = content.chars().count();
        let Some(shortened) = shorten_output(content, total - target) else {
            continue; // would not get shorter
        };
        total = total - len + shortened.chars().count();
        *content = shortened;
        shortened_any = true;
    }
    shortened_any
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_model::ToolCall;
    use serde_json::json;

    /// `messages` after [`record_shortenings`], as a copy.
    fn fit_history(messages: &[Message], max_tokens: u32) -> Vec<Message> {
        let mut out = messages.to_vec();
        record_shortenings(&mut out, max_tokens);
        out
    }

    fn call(id: &str) -> Message {
        Message::Assistant {
            content: vec![],
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "t".into(),
                arguments: json!({}),
                ..Default::default()
            }],
            reasoning: None,
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
    fn shorten_output_keeps_the_head_removes_at_least_the_excess_and_never_grows() {
        let text = "h".repeat(1_000);
        let shorter = shorten_output(&text, 300).unwrap();
        assert!(shorter.starts_with("hhh") && shorter.contains(TRUNCATION_MARKER_PREFIX));
        assert!(shorter.chars().count() <= 1_000 - 300);
        // More than it has to give: only the marker is left.
        let floor = shorten_output(&text, 10_000).unwrap();
        assert!(floor.starts_with('\n') && floor.contains(TRUNCATION_MARKER_PREFIX));
        assert!(floor.chars().count() < 1_000);
        // Already as short as a marker: not touched again.
        assert_eq!(shorten_output("short", 3), None);
        assert_eq!(shorten_output(&floor, 10), None);
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
        // ~3000 tokens of content, budget 2750 tokens = 11000 chars.
        let fit = fit_history(&m, 2750);
        assert_eq!(fit.len(), m.len(), "nothing is dropped");
        assert!(
            estimate_tokens(&fit) <= 2750 * 3 / 4 + 10,
            "down to three quarters"
        );
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
    fn it_says_whether_it_shortened_and_does_not_shorten_twice() {
        let mut m = convo();
        assert!(!record_shortenings(&mut m.clone(), 0));
        assert!(!record_shortenings(&mut m.clone(), 10_000));
        assert!(record_shortenings(&mut m, 2750));
        let once = m.clone();
        // Under three quarters of the budget now: the next call has nothing to do, and the
        // history is what it was (the requests after a shortening extend it unchanged).
        assert!(!record_shortenings(&mut m, 2750));
        assert_eq!(m, once);
        // Over budget with nothing left to shorten is not a shortening either.
        let mut floor = convo();
        assert!(record_shortenings(&mut floor, 10));
        assert!(!record_shortenings(&mut floor, 10));
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
                            ..Default::default()
                        })
                        .collect(),
                    reasoning: None,
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

                // A second pass has nothing to do: the next request extends this history as it is.
                let mut again = fitted.clone();
                prop_assert!(!record_shortenings(&mut again, max_tokens));
                prop_assert_eq!(&again, &fitted);

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
