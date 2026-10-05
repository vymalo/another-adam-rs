//! The model's words, sent as they are written.
//!
//! A model turn that streams ([`LlmAgentBuilder::stream_text`](crate::LlmAgentBuilder::stream_text),
//! on by default) is one journaled step like any other, but while it runs it says what the model
//! has written so far: [`RunEvent::TextDelta`] events, the pieces of one **stream** whose id is
//! made inside the step and recorded with the answer, so a replay (no model call, no pieces) still
//! knows which stream the words were. `adam-a2a-runtime` serves them as `text-stream/v1`.
//!
//! What the model's client yields in dribs is cut into pieces here ([`Coalescer`]): one goes out when
//! [`FLUSH_BYTES`] have gathered or [`FLUSH_INTERVAL`] has passed since the last one, whichever
//! comes first (the first at once, so the first word is not held), never more than
//! [`MAX_TEXT_DELTA_BYTES`] at a time. A stream is opened by the first word that is not blank, so a
//! turn that only breathes before a tool call opens none.

use std::time::Duration;

use adam_model::{DynModel, ModelDelta, ModelError, ModelRequest, ModelResponse};
use adam_runtime::{CancelToken, Emitter, MAX_TEXT_DELTA_BYTES, RunEvent, floor_boundary};
use futures::StreamExt;
use tokio::time::{Instant, sleep_until};

/// The longest text waits before it is sent. The contract (`text-stream/v1`) asks agents for a chunk
/// at most every 100 ms or every 200 bytes, whichever comes first.
pub(crate) const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// The most bytes that gather before a piece is sent.
pub(crate) const FLUSH_BYTES: usize = 200;

/// A piece of the text, ready to send: where it begins (in bytes of the whole text) and what it is.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Piece {
    pub(crate) offset: u64,
    pub(crate) text: String,
}

/// Decides when the text that arrives is sent, and in what pieces. Pure: the caller says what time
/// it is.
#[derive(Debug)]
pub(crate) struct Coalescer {
    interval: Duration,
    flush_bytes: usize,
    /// Everything that arrived, sent or not.
    text: String,
    /// How much of `text`, in bytes, has been sent.
    sent: usize,
    /// When the last piece went out. `None` until the first.
    flushed: Option<Instant>,
    /// Whether a word that is not blank has arrived: nothing is sent before it.
    open: bool,
}

impl Coalescer {
    pub(crate) fn new(interval: Duration, flush_bytes: usize) -> Self {
        Self {
            interval,
            flush_bytes,
            text: String::new(),
            sent: 0,
            flushed: None,
            open: false,
        }
    }

    /// The text that arrived, sent or not.
    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Whether the stream has been opened: whether a first piece has been sent, or is due.
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// `text` arrived at `now`: the pieces that are due.
    pub(crate) fn push(&mut self, text: &str, now: Instant) -> Vec<Piece> {
        self.text.push_str(text);
        self.open |= text.chars().any(|c| !c.is_whitespace());
        self.due(now, false)
    }

    /// When the text that is waiting is due, if any is: the moment to call [`Coalescer::tick`].
    pub(crate) fn deadline(&self) -> Option<Instant> {
        if !self.open || self.sent == self.text.len() {
            return None;
        }
        self.flushed.map(|at| at + self.interval)
    }

    /// `now` has come: the pieces that are due, if the text that waits has waited long enough.
    pub(crate) fn tick(&mut self, now: Instant) -> Vec<Piece> {
        self.due(now, false)
    }

    /// The text is whole, and is `whole`: everything that has not been sent, whatever the time, and
    /// the end of it (the model's answer is what counts: text it did not stream, if its answer
    /// carries more than it streamed, is the tail). Empty when nothing was ever sent.
    pub(crate) fn finish(&mut self, whole: &str, now: Instant) -> Vec<Piece> {
        if let Some(tail) = whole.strip_prefix(self.text.as_str()) {
            let tail = tail.to_owned();
            self.text.push_str(&tail);
            self.open |= tail.chars().any(|c| !c.is_whitespace());
        }
        self.due(now, true)
    }

