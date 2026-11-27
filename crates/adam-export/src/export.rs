//! The pipeline: read the runs of each source, turn each into its records, put the ones that pass
//! into one sink.

use std::collections::BTreeSet;
use std::io::Write;

use adam_error::{Classify, ErrorClass};
use adam_runtime::Runtime;

use crate::{
    ExportOptions, Filter, JsonLines, Record, RecordSink, RunSource, RuntimeSource, Samples,
    SinkError, SourceError, SourcePage, SourceQuery, SourceRun, estimated_tokens, record_with,
    samples_of, turns_lost_to_compaction,
};

/// Runs a source lists per page.
const PAGE: usize = 100;

/// What an export did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// Runs listed.
    pub seen: u64,
    /// Records written: runs, or samples with [`Samples::Turn`].
    pub written: u64,
    /// Runs the filter refused.
    pub filtered: u64,
    /// Records left out because their estimated tokens are over
    /// [`ExportOptions::max_tokens`]: runs, or samples with [`Samples::Turn`].
    pub over_budget: u64,
    /// With [`Samples::Turn`]: the turns of the runs written before their last compaction, which
    /// have no sample ([`turns_lost_to_compaction`]).
    pub turns_lost_to_compaction: u64,
    /// With [`Samples::Turn`]: the samples written with `exact: false`.
    pub inexact: u64,
    /// Runs whose state is not a conversation with messages (another kind of agent, or a run
    /// that never started).
    pub not_a_conversation: u64,
    /// Runs whose state could not be read; each one is logged with its id.
    pub unreadable: u64,
}

impl Summary {
    /// Count what a page listed and did not yield.
    fn count_listed(&mut self, page: &SourcePage) {
        self.seen += page.listed();
        self.filtered += page.skipped.outside_query;
        self.not_a_conversation += page.skipped.not_a_conversation;
        self.unreadable += page.skipped.unreadable;
    }
}

/// Why an export stopped.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExportError {
    /// A source could not answer a page.
    #[error("listing the runs failed")]
    Source(#[from] SourceError),
    /// The sink refused a record, or could not finish.
    #[error("writing a record failed")]
    Sink(#[from] SinkError),
    /// The sources cannot be told apart in the records: with several, each needs a label of its
    /// own. Nothing was read.
    #[error("{0}")]
    Labels(String),
}

impl Classify for ExportError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Source(e) => e.class(),
            Self::Sink(e) => e.class(),
            Self::Labels(_) => ErrorClass::Invalid,
        }
    }
}

/// Exports runs from sources into a sink: the records a [`Filter`] accepts, made with
/// [`ExportOptions`].
///
/// One record per run, or one per assistant turn with [`ExportOptions::samples`]
/// ([`samples_of`]; the samples of a run oldest first). A record over
/// [`ExportOptions::max_tokens`] is left out and counted. With [`Samples::Turn`] a compacted run
/// is kept whatever [`Filter::include_compacted`] says: its samples start at its last compaction,
/// so they are what the model was sent.
#[derive(Debug, Clone)]
pub struct Exporter {
    filter: Filter,
    /// What a record must pass: `filter`, keeping compacted runs in turn mode.
    record_filter: Filter,
    options: ExportOptions,
}

impl Exporter {
    /// An export of what `filter` accepts, made with `options`.
    pub fn new(filter: Filter, options: ExportOptions) -> Self {
        let record_filter = match options.samples {
            Samples::Run => filter.clone(),
            Samples::Turn => Filter {
                include_compacted: true,
                ..filter.clone()
            },
        };
        Self {
            filter,
            record_filter,
            options,
        }
    }

    /// The filter.
    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    /// The options.
    pub fn options(&self) -> &ExportOptions {
        &self.options
    }

    /// Export the runs of every agent of each of `sources`, in order, into `sink`, and finish it.
    ///
    /// Every record of a labelled source names it in `source`. One source may be unnamed (its
    /// records have no `source`); several must each have a label of their own. The pseudonyms
    /// are the same for every source ([`ExportOptions`]' key), so ids link across them.
    ///
    /// A run a source cannot read is counted and the export goes on; a page a source cannot
    /// answer, or a record the sink refuses, stops it.
    ///
    /// # Errors
    ///
    /// [`ExportError`].
    pub async fn run(
        &self,
        sources: &[&dyn RunSource],
        sink: &mut dyn RecordSink,
    ) -> Result<Summary, ExportError> {
        check_labels(sources)?;
        let mut summary = Summary::default();
        for source in sources {
            for agent in source.agents() {
                self.export_agent(*source, agent, sink, &mut summary)
                    .await?;
            }
        }
        sink.finish().await?;
        Ok(summary)
    }

    /// Export the runs of `agent` from `source`, page after page.
    async fn export_agent(
        &self,
        source: &dyn RunSource,
        agent: &str,
        sink: &mut dyn RecordSink,
        summary: &mut Summary,
    ) -> Result<(), ExportError> {
        let query = SourceQuery::of(agent, &self.filter, PAGE);
        let mut after = None;
        loop {
            let page = source.page(&query, after.as_ref()).await?;
            summary.count_listed(&page);
            for run in &page.runs {
                self.export_run(run, source.label(), sink, summary).await?;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(()),
            }
        }
    }

