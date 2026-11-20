//! The chat loop of the example, apart from `main` so that a test drives it
//! (`tests/example.rs`): lines in, the runs' events out.
//!
//! A plain line is a `send` to the current conversation (the first starts one). `/steer <text>`,
//! `/answer <text>` and `/cancel` reach the run being followed, `/list` lists the engine's runs and
//! `/quit` (or the end of the input) shuts the engine down. While a run is followed its events are
//! printed as they come: the text as it streams, the question when it waits, `Behind` when the
//! reader lost events, and how it ended.

use std::io::{self, Write};

use adam_engine::adam_core::RunId;
use adam_engine::adam_runtime::RunEvent;
use adam_engine::{
    ConversationId, Delivered, Engine, EngineError, EngineEvent, ListQuery, RunEvents, RunInfo,
    RunState, Say,
};
use futures::StreamExt;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// What `/help` and an unknown command print.
pub const HELP: &str = "type a message, or /steer <text>, /answer <text>, /cancel, /list, /quit";

/// Read `input` a line at a time and write what happens to `out`, until `/quit` or the end of
/// `input`; then shut `engine` down.
pub async fn run(
    engine: Engine,
    input: impl AsyncBufRead + Unpin,
    out: impl Write,
) -> io::Result<()> {
    let mut chat = Chat {
        engine,
        out: Out {
            w: out,
            mid_line: false,
        },
        conversation: None,
        followed: None,
    };
    let mut lines = input.lines();
    loop {
        // Events first when both are ready, so what a run said is printed before the next command.
        tokio::select! {
            biased;
            Some(event) = next_event(&mut chat.followed) => chat.on_event(event).await?,
            line = lines.next_line() => match line? {
                Some(line) => {
                    if !chat.on_line(line.trim()).await? {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    let stopped = chat.engine.shutdown().await;
    match stopped {
        Ok(()) => chat.out.line("bye"),
        Err(e) => chat.out.line(&format!("! the engine stopped with: {e}")),
    }
}

/// The next event of the followed run; never ready when no run is followed.
async fn next_event(followed: &mut Option<Followed>) -> Option<Result<EngineEvent, EngineError>> {
    match followed {
        Some(followed) => followed.events.next().await,
        None => std::future::pending().await,
    }
}

/// The run whose events are printed.
struct Followed {
    run: RunId,
    events: RunEvents,
    /// The text stream being printed, and how many of its bytes are shown.
    text: Option<(String, u64)>,
    /// Some text of this run was shown.
    shown: bool,
    /// Events were lost: the end prints the whole output.
    lost: bool,
    /// The question last printed, so a second read of the same wait prints nothing.
    asked: Option<String>,
}

struct Chat<W> {
    engine: Engine,
    out: Out<W>,
    conversation: Option<ConversationId>,
    followed: Option<Followed>,
}

impl<W: Write> Chat<W> {
    /// One line of input; `false` to stop.
    async fn on_line(&mut self, line: &str) -> io::Result<bool> {
        let (command, rest) = match line.split_once(' ') {
            Some((command, rest)) => (command, rest.trim()),
            None => (line, ""),
        };
        match command {
            "" => {}
            "/quit" => return Ok(false),
            "/list" => self.list().await?,
            "/cancel" => {
                if let Some(run) = self.run()? {
                    match self.engine.cancel(run).await {
                        // The stream says it ended.
                        Ok(_) => {}
                        Err(e) => self.out.refused(&e)?,
                    }
                }
            }
            "/steer" | "/answer" if rest.is_empty() => self.out.line(HELP)?,
            "/steer" => {
                if let Some(run) = self.run()? {
                    match self.engine.steer(run, Say::new(rest)).await {
                        Ok(_) => self.out.line("[steered]")?,
                        Err(e) => self.out.refused(&e)?,
                    }
                }
            }
            "/answer" => {
                if let Some(run) = self.run()? {
                    match self.engine.answer(run, Say::new(rest)).await {
                        Ok(_) => {
                            self.answered();
                            self.out.line("[answered]")?;
                        }
                        Err(e) => self.out.refused(&e)?,
                    }
                }
            }
            _ if command.starts_with('/') => self.out.line(HELP)?,
            _ => self.send(line).await?,
        }
        Ok(true)
    }

    /// A message to the conversation: a new run, or the open one's steer or answer.
    async fn send(&mut self, text: &str) -> io::Result<()> {
        let turn = match self
            .engine
            .send(self.conversation.as_ref(), Say::new(text))
            .await
        {
            Ok(turn) => turn,
            Err(e) => return self.out.refused(&e),
        };
        self.conversation = Some(turn.conversation);
        match turn.delivered {
            Delivered::Answered => {
                self.answered();
                self.out.line("[answered]")?;
            }
            Delivered::Steered => self.out.line("[steered]")?,
            Delivered::Started | Delivered::Continued | Delivered::Repeated => {}
        }
        if self.followed.as_ref().map(|f| f.run) == Some(turn.run) {
            return Ok(());
        }
        let events = match self.engine.events(turn.run).await {
            Ok(events) => events,
            Err(e) => return self.out.refused(&e),
        };
        self.followed = Some(Followed {
            run: turn.run,
            events,
            text: None,
            shown: false,
            lost: false,
            asked: None,
        });
        // Subscribed first, then read: a question asked before the subscription is in this read,
        // one asked after it comes with a `Status` event.
        match self.engine.get(turn.run).await {
            Ok(Some(info)) => self.waits(&info),
            Ok(None) => Ok(()),
            Err(e) => self.out.refused(&e),
        }
    }

    /// The followed run took an answer: a question it asks next is printed, the same one too.
    fn answered(&mut self) {
        if let Some(followed) = &mut self.followed {
            followed.asked = None;
        }
    }

    /// The run a command is for: the followed one.
    fn run(&mut self) -> io::Result<Option<RunId>> {
        match &self.followed {
            Some(followed) => Ok(Some(followed.run)),
            None => {
                self.out.line("! no run is open: type a message first")?;
                Ok(None)
            }
        }
    }

    async fn list(&mut self) -> io::Result<()> {
        let page = match self.engine.list(ListQuery::new()).await {
            Ok(page) => page,
            Err(e) => return self.out.refused(&e),
        };
        self.out
            .line(&format!("{} run(s), the latest first:", page.total))?;
        for info in &page.items {
            let said = info
                .output
                .as_deref()
                .or(info.question.as_deref())
                .or(info.error.as_deref())
                .unwrap_or("");
            self.out.line(&format!(
                "  {} {:?}: {}",
                short(info.run),
                info.state,
                first_line(said)
            ))?;
        }
        Ok(())
    }

    async fn on_event(&mut self, event: Result<EngineEvent, EngineError>) -> io::Result<()> {
        let Some(followed) = &mut self.followed else {
            return Ok(());
        };
        match event {
            Ok(EngineEvent::Live(RunEvent::TextDelta {
                stream,
                offset,
                text,
                last,
                ..
            })) => {
                // A piece may come twice (after a `Snapshot`): print only what is new.
                let shown = match &mut followed.text {
                    Some((current, shown)) if *current == stream => shown,
                    text => {
                        self.out.end_line()?;
                        &mut text.insert((stream, 0)).1
                    }
                };
                let end = offset + text.len() as u64;
                if end > *shown {
                    let from = usize::try_from(shown.saturating_sub(offset)).unwrap_or(0);
                    if offset > *shown {
                        followed.lost = true;
                    }
                    if let Some(new) = text.get(from..) {
                        self.out.text(new)?;
                        followed.shown = true;
                    }
                    *shown = end;
                }
                if last {
                    self.out.end_line()?;
                }
            }
            Ok(EngineEvent::Live(RunEvent::Status { .. })) => {
                // A prompt to read the run: the store says whether it waits, and on what.
                let run = followed.run;
                match self.engine.get(run).await {
                    Ok(Some(info)) => self.waits(&info)?,
                    Ok(None) => {}
                    Err(e) => self.out.refused(&e)?,
                }
            }
            Ok(EngineEvent::Live(_)) => {}
            Ok(EngineEvent::Behind { skipped }) => {
                followed.lost = true;
                self.out
                    .line(&format!("[behind: {skipped} events skipped]"))?;
            }
            Ok(EngineEvent::Snapshot(info)) => self.waits(&info)?,
            Ok(EngineEvent::Ended(info)) => {
                let whole = followed.lost || !followed.shown;
                self.followed = None;
                self.out.end_line()?;
                match (info.state, info.output.as_deref()) {
                    (RunState::Done, Some(output)) if whole => self.out.line(output)?,
                    _ => {}
                }
                let state = format!("{:?}", info.state).to_lowercase();
                match info.error.as_deref() {
                    Some(reason) => self.out.line(&format!("[{state}: {reason}]"))?,
                    None => self.out.line(&format!("[{state}]"))?,
                }
            }
            Ok(_) => {}
            Err(e) => {
                self.followed = None;
                self.out.refused(&e)?;
            }
        }
        Ok(())
    }

    /// Print the question of a waiting run, once.
    fn waits(&mut self, info: &RunInfo) -> io::Result<()> {
        let (Some(followed), RunState::Waiting, Some(question)) =
            (&mut self.followed, info.state, info.question.as_deref())
        else {
            return Ok(());
        };
        if followed.asked.as_deref() == Some(question) {
            return Ok(());
        }
        followed.asked = Some(question.to_owned());
        self.out.end_line()?;
        self.out.line(&format!("? {question}"))?;
        self.out
            .line("  (answer with /answer <text>, or just type it)")
    }
}

/// The output, and whether a streamed text left its line open.
struct Out<W> {
    w: W,
    mid_line: bool,
}

impl<W: Write> Out<W> {
    fn text(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.w.write_all(text.as_bytes())?;
        self.mid_line = !text.ends_with('\n');
        self.w.flush()
    }

    fn end_line(&mut self) -> io::Result<()> {
        if self.mid_line {
            self.mid_line = false;
            self.w.write_all(b"\n")?;
        }
        Ok(())
    }

    fn line(&mut self, line: &str) -> io::Result<()> {
        self.end_line()?;
        writeln!(self.w, "{line}")?;
        self.w.flush()
    }

    /// A call the engine refused, with the class a host decides from.
    fn refused(&mut self, e: &EngineError) -> io::Result<()> {
        use adam_engine::adam_error::Classify;
        self.line(&format!("! {:?}: {e}", e.class()))
    }
}

fn short(run: RunId) -> String {
    run.to_string().chars().take(8).collect()
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}
