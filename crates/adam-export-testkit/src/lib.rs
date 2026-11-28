//! Conformance suites for the two ports of `adam-export`: [`adam_export::RunSource`] (where runs
//! come from) and [`adam_export::RecordSink`] (where records go).
//!
//! Every adapter runs the same cases, so "passes the testkit" means "the exporter can read from it
//! (or write to it) like from every other". A source's maker is given the runs a case needs
//! ([`SeedRun`]) and puts them where its source reads; a sink's maker gives a fresh sink and a way
//! to read its destination back ([`SinkFixture`]).
//!
//! ```ignore
//! async fn make_source(seeds: Vec<adam_export_testkit::SeedRun>) -> Option<Box<dyn adam_export::RunSource>> {
//!     /* write `seeds` into a fresh store; `None` skips */
//! }
//! adam_export_testkit::source_conformance!(make_source);
//!
//! async fn make_sink() -> Option<adam_export_testkit::SinkFixture> { /* None skips */ }
//! adam_export_testkit::sink_conformance!(make_sink);
//! ```
//!
//! Each case names its agents uniquely, so cases may share one database and run in parallel.

#![warn(missing_docs)]
// A conformance suite is test code: a failed unwrap is a failed assertion.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};

use adam_core::RunId;
use adam_core::store::RunStatus;
use adam_export::{Record, RecordSink, RunArtifact};
use adam_llm_agent::Conversation;
use adam_model::{Message, Usage};
use async_trait::async_trait;
use serde_json::Value;

pub use adam_core::testing::skipped;

/// One run a source case needs, for the maker to put where its source reads.
///
/// Every seeded run has a conversation id, unique to it, and is not open (`done` or `failed`).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SeedRun {
    /// The run's id; the source yields it as `meta.id`.
    pub id: RunId,
    /// Its agent.
    pub agent: String,
    /// Its conversation id.
    pub conversation_id: String,
    /// Its status.
    pub status: RunStatus,
    /// Its JSON artifacts, in the order the run committed them; the source yields them as
    /// `meta.artifacts`. Empty unless a case adds some ([`with_artifact`](Self::with_artifact)).
    pub artifacts: Vec<RunArtifact>,
    /// Its state.
    pub state: SeedState,
}

/// What a seeded run's state is. Closed: a new kind fails to compile in every maker.
#[derive(Debug, Clone, PartialEq)]
pub enum SeedState {
    /// A conversation with messages, which the source yields.
    Conversation(Box<Conversation>),
    /// A readable state that is not a conversation with messages (another kind of agent), which
    /// the source counts in `Skipped::not_a_conversation`.
    Other(Value),
    /// A state the source cannot read (a corrupt document), which it counts in
    /// `Skipped::unreadable`.
    Unreadable,
}

impl SeedRun {
    /// A run of `agent` in `status` whose state is `state`, with a fresh id and conversation id.
    pub fn new(agent: impl Into<String>, status: RunStatus, state: SeedState) -> Self {
        let id = RunId::new();
        Self {
            id,
            agent: agent.into(),
            conversation_id: format!("seed/{id}"),
            status,
            artifacts: Vec::new(),
            state,
        }
    }

    /// With `artifact` after the run's other artifacts.
    #[must_use]
    pub fn with_artifact(mut self, artifact: RunArtifact) -> Self {
        self.artifacts.push(artifact);
        self
    }

    /// A finished run of `agent` whose conversation is [`conversation`]`(text)`.
    pub fn done(agent: impl Into<String>, text: &str) -> Self {
        Self::new(
            agent,
            RunStatus::Done,
            SeedState::Conversation(Box::new(conversation(text))),
        )
    }
}

/// A conversation of one model call: `text` from the person, an answer from the model.
pub fn conversation(text: &str) -> Conversation {
    Conversation {
        messages: vec![
            Message::user_text(text),
            Message::assistant_text(format!("answered: {text}")),
        ],
        turns: 1,
        usage: Usage::new(10, 5),
        ..Conversation::default()
    }
}

/// What a sink's maker gives: a fresh sink, and how a reader of its destination sees it.
pub struct SinkFixture {
    /// The sink under test, empty.
    pub sink: Box<dyn RecordSink>,
    /// Reads its destination back.
    pub readback: Box<dyn Readback>,
}