    /// Put the records of `run` that pass into `sink`.
    async fn export_run(
        &self,
        run: &SourceRun,
        label: Option<&str>,
        sink: &mut dyn RecordSink,
        summary: &mut Summary,
    ) -> Result<(), ExportError> {
        let SourceRun {
            meta,
            conversation,
            requests,
        } = run;
        // The last call's, as `Conversation::request` names it: none when that call recorded
        // nothing, never an earlier call's.
        let last_request = conversation
            .request
            .as_ref()
            .and(requests.last())
            .map(|entry| &entry.shape);
        let exported = record_with(meta, conversation, last_request, &self.options);
        if !self.record_filter.accepts(&exported) {
            summary.filtered += 1;
            return Ok(());
        }
        let records = match self.options.samples {
            Samples::Run => vec![exported],
            Samples::Turn => {
                summary.turns_lost_to_compaction +=
                    u64::from(turns_lost_to_compaction(conversation));
                samples_of(meta, conversation, requests, &self.options)
            }
        };
        for mut record in records {
            if self.over_budget(&record) {
                summary.over_budget += 1;
                continue;
            }
            if record.sample.is_some_and(|sample| !sample.exact) {
                summary.inexact += 1;
            }
            record.source = label.map(str::to_owned);
            sink.put(&record).await?;
            summary.written += 1;
        }
        Ok(())
    }

    /// Whether `record`'s estimated tokens are over [`ExportOptions::max_tokens`].
    fn over_budget(&self, record: &Record) -> bool {
        self.options
            .max_tokens
            .is_some_and(|max| estimated_tokens(record) > u64::from(max))
    }
}

/// One source may be unnamed; several must each have a label of their own, not blank.
fn check_labels(sources: &[&dyn RunSource]) -> Result<(), ExportError> {
    if let [only] = sources {
        return match only.label() {
            Some(label) if label.trim().is_empty() => {
                Err(ExportError::Labels("a source's label is blank".into()))
            }
            _ => Ok(()),
        };
    }
    let mut seen = BTreeSet::new();
    for source in sources {
        match source.label() {
            None => {
                return Err(ExportError::Labels(
                    "several sources are exported and one has no label".into(),
                ));
            }
            Some(label) if label.trim().is_empty() => {
                return Err(ExportError::Labels("a source's label is blank".into()));
            }
            Some(label) if !seen.insert(label) => {
                return Err(ExportError::Labels(format!(
                    "two sources are labelled {label:?}"
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Write `record` as one line of JSON, as [`JsonLines`] does.
///
/// # Errors
///
/// The sink's.
pub fn write_record(out: &mut impl Write, record: &Record) -> Result<(), ExportError> {
    Ok(crate::sink::write_line(out, record)?)
}

/// Export the runs of each of `agents` from `runtime`'s store as JSON Lines to `out`, and flush
/// it: the convenience path, an [`Exporter`] over one unnamed [`RuntimeSource`] into
/// [`JsonLines`]. What `Exporter::run` and `RuntimeSource` say holds; each agent must be
/// registered on `runtime`.
///
/// # Errors
///
/// [`ExportError`].
pub async fn export_runs(
    runtime: &Runtime,
    agents: &[String],
    filter: &Filter,
    options: &ExportOptions,
    out: &mut (impl Write + Send),
) -> Result<Summary, ExportError> {
    let source = RuntimeSource::new(runtime.clone()).only(agents.iter().cloned());
    let mut sink = JsonLines::new(out);
    Exporter::new(filter.clone(), options.clone())
        .run(&[&source], &mut sink)
        .await
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::Cursor;

    struct Named(Option<&'static str>);

    #[async_trait]
    impl RunSource for Named {
        fn label(&self) -> Option<&str> {
            self.0
        }

        fn agents(&self) -> &[String] {
            &[]
        }

        async fn page(
            &self,
            _: &SourceQuery,
            _: Option<&Cursor>,
        ) -> Result<SourcePage, SourceError> {
            Ok(SourcePage::default())
        }
    }

    #[test]
    fn several_sources_need_labels_of_their_own() {
        let (a, b, none, blank) = (
            Named(Some("a")),
            Named(Some("b")),
            Named(None),
            Named(Some(" ")),
        );
        assert!(check_labels(&[]).is_ok());
        assert!(check_labels(&[&none]).is_ok());
        assert!(check_labels(&[&a]).is_ok());
        assert!(check_labels(&[&a, &b]).is_ok());
        for refused in [
            &[&blank as &dyn RunSource][..],
            &[&a, &none],
            &[&a, &a],
            &[&a, &blank],
        ] {
            let e = check_labels(refused).expect_err("refused");
            assert_eq!(e.class(), ErrorClass::Invalid, "{e}");
        }
    }
}
