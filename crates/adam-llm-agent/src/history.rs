//! Fitting the history into the model's context budget.

use adam_model::Message;

/// Prefix of the marker appended to a truncated tool output. Public through
/// the crate root so callers and tests can recognise truncation.
pub const TRUNCATION_MARKER_PREFIX: &str = "[truncated:";

/// What the marker says after the count: one deterministic sentence, so a cut says how to get more
/// of what it removed.
const MARKER_TAIL: &str = " chars of tool output omitted to fit the history limit; a kept output is read back with read_output, a file with its read tool]";

/// A tool result shorter than this many characters is never shortened: small structured results
/// hold ids nothing else repeats (a session's, a plan's, a shared file's), and they are not what
/// fills a history.
pub const SHORTENING_FLOOR: usize = 512;

fn marker(removed: usize) -> String {
    format!("\n{TRUNCATION_MARKER_PREFIX} {removed}{MARKER_TAIL}")
}

/// An output as a shortening left it, or as it was made: what is kept of the output, how many
/// characters earlier cuts removed, and the pointer line a cut moved after its marker.
struct Cut<'a> {
    head: &'a str,
    /// What the marker at the end says was omitted: 0 when there is none.
    omitted: usize,
    /// The pointer line that follows the marker ([`crate::read_pointer`]), when a cut moved it.
    moved_pointer: Option<&'a str>,
    /// The marker and what follows it, in characters.
    tail_chars: usize,
}

fn parse(content: &str) -> Cut<'_> {
    let whole = Cut {
        head: content,
        omitted: 0,
        moved_pointer: None,
        tail_chars: 0,
    };
    let key = format!("\n{TRUNCATION_MARKER_PREFIX} ");
    let Some(at) = content.rfind(&key) else {
        return whole;
    };
    let after = &content[at + key.len()..];
    let figures = after.bytes().take_while(u8::is_ascii_digit).count();
    let Ok(omitted) = after[..figures].parse::<usize>() else {
        return whole;
    };
    let Some(rest) = after[figures..].strip_prefix(MARKER_TAIL) else {
        return whole;
    };
    let moved_pointer = match rest {
        "" => None,
        line => match line.strip_prefix('\n') {
            Some(line) if !line.contains('\n') && crate::kept::read_pointer(line) == Some(line) => {
                Some(line)
            }
            _ => return whole,
        },
    };
    Cut {
        head: &content[..at],
        omitted,
        moved_pointer,
        tail_chars: content[at..].chars().count(),
    }
}

