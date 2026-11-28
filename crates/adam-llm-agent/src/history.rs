//! Fitting the history into the model's context budget.

use std::collections::{HashMap, VecDeque};

use adam_core::RunId;
use adam_model::{ContentPart, Message};

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
    /// The pointer line that follows the marker, when a cut moved it: always the canonical line
    /// ([`crate::kept::pointer_line`]).
    moved_pointer: Option<&'a str>,
    /// The marker and what follows it, in characters.
    tail_chars: usize,
}

/// `id` is the output id of the call whose result `content` is, when the loop knows it: a moved
/// pointer is then valid only for that id. With none (a history carried into a new run), any
/// canonical pointer line is.
fn parse<'a>(content: &'a str, id: Option<&str>) -> Cut<'a> {
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
            Some(line)
                if !line.contains('\n')
                    && crate::kept::canonical_pointer_id(line)
                        .is_some_and(|found| id.is_none_or(|id| id == found)) =>
            {
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
/// * **The pointer stays, written afresh.** `id` is the output id of the call whose result this is
///   (`None`: the loop does not know one). When the result has a line for that id with the shape of
///   what the kept-output code writes ([`crate::kept::find_pointer`]), the canonical line
///   ([`crate::kept::pointer_line`]) is put after the marker when a cut removes it, so `read_output`
///   stays reachable whatever the cut. A line that names another id, or has the right words inside
///   other text, is just text, and a cut removes it like the rest.
pub(crate) fn shorten_output(content: &str, excess: usize, id: Option<&str>) -> Option<String> {
    let len = content.chars().count();
    if len < SHORTENING_FLOOR {
        return None;
    }
    let cut = parse(content, id);
    let head_chars = cut.head.chars().count();
    if head_chars == 0 {
        return None; // nothing but a marker (and its pointer)
    }
    let found = id.and_then(|id| crate::kept::find_pointer(cut.head, id).map(|at| (id, at)));
    let canonical: Option<String> = cut
        .moved_pointer
        .map(str::to_owned)
        .or_else(|| found.as_ref().map(|(id, _)| crate::kept::pointer_line(id)));
    let pointer_chars = canonical.as_ref().map_or(0, |p| p.chars().count() + 1);
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
    if cut.moved_pointer.is_none()
        && let Some((_, at)) = &found
    {
        let start = cut.head[..at.start].chars().count();
        let end = start + cut.head[at.clone()].chars().count();
        if keep < end {
            // A cut through the line takes all of it: the canonical line is shown once, after the
            // marker, and counted as kept.
            keep = keep.min(start);
            relocated = true;
            moved_chars = canonical.as_ref().map_or(0, |p| p.chars().count());
        }
    }
    let kept_head: String = cut.head.chars().take(keep).collect();
    let removed = head_chars - keep;
    let mut shortened = kept_head;
    shortened.push_str(&marker(cut.omitted + removed - moved_chars.min(removed)));
    if relocated && let Some(line) = canonical {
        shortened.push('\n');
        shortened.push_str(&line);
    }
    (shortened.chars().count() < len).then_some(shortened)
}

/// What an image counts in the estimate, in characters: 4,000, about 1,000 tokens at a character
/// to a quarter of a token. *Unverified*: what an image costs depends on the provider and on the
/// size of the image.
pub(crate) const IMAGE_CHARS: usize = 4_000;

/// Characters of one content part.
fn part_chars(p: &ContentPart) -> usize {
    match p {
        ContentPart::Text { text } => text.chars().count(),
        ContentPart::Image(_) => IMAGE_CHARS,
    }
}

/// Characters a tool call's arguments take when they are written: the serialisation the model is
/// sent, counted without keeping it.
fn arguments_chars(arguments: &serde_json::Value) -> usize {
    #[cfg(test)]
    SERIALISATIONS.with(|n| n.set(n.get() + 1));
    arguments.to_string().chars().count()
}

/// Characters a carrier value takes when it is written: an opaque member, a call's member and a
/// reasoning block are sent with the request, so they count.
fn value_chars(value: &serde_json::Value) -> usize {
    value.to_string().chars().count()
}

#[cfg(test)]
thread_local! {
    /// How many tool call arguments were serialised on this thread: the test's proof that a warm
    /// count does not walk the history again.
    static SERIALISATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn serialisations() -> usize {
    SERIALISATIONS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn reset_serialisations() {
    SERIALISATIONS.with(|n| n.set(0));
}

/// Characters a message contributes to the estimate.
pub(crate) fn message_chars(m: &Message) -> usize {
    match m {
        Message::User { content } => content.iter().map(part_chars).sum(),
        Message::Assistant {
            content,
            tool_calls,
            reasoning,
            provider,
            ..
        } => {
            // The reasoning a client echoes is sent, so it counts; so do the carrier's opaque
            // members, a call's own, and a raw reasoning block (all sent with the request).
            let carrier = provider.as_ref().map_or(0, |turn| {
                turn.members.values().map(value_chars).sum::<usize>()
                    + turn.reasoning.as_ref().map_or(0, value_chars)
            });
            let opaque = tool_calls
                .iter()
                .filter_map(|call| call.opaque.as_ref())
                .flat_map(|members| members.values())
                .map(value_chars)
                .sum::<usize>();
            reasoning.as_deref().map_or(0, |r| r.chars().count())
                + content.iter().map(part_chars).sum::<usize>()
                + tool_calls
                    .iter()
                    .map(|c| c.name.chars().count() + arguments_chars(&c.arguments))
                    .sum::<usize>()
                + carrier
                + opaque
        }
        Message::Tool {
            content, images, ..
        } => content.chars().count() + images.len() * IMAGE_CHARS,
    }
}

/// The characters of a history as it grows: a count from the messages appended since the last one,
/// never from the whole transcript again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Chars {
    len: usize,
    chars: usize,
}

impl Chars {
    /// The total characters of `messages`. A history of one run only ever grows between two counts,
    /// so only the messages past the last count are walked; a shorter history (a compaction) is
    /// counted in full. A rewrite that changes a message in place without changing the count -
    /// `record_shortenings` itself, the start message's images in `LlmAgent::admit_images`, and the
    /// images of a refused request in `Conversation::replace_unsent_images` - leaves this count
    /// stale: `record_shortenings` updates it, and the other two call `LlmAgent::forget_chars`.
    pub(crate) fn total(&mut self, messages: &[Message]) -> usize {
        if self.len <= messages.len() {
            self.chars += messages[self.len..]
                .iter()
                .map(message_chars)
                .sum::<usize>();
        } else {
            self.chars = messages.iter().map(message_chars).sum();
        }
        self.len = messages.len();
        self.chars
    }
}

/// The character count of each run's history as this worker last made it, so a turn counts only
/// what it appended since. The guard covers the two changes that rewrite the history and record
/// it: `shortenings` and `compactions` must be the same. A message rewritten in place without
/// either counter moving - the start message's images, a refused request's images - is not seen by
/// the guard; those rewrites call `LlmAgent::forget_chars` in the process that makes them. A run's
/// first turn is always counted in full, so a run id the store reuses after a purge never reads
/// another run's total.
#[derive(Debug, Default)]
pub(crate) struct RunChars {
    entries: HashMap<RunId, (Chars, usize, usize)>,
    order: VecDeque<RunId>,
}

/// The most runs whose totals are remembered; past it the oldest entry goes, and a run that comes
/// back counts in full once.
const RUN_CHARS_MAX: usize = 1024;

impl RunChars {
    /// The count to start `run`'s turn from: its last one when the history is still the one it
    /// counted (nothing shortened or compacted it, and this is not the run's first turn), else an
    /// empty one, which counts every message.
    pub(crate) fn begin(
        &mut self,
        run: RunId,
        turns: u32,
        shortenings: usize,
        compactions: usize,
    ) -> Chars {
        if turns == 0 {
            return Chars::default();
        }
        match self.entries.get(&run) {
            Some((chars, shortened, compacted))
                if *shortened == shortenings && *compacted == compactions =>
            {
                *chars
            }
            _ => Chars::default(),
        }
    }

    /// Remember what the turn's count says the history holds, for the next turn of `run`.
    pub(crate) fn end(&mut self, run: RunId, chars: Chars, shortenings: usize, compactions: usize) {
        if !self.entries.contains_key(&run) {
            self.order.push_back(run);
            while self.order.len() > RUN_CHARS_MAX {
                if let Some(oldest) = self.order.pop_front() {
                    self.entries.remove(&oldest);
                }
            }
        }
        self.entries.insert(run, (chars, shortenings, compactions));
    }

    /// Forget `run`: it ended, so nothing of its history will be counted again.
    pub(crate) fn forget(&mut self, run: RunId) {
        if self.entries.remove(&run).is_some() {
            self.order.retain(|r| *r != run);
        }
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
/// shortens the same way. `counted` is only how the count is made cheaper ([`Chars`]): it carries
/// the characters of the messages already counted, so only those appended since are walked, and it
/// is left exact for the next turn.
#[cfg(test)]
pub(crate) fn record_shortenings(
    messages: &mut [Message],
    max_tokens: u32,
    output_id_of: &dyn Fn(&str) -> Option<String>,
    counted: &mut Chars,
) -> bool {
    record_shortenings_from(messages, max_tokens, output_id_of, counted).is_some()
}

/// What one shortening of a history did ([`record_shortenings_from`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shortened {
    /// The position of the first message it changed: a signed message at or after it no longer
    /// holds its signature ([`crate::signing::unsign_from`]).
    pub(crate) first: usize,
    /// The characters it took out of the history, less the markers and lines it left
    /// ([`ShorteningUsage::chars_removed`](adam_runtime::ShorteningUsage::chars_removed)).
    pub(crate) chars_removed: usize,
}

/// [`record_shortenings`], saying what it changed (`None`: nothing).
pub(crate) fn record_shortenings_from(
    messages: &mut [Message],
    max_tokens: u32,
    output_id_of: &dyn Fn(&str) -> Option<String>,
    counted: &mut Chars,
) -> Option<Shortened> {
    if max_tokens == 0 {
        return None;
    }
    let budget = usize::try_from(max_tokens)
        .unwrap_or(usize::MAX)
        .saturating_mul(4);
    let mut total = counted.total(messages);
    if total <= budget {
        return None;
    }
    let before = total;
    let target = budget - budget / 4;
    let protected_from = messages
        .iter()
        .rposition(|m| matches!(m, Message::Assistant { .. }))
        .map_or(0, |i| i + 1);
    let mut first_changed: Option<usize> = None;
    for (at, m) in messages[..protected_from].iter_mut().enumerate() {
        if total <= target {
            break;
        }
        let Message::Tool {
            content,
            call_id,
            images,
            ..
        } = m
        else {
            continue;
        };
        // The images of an old result go with its text: each becomes one line that names it, after
        // what is left of the text, and the model reads it again by calling the tool. Once: the
        // result has no image after that, so a replay finds nothing more to take out.
        let removal = (!images.is_empty()).then(|| {
            let lines: Vec<String> = images.iter().map(crate::images::removed_line).collect();
            lines.join("\n")
        });
        if let Some(added) = &removal {
            total = total - images.len() * IMAGE_CHARS + added.chars().count() + 1;
        }
        if removal.is_none() || total > target {
            let len = content.chars().count();
            let id = output_id_of(call_id);
            if let Some(shortened) = shorten_output(content, total - target, id.as_deref()) {
                total = total - len + shortened.chars().count();
                *content = shortened;
                first_changed.get_or_insert(at);
            }
        }
        if let Some(added) = removal {
            images.clear();
            content.push('\n');
            content.push_str(&added);
            first_changed.get_or_insert(at);
        }
    }
    counted.chars = total;
    counted.len = messages.len();
    first_changed.map(|first| Shortened {
        first,
        chars_removed: before.saturating_sub(total),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_model::{ProviderTurn, ToolCall};
    use serde_json::json;
    use std::collections::BTreeMap;

    /// `messages` after [`record_shortenings`], as a copy.
    fn fit_history(messages: &[Message], max_tokens: u32) -> Vec<Message> {
        let mut out = messages.to_vec();
        record_shortenings(&mut out, max_tokens, &no_ids, &mut Chars::default());
        out
    }

    fn no_ids(_: &str) -> Option<String> {
        None
    }

    const ID: &str = "o-0123456789abcdef";

    fn one_id(_: &str) -> Option<String> {
        Some(ID.to_owned())
    }

    fn call(id: &str) -> Message {
        Message::Assistant {
            extra_content: None,
            content: vec![],
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "t".into(),
                arguments: json!({}),
                ..Default::default()
            }],
            reasoning: None,
            provider: None,
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
        let shorter = shorten_output(&text, 300, None).unwrap();
        assert!(shorter.starts_with("hhh") && shorter.contains(TRUNCATION_MARKER_PREFIX));
        assert!(shorter.chars().count() <= 1_000 - 300);
        // More than it has to give: only the marker is left.
        let floor = shorten_output(&text, 10_000, None).unwrap();
        assert!(floor.starts_with('\n') && floor.contains(TRUNCATION_MARKER_PREFIX));
        assert!(floor.chars().count() < 1_000);
        // Already as short as a marker: not touched again.
        assert_eq!(shorten_output("short", 3, None), None);
        assert_eq!(shorten_output(&floor, 10, None), None);
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
        assert!(!record_shortenings(
            &mut m.clone(),
            0,
            &no_ids,
            &mut Chars::default()
        ));
        assert!(!record_shortenings(
            &mut m.clone(),
            10_000,
            &no_ids,
            &mut Chars::default()
        ));
        assert!(record_shortenings(
            &mut m,
            2750,
            &no_ids,
            &mut Chars::default()
        ));
        let once = m.clone();
        // Under three quarters of the budget now: the next call has nothing to do, and the
        // history is what it was (the requests after a shortening extend it unchanged).
        assert!(!record_shortenings(
            &mut m,
            2750,
            &no_ids,
            &mut Chars::default()
        ));
        assert_eq!(m, once);
        // Over budget with nothing left to shorten is not a shortening either.
        let mut floor = convo();
        assert!(record_shortenings(
            &mut floor,
            10,
            &no_ids,
            &mut Chars::default()
        ));
        assert!(!record_shortenings(
            &mut floor,
            10,
            &no_ids,
            &mut Chars::default()
        ));
    }

    /// An assistant message whose call carries 50 KB of arguments: the serialisation a whole-history
    /// count pays for once per historical call.
    fn big_call(n: usize) -> Message {
        Message::Assistant {
            extra_content: None,
            content: vec![],
            tool_calls: vec![ToolCall {
                id: format!("c{n}"),
                name: "t".into(),
                arguments: json!({"blob": "x".repeat(50_000)}),
                ..Default::default()
            }],
            reasoning: None,
            provider: None,
        }
    }

    /// A turn counts only what the history gained since the last count: the 40 calls already
    /// counted are not serialised again, and the total is the one a full count gives.
    #[test]
    fn a_warm_count_walks_only_the_messages_appended_since() {
        let mut messages: Vec<Message> = (0..40).map(big_call).collect();
        let mut counted = Chars::default();
        reset_serialisations();
        let first = counted.total(&messages);
        assert_eq!(serialisations(), 40);
        assert_eq!(first, messages.iter().map(message_chars).sum::<usize>());
        // The turn appends its answer; the next count serialises that one call only.
        messages.push(big_call(40));
        reset_serialisations();
        let second = counted.total(&messages);
        assert_eq!(serialisations(), 1);
        assert_eq!(second, messages.iter().map(message_chars).sum::<usize>());
    }

    /// The shortening decision and the history it leaves are the same whether the count was made
    /// fresh or carried from the turn before.
    #[test]
    fn a_warm_count_shortens_exactly_as_a_cold_one() {
        let history = convo();
        let mut cold = history.clone();
        let mut warm = history.clone();
        let mut counted = Chars::default();
        counted.total(&warm);
        let cold_cut = record_shortenings(&mut cold, 1000, &no_ids, &mut Chars::default());
        let warm_cut = record_shortenings(&mut warm, 1000, &no_ids, &mut counted);
        assert!(cold_cut, "the history is over the budget");
        assert_eq!(cold_cut, warm_cut);
        assert_eq!(cold, warm);
        // The count a shortened history leaves is exact for the next turn.
        assert_eq!(counted.chars, warm.iter().map(message_chars).sum::<usize>());
    }

    /// A run's total is used only for the history it counted: its first turn, and every turn after
    /// one that neither shortened nor compacted it.
    #[test]
    fn a_run_reuses_its_total_only_for_the_history_it_counted() {
        let run = RunId::new();
        let mut totals = RunChars::default();
        assert_eq!(totals.begin(run, 0, 0, 0), Chars::default());
        let mut counted = Chars::default();
        counted.total(&convo());
        totals.end(run, counted, 0, 0);
        assert_eq!(totals.begin(run, 1, 0, 0), counted);
        // Another worker shortened or compacted the history: this total is not its.
        assert_eq!(totals.begin(run, 1, 1, 0), Chars::default());
        assert_eq!(totals.begin(run, 1, 0, 1), Chars::default());
        // A run that ends is forgotten, so an id the store reuses starts afresh.
        totals.forget(run);
        assert_eq!(totals.begin(run, 1, 0, 0), Chars::default());
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

    #[test]
    fn the_carrier_counts_toward_the_estimate() {
        let plain = Message::Assistant {
            extra_content: None,
            content: vec![],
            tool_calls: vec![ToolCall::new("c", "t", json!({}))],
            reasoning: None,
            provider: None,
        };
        let mut carrier = plain.clone();
        let member = json!({"google": {"thought_signature": "sig"}});
        let block = json!({"blocks": [{"text": "think"}]});
        let call_member = json!({"extra_content": {"sig": "call"}});
        let Message::Assistant {
            provider,
            tool_calls,
            ..
        } = &mut carrier
        else {
            unreachable!("an assistant message")
        };
        let mut turn = ProviderTurn::new("m", "e");
        turn.members = BTreeMap::from([("extra_content".into(), member.clone())]);
        turn.reasoning = Some(block.clone());
        *provider = Some(turn);
        tool_calls[0].opaque = Some(BTreeMap::from([(
            "extra_content".into(),
            call_member.clone(),
        )]));

        // Every carrier value is sent, so it counts: a count that ignored them would give 0 here.
        let expected = member.to_string().chars().count()
            + block.to_string().chars().count()
            + call_member.to_string().chars().count();
        assert_eq!(message_chars(&carrier) - message_chars(&plain), expected);
    }

    const KEPT_LINE: &str = "[kept as o-0123456789abcdef: 7000 lines; read_output {\"id\":\"o-0123456789abcdef\"} reads it]";

    /// What a marked output still says: the head, the count, a pointer moved after the marker.
    fn account(text: &str) -> (usize, usize, usize) {
        let cut = parse(text, None);
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
        let once = shorten_output(&original, 86_000, None).unwrap();
        let (head, omitted, _) = account(&once);
        assert_eq!(head + omitted, 100_000);
        // The output grew by nothing, and is cut again: the marker says what both cuts removed.
        let twice = shorten_output(&once, 5_000, None).unwrap();
        let (head2, omitted2, _) = account(&twice);
        assert_eq!(head2 + omitted2, 100_000, "{twice:?}");
        assert!(omitted2 > omitted && head2 < head);
        // Down to the marker alone, the count still names everything that is gone.
        let floor = shorten_output(&twice, usize::MAX / 4, None).unwrap();
        let (head3, omitted3, _) = account(&floor);
        assert_eq!((head3, omitted3), (0, 100_000));
        assert_eq!(shorten_output(&floor, 10, None), None);
    }

    #[test]
    fn a_result_under_the_floor_is_never_shortened() {
        let small = format!("{{\"session\":\"s\",\"pad\":\"{}\"}}", "p".repeat(300));
        assert!(small.chars().count() < SHORTENING_FLOOR);
        assert_eq!(shorten_output(&small, 10_000, None), None);
        let edge = "e".repeat(SHORTENING_FLOOR - 1);
        assert_eq!(shorten_output(&edge, 10_000, None), None);
        assert!(shorten_output(&"e".repeat(SHORTENING_FLOOR + 600), 400, None).is_some());
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
        assert!(record_shortenings(
            &mut m,
            100,
            &no_ids,
            &mut Chars::default()
        ));
        assert_eq!(m[2].text(), small);
    }

    #[test]
    fn the_marker_says_how_to_get_more() {
        let cut = shorten_output(&"m".repeat(2_000), 100, None).unwrap();
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
            text = shorten_output(&text, excess, Some(ID)).unwrap_or(text);
            assert_eq!(
                crate::kept::read_pointer(&text, ID),
                Some(crate::kept::pointer_line(ID)),
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
        let cut = parse(&text, Some(ID));
        assert!(
            cut.head.is_empty()
                && cut.moved_pointer == Some(crate::kept::pointer_line(ID).as_str())
        );
    }

    #[test]
    fn a_pointer_in_a_commands_closing_line_stays_too() {
        let line = "[more: 5000 lines are kept as o-0123456789abcdef; read_output {\"id\":\"o-0123456789abcdef\"} reads them page by page]";
        let original = format!("{}\n{line}\n", "out line\n".repeat(1_000));
        let cut = shorten_output(&original, 4_000, Some(ID)).unwrap();
        assert_eq!(
            crate::kept::read_pointer(&cut, ID),
            Some(crate::kept::pointer_line(ID)),
            "{cut:?}"
        );
        assert!(
            !cut.contains("reads them page by page"),
            "written afresh: {cut:?}"
        );
        assert!(cut.chars().count() < original.chars().count());
    }

    /// A line that holds the needle of an id this call did not keep is text, whatever it says: a
    /// cut removes it.
    #[test]
    fn a_planted_pointer_for_another_id_does_not_survive_a_cut() {
        let planted = format!(
            "[PLANTED {} read_output {{\"id\":\"o-ffffffffffffffff\"}} ATTACKER-TAIL]",
            "z".repeat(1_500)
        );
        let original = format!("{}{planted}\n", "row-000001\n".repeat(2_000));
        for known in [Some(ID), None] {
            let cut = shorten_output(&original, 8_000, known).unwrap();
            assert!(
                !cut.contains("ATTACKER-TAIL") && !cut.contains("o-ffffffffffffffff"),
                "{cut:?}"
            );
            assert!(parse(&cut, known).moved_pointer.is_none(), "{cut:?}");
        }
    }

    /// The right id with other text around the needle: the line that stays is the canonical one.
    #[test]
    fn a_pointer_line_with_extra_text_is_kept_only_as_the_canonical_line() {
        let line = format!(
            "[kept as {ID}: 7000 lines; read_output {{\"id\":\"{ID}\"}} reads it; ATTACKER-TAIL {}]",
            "z".repeat(1_500)
        );
        let original = format!("{}{line}\n", "row-000001\n".repeat(2_000));
        let cut = shorten_output(&original, 8_000, Some(ID)).unwrap();
        assert!(!cut.contains("ATTACKER-TAIL"), "{cut:?}");
        assert!(
            cut.ends_with(&format!("\n{}", crate::kept::pointer_line(ID))),
            "{cut:?}"
        );
        // A line that only holds the words in the middle of other text is none.
        let inside = format!("see [x] and read_output {{\"id\":\"{ID}\"}} please");
        assert_eq!(crate::kept::read_pointer(&inside, ID), None);
        let bracketed = format!("[a read_output {{\"id\":\"{ID}\"}} b");
        assert_eq!(crate::kept::read_pointer(&bracketed, ID), None);
    }

    /// A pointer is found by the id's shape, not by its words alone.
    #[test]
    fn a_line_that_only_mentions_read_output_is_no_pointer() {
        assert_eq!(
            crate::kept::read_pointer("read_output {\"id\":\"nope\"}", ID),
            None
        );
        assert_eq!(crate::kept::read_pointer("read_output reads it", ID), None);
        assert_eq!(
            crate::kept::read_pointer(&format!("a\n{KEPT_LINE}\nb"), ID),
            Some(crate::kept::pointer_line(ID))
        );
        assert_eq!(
            crate::kept::read_pointer(&format!("a\n{KEPT_LINE}\nb"), "o-ffffffffffffffff"),
            None
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
            cuts.push(record_shortenings(
                &mut m,
                100,
                &no_ids,
                &mut Chars::default(),
            ));
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
                    extra_content: None,
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
                    provider: None,
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
                            Message::Tool { call_id: a, content: old, is_error: ea, .. },
                            Message::Tool { call_id: b, content: new, is_error: eb, .. },
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
                prop_assert!(!record_shortenings(&mut again, max_tokens, &no_ids, &mut Chars::default()));
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
                                    || parse(old, None).head.is_empty(),
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
                    record_shortenings(&mut messages, budgets[n % budgets.len()], &one_id, &mut Chars::default());
                    messages.push(call("last"));
                    messages.push(Message::tool_result("last", "tail"));
                    record_shortenings(&mut messages, budgets[n % budgets.len()], &one_id, &mut Chars::default());
                    messages.truncate(messages.len() - 2);
                }
                for (at, original) in &originals {
                    let now = messages[*at].text();
                    if now == *original {
                        continue;
                    }
                    prop_assert!(original.chars().count() >= SHORTENING_FLOOR);
                    let cut = parse(&now, Some(ID));
                    prop_assert_eq!(now.matches(TRUNCATION_MARKER_PREFIX).count(), 1, "{:?}", now);
                    let moved = cut.moved_pointer.map_or(0, |p| p.chars().count());
                    prop_assert_eq!(
                        cut.head.chars().count() + cut.omitted + moved,
                        original.chars().count(),
                        "{:?} of {:?}", now, original
                    );
                    prop_assert!(original.starts_with(cut.head));
                    if let Some(pointer) = crate::kept::read_pointer(original, ID) {
                        prop_assert_eq!(crate::kept::read_pointer(&now, ID), Some(pointer));
                    }
                }
            }
        }
    }

    /// The position of the first message a shortening changed is given, for the signatures after
    /// it to be dropped (ADR 0068), and none when nothing was shortened.
    #[test]
    fn the_first_changed_message_is_reported() {
        let tool = |id: &str, n: usize| Message::Tool {
            call_id: id.into(),
            content: "x".repeat(n),
            is_error: false,
            images: Vec::new(),
            extra_content: None,
        };
        let assistant = |id: &str| Message::Assistant {
            content: Vec::new(),
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "t".into(),
                arguments: json!({}),
                ..Default::default()
            }],
            reasoning: None,
            provider: None,
            extra_content: None,
        };
        let mut messages = vec![
            Message::user_text("go"),
            assistant("a"),
            tool("a", 100),
            assistant("b"),
            tool("b", 20_000),
            assistant("c"),
        ];
        let before = messages.clone();
        assert_eq!(
            record_shortenings_from(&mut messages, 100_000, &no_ids, &mut Chars::default()),
            None
        );
        assert_eq!(messages, before);
        let shortened =
            record_shortenings_from(&mut messages, 1_000, &no_ids, &mut Chars::default())
                .expect("shortened");
        let first = shortened.first;
        assert!(first >= 2, "{first}");
        assert_eq!(messages[..first], before[..first]);
        assert_ne!(messages[first], before[first]);
        // What it took out is what the history lost, the marker it left counted against it.
        let chars = |messages: &[Message]| messages.iter().map(message_chars).sum::<usize>();
        assert_eq!(shortened.chars_removed, chars(&before) - chars(&messages));
        assert!(shortened.chars_removed > 15_000, "{shortened:?}");
    }
}