    fn due(&mut self, now: Instant, force: bool) -> Vec<Piece> {
        if !self.open || self.sent == self.text.len() {
            return Vec::new();
        }
        let waiting = self.text.len() - self.sent;
        let due = force
            || waiting >= self.flush_bytes
            || self
                .flushed
                .is_none_or(|at| now.saturating_duration_since(at) >= self.interval);
        if !due {
            return Vec::new();
        }
        let mut pieces = Vec::new();
        while self.sent < self.text.len() {
            let rest = &self.text[self.sent..];
            let end = floor_boundary(rest, MAX_TEXT_DELTA_BYTES);
            pieces.push(Piece {
                offset: self.sent as u64,
                text: rest[..end].to_owned(),
            });
            self.sent += end;
        }
        self.flushed = Some(now);
        pieces
    }

    /// Where the next piece would begin: how much has been sent, in bytes.
    pub(crate) fn sent(&self) -> u64 {
        self.sent as u64
    }
}

/// What one streamed model call said and wrote.
#[derive(Debug)]
pub(crate) struct Streamed {
    pub(crate) response: ModelResponse,
    /// The stream the words were sent as, when any were.
    pub(crate) stream: Option<String>,
}

/// Why [`stream_response`] gave no response.
#[derive(Debug)]
pub(crate) enum StreamStop {
    /// The model's client failed, before the first byte or in the middle of the answer.
    Failed(ModelError),
    /// The run was cancelled: the request was dropped, and with it the connection, wherever the
    /// answer had got to.
    Cancelled,
}

impl From<ModelError> for StreamStop {
    fn from(error: ModelError) -> Self {
        Self::Failed(error)
    }
}

/// Calls `model` with [`ModelClient::stream`](adam_model::ModelClient::stream) and sends the words
/// as [`RunEvent::TextDelta`] events through `emitter` while they arrive, and the reasoning that
/// comes before them as [`RunEvent::ReasoningDelta`] events of a stream of its own. Returns the
/// assembled response (the stream's last item) and the id of the stream the words were sent as, if
/// any were.
///
/// A failure is the failure of the call, whether the model's client gave it before the first byte or
/// in the middle of the answer, so the caller treats it as it treats a failed
/// [`ModelClient::complete`](adam_model::ModelClient::complete); a stream that was open says it is
/// abandoned first.
///
/// `cancel` is the run's: when it fires, at any point (the request still being made, the answer
/// still coming, the model silent in the middle of it) the request and the stream are dropped at
/// once, an open stream says it is abandoned, and the call is [`StreamStop::Cancelled`].
///
/// `new_stream` makes the id of the stream of words, called when the first word that is not blank
/// arrives, and `new_reasoning_stream` the id of the stream of reasoning, called when the first
/// reasoning that is not blank does. The reasoning stream ends (`last`) when the words, a tool call
/// or the end of the answer begin, so it is always over before the words of its turn are.
pub(crate) async fn stream_response(
    model: &DynModel,
    request: ModelRequest,
    emitter: &Emitter,
    cancel: &CancelToken,
    new_stream: impl FnOnce() -> String + Send + 'static,
    new_reasoning_stream: impl FnOnce() -> String + Send + 'static,
) -> Result<Streamed, StreamStop> {
    let mut deltas = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(StreamStop::Cancelled),
        started = model.stream(request) => started?,
    };
    let mut sender = Sender {
        emitter,
        words: Lane::new(Kind::Words, Box::new(new_stream)),
        reasoning: Lane::new(Kind::Reasoning, Box::new(new_reasoning_stream)),
    };
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                sender.abandon().await;
                return Err(StreamStop::Cancelled);
            }
            () = sleep_until_due(sender.deadline()) => {
                sender.tick().await;
                continue;
            }
            item = deltas.next() => item,
        };
        match next {
            Some(Ok(ModelDelta::Reasoning(thought))) => {
                sender.reasoning.push(emitter, &thought).await
            }
            Some(Ok(ModelDelta::Text(text))) => {
                // The reasoning is over once the words begin.
                sender.reasoning.finish(emitter, None).await;
                sender.words.push(emitter, &text).await;
            }
            Some(Ok(ModelDelta::ToolCallStarted { .. })) => {
                sender.reasoning.finish(emitter, None).await;
            }
            Some(Ok(ModelDelta::Finished(response))) => {
                sender
                    .reasoning
                    .finish(
                        emitter,
                        Some(response.reasoning.as_deref().unwrap_or_default()),
                    )
                    .await;
                sender
                    .words
                    .finish(emitter, Some(&response.message.text()))
                    .await;
                return Ok(Streamed {
                    response,
                    stream: sender.words.stream,
                });
            }
            Some(Err(error)) => {
                sender.abandon().await;
                return Err(StreamStop::Failed(error));
            }
            None => {
                sender.abandon().await;
                return Err(StreamStop::Failed(ModelError::protocol(
                    "the model's stream ended without a final message",
                )));
            }
        }
    }
}