/// How a case reads a sink's destination, as a reader of it would (a trainer reading the file,
/// a client listing the bucket).
#[async_trait]
pub trait Readback: Send + Sync {
    /// The records the destination holds now, in order.
    async fn records(&self) -> Vec<Record>;
}

/// A byte buffer several handles share: give one handle to a [`JsonLines`](adam_export::JsonLines)
/// (behind a `BufWriter` to check that `finish` flushes) and keep another as its [`Readback`],
/// which reads the bytes as JSON Lines.
#[derive(Debug, Clone, Default)]
pub struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedBuffer {
    /// The bytes written so far.
    pub fn bytes(&self) -> Vec<u8> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Readback for SharedBuffer {
    async fn records(&self) -> Vec<Record> {
        let bytes = self.bytes();
        let text = std::str::from_utf8(&bytes).expect("JSON Lines are UTF-8");
        assert!(
            text.is_empty() || text.ends_with('\n'),
            "every line ends with a newline: {text:?}"
        );
        text.lines()
            .map(|line| serde_json::from_str(line).expect("a line is one record"))
            .collect()
    }
}

/// Generate one `#[tokio::test]` per source case.
///
/// `$make` is a path to an `async fn(Vec<SeedRun>) -> Option<Box<dyn RunSource>>`: it puts the
/// runs where a fresh source reads them and returns that source. `None` skips the suite (and
/// fails instead under `ADAM_TEST_REQUIRE_DB=1`, see [`skipped`]). The calling crate needs
/// `tokio` with `macros` and `rt-multi-thread` as a dev-dependency.
#[macro_export]
macro_rules! source_conformance {
    ($make:path) => {
        $crate::source_conformance!(@cases $make;
            an_empty_source_yields_nothing,
            pages_are_disjoint_and_cover_every_run_once,
            the_cursor_resumes_after_the_last_item,
            skipped_runs_are_counted_not_yielded,
            a_page_holds_only_the_query_s_agent_statuses_and_times,
            a_run_keeps_its_bookkeeping_and_conversation,
            a_run_keeps_its_json_artifacts_in_order,
        );
    };
    (@cases $make:path; $($case:ident),+ $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn $case() {
                $crate::cases::source::$case($make, stringify!($case)).await;
            }
        )+
    };
}

/// Generate one `#[tokio::test]` per sink case.
///
/// `$make` is a path to an `async fn() -> Option<SinkFixture>`: a fresh, empty sink and its
/// [`Readback`]. `None` skips the suite (and fails instead under `ADAM_TEST_REQUIRE_DB=1`). The
/// calling crate needs `tokio` with `macros` and `rt-multi-thread` as a dev-dependency.
#[macro_export]
macro_rules! sink_conformance {
    ($make:path) => {
        $crate::sink_conformance!(@cases $make;
            an_empty_sink_finishes_with_nothing,
            records_arrive_in_order_once_finished,
            finish_twice_is_harmless,
            a_put_after_finish_is_refused,
            a_record_reads_back_whole,
        );
    };
    (@cases $make:path; $($case:ident),+ $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn $case() {
                let Some(fixture) = $make().await else {
                    $crate::skipped(&format!("{}: sink not configured", stringify!($case)));
                    return;
                };
                $crate::cases::sink::$case(fixture).await;
            }
        )+
    };
}

/// The cases, as plain async functions for harnesses that do not use the macros.
pub mod cases {
    /// The cases of [`source_conformance!`](crate::source_conformance).
    pub mod source {
        use std::collections::BTreeSet;
        use std::future::Future;

        use adam_core::RunId;
        use adam_core::store::RunStatus;
        use adam_core::time::Timestamp;
        use adam_export::{RunArtifact, RunSource, Skipped, SourceQuery, SourceRun};
        use serde_json::json;

        use crate::{SeedRun, SeedState};

        /// More pages than any case needs: a source that never ends fails instead of hanging.
        const MAX_PAGES: usize = 1_000;

