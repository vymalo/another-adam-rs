//! Server-sent events: a small incremental parser and the driver that turns an
//! SSE byte stream into [`ModelDelta`]s.

use std::collections::VecDeque;
use std::time::Duration;

use adam_model::{ModelDelta, ModelError};
use futures::Stream;
use futures::stream::{self, StreamExt};

use crate::wire::Assembler;

/// Incremental SSE parser. Only `data:` fields matter for chat completions;
/// comments (`:` lines), `event:`, `id:` and `retry:` are ignored.
#[derive(Default)]
pub(crate) struct SseParser {
    /// Bytes of the current, unterminated line.
    line: Vec<u8>,
    /// `data:` lines of the event being built.
    data: Vec<String>,
}

impl SseParser {
    /// Feed bytes; returns the payload of every event completed by them.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut events = Vec::new();
        let mut rest = bytes;
        while let Some(end) = memchr::memchr(b'\n', rest) {
            let (segment, after) = rest.split_at(end);
            // A whole line in this chunk is handled where it lies: only a line cut by a chunk
            // boundary is carried in `self.line`.
            if self.line.is_empty() {
                self.handle_line(segment, &mut events);
            } else {
                self.line.extend_from_slice(segment);
                let line = std::mem::take(&mut self.line);
                self.handle_line(&line, &mut events);
            }
            rest = &after[1..];
        }
        self.line.extend_from_slice(rest);
        events
    }

    /// The connection closed: an event without its blank-line terminator is
    /// still delivered.
    pub(crate) fn finish(&mut self) -> Vec<String> {
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

struct State<S> {
    bytes: S,
    parser: SseParser,
    assembler: Option<Assembler>,
    pending: VecDeque<ModelDelta>,
    failure: Option<ModelError>,
    idle: Duration,
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

    /// End of stream (`[DONE]` or EOF): emit `Finished`, or a transient error
    /// when the server never said why it stopped.
    fn finalize(&mut self) {
        self.done = true;
        if let Some(assembler) = self.assembler.take() {
            match assembler.finish() {
                Ok(response) => self.pending.push_back(ModelDelta::Finished(response)),
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
    let state = State {
        bytes: Box::pin(bytes),
        parser: SseParser::default(),
        assembler: Some(Assembler::default()),
        pending: VecDeque::new(),
        failure: None,
        idle,
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
                    let events = st.parser.push(chunk.as_ref());
                    st.handle_events(events, false);
                }
                Ok(None) => {
                    let events = st.parser.finish();
                    st.handle_events(events, true);
                    // Some servers close without `[DONE]`; that is fine as long
                    // as they said why they stopped (checked by `finalize`).
                    if !st.done {
                        st.finalize();
                    }
                }
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
        let mut out = p.push(input.as_bytes());
        out.extend(p.finish());
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
                out.extend(parser.push(&input[from..cut]));
                from = cut;
            }
        }
        out.extend(parser.push(&input[from..]));
        out.extend(parser.finish());
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
        assert!(p.push(b"data: a\r").is_empty());
        assert!(p.push(b"\n\r").is_empty());
        assert_eq!(p.push(b"\n"), ["a"]);

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
            let mut out = p.push(&input[..split]);
            out.extend(p.push(&input[split..]));
            out.extend(p.finish());
            assert_eq!(out, ["h\u{e9}llo \u{1f600}", "[DONE]"], "split at {split}");
        }
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
                out.extend(parser.push(&input[from..cut]));
                from = cut;
            }
            out.extend(parser.finish());
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
