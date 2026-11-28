//! `JsonLines` is the reference of the sink suite: over a buffered writer, so the cases also
//! check that `finish` flushes what the buffer holds.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::io::BufWriter;

use adam_export::JsonLines;
use adam_export_testkit::{SharedBuffer, SinkFixture};

async fn make_sink() -> Option<SinkFixture> {
    let buffer = SharedBuffer::default();
    Some(SinkFixture {
        sink: Box::new(JsonLines::new(BufWriter::new(buffer.clone()))),
        readback: Box::new(buffer),
    })
}

adam_export_testkit::sink_conformance!(make_sink);