        /// The source of `seeds`, or `None` after reporting the skip.
        async fn source_of<M, F>(
            make: M,
            seeds: Vec<SeedRun>,
            case: &str,
        ) -> Option<Box<dyn RunSource>>
        where
            M: FnOnce(Vec<SeedRun>) -> F,
            F: Future<Output = Option<Box<dyn RunSource>>>,
        {
            let source = make(seeds).await;
            if source.is_none() {
                crate::skipped(&format!("{case}: source not configured"));
            }
            source
        }

        /// An agent name no other case uses.
        fn agent(case: &str) -> String {
            format!("export-{case}-{}", RunId::new())
        }

        /// Every page of `query` from the first: the runs yielded and the runs skipped.
        async fn drain(source: &dyn RunSource, query: &SourceQuery) -> (Vec<SourceRun>, Skipped) {
            let mut runs = Vec::new();
            let mut skipped = Skipped::default();
            let mut after = None;
            for _ in 0..MAX_PAGES {
                let page = source.page(query, after.as_ref()).await.expect("a page");
                assert!(
                    page.listed() <= u64::try_from(query.limit).expect("a small limit"),
                    "a page lists at most the query's limit: {} over {}",
                    page.listed(),
                    query.limit
                );
                skipped.outside_query += page.skipped.outside_query;
                skipped.not_a_conversation += page.skipped.not_a_conversation;
                skipped.unreadable += page.skipped.unreadable;
                runs.extend(page.runs);
                match page.next {
                    Some(next) => after = Some(next),
                    None => return (runs, skipped),
                }
            }
            panic!("the source gave {MAX_PAGES} pages and did not end");
        }

        fn ids(runs: &[SourceRun]) -> Vec<RunId> {
            runs.iter().map(|run| run.meta.id).collect()
        }

        /// A source with no run yields nothing and ends at once; its label, if any, is not blank.
        pub async fn an_empty_source_yields_nothing<M, F>(make: M, case: &str)
        where
            M: FnOnce(Vec<SeedRun>) -> F,
            F: Future<Output = Option<Box<dyn RunSource>>>,
        {
            let Some(source) = source_of(make, Vec::new(), case).await else {
                return;
            };
            if let Some(label) = source.label() {
                assert!(!label.trim().is_empty(), "a label is a name: {label:?}");
            }
            let page = source
                .page(&SourceQuery::new(agent(case), 10), None)
                .await
                .expect("a page");
            assert!(page.runs.is_empty(), "{page:?}");
            assert_eq!(page.skipped.total(), 0, "{page:?}");
            assert_eq!(page.next, None, "an empty listing is its last page");
        }

        /// Seven runs read three a page: every run once, none twice, nothing else.
        pub async fn pages_are_disjoint_and_cover_every_run_once<M, F>(make: M, case: &str)
        where
            M: FnOnce(Vec<SeedRun>) -> F,
            F: Future<Output = Option<Box<dyn RunSource>>>,
        {
            let agent = agent(case);
            let seeds: Vec<SeedRun> = (0..7)
                .map(|n| SeedRun::done(agent.as_str(), &format!("task {n}")))
                .collect();
            let want: BTreeSet<RunId> = seeds.iter().map(|seed| seed.id).collect();
            let Some(source) = source_of(make, seeds, case).await else {
                return;
            };
            let (runs, skipped) =
                drain(source.as_ref(), &SourceQuery::new(agent.as_str(), 3)).await;
            let got = ids(&runs);
            let distinct: BTreeSet<RunId> = got.iter().copied().collect();
            assert_eq!(
                got.len(),
                distinct.len(),
                "a run was yielded twice: {got:?}"
            );
            assert_eq!(distinct, want);
            assert_eq!(skipped.total(), 0, "{skipped:?}");
        }