/// Says the reasoning of an answer that was **not** streamed (`stream_text` off, or a client that
/// cannot stream) as a stream of its own, in pieces, ended: it arrives whole, so it is cut and sent
/// at once. Nothing is sent for reasoning that is blank. Returns the id of the stream, when there
/// was one.
pub(crate) async fn say_reasoning(
    emitter: &Emitter,
    reasoning: &str,
    new_stream: impl FnOnce() -> String + Send + 'static,
) -> Option<String> {
    let mut lane = Lane::new(Kind::Reasoning, Box::new(new_stream));
    lane.push(emitter, reasoning).await;
    lane.finish(emitter, Some(reasoning)).await;
    lane.stream
}

/// Resolves at `at`; never, when there is nothing waiting to be sent.
async fn sleep_until_due(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Which of the two streams of a model turn a [`Lane`] sends: they have the same shape and different
/// events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// What the model writes as its answer: [`RunEvent::TextDelta`].
    Words,
    /// What it thinks before: [`RunEvent::ReasoningDelta`].
    Reasoning,
}

type NewStream = Box<dyn FnOnce() -> String + Send>;

/// One stream of a model turn: its coalescer, its id (made when it opens) and whether it has ended.
struct Lane {
    kind: Kind,
    coalescer: Coalescer,
    new_stream: Option<NewStream>,
    stream: Option<String>,
    ended: bool,
}

impl Lane {
    fn new(kind: Kind, new_stream: NewStream) -> Self {
        Self {
            kind,
            coalescer: Coalescer::new(FLUSH_INTERVAL, FLUSH_BYTES),
            new_stream: Some(new_stream),
            stream: None,
            ended: false,
        }
    }

    async fn push(&mut self, emitter: &Emitter, text: &str) {
        if self.ended {
            return;
        }
        let pieces = self.coalescer.push(text, Instant::now());
        self.send(emitter, pieces, false, false).await;
    }

    async fn tick(&mut self, emitter: &Emitter) {
        let pieces = self.coalescer.tick(Instant::now());
        self.send(emitter, pieces, false, false).await;
    }

    /// The lane is done: what has not gone goes, and the last piece says it is the last. `whole` is
    /// what the model's answer says the text is (it may carry a tail the stream did not); `None`
    /// when the end is only the end. A lane that already ended, or never opened, says nothing.
    async fn finish(&mut self, emitter: &Emitter, whole: Option<&str>) {
        if self.ended {
            return;
        }
        let pieces = self
            .coalescer
            .finish(whole.unwrap_or_default(), Instant::now());
        self.send(emitter, pieces, true, false).await;
        // A lane that never opened is over too: what it is sent later (reasoning that comes after the
        // words began) is not a stream that begins after them.
        self.ended = true;
    }

    /// The model failed: what has not gone goes, and the last piece says there is no more.
    async fn abandon(&mut self, emitter: &Emitter) {
        if self.ended {
            return;
        }
        let pieces = self.coalescer.finish("", Instant::now());
        self.send(emitter, pieces, true, true).await;
    }

