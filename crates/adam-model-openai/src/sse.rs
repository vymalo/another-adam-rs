//! Server-sent events: a small incremental parser and the driver that turns an
//! SSE byte stream into [`ModelDelta`]s.

use std::collections::VecDeque;
use std::fmt::Display;
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
    fn handle_events(&mut self, events: Vec<String>) {
        for data in events {
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
            match assembler.push(&data) {
                Ok(deltas) => self.pending.extend(deltas),
                Err(e) => {
                    self.fail(e);
                    return;
                }
            }
        }
    }

    /// End of stream (`[DONE]` or EOF): emit `Finished`, or a protocol error
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
    E: Display + Send,
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
                Err(_elapsed) => st.fail(ModelError::Transient(format!(
                    "no data from the model for {:?}",
                    st.idle
                ))),
                Ok(Some(Err(e))) => {
                    st.fail(ModelError::Transient(format!("stream interrupted: {e}")));
                }
                Ok(Some(Ok(chunk))) => {
                    let events = st.parser.push(chunk.as_ref());
                    st.handle_events(events);
                }
                Ok(None) => {
                    let events = st.parser.finish();
                    st.handle_events(events);
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
    ) -> impl Stream<Item = Result<Vec<u8>, String>> + Send + 'static {
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
    async fn eof_without_finish_reason_is_a_protocol_error() {
        let items: Vec<_> = deltas(
            byte_stream(TEXT_STREAM[..1].to_vec()),
            Duration::from_secs(5),
        )
        .collect()
        .await;
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0], Ok(ModelDelta::Text(_))));
        assert!(matches!(items[1], Err(ModelError::Protocol(_))));
        // [DONE] with no finish_reason is just as truncated.
        let items: Vec<_> = deltas(
            byte_stream(vec!["data: [DONE]\n\n"]),
            Duration::from_secs(5),
        )
        .collect()
        .await;
        assert!(matches!(items.as_slice(), [Err(ModelError::Protocol(_))]));
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
            Err("connection reset".to_string()),
            Ok(TEXT_STREAM[1].as_bytes().to_vec()),
        ]);
        let items: Vec<_> = deltas(bytes, Duration::from_secs(5)).collect().await;
        assert_eq!(items.len(), 2);
        assert!(matches!(items[1], Err(ModelError::Transient(_))));
    }

    #[tokio::test]
    async fn malformed_chunk_is_a_protocol_error() {
        let items: Vec<_> = deltas(byte_stream(vec!["data: {oops\n\n"]), Duration::from_secs(5))
            .collect()
            .await;
        assert!(matches!(items.as_slice(), [Err(ModelError::Protocol(_))]));
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_is_transient() {
        let bytes = stream::iter(vec![Ok::<_, String>(TEXT_STREAM[0].as_bytes().to_vec())])
            .chain(stream::pending());
        let items: Vec<_> = deltas(bytes, Duration::from_secs(30)).collect().await;
        assert_eq!(items.len(), 2);
        assert!(matches!(items[1], Err(ModelError::Transient(_))));
    }
}