        /// The cursor of a page starts the next one after it: the same cursor answers the same
        /// page again, and the pages after it hold the rest.
        pub async fn the_cursor_resumes_after_the_last_item<M, F>(make: M, case: &str)
        where
            M: FnOnce(Vec<SeedRun>) -> F,
            F: Future<Output = Option<Box<dyn RunSource>>>,
        {
            let agent = agent(case);
            let seeds: Vec<SeedRun> = (0..5)
                .map(|n| SeedRun::done(agent.as_str(), &format!("task {n}")))
                .collect();
            let want: BTreeSet<RunId> = seeds.iter().map(|seed| seed.id).collect();
            let Some(source) = source_of(make, seeds, case).await else {
                return;
            };
            let query = SourceQuery::new(agent.as_str(), 2);
            let first = source.page(&query, None).await.expect("a page");
            assert_eq!(first.runs.len(), 2, "{first:?}");
            let cursor = first.next.clone().expect("more pages after a full one");
            let second = source.page(&query, Some(&cursor)).await.expect("a page");
            let again = source.page(&query, Some(&cursor)).await.expect("a page");
            assert_eq!(
                ids(&second.runs),
                ids(&again.runs),
                "a cursor answers one page"
            );
            assert_eq!(second.next, again.next);
            let before: BTreeSet<RunId> = ids(&first.runs).into_iter().collect();
            assert!(
                ids(&second.runs).iter().all(|id| !before.contains(id)),
                "the page after a cursor holds nothing before it"
            );
            let mut seen = before;
            seen.extend(ids(&second.runs));
            let mut after = second.next;
            while let Some(cursor) = after {
                let page = source.page(&query, Some(&cursor)).await.expect("a page");
                for id in ids(&page.runs) {
                    assert!(seen.insert(id), "{id} was yielded twice");
                }
                after = page.next;
                assert!(seen.len() <= want.len(), "more runs than were seeded");
            }
            assert_eq!(seen, want);
        }

        /// Runs whose state is not a conversation, or cannot be read, are counted, not yielded.
        pub async fn skipped_runs_are_counted_not_yielded<M, F>(make: M, case: &str)
        where
            M: FnOnce(Vec<SeedRun>) -> F,
            F: Future<Output = Option<Box<dyn RunSource>>>,
        {
            let agent = agent(case);
            let good = [
                SeedRun::done(agent.as_str(), "one"),
                SeedRun::done(agent.as_str(), "two"),
            ];
            let want: BTreeSet<RunId> = good.iter().map(|seed| seed.id).collect();
            let mut seeds = good.to_vec();
            seeds.extend([
                SeedRun::new(
                    agent.as_str(),
                    RunStatus::Done,
                    SeedState::Other(json!({"kind": "another agent"})),
                ),
                SeedRun::new(
                    agent.as_str(),
                    RunStatus::Done,
                    SeedState::Other(json!({"messages": []})),
                ),
                SeedRun::new(agent.as_str(), RunStatus::Done, SeedState::Unreadable),
            ]);
            let Some(source) = source_of(make, seeds, case).await else {
                return;
            };
            let (runs, skipped) =
                drain(source.as_ref(), &SourceQuery::new(agent.as_str(), 2)).await;
            assert_eq!(ids(&runs).into_iter().collect::<BTreeSet<_>>(), want);
            assert_eq!(skipped.not_a_conversation, 2, "{skipped:?}");
            assert_eq!(skipped.unreadable, 1, "{skipped:?}");
            assert_eq!(skipped.outside_query, 0, "{skipped:?}");
        }