    fn event(
        &self,
        stream: String,
        offset: u64,
        text: String,
        last: bool,
        abandoned: bool,
    ) -> RunEvent {
        match self.kind {
            Kind::Words => RunEvent::TextDelta {
                stream,
                offset,
                text,
                last,
                abandoned,
            },
            Kind::Reasoning => RunEvent::ReasoningDelta {
                stream,
                offset,
                text,
                last,
                abandoned,
            },
        }
    }

    /// Sends `pieces` as events of the stream, opening it if this is the first. With `end`, the last
    /// of them (an empty one, if the text had all gone) ends the stream, and `abandoned` says the
    /// model did not finish. Nothing is sent for a stream that was never opened.
    async fn send(&mut self, emitter: &Emitter, pieces: Vec<Piece>, end: bool, abandoned: bool) {
        if !self.coalescer.is_open() || (pieces.is_empty() && !end) {
            return;
        }
        let stream = match &self.stream {
            Some(stream) => stream.clone(),
            None => {
                let Some(make) = self.new_stream.take() else {
                    return;
                };
                let id = make();
                self.stream = Some(id.clone());
                id
            }
        };
        if end {
            self.ended = true;
        }
        let mut pieces = pieces.into_iter().peekable();
        if end && pieces.peek().is_none() {
            // Everything had gone already: the end is a piece of its own, with nothing in it.
            let event = self.event(
                stream,
                self.coalescer.sent(),
                String::new(),
                true,
                abandoned,
            );
            emitter.emit(event).await;
            return;
        }
        while let Some(piece) = pieces.next() {
            let last = end && pieces.peek().is_none();
            let event = self.event(
                stream.clone(),
                piece.offset,
                piece.text,
                last,
                last && abandoned,
            );
            emitter.emit(event).await;
        }
    }
}

/// The half of [`stream_response`] that says things: the two lanes of a turn, reasoning and words.
struct Sender<'a> {
    emitter: &'a Emitter,
    words: Lane,
    reasoning: Lane,
}

