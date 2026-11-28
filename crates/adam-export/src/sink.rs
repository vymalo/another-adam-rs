//! Where records go: the [`RecordSink`] port, and [`JsonLines`], the adapter over any
//! [`std::io::Write`].

use std::io::Write;

use adam_error::{BoxError, Classify, ErrorClass};
use async_trait::async_trait;

use crate::Record;

/// Where an export writes its records: a file, standard output, an object store, a dataset, a
/// table.
///
/// One export owns its sink, so the methods take `&mut self`: the records arrive in the order
/// they were put, and no lock is needed for that. (A `Store` and a `Notifier` take `&self`
/// because many workers share one; a sink has one writer.) The suite `sink_conformance!`
/// (`adam-export-testkit`) checks an implementation.
///
/// The contract:
///
/// * records are kept in the order of the calls to [`put`](Self::put), each once;
/// * after [`finish`](Self::finish) returns `Ok`, every record put is durable and visible to a
///   reader of the destination (before it, a sink may hold them in a buffer);
/// * `finish` is **idempotent**: a second call succeeds and adds nothing;
/// * a `put` after a successful `finish` is [`SinkError::Finished`], and writes nothing.
#[async_trait]
pub trait RecordSink: Send {
    /// Keep `record`, after the ones put before it.
    ///
    /// # Errors
    ///
    /// [`SinkError`]: the destination refused it, or the sink is finished.
    async fn put(&mut self, record: &Record) -> Result<(), SinkError>;

    /// Make every record put so far durable and visible. Idempotent.
    ///
    /// # Errors
    ///
    /// [`SinkError`]: the destination refused it; the sink is then not finished, and `finish`
    /// may be called again.
    async fn finish(&mut self) -> Result<(), SinkError>;
}

#[async_trait]
impl<S: RecordSink + ?Sized> RecordSink for Box<S> {
    async fn put(&mut self, record: &Record) -> Result<(), SinkError> {
        (**self).put(record).await
    }

    async fn finish(&mut self) -> Result<(), SinkError> {
        (**self).finish().await
    }
}

/// Why a sink refused a record or could not finish.
///
/// No backend type appears here: an adapter boxes its backend's error as the `source` of
/// [`Backend`](Self::Backend) after choosing its class.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SinkError {
    /// Writing failed (a full disk, a closed pipe).
    #[error("writing failed")]
    Io(#[from] std::io::Error),
    /// A record was put after the sink finished: the caller's mistake.
    #[error("a record was put after the sink finished")]
    Finished,
    /// The backend refused. The adapter that boxed `source` chose `class`.
    #[error("the sink's backend failed")]
    Backend {
        /// What to do about it.
        class: ErrorClass,
        /// The backend's own error.
        #[source]
        source: BoxError,
    },
}

impl SinkError {
    /// The backend's `source`, of class `class`.
    pub fn backend(class: ErrorClass, source: impl Into<BoxError>) -> Self {
        Self::Backend {
            class,
            source: source.into(),
        }
    }
}

impl Classify for SinkError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Io(_) => ErrorClass::Transient,
            Self::Finished => ErrorClass::Internal,
            Self::Backend { class, .. } => *class,
        }
    }
}

/// JSON Lines over a [`Write`]: one record as one line of JSON, and a flush on
/// [`finish`](RecordSink::finish).
///
/// The writes are blocking: give it a buffered writer (a `BufWriter` over a file or standard
/// output), as the binaries do.
#[derive(Debug)]
pub struct JsonLines<W> {
    out: W,
    finished: bool,
}

impl<W: Write + Send> JsonLines<W> {
    /// Records as lines of `out`.
    pub fn new(out: W) -> Self {
        Self {
            out,
            finished: false,
        }
    }

    /// The writer.
    pub fn get_ref(&self) -> &W {
        &self.out
    }

    /// The writer, given back; what was put and not finished may still be in its buffer.
    pub fn into_inner(self) -> W {
        self.out
    }
}

#[async_trait]
impl<W: Write + Send> RecordSink for JsonLines<W> {
    async fn put(&mut self, record: &Record) -> Result<(), SinkError> {
        if self.finished {
            return Err(SinkError::Finished);
        }
        write_line(&mut self.out, record)
    }

    async fn finish(&mut self) -> Result<(), SinkError> {
        self.out.flush()?;
        self.finished = true;
        Ok(())
    }
}

/// `record` as one line of JSON on `out`.
pub(crate) fn write_line(out: &mut impl Write, record: &Record) -> Result<(), SinkError> {
    serde_json::to_writer(&mut *out, record).map_err(|e| {
        // A record is plain data: only the writer can fail it.
        if e.is_io() {
            SinkError::Io(e.into())
        } else {
            SinkError::backend(ErrorClass::Internal, e)
        }
    })?;
    out.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> Record {
        serde_json::from_value(serde_json::json!({
            "schema": crate::SCHEMA, "id": id, "agent": "coder", "status": "done",
            "created_at": "2026-11-27T00:00:00Z", "updated_at": "2026-11-27T00:00:00Z",
            "turns": 1, "tool_calls": 0, "usage": {"input_tokens": 1, "output_tokens": 1},
            "models": [], "compacted": false, "facets": {}, "outcome": {"status": "done"},
            "tools_source": "none", "messages": [{"role": "assistant", "content": "hi"}],
        }))
        .expect("a record")
    }

    /// One record a line, the same bytes `serde_json` writes, in the order put; `finish` twice is
    /// harmless and a `put` after it is refused.
    #[tokio::test]
    async fn each_record_is_one_line_of_json_in_order() {
        let mut sink = JsonLines::new(Vec::new());
        let (a, b) = (record("a"), record("b"));
        sink.put(&a).await.expect("put");
        sink.put(&b).await.expect("put");
        sink.finish().await.expect("finished");
        sink.finish().await.expect("finished again");
        let e = sink.put(&a).await.expect_err("finished");
        assert_eq!(e.class(), ErrorClass::Internal);
        let text = String::from_utf8(sink.into_inner()).expect("utf-8");
        let want = format!(
            "{}\n{}\n",
            serde_json::to_string(&a).expect("json"),
            serde_json::to_string(&b).expect("json")
        );
        assert_eq!(text, want);
    }

    /// A writer that refuses is `Io`, `Transient`, and the sink is not finished.
    #[tokio::test]
    async fn a_refused_write_is_transient() {
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("disk full"))
            }
        }
        let mut sink = JsonLines::new(Full);
        let e = sink.put(&record("a")).await.expect_err("refused");
        assert!(matches!(e, SinkError::Io(_)), "{e:?}");
        assert_eq!(e.class(), ErrorClass::Transient);
        let e = sink.finish().await.expect_err("refused");
        assert_eq!(e.class(), ErrorClass::Transient);
        assert!(!sink.finished);
    }
}