        /// A page yields only runs of its agent, in its statuses, in its time window.
        pub async fn a_page_holds_only_the_query_s_agent_statuses_and_times<M, F>(
            make: M,
            case: &str,
        ) where
            M: FnOnce(Vec<SeedRun>) -> F,
            F: Future<Output = Option<Box<dyn RunSource>>>,
        {
            let (a, b) = (agent(case), agent(case));
            let failed = SeedRun::new(
                a.as_str(),
                RunStatus::Failed,
                SeedState::Conversation(Box::new(crate::conversation("broke"))),
            );
            let seeds = vec![
                SeedRun::done(a.as_str(), "one"),
                SeedRun::done(a.as_str(), "two"),
                failed,
                SeedRun::done(b.as_str(), "other agent"),
            ];
            let Some(source) = source_of(make, seeds, case).await else {
                return;
            };
            let yielded = |query: SourceQuery| {
                let source = source.as_ref();
                async move { drain(source, &query).await.0 }
            };
            let done = yielded(SourceQuery::new(a.as_str(), 10).statuses([RunStatus::Done])).await;
            assert_eq!(done.len(), 2, "{done:?}");
            assert!(
                done.iter()
                    .all(|run| run.meta.agent == a && run.meta.status == RunStatus::Done),
                "{done:?}"
            );
            let every = yielded(SourceQuery::new(a.as_str(), 10)).await;
            assert_eq!(every.len(), 3, "every status: {every:?}");
            assert!(every.iter().all(|run| run.meta.agent == a), "{every:?}");
            let other = yielded(SourceQuery::new(b.as_str(), 10)).await;
            assert_eq!(other.len(), 1, "{other:?}");
            assert_eq!(other[0].meta.agent, b);

            let times: Vec<Timestamp> = every.iter().map(|run| run.meta.updated_at).collect();
            let (first, last) = (
                *times.iter().min().expect("runs"),
                *times.iter().max().expect("runs"),
            );
            let after_last =
                Timestamp::from_millisecond(last.as_millisecond() + 1).expect("an instant");
            let since_first = yielded(SourceQuery::new(a.as_str(), 10).since(Some(first))).await;
            assert_eq!(
                since_first.len(),
                3,
                "`since` is inclusive: {since_first:?}"
            );
            let since_later =
                yielded(SourceQuery::new(a.as_str(), 10).since(Some(after_last))).await;
            assert!(since_later.is_empty(), "{since_later:?}");
            let until_first = yielded(SourceQuery::new(a.as_str(), 10).until(Some(first))).await;
            assert!(
                until_first.is_empty(),
                "`until` is exclusive: {until_first:?}"
            );
            let until_later =
                yielded(SourceQuery::new(a.as_str(), 10).until(Some(after_last))).await;
            assert_eq!(until_later.len(), 3, "{until_later:?}");
        }

        /// A yielded run is the seeded one: its id, agent, conversation id, status and
        /// conversation; it recorded no request, so it has none.
        pub async fn a_run_keeps_its_bookkeeping_and_conversation<M, F>(make: M, case: &str)
        where
            M: FnOnce(Vec<SeedRun>) -> F,
            F: Future<Output = Option<Box<dyn RunSource>>>,
        {
            let agent = agent(case);
            let seed = SeedRun::done(agent.as_str(), "keep me");
            let Some(source) = source_of(make, vec![seed.clone()], case).await else {
                return;
            };
            let (runs, _) = drain(source.as_ref(), &SourceQuery::new(agent.as_str(), 10)).await;
            let [run] = runs.as_slice() else {
                panic!("one run: {runs:?}");
            };
            assert_eq!(run.meta.id, seed.id);
            assert_eq!(run.meta.agent, seed.agent);
            assert_eq!(
                run.meta.conversation_id.as_deref(),
                Some(seed.conversation_id.as_str())
            );
            assert_eq!(run.meta.parent_id, None);
            assert_eq!(run.meta.status, RunStatus::Done);
            assert!(run.meta.created_at <= run.meta.updated_at, "{:?}", run.meta);
            let SeedState::Conversation(conversation) = &seed.state else {
                unreachable!("a done seed is a conversation");
            };
            assert_eq!(run.conversation, **conversation);
            assert!(run.requests.is_empty(), "{:?}", run.requests);
            assert!(run.meta.artifacts.is_empty(), "{:?}", run.meta.artifacts);
        }

        /// A yielded run carries its JSON artifacts, data included and in the order it committed
        /// them: what the outcome contract is read from (ADR 0085, slice E4).
        pub async fn a_run_keeps_its_json_artifacts_in_order<M, F>(make: M, case: &str)
        where
            M: FnOnce(Vec<SeedRun>) -> F,
            F: Future<Output = Option<Box<dyn RunSource>>>,
        {
            let agent = agent(case);
            let seed = SeedRun::done(agent.as_str(), "deliver")
                .with_artifact(RunArtifact::json(
                    adam_export::CHECKS_ARTIFACT,
                    json!({"passed": true}),
                ))
                .with_artifact(RunArtifact::new("note", None, json!("a plain value")))
                .with_artifact(RunArtifact::json(
                    adam_export::PULL_REQUEST_ARTIFACT,
                    json!({"number": "7", "repository": "seed", "verified": true}),
                ));
            let Some(source) = source_of(make, vec![seed.clone()], case).await else {
                return;
            };
            let (runs, _) = drain(source.as_ref(), &SourceQuery::new(agent.as_str(), 10)).await;
            let [run] = runs.as_slice() else {
                panic!("one run: {runs:?}");
            };
            assert_eq!(run.meta.artifacts, seed.artifacts);
        }
    }

