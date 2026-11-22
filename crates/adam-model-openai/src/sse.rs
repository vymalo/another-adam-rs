//! Server-sent events: a small incremental parser and the driver that turns an
//! SSE byte stream into [`ModelDelta`]s.

use std::collections::VecDeque;
use std::time::Duration;

use adam_model::{ModelDelta, ModelError};
use futures::Stream;
use futures::stream::{self, StreamExt};

use crate::wire::Assembler;

/// The most bytes one SSE line may carry.
pub(crate) const MAX_LINE_BYTES: usize = 1 << 20;
/// The most bytes one event's `data:` lines may total.
pub(crate) const MAX_EVENT_BYTES: usize = 4 << 20;
/// The most bytes one streamed response body may carry.
pub(crate) const MAX_STREAM_BYTES: usize = 64 << 20;

/// The caps a stream is read under. Tests lower them; a deployment keeps the defaults.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) line: usize,
    pub(crate) event: usize,
    pub(crate) stream: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            line: MAX_LINE_BYTES,
            event: MAX_EVENT_BYTES,
            stream: MAX_STREAM_BYTES,
        }
    }
}

/// Incremental SSE parser. Only `data:` fields matter for chat completions;
/// comments (`:` lines), `event:`, `id:` and `retry:` are ignored.
#[derive(Default)]
pub(crate) struct SseParser {
    /// Bytes of the current, unterminated line.
    line: Vec<u8>,
    /// `data:` lines of the event being built.
    data: Vec<String>,
    /// Bytes those lines carry, so the event cap is checked without joining them.
    data_bytes: usize,
    limits: Limits,
}

impl SseParser {
    pub(crate) fn with_limits(limits: Limits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }

    /// Feed bytes; returns the payload of every event completed by them.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, ModelError> {
        let mut events = Vec::new();
        let mut rest = bytes;
        while let Some(end) = memchr::memchr(b'\n', rest) {
            let (segment, after) = rest.split_at(end);
            // A whole line in this chunk is handled where it lies: only a line cut by a chunk
            // boundary is carried in `self.line`.
            if self.line.is_empty() {
                if segment.len() > self.limits.line {
                    return Err(over_line(self.limits.line));
                }
                self.handle_line(segment, &mut events)?;
            } else {
                if self.line.len() + segment.len() > self.limits.line {
                    return Err(over_line(self.limits.line));
                }
                self.line.extend_from_slice(segment);
                let line = std::mem::take(&mut self.line);
                self.handle_line(&line, &mut events)?;
            }
            rest = &after[1..];
        }
        if self.line.len() + rest.len() > self.limits.line {
            return Err(over_line(self.limits.line));
        }
        self.line.extend_from_slice(rest);
        Ok(events)
    }

    /// The connection closed: an event without its blank-line terminator is
    /// still delivered.
    pub(crate) fn finish(&mut self) -> Result<Vec<String>, ModelError> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.handle_line(&line, &mut events)?;
        }
        self.dispatch(&mut events);
        Ok(events)
    }

    fn handle_line(&mut self, line: &[u8], events: &mut Vec<String>) -> Result<(), ModelError> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            self.dispatch(events);
            return Ok(());
        }
        if line[0] == b':' {
            return Ok(());
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(i) => {
                let value = &line[i + 1..];
                (&line[..i], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (line, &b""[..]),
        };
        if field == b"data" {
            // The join adds a newline between lines; count it so the cap is the payload's size.
            let added = value.len() + usize::from(!self.data.is_empty());
            if self.data_bytes + added > self.limits.event {
                return Err(over_event(self.limits.event));
            }
            self.data_bytes += added;
            self.data.push(String::from_utf8_lossy(value).into_owned());
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<String>) {
        if !self.data.is_empty() {
            events.push(self.data.join("\n"));
            self.data.clear();
            self.data_bytes = 0;
        }
    }
}

fn over_line(cap: usize) -> ModelError {
    ModelError::protocol(format!("the model stream sent a line over {cap} bytes"))
}

fn over_event(cap: usize) -> ModelError {
    ModelError::protocol(format!("the model stream sent an event over {cap} bytes"))
}

fn over_stream(cap: usize) -> ModelError {
    ModelError::protocol(format!("the model stream sent over {cap} bytes"))
}

struct State<S> {
    bytes: S,
    parser: SseParser,
    assembler: Option<Assembler>,
    pending: VecDeque<ModelDelta>,
    failure: Option<ModelError>,
    idle: Duration,
    limits: Limits,
    received: usize,
    done: bool,
}

impl<S> State<S> {
    /// Process complete SSE payloads. Deltas produced before a failure are
    /// still delivered ahead of it.
    fn handle_events(&mut self, events: Vec<String>, closed: bool) {
        let count = events.len();
        for (i, data) in events.into_iter().enumerate() {
            if self.done {
                return;
            }
            let Some(assembler) = self.assembler.as_mut() else {
                return;
            };
            if data.trim() == "[DONE]" {
                self.finalize();
                return;
            }
            let pushed = if closed && i + 1 == count {
                assembler.push_last(&data)
            } else {
                assembler.push(&data)
            };
            match pushed {
                Ok(deltas) => self.pending.extend(deltas),
                Err(e) => {
                    self.fail(e);
                    return;
                }
            }
        }
    }

    /// End of stream (`[DONE]` or EOF): say the calls whose arguments were never announced, then
    /// `Finished`, or a transient error when the server never said why it stopped.
    fn finalize(&mut self) {
        self.done = true;
        if let Some(mut assembler) = self.assembler.take() {
            let rest = assembler.remaining();
            match assembler.finish() {
                Ok(response) => {
                    self.pending.extend(rest);
                    self.pending.push_back(ModelDelta::Finished(response));
                }
                Err(e) => self.failure = Some(e),
            }
        }
    }

    fn fail(&mut self, error: ModelError) {
        self.done = true;
        self.assembler = None;
        self.failure = Some(error);
    }
}

/// Turn an SSE byte stream (a chat-completions response body) into deltas.
///
/// The stream ends with exactly one of: a [`ModelDelta::Finished`], or one
/// `Err` item. `idle` bounds the wait for each chunk of bytes.
pub(crate) fn deltas<S, B, E>(
    bytes: S,
    idle: Duration,
) -> impl Stream<Item = Result<ModelDelta, ModelError>> + Send + 'static
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
    B: AsRef<[u8]> + Send,
    E: std::error::Error + Send + Sync + 'static,
{
    deltas_with_limits(bytes, idle, Limits::default())
}

/// [`deltas`] under explicit caps; the tests lower them.
pub(crate) fn deltas_with_limits<S, B, E>(
    bytes: S,
    idle: Duration,
    limits: Limits,
) -> impl Stream<Item = Result<ModelDelta, ModelError>> + Send + 'static
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
    B: AsRef<[u8]> + Send,
    E: std::error::Error + Send + Sync + 'static,
{
    let state = State {
        bytes: Box::pin(bytes),
        parser: SseParser::with_limits(limits),
        assembler: Some(Assembler::default()),
        pending: VecDeque::new(),
        failure: None,
        idle,
        limits,
        received: 0,
        done: false,
    };
    stream::unfold(state, |mut st| async move {
        loop {
            if let Some(delta) = st.pending.pop_front() {
                return Some((Ok(delta), st));
            }
            if let Some(error) = st.failure.take() {
                return Some((Err(error), st));
            }
            if st.done {
                return None;
            }
            match tokio::time::timeout(st.idle, st.bytes.next()).await {
                Err(_elapsed) => st.fail(ModelError::transient(format!(
                    "no data from the model for {:?}",
                    st.idle
                ))),
                Ok(Some(Err(e))) => {
                    st.fail(ModelError::transient("stream interrupted").with_source(e));
                }
                Ok(Some(Ok(chunk))) => {
                    let chunk = chunk.as_ref();
                    st.received = st.received.saturating_add(chunk.len());
                    if st.received > st.limits.stream {
                        st.fail(over_stream(st.limits.stream));
                        continue;
                    }
                    match st.parser.push(chunk) {
                        Ok(events) => st.handle_events(events, false),
                        Err(e) => st.fail(e),
                    }
                }
                Ok(None) => match st.parser.finish() {
                    Ok(events) => {
                        st.handle_events(events, true);
                        // Some servers close without `[DONE]`; that is fine as long
                        // as they said why they stopped (checked by `finalize`).
                        if !st.done {
                            st.finalize();
                        }
                    }
                    Err(e) => st.fail(e),
                },
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use adam_model::FinishReason;
    use futures::TryStreamExt;

    use super::*;

    fn parse(input: &str) -> Vec<String> {
        let mut p = SseParser::default();
        let mut out = p.push(input.as_bytes()).expect("under the caps");
        out.extend(p.finish().expect("under the caps"));
        out
    }

    /// The parser as it was before the bulk scan — one byte at a time — kept as the oracle.
    mod oracle {
        #[derive(Default)]
        pub(super) struct ByteAtATime {
            line: Vec<u8>,
            data: Vec<String>,
        }

        impl ByteAtATime {
            pub(super) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
                let mut events = Vec::new();
                for &b in bytes {
                    if b == b'\n' {
                        let line = std::mem::take(&mut self.line);
                        self.handle_line(&line, &mut events);
                    } else {
                        self.line.push(b);
                    }
                }
                events
            }

            pub(super) fn finish(&mut self) -> Vec<String> {
                let mut events = Vec::new();
                if !self.line.is_empty() {
                    let line = std::mem::take(&mut self.line);
                    self.handle_line(&line, &mut events);
                }
                self.dispatch(&mut events);
                events
            }

            fn handle_line(&mut self, line: &[u8], events: &mut Vec<String>) {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if line.is_empty() {
                    self.dispatch(events);
                    return;
                }
                if line[0] == b':' {
                    return;
                }
                let (field, value) = match line.iter().position(|&b| b == b':') {
                    Some(i) => {
                        let value = &line[i + 1..];
                        (&line[..i], value.strip_prefix(b" ").unwrap_or(value))
                    }
                    None => (line, &b""[..]),
                };
                if field == b"data" {
                    self.data.push(String::from_utf8_lossy(value).into_owned());
                }
            }

            fn dispatch(&mut self, events: &mut Vec<String>) {
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
            }
        }

        /// The oracle fed one byte at a time.
        pub(super) fn events(input: &[u8]) -> Vec<String> {
            let mut parser = ByteAtATime::default();
            let mut out = Vec::new();
            for &b in input {
                out.extend(parser.push(&[b]));
            }
            out.extend(parser.finish());
            out
        }
    }

    /// Feed `input` split at `cuts`, then close the stream.
    fn events_at_cuts(input: &[u8], cuts: &[usize]) -> Vec<String> {
        let mut parser = SseParser::default();
        let mut out = Vec::new();
        let mut from = 0;
        for &cut in cuts {
            let cut = cut.min(input.len());
            if cut > from {
                out.extend(parser.push(&input[from..cut]).expect("under the caps"));
                from = cut;
            }
        }
        out.extend(parser.push(&input[from..]).expect("under the caps"));
        out.extend(parser.finish().expect("under the caps"));
        out
    }

    /// Over 256 KiB of events, with multi-byte characters and an unterminated tail.
    fn large_stream() -> Vec<u8> {
        let mut body = String::new();
        let mut i = 0;
        while body.len() < 256 * 1024 {
            body.push_str(&format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"event {i}: h\u{e9}llo \u{1f600}\"}}}}]}}\n\n"
            ));
            i += 1;
        }
        body.push_str("data: tail \u{e9}");
        body.into_bytes()
    }

    #[test]
    fn a_large_stream_in_awkward_chunks_matches_the_byte_at_a_time_parser() {
        let input = large_stream();
        assert!(input.len() >= 256 * 1024, "{} bytes", input.len());
        let expected = oracle::events(&input);
        assert!(expected.len() > 1000, "{} events", expected.len());
        assert!(expected[0].contains("event 0: h\u{e9}llo \u{1f600}"));
        assert_eq!(expected.last().map(String::as_str), Some("tail \u{e9}"));

        // Sizes that straddle lines and characters, including one larger than most events.
        let sizes = [1usize, 2, 3, 5, 7, 11, 13, 17, 31, 257, 4093];
        let mut cuts = Vec::new();
        let mut at = 0;
        let mut i = 0;
        while at < input.len() {
            at = (at + sizes[i % sizes.len()]).min(input.len());
            cuts.push(at);
            i += 1;
        }
        // And one boundary inside a multi-byte character, which no size cycle guarantees.
        let inside = input
            .iter()
            .position(|&b| b >= 0x80)
            .expect("a multi-byte character")
            + 1;
        assert!(
            std::str::from_utf8(&input[..inside]).is_err(),
            "cut {inside} is inside a multi-byte character"
        );
        cuts.push(inside);
        cuts.sort_unstable();
        cuts.dedup();

        assert_eq!(events_at_cuts(&input, &cuts), expected);
        assert_eq!(events_at_cuts(&input, &[input.len()]), expected);
    }

    #[test]
    fn a_chunk_ending_on_cr_completes_the_line_on_the_next() {
        let mut p = SseParser::default();
        assert!(p.push(b"data: a\r").expect("under the caps").is_empty());
        assert!(p.push(b"\n\r").expect("under the caps").is_empty());
        assert_eq!(p.push(b"\n").expect("under the caps"), ["a"]);

        // CRLF everywhere, cut between every `\r` and `\n` and elsewhere.
        let input = b"data: a\r\n\r\ndata: b\r\n\r\ndata: c";
        let expected = oracle::events(input);
        assert_eq!(expected, ["a", "b", "c"]);
        for cuts in [
            vec![8, 9, 10, 19, 20, 21],
            vec![7, 8, 9, 18, 19, 20],
            (0..input.len()).collect::<Vec<_>>(),
        ] {
            assert_eq!(events_at_cuts(input, &cuts), expected, "cuts {cuts:?}");
        }
    }

    #[test]
    fn parser_basics() {
        assert_eq!(parse("data: a\n\ndata: b\n\n"), ["a", "b"]);
        assert_eq!(parse("data:a\r\n\r\ndata: b\r\n\r\n"), ["a", "b"]);
        assert_eq!(parse(": keepalive\n\ndata: x\n\n"), ["x"]);
        assert_eq!(parse("event: message\nid: 1\ndata: x\n\n"), ["x"]);
        assert_eq!(parse("data: a\ndata: b\n\n"), ["a\nb"]);
        // No terminating blank line, no trailing newline.
        assert_eq!(parse("data: tail"), ["tail"]);
        assert!(parse("\n\n\n").is_empty());
    }

    #[test]
    fn parser_handles_arbitrary_splits_including_inside_utf8() {
        let input = "data: h\u{e9}llo \u{1f600}\n\ndata: [DONE]\n\n".as_bytes();
        for split in 0..input.len() {
            let mut p = SseParser::default();
            let mut out = p.push(&input[..split]).expect("under the caps");
            out.extend(p.push(&input[split..]).expect("under the caps"));
            out.extend(p.finish().expect("under the caps"));
            assert_eq!(out, ["h\u{e9}llo \u{1f600}", "[DONE]"], "split at {split}");
        }
    }

    fn limits(line: usize, event: usize, stream: usize) -> Limits {
        Limits {
            line,
            event,
            stream,
        }
    }

    #[test]
    fn the_default_caps_are_the_documented_ones() {
        let l = Limits::default();
        assert_eq!(l.line, 1 << 20, "one line: 1 MiB");
        assert_eq!(l.event, 4 << 20, "one event: 4 MiB");
        assert_eq!(l.stream, 64 << 20, "one stream: 64 MiB");
    }

    #[test]
    fn a_line_over_the_cap_is_refused_before_it_grows() {
        // A whole line in one chunk, newline included.
        let mut p = SseParser::with_limits(limits(8, 1 << 10, 1 << 20));
        let err = p.push(b"data: abc\n").expect_err("a 9-byte line");
        assert!(matches!(err, ModelError::Protocol { .. }), "{err:?}");
        assert!(err.to_string().contains("over 8 bytes"), "{err}");

        // The same line cut by chunk boundaries, refused on the byte that crosses the cap.
        let mut p = SseParser::with_limits(limits(8, 1 << 10, 1 << 20));
        assert!(p.push(b"data: ab").is_ok(), "8 bytes so far");
        let err = p.push(b"c").expect_err("the 9th byte");
        assert!(err.to_string().contains("over 8 bytes"), "{err}");

        // And when the newline arrives in a later chunk, the joined line is refused too.
        let mut p = SseParser::with_limits(limits(8, 1 << 10, 1 << 20));
        assert!(p.push(b"data: ab").is_ok(), "8 bytes so far");
        let err = p.push(b"c\n").expect_err("the 9th byte and its newline");
        assert!(err.to_string().contains("over 8 bytes"), "{err}");

        // A line of exactly the cap is accepted, and the event ends at the blank line.
        let mut p = SseParser::with_limits(limits(8, 1 << 10, 1 << 20));
        assert!(p.push(b"data: ab\n").expect("exactly 8 bytes").is_empty());
        assert_eq!(p.push(b"\n").expect("the event ends"), ["ab"]);
    }

    #[test]
    fn an_event_over_the_cap_is_refused_and_the_next_event_starts_clean() {
        // Values of 10 bytes each, an event cap of 16.
        let mut p = SseParser::with_limits(limits(1 << 10, 16, 1 << 20));
        assert!(p.push(b"data: 0123456789\n").is_ok(), "10 of 16");
        let err = p.push(b"data: 0123456789\n").expect_err("20 of 16");
        assert!(err.to_string().contains("over 16 bytes"), "{err}");

        // A dispatched event resets the count: two 10-byte events in a row are fine.
        let mut p = SseParser::with_limits(limits(1 << 10, 16, 1 << 20));
        assert_eq!(
            p.push(b"data: 0123456789\n\n").expect("one event"),
            ["0123456789"]
        );
        assert_eq!(
            p.push(b"data: 0123456789\n\n").expect("the next event"),
            ["0123456789"]
        );

        // The newline the join adds is counted: 10 + 1 + 10 is over a cap of 20, while the
        // values alone (20) are not.
        let mut p = SseParser::with_limits(limits(1 << 10, 20, 1 << 20));
        assert!(p.push(b"data: 0123456789\n").is_ok(), "10 of 20");
        assert!(
            p.push(b"data: 0123456789\n").is_err(),
            "the join byte makes 21"
        );
    }

    #[tokio::test]
    async fn a_stream_over_the_cap_is_refused() {
        let comments = |n: usize| Ok::<_, std::io::Error>(vec![b':'; n]);
        let out: Vec<_> = deltas_with_limits(
            stream::iter(vec![comments(40), comments(40)]),
            Duration::from_secs(5),
            limits(1 << 10, 1 << 10, 64),
        )
        .collect()
        .await;
        assert_eq!(out.len(), 1, "{out:?}");
        let err = out[0].as_ref().expect_err("over the stream cap");
        assert!(matches!(err, ModelError::Protocol { .. }), "{err:?}");
        assert!(err.to_string().contains("over 64 bytes"), "{err}");
    }

    fn byte_stream(
        chunks: Vec<&'static str>,
    ) -> impl Stream<Item = Result<Vec<u8>, std::io::Error>> + Send + 'static {
        stream::iter(chunks.into_iter().map(|c| Ok(c.as_bytes().to_vec())))
    }

    const TEXT_STREAM: [&str; 4] = [
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    ];

    #[tokio::test]
    async fn text_stream_ends_with_finished() {
        let out: Vec<_> = deltas(byte_stream(TEXT_STREAM.to_vec()), Duration::from_secs(5))
            .try_collect()
            .await
            .unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], ModelDelta::Text("Hel".into()));
        assert_eq!(out[1], ModelDelta::Text("lo".into()));
        let ModelDelta::Finished(r) = &out[2] else {
            panic!("{out:?}")
        };
        assert_eq!(r.message.text(), "Hello");
        assert_eq!(r.finish, FinishReason::Stop);
        assert_eq!(r.usage.output_tokens, 2);
    }

    /// A call whose arguments never came is announced just before `Finished`, as the call the turn
    /// holds (`{}`), never while the stream goes on.
    #[tokio::test]
    async fn a_call_with_no_arguments_is_announced_just_before_finished() {
        let out: Vec<_> = deltas(
            byte_stream(vec![
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"w\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\n\n",
            ]),
            Duration::from_secs(5),
        )
        .try_collect()
        .await
        .unwrap();
        assert_eq!(out.len(), 3, "started, complete, finished: {out:?}");
        assert!(matches!(out[0], ModelDelta::ToolCallStarted { .. }));
        assert_eq!(
            out[1],
            ModelDelta::ToolCallComplete(adam_model::ToolCall::new(
                "c1",
                "w",
                serde_json::json!({})
            ))
        );
        assert!(matches!(out[2], ModelDelta::Finished(_)));
    }

    #[tokio::test]
    async fn eof_without_done_is_ok_when_a_finish_reason_was_seen() {
        let out: Vec<_> = deltas(
            byte_stream(TEXT_STREAM[..3].to_vec()),
            Duration::from_secs(5),
        )
        .try_collect()
        .await
        .unwrap();
        assert!(matches!(out.last(), Some(ModelDelta::Finished(_))));
    }

    #[tokio::test]
    async fn eof_without_finish_reason_is_transient() {
        let items: Vec<_> = deltas(
            byte_stream(TEXT_STREAM[..1].to_vec()),
            Duration::from_secs(5),
        )
        .collect()
        .await;
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0], Ok(ModelDelta::Text(_))));
        assert!(matches!(items[1], Err(ModelError::Transient { .. })));
        // [DONE] with no finish_reason is just as truncated.
        let items: Vec<_> = deltas(
            byte_stream(vec!["data: [DONE]\n\n"]),
            Duration::from_secs(5),
        )
        .collect()
        .await;
        assert!(matches!(
            items.as_slice(),
            [Err(ModelError::Transient { .. })]
        ));
    }

    #[tokio::test]
    async fn a_chunk_cut_by_eof_is_transient() {
        let items: Vec<_> = deltas(
            byte_stream(vec!["data: {\"choices\":[{\"delta\":{\"content\":\"Hel"]),
            Duration::from_secs(5),
        )
        .collect()
        .await;
        assert!(
            matches!(items.as_slice(), [Err(ModelError::Transient { .. })]),
            "{items:?}"
        );
    }

    #[tokio::test]
    async fn a_complete_chunk_that_is_not_json_stays_protocol() {
        let items: Vec<_> = deltas(byte_stream(vec!["data: {oops}"]), Duration::from_secs(5))
            .collect()
            .await;
        assert!(
            matches!(items.as_slice(), [Err(ModelError::Protocol { .. })]),
            "{items:?}"
        );
    }

    #[tokio::test]
    async fn events_after_done_are_ignored() {
        let mut chunks = TEXT_STREAM.to_vec();
        chunks.push("data: {\"choices\":[{\"delta\":{\"content\":\"late\"}}]}\n\n");
        let out: Vec<_> = deltas(byte_stream(chunks), Duration::from_secs(5))
            .try_collect()
            .await
            .unwrap();
        assert_eq!(out.len(), 3);
    }

    #[tokio::test]
    async fn transport_error_mid_stream_is_transient_and_final() {
        let bytes = stream::iter(vec![
            Ok(TEXT_STREAM[0].as_bytes().to_vec()),
            Err(std::io::Error::other("connection reset")),
            Ok(TEXT_STREAM[1].as_bytes().to_vec()),
        ]);
        let items: Vec<_> = deltas(bytes, Duration::from_secs(5)).collect().await;
        assert_eq!(items.len(), 2);
        assert!(matches!(items[1], Err(ModelError::Transient { .. })));
    }

    #[tokio::test]
    async fn malformed_chunk_is_a_protocol_error() {
        let items: Vec<_> = deltas(byte_stream(vec!["data: {oops\n\n"]), Duration::from_secs(5))
            .collect()
            .await;
        assert!(matches!(
            items.as_slice(),
            [Err(ModelError::Protocol { .. })]
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_is_transient() {
        let bytes = stream::iter(vec![Ok::<_, std::io::Error>(
            TEXT_STREAM[0].as_bytes().to_vec(),
        )])
        .chain(stream::pending());
        let items: Vec<_> = deltas(bytes, Duration::from_secs(30)).collect().await;
        assert_eq!(items.len(), 2);
        assert!(matches!(items[1], Err(ModelError::Transient { .. })));
    }

    mod prop {
        use proptest::collection::vec;
        use proptest::prelude::*;

        use super::*;

        /// A byte stream shaped like SSE (data lines, comments, other fields,
        /// both line endings, unicode, blank lines) or plain noise.
        fn arb_input() -> impl Strategy<Value = Vec<u8>> {
            let line = prop_oneof![
                "\\PC{0,30}".prop_map(|v| format!("data: {v}")),
                "\\PC{0,30}".prop_map(|v| format!("data:{v}")),
                "\\PC{0,30}".prop_map(|v| format!(": {v}")),
                "\\PC{0,30}".prop_map(|v| format!("event: {v}")),
                "\\PC{0,30}".prop_map(|v| format!("id: {v}")),
                Just("data".to_owned()),
                Just(String::new()),
            ];
            let ending = prop_oneof![Just("\n"), Just("\r\n")];
            let structured = vec((line, ending), 0..20).prop_map(|lines| {
                lines
                    .into_iter()
                    .flat_map(|(l, e)| format!("{l}{e}").into_bytes())
                    .collect::<Vec<u8>>()
            });
            prop_oneof![structured, vec(any::<u8>(), 0..200)]
        }

        fn events_in_chunks(input: &[u8], cuts: &[usize]) -> Vec<String> {
            let mut cuts: Vec<usize> = cuts.iter().map(|c| c % (input.len() + 1)).collect();
            cuts.sort_unstable();
            let mut parser = SseParser::default();
            let mut out = Vec::new();
            let mut from = 0;
            for cut in cuts.into_iter().chain([input.len()]) {
                out.extend(parser.push(&input[from..cut]).expect("under the caps"));
                from = cut;
            }
            out.extend(parser.finish().expect("under the caps"));
            out
        }

        proptest! {
            /// However the network cuts the byte stream (also inside a
            /// multi-byte character, or between `\r` and `\n`), the parser
            /// yields the same events as when fed everything at once.
            #[test]
            fn prop_any_split_yields_same_events(
                input in arb_input(),
                cuts in vec(any::<usize>(), 0..8),
            ) {
                prop_assert_eq!(events_in_chunks(&input, &cuts), events_in_chunks(&input, &[]));
                // And byte by byte, the finest split there is.
                let every: Vec<usize> = (0..input.len()).collect();
                prop_assert_eq!(events_in_chunks(&input, &every), events_in_chunks(&input, &[]));
                // Whatever the split, the events are the old parser's.
                prop_assert_eq!(events_in_chunks(&input, &cuts), oracle::events(&input));
                prop_assert_eq!(events_in_chunks(&input, &every), oracle::events(&input));
            }
        }
    }
}