/// `content` shortened by at least `excess` characters, keeping its head and ending in a marker
/// ([`TRUNCATION_MARKER_PREFIX`]); `None` when that would not make it shorter, or when it is under
/// [`SHORTENING_FLOOR`]. Both [`record_shortenings`] and the history a continued run carries
/// shorten tool outputs this way.
///
/// * **The count is cumulative.** An output that ends with a marker is cut from its head, and the
///   new marker says the old count plus what this cut removed, so the marker always says how much
///   of the original is missing (a pointer line, which stays, is not counted).
/// * **The pointer stays.** A line that tells the model how to read a kept output back
///   ([`crate::read_pointer`]) is put after the marker when the cut removes it, so `read_output`
///   stays reachable whatever the cut.
pub(crate) fn shorten_output(content: &str, excess: usize) -> Option<String> {
    let len = content.chars().count();
    if len < SHORTENING_FLOOR {
        return None;
    }
    let cut = parse(content);
    let head_chars = cut.head.chars().count();
    if head_chars == 0 {
        return None; // nothing but a marker (and its pointer)
    }
    let pointer = cut
        .moved_pointer
        .or_else(|| crate::kept::read_pointer(content));
    let pointer_chars = pointer.map_or(0, |p| p.chars().count() + 1);
    // The marker is at most this long, for any count up to what the whole output holds.
    let marker_max = marker(cut.omitted.saturating_add(len)).chars().count();
    let wanted = excess
        .saturating_add(marker_max)
        .saturating_add(pointer_chars)
        .saturating_sub(cut.tail_chars);
    let mut keep = head_chars.saturating_sub(wanted);
    // The pointer line was moved by an earlier cut, or sits in the head, or is not there.
    let mut relocated = cut.moved_pointer.is_some();
    let mut moved_chars = 0;
    if let Some(line) = pointer.filter(|_| cut.moved_pointer.is_none())
        && let Some(byte) = cut.head.find(line)
    {
        let start = cut.head[..byte].chars().count();
        let end = start + line.chars().count();
        if keep < end {
            // A cut through the line takes all of it: the line is shown once, after the marker.
            keep = keep.min(start);
            relocated = true;
            moved_chars = end - start;
        }
    }
    let kept_head: String = cut.head.chars().take(keep).collect();
    let removed = head_chars - keep;
    let mut shortened = kept_head;
    shortened.push_str(&marker(cut.omitted + removed - moved_chars));
    if relocated && let Some(line) = pointer {
        shortened.push('\n');
        shortened.push_str(line);
    }
    (shortened.chars().count() < len).then_some(shortened)
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

    const KEPT_LINE: &str = "[kept as o-0123456789abcdef: 7000 lines; read_output {\"id\":\"o-0123456789abcdef\"} reads it]";

    /// What a marked output still says: the head, the count, a pointer moved after the marker.
    fn account(text: &str) -> (usize, usize, usize) {
        let cut = parse(text);
        assert_eq!(
            text.matches(TRUNCATION_MARKER_PREFIX).count(),
            1,
            "exactly one marker: {text:?}"
        );
        (
            cut.head.chars().count(),
            cut.omitted,
            cut.moved_pointer.map_or(0, |p| p.chars().count()),
        )
    }

    #[test]
    fn a_second_cut_adds_to_the_count_of_the_first() {
        let original = "h".repeat(100_000);
        let once = shorten_output(&original, 86_000).unwrap();
        let (head, omitted, _) = account(&once);
        assert_eq!(head + omitted, 100_000);
        // The output grew by nothing, and is cut again: the marker says what both cuts removed.
        let twice = shorten_output(&once, 5_000).unwrap();
        let (head2, omitted2, _) = account(&twice);
        assert_eq!(head2 + omitted2, 100_000, "{twice:?}");
        assert!(omitted2 > omitted && head2 < head);
        // Down to the marker alone, the count still names everything that is gone.
        let floor = shorten_output(&twice, usize::MAX / 4).unwrap();
        let (head3, omitted3, _) = account(&floor);
        assert_eq!((head3, omitted3), (0, 100_000));
        assert_eq!(shorten_output(&floor, 10), None);
    }

    #[test]
    fn a_result_under_the_floor_is_never_shortened() {
        let small = format!("{{\"session\":\"s\",\"pad\":\"{}\"}}", "p".repeat(300));
        assert!(small.chars().count() < SHORTENING_FLOOR);
        assert_eq!(shorten_output(&small, 10_000), None);
        let edge = "e".repeat(SHORTENING_FLOOR - 1);
        assert_eq!(shorten_output(&edge, 10_000), None);
        assert!(shorten_output(&"e".repeat(SHORTENING_FLOOR + 600), 400).is_some());
        // A history far over budget leaves such a result alone.
        let mut m = vec![
            Message::user_text("go"),
            call("c1"),
            Message::tool_result("c1", small.clone()),
            call("c2"),
            big("c2", 50_000, 'z'),
            call("c3"),
            big("c3", 10, 'y'),
        ];
        assert!(record_shortenings(&mut m, 100));
        assert_eq!(m[2].text(), small);
    }

    #[test]
    fn the_marker_says_how_to_get_more() {
        let cut = shorten_output(&"m".repeat(2_000), 100).unwrap();
        assert!(
            cut.ends_with("omitted to fit the history limit; a kept output is read back with read_output, a file with its read tool]"),
            "{cut}"
        );
    }

    #[test]
    fn the_pointer_to_a_kept_output_outlives_any_cut() {
        let original = format!("{KEPT_LINE}\n{}", "row-000001\n".repeat(2_000));
        let mut text = original.clone();
        // A shallow cut keeps the line where it is; deeper ones move it after the marker, once.
        for excess in [200, 5_000, 30_000, usize::MAX / 4] {
            text = shorten_output(&text, excess).unwrap_or(text);
            assert_eq!(
                crate::kept::read_pointer(&text),
                Some(KEPT_LINE),
                "{text:?}"
            );
            assert_eq!(text.matches("read_output {").count(), 1, "{text:?}");
            let (head, omitted, moved) = account(&text);
            assert_eq!(
                head + omitted + moved,
                original.chars().count(),
                "{excess}: {text:?}"
            );
        }
        let cut = parse(&text);
        assert!(cut.head.is_empty() && cut.moved_pointer == Some(KEPT_LINE));
    }

    #[test]
    fn a_pointer_in_a_commands_closing_line_stays_too() {
        let line = "[more: 5000 lines are kept as o-0123456789abcdef; read_output {\"id\":\"o-0123456789abcdef\"} reads them page by page]";
        let original = format!("{}\n{line}\n", "out line\n".repeat(1_000));
        let cut = shorten_output(&original, 4_000).unwrap();
        assert_eq!(crate::kept::read_pointer(&cut), Some(line), "{cut:?}");
        assert!(cut.chars().count() < original.chars().count());
    }

    /// A pointer is found by the id's shape, not by its words alone.
    #[test]
    fn a_line_that_only_mentions_read_output_is_no_pointer() {
        assert_eq!(
            crate::kept::read_pointer("read_output {\"id\":\"nope\"}"),
            None
        );
        assert_eq!(crate::kept::read_pointer("read_output reads it"), None);
        assert_eq!(
            crate::kept::read_pointer(&format!("a\n{KEPT_LINE}\nb")),
            Some(KEPT_LINE)
        );
    }

    /// "Rarely" holds while a cut can reach three quarters of the budget. When everything outside
    /// the protected tail is already at its marker and the history is still over budget, each
    /// turn's large output is cut as it leaves the tail: a shortening at every such turn.
    #[test]
    fn with_nothing_left_to_take_each_large_output_is_cut_as_it_leaves_the_tail() {
        let mut m = vec![Message::user_text("go")];
        let mut cuts = Vec::new();
        for turn in 0..5 {
            m.push(call(&format!("c{turn}")));
            m.push(big(&format!("c{turn}"), 5_000, 'x'));
            cuts.push(record_shortenings(&mut m, 100));
        }
        assert_eq!(cuts, [false, true, true, true, true]);
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
                            // Nothing was left to take: under the floor, or already only a marker.
                            prop_assert!(
                                old.chars().count() < SHORTENING_FLOOR
                                    || parse(old).head.is_empty(),
                                "{old:?} could have been shortened"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Any number of passes with growth in between: every shortened output says exactly how much of
    /// its original is gone, has one marker, keeps its pointer, and a small result is never touched.
    mod growth {
        use proptest::collection::vec;
        use proptest::prelude::*;

        use super::*;

        fn output() -> impl Strategy<Value = String> {
            (0usize..4_000, 0usize..3, "[a-zé\\[]").prop_map(|(n, kind, fill)| {
                let body = fill.repeat(n);
                match kind {
                    0 => body,
                    1 => format!("{KEPT_LINE}\n{body}"),
                    _ => format!("{body}\n[more: 9 lines are kept as o-0123456789abcdef; read_output {{\"id\":\"o-0123456789abcdef\"}} reads them page by page]\n"),
                }
            })
        }

        proptest! {
            #[test]
            fn prop_every_pass_keeps_the_count_the_marker_and_the_pointer(
                outputs in vec(output(), 1..9),
                budgets in vec(1u32..1500, 1..9),
            ) {
                let mut messages = vec![Message::user_text("go")];
                let mut originals: Vec<(usize, String)> = Vec::new();
                for (n, text) in outputs.iter().enumerate() {
                    messages.push(call(&format!("c{n}")));
                    originals.push((messages.len(), text.clone()));
                    messages.push(Message::tool_result(format!("c{n}"), text.clone()));
                    // A pass after each growth, with the budget of that pass.
                    record_shortenings(&mut messages, budgets[n % budgets.len()]);
                    messages.push(call("last"));
                    messages.push(Message::tool_result("last", "tail"));
                    record_shortenings(&mut messages, budgets[n % budgets.len()]);
                    messages.truncate(messages.len() - 2);
                }
                for (at, original) in &originals {
                    let now = messages[*at].text();
                    if now == *original {
                        continue;
                    }
                    prop_assert!(original.chars().count() >= SHORTENING_FLOOR);
                    let cut = parse(&now);
                    prop_assert_eq!(now.matches(TRUNCATION_MARKER_PREFIX).count(), 1, "{:?}", now);
                    let moved = cut.moved_pointer.map_or(0, |p| p.chars().count());
                    prop_assert_eq!(
                        cut.head.chars().count() + cut.omitted + moved,
                        original.chars().count(),
                        "{:?} of {:?}", now, original
                    );
                    prop_assert!(original.starts_with(cut.head));
                    if let Some(pointer) = crate::kept::read_pointer(original) {
                        prop_assert_eq!(crate::kept::read_pointer(&now), Some(pointer));
                    }
                }
            }
        }
    }
}