    /// The cases of [`sink_conformance!`](crate::sink_conformance).
    pub mod sink {
        use adam_core::RunId;
        use adam_core::store::RunStatus;
        use adam_core::time::Timestamp;
        use adam_error::{Classify, ErrorClass};
        use adam_export::{ExportOptions, Pseudonyms, Record, RunMeta, record_of};

        use crate::SinkFixture;

        /// The record of a finished run asking `text`.
        fn record(text: &str) -> Record {
            let at = Timestamp::from_second(1_800_000_000).expect("an instant");
            let run = RunMeta {
                id: RunId::new(),
                agent: "coder".to_owned(),
                conversation_id: Some(format!("sink/{text}")),
                parent_id: None,
                status: RunStatus::Done,
                created_at: at,
                updated_at: at,
                error: None,
                artifacts: Vec::new(),
            };
            let options = ExportOptions::new(Pseudonyms::new(b"adam-export-testkit"));
            record_of(&run, &crate::conversation(text), &options)
        }

        /// Finishing a sink nothing was put in leaves its destination empty.
        pub async fn an_empty_sink_finishes_with_nothing(mut fixture: SinkFixture) {
            fixture.sink.finish().await.expect("finished");
            assert!(fixture.readback.records().await.is_empty());
        }

        /// Once finished, the destination holds every record put, in order, once.
        pub async fn records_arrive_in_order_once_finished(mut fixture: SinkFixture) {
            let records: Vec<Record> = (0..5).map(|n| record(&format!("task {n}"))).collect();
            for record in &records {
                fixture.sink.put(record).await.expect("put");
            }
            fixture.sink.finish().await.expect("finished");
            assert_eq!(fixture.readback.records().await, records);
        }

        /// A second `finish` succeeds and adds nothing.
        pub async fn finish_twice_is_harmless(mut fixture: SinkFixture) {
            let one = record("once");
            fixture.sink.put(&one).await.expect("put");
            fixture.sink.finish().await.expect("finished");
            fixture.sink.finish().await.expect("finished again");
            assert_eq!(fixture.readback.records().await, [one]);
        }

        /// A `put` after `finish` is refused as the caller's mistake, and writes nothing.
        pub async fn a_put_after_finish_is_refused(mut fixture: SinkFixture) {
            let before = record("before");
            fixture.sink.put(&before).await.expect("put");
            fixture.sink.finish().await.expect("finished");
            let e = fixture
                .sink
                .put(&record("after"))
                .await
                .expect_err("a put after finish");
            assert_eq!(e.class(), ErrorClass::Internal, "{e}");
            assert_eq!(fixture.readback.records().await, [before]);
        }

        /// A record with every optional field set (a source, a parent, a sample, reasoning, an
        /// outcome with a pull request and checks) reads back equal.
        pub async fn a_record_reads_back_whole(mut fixture: SinkFixture) {
            let mut whole = record("whole");
            whole.source = Some("testkit".to_owned());
            whole.parent = Some("0".repeat(32));
            whole.train_on = adam_export::TrainOn::Last;
            whole.sample = Some(adam_export::Sample {
                turn: 0,
                index: 0,
                of: 1,
                exact: true,
            });
            whole
                .outcome
                .pull_requests
                .push(adam_export::PullRequestOutcome {
                    number: 7,
                    repository: Some("1".repeat(32)),
                    verified: true,
                });
            whole.outcome.checks = Some(adam_export::ChecksOutcome {
                passed: true,
                reports: 2,
                failed: 1,
            });
            whole.messages.push(adam_export::ChatMessage::Assistant {
                content: "line one\nline \"two\"\u{2028}".to_owned(),
                tool_calls: Vec::new(),
                reasoning_content: Some("because".to_owned()),
            });
            fixture.sink.put(&whole).await.expect("put");
            fixture.sink.finish().await.expect("finished");
            assert_eq!(fixture.readback.records().await, [whole]);
        }
    }
}