impl Sender<'_> {
    /// When what waits in either lane is due.
    fn deadline(&self) -> Option<Instant> {
        [
            self.words.coalescer.deadline(),
            self.reasoning.coalescer.deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    async fn tick(&mut self) {
        self.reasoning.tick(self.emitter).await;
        self.words.tick(self.emitter).await;
    }

    /// The model failed: both lanes that are open say there is no more, the reasoning first.
    async fn abandon(&mut self) {
        self.reasoning.abandon(self.emitter).await;
        self.words.abandon(self.emitter).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use adam_core::RunId;
    use adam_model::{Message, MockModel, ToolCall};
    use adam_runtime::CollectingSink;
    use futures::stream::{self, BoxStream};
    use serde_json::json;

    use super::*;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn coalescer() -> Coalescer {
        Coalescer::new(FLUSH_INTERVAL, FLUSH_BYTES)
    }

    fn texts(pieces: &[Piece]) -> Vec<(u64, &str)> {
        pieces.iter().map(|p| (p.offset, p.text.as_str())).collect()
    }

    #[test]
    fn the_first_words_go_at_once_and_the_next_wait_for_the_interval() {
        let t0 = Instant::now();
        let mut c = coalescer();
        assert_eq!(texts(&c.push("Fib", t0)), [(0, "Fib")]);
        // Within the interval: held.
        assert!(c.push("onacci ", at(t0, 10)).is_empty());
        assert!(c.push("in ", at(t0, 99)).is_empty());
        assert_eq!(c.deadline(), Some(at(t0, 100)));
        // At the interval the next push sends everything that waited, as one piece.
        assert_eq!(
            texts(&c.push("Rust.", at(t0, 100))),
            [(3, "onacci in Rust.")]
        );
        assert_eq!(c.deadline(), None);
    }

    #[test]
    fn a_tick_sends_what_waited_when_no_more_comes() {
        let t0 = Instant::now();
        let mut c = coalescer();
        c.push("Hi", t0);
        assert!(c.push(" there", at(t0, 5)).is_empty());
        assert!(c.tick(at(t0, 50)).is_empty());
        assert_eq!(texts(&c.tick(at(t0, 100))), [(2, " there")]);
        assert!(c.tick(at(t0, 500)).is_empty());
    }

    #[test]
    fn enough_bytes_go_before_the_interval() {
        let t0 = Instant::now();
        let mut c = coalescer();
        c.push("a", t0);
        assert!(c.push(&"b".repeat(FLUSH_BYTES - 1), at(t0, 1)).is_empty());
        let sent = c.push("c", at(t0, 2));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].offset, 1);
        assert_eq!(sent[0].text.len(), FLUSH_BYTES);
    }

    #[test]
    fn a_piece_is_never_longer_than_the_bound_and_never_splits_a_character() {
        let t0 = Instant::now();
        let mut c = coalescer();
        // A model that sends everything in one delta: 2500 bytes of two-byte characters.
        let sent = c.push(&"é".repeat(1250), t0);
        assert_eq!(sent.len(), 3);
        assert!(sent.iter().all(|p| p.text.len() <= MAX_TEXT_DELTA_BYTES));
        assert!(sent.iter().all(|p| p.text.chars().all(|c| c == 'é')));
        // The offsets are the sum of the lengths before them, in bytes.
        let mut next = 0;
        for piece in &sent {
            assert_eq!(piece.offset, next);
            next += piece.text.len() as u64;
        }
        assert_eq!(next, 2500);
        assert_eq!(c.sent(), 2500);
    }

    #[test]
    fn no_stream_opens_on_blanks() {
        let t0 = Instant::now();
        let mut c = coalescer();
        assert!(c.push("\n\n", t0).is_empty());
        assert!(!c.is_open());
        assert_eq!(c.deadline(), None);
        assert!(c.finish("\n\n", at(t0, 1)).is_empty());
        assert!(!c.is_open());

        // The blanks before the first word go with it: the offsets are of the whole text.
        let mut c = coalescer();
        assert!(c.push("\n", t0).is_empty());
        assert_eq!(texts(&c.push("Hi", at(t0, 1))), [(0, "\nHi")]);
    }

    #[test]
    fn the_end_sends_the_rest_and_the_tail_the_answer_has_beyond_what_streamed() {
        let t0 = Instant::now();
        let mut c = coalescer();
        c.push("Fib", t0);
        c.push("onacci", at(t0, 1));
        // The answer says a little more than the stream did.
        assert_eq!(
            texts(&c.finish("Fibonacci in Rust.", at(t0, 2))),
            [(3, "onacci in Rust.")]
        );
        assert_eq!(c.text(), "Fibonacci in Rust.");
        assert_eq!(c.sent(), 18);
        // Nothing left: the end has no piece to give, the sender makes an empty one.
        assert!(c.finish("Fibonacci in Rust.", at(t0, 3)).is_empty());
    }

    #[test]
    fn an_answer_that_is_not_what_streamed_adds_nothing() {
        let t0 = Instant::now();
        let mut c = coalescer();
        c.push("Fib", t0);
        // Not an extension of what was sent: the pieces already sent stand; nothing is invented.
        assert!(c.finish("Something else", at(t0, 1)).is_empty());
        assert_eq!(c.text(), "Fib");
    }

    /// The failure of a call that nobody cancelled.
    fn failed(stop: StreamStop) -> ModelError {
        match stop {
            StreamStop::Failed(error) => error,
            StreamStop::Cancelled => panic!("nobody cancelled this call"),
        }
    }

    /// What the events of a model call say, for a model that answers with `script`.
    async fn run(model: Arc<MockModel>) -> (Result<Streamed, ModelError>, Vec<RunEvent>) {
        let sink = CollectingSink::new();
        let run = RunId::new();
        let emitter = Emitter::new(run, "test", Arc::new(sink.clone()));
        let dynamic: DynModel = model;
        let result = stream_response(
            &dynamic,
            ModelRequest::new("m"),
            &emitter,
            &CancelToken::new(),
            || "s1".to_owned(),
            || "r1".to_owned(),
        )
        .await
        .map_err(failed);
        (result, sink.events_for(run))
    }

    fn pieces(events: &[RunEvent]) -> Vec<(u64, String, bool, bool)> {
        events
            .iter()
            .map(|e| match e {
                RunEvent::TextDelta {
                    stream,
                    offset,
                    text,
                    last,
                    abandoned,
                } => {
                    assert_eq!(stream, "s1");
                    (*offset, text.clone(), *last, *abandoned)
                }
                other => panic!("not a text delta: {other:?}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn a_text_answer_is_sent_in_pieces_that_end_with_an_empty_last_one() {
        let model = Arc::new(MockModel::new());
        model.push_text("Fibonacci in Rust.");
        let (result, events) = run(model).await;
        let streamed = result.expect("an answer");
        assert_eq!(streamed.stream.as_deref(), Some("s1"));
        assert_eq!(streamed.response.message.text(), "Fibonacci in Rust.");
        // The mock streams the answer as one piece: the end is a piece of its own.
        assert_eq!(
            pieces(&events),
            [
                (0, "Fibonacci in Rust.".to_owned(), false, false),
                (18, String::new(), true, false)
            ]
        );
    }

    #[tokio::test]
    async fn an_answer_with_tool_calls_and_no_words_opens_no_stream() {
        let model = Arc::new(MockModel::new());
        model.push_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "t".into(),
            arguments: json!({}),
        }]);
        let (result, events) = run(model).await;
        let streamed = result.expect("an answer");
        assert_eq!(streamed.stream, None);
        assert!(events.is_empty());
        assert_eq!(streamed.response.message.tool_calls().len(), 1);
    }

    #[tokio::test]
    async fn a_failure_before_the_first_byte_is_the_calls_failure_and_says_nothing() {
        let model = Arc::new(MockModel::new());
        model.push_error(ModelError::transient("blip"));
        let (result, events) = run(model).await;
        assert!(matches!(result, Err(ModelError::Transient { .. })));
        assert!(events.is_empty());
    }

    /// A model whose stream is `items`, whatever the request.
    struct Scripted(std::sync::Mutex<Option<Vec<Result<ModelDelta, ModelError>>>>);

    #[async_trait::async_trait]
    impl adam_model::ModelClient for Scripted {
        async fn complete(&self, _: ModelRequest) -> Result<ModelResponse, ModelError> {
            Err(ModelError::invalid_request("not streaming"))
        }

        async fn stream(
            &self,
            _: ModelRequest,
        ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
            let items = self.0.lock().expect("lock").take().expect("one call");
            Ok(stream::iter(items).boxed())
        }
    }

    fn scripted(items: Vec<Result<ModelDelta, ModelError>>) -> DynModel {
        Arc::new(Scripted(std::sync::Mutex::new(Some(items))))
    }

    async fn run_items(
        items: Vec<Result<ModelDelta, ModelError>>,
    ) -> (Result<Streamed, ModelError>, Vec<RunEvent>) {
        let sink = CollectingSink::new();
        let run = RunId::new();
        let emitter = Emitter::new(run, "test", Arc::new(sink.clone()));
        let result = stream_response(
            &scripted(items),
            ModelRequest::new("m"),
            &emitter,
            &CancelToken::new(),
            || "s1".to_owned(),
            || "r1".to_owned(),
        )
        .await
        .map_err(failed);
        (result, sink.events_for(run))
    }

    #[tokio::test]
    async fn a_failure_in_the_middle_ends_the_open_stream_abandoned_with_what_was_written() {
        let (result, events) = run_items(vec![
            Ok(ModelDelta::Text("Fib".into())),
            Ok(ModelDelta::Text("onacci".into())),
            Err(ModelError::transient("connection reset")),
        ])
        .await;
        assert!(matches!(result, Err(ModelError::Transient { .. })));
        // "Fib" went at once; "onacci" waited, and goes with the end.
        assert_eq!(
            pieces(&events),
            [
                (0, "Fib".to_owned(), false, false),
                (3, "onacci".to_owned(), true, true)
            ]
        );
    }

    #[tokio::test]
    async fn a_failure_after_everything_went_ends_with_an_empty_abandoned_piece() {
        let (result, events) = run_items(vec![
            Ok(ModelDelta::Text("Fib".into())),
            Err(ModelError::transient("connection reset")),
        ])
        .await;
        assert!(result.is_err());
        assert_eq!(
            pieces(&events),
            [
                (0, "Fib".to_owned(), false, false),
                (3, String::new(), true, true)
            ]
        );
    }

    #[tokio::test]
    async fn a_failure_with_nothing_written_says_nothing() {
        let (result, events) =
            run_items(vec![Err(ModelError::transient("connection reset"))]).await;
        assert!(result.is_err());
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_its_final_message_is_a_protocol_error_and_is_abandoned() {
        let (result, events) = run_items(vec![Ok(ModelDelta::Text("Fib".into()))]).await;
        assert!(matches!(result, Err(ModelError::Protocol { .. })));
        assert_eq!(
            pieces(&events),
            [
                (0, "Fib".to_owned(), false, false),
                (3, String::new(), true, true)
            ]
        );
    }

    #[tokio::test]
    async fn words_before_a_tool_call_are_a_stream_and_the_calls_are_the_responses() {
        let call = ToolCall {
            id: "c1".into(),
            name: "t".into(),
            arguments: json!({}),
        };
        let response = ModelResponse {
            message: Message::Assistant {
                content: vec![adam_model::ContentPart::text("Let me look.")],
                tool_calls: vec![call],
                reasoning: None,
            },
            finish: adam_model::FinishReason::ToolCalls,
            usage: adam_model::Usage::default(),
            reasoning: None,
        };
        let (result, events) = run_items(vec![
            Ok(ModelDelta::Text("Let me look.".into())),
            Ok(ModelDelta::ToolCallStarted {
                id: "c1".into(),
                name: "t".into(),
            }),
            Ok(ModelDelta::Finished(response)),
        ])
        .await;
        let streamed = result.expect("an answer");
        assert_eq!(streamed.stream.as_deref(), Some("s1"));
        assert_eq!(streamed.response.message.tool_calls().len(), 1);
        assert_eq!(
            pieces(&events),
            [
                (0, "Let me look.".to_owned(), false, false),
                (12, String::new(), true, false)
            ]
        );
    }

    /// A model that writes slowly: a word every 30 ms, then stops for a second before it finishes.
    struct Slow;

    #[async_trait::async_trait]
    impl adam_model::ModelClient for Slow {
        async fn complete(&self, _: ModelRequest) -> Result<ModelResponse, ModelError> {
            Err(ModelError::invalid_request("not streaming"))
        }

        async fn stream(
            &self,
            _: ModelRequest,
        ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
            let words = ["one ", "two ", "three ", "four "];
            let items = stream::iter(words)
                .then(|w| async move {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(ModelDelta::Text(w.to_owned()))
                })
                .chain(stream::once(async {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    Ok(ModelDelta::Finished(ModelResponse::text(
                        "one two three four ",
                    )))
                }));
            Ok(items.boxed())
        }
    }

    /// The timer sends what waits while the model is silent: without it the last words would wait
    /// for the end of the answer.
    #[tokio::test]
    async fn what_waits_is_sent_when_the_interval_is_over_even_if_the_model_goes_quiet() {
        let sink = CollectingSink::new();
        let run = RunId::new();
        let emitter = Emitter::new(run, "test", Arc::new(sink.clone()));
        let model: DynModel = Arc::new(Slow);
        let started = Instant::now();
        let call = tokio::spawn({
            let emitter = emitter.clone();
            async move {
                stream_response(
                    &model,
                    ModelRequest::new("m"),
                    &emitter,
                    &CancelToken::new(),
                    || "s1".to_owned(),
                    || "r1".to_owned(),
                )
                .await
                .map_err(failed)
            }
        });
        // Over half a second in, the words have all been written and the model is silent for
        // another half second: they have gone already, in more than one piece, and the stream is
        // not ended.
        tokio::time::sleep_until(started + Duration::from_millis(600)).await;
        let so_far = pieces(&sink.events_for(run));
        let joined: String = so_far.iter().map(|p| p.1.as_str()).collect();
        assert_eq!(joined, "one two three four ", "{so_far:?}");
        assert!(so_far.iter().all(|p| !p.2), "not ended yet: {so_far:?}");
        assert!(so_far.len() >= 2, "in more than one piece: {so_far:?}");
        let streamed = call.await.expect("joined").expect("an answer");
        assert_eq!(streamed.stream.as_deref(), Some("s1"));
        let all = pieces(&sink.events_for(run));
        assert_eq!(all.last().map(|p| (p.2, p.3)), Some((true, false)));
        // The offsets chain.
        let mut next = 0;
        for (offset, text, _, _) in &all {
            assert_eq!(*offset, next);
            next += text.len() as u64;
        }
    }

    /// A cancel is heard whatever the model is doing: it drops the request, and a stream that is
    /// open says that it is abandoned and what had been written.
    #[tokio::test]
    async fn a_cancel_drops_the_stream_and_ends_an_open_one_abandoned() {
        let sink = CollectingSink::new();
        let run = RunId::new();
        let emitter = Emitter::new(run, "test", Arc::new(sink.clone()));
        let model: DynModel = Arc::new(Slow);
        let cancel = CancelToken::new();
        let call = tokio::spawn({
            let (emitter, cancel) = (emitter.clone(), cancel.clone());
            async move {
                stream_response(
                    &model,
                    ModelRequest::new("m"),
                    &emitter,
                    &cancel,
                    || "s1".to_owned(),
                    || "r1".to_owned(),
                )
                .await
            }
        });
        // The words are written in 120 ms; the model is then silent for a second.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let cancelled_at = Instant::now();
        cancel.cancel();
        let stop = call.await.expect("joined").expect_err("no answer");
        assert!(matches!(stop, StreamStop::Cancelled), "{stop:?}");
        assert!(
            cancelled_at.elapsed() < Duration::from_millis(500),
            "the silent model is not waited for: {:?}",
            cancelled_at.elapsed()
        );
        let all = pieces(&sink.events_for(run));
        let joined: String = all.iter().map(|p| p.1.as_str()).collect();
        assert_eq!(joined, "one two three four ");
        assert_eq!(all.last().map(|p| (p.2, p.3)), Some((true, true)));
    }

    /// A model that has not begun to answer is not waited for either, and a run that is cancelled
    /// already does not get as far as asking.
    #[tokio::test]
    async fn a_cancel_before_the_first_byte_drops_the_request() {
        struct Never;
        #[async_trait::async_trait]
        impl adam_model::ModelClient for Never {
            async fn complete(&self, _: ModelRequest) -> Result<ModelResponse, ModelError> {
                std::future::pending().await
            }
            async fn stream(
                &self,
                _: ModelRequest,
            ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError>
            {
                std::future::pending().await
            }
        }
        let sink = CollectingSink::new();
        let run = RunId::new();
        let emitter = Emitter::new(run, "test", Arc::new(sink.clone()));
        let model: DynModel = Arc::new(Never);
        let cancel = CancelToken::new();
        let fire = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            fire.cancel();
        });
        let stop = tokio::time::timeout(
            Duration::from_secs(2),
            stream_response(
                &model,
                ModelRequest::new("m"),
                &emitter,
                &cancel,
                || "s1".to_owned(),
                || "r1".to_owned(),
            ),
        )
        .await
        .expect("a cancel ends the call")
        .expect_err("no answer");
        assert!(matches!(stop, StreamStop::Cancelled), "{stop:?}");
        assert!(sink.events_for(run).is_empty(), "no stream was opened");
    }
}
