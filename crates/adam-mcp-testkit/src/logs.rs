//! Helpers for the tests of a client: what was logged, and waiting for a condition.

use std::cell::RefCell;
use std::future::Future;
use std::io::Write;
use std::sync::{Arc, Mutex, Once, PoisonError};
use std::time::{Duration, Instant};

/// The most a [`wait_until`] waits.
const DEADLINE: Duration = Duration::from_secs(20);

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

thread_local! {
    /// Where this thread's log lines go while a [`LogCapture`] lives on it.
    static CURRENT: RefCell<Option<Buffer>> = const { RefCell::new(None) };
}

/// The writer of the one global subscriber: it hands each line to the capture of the thread that
/// logged it, and drops it when there is none.
#[derive(Clone, Copy)]
struct PerThread;

impl Write for PerThread {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CURRENT.with(|current| {
            if let Some(buffer) = current.borrow().as_ref() {
                buffer
                    .0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for PerThread {
    type Writer = PerThread;

    fn make_writer(&'a self) -> PerThread {
        *self
    }
}

/// Everything logged, at every level, by the code that runs on this thread while it lives.
///
/// Use it in a `#[tokio::test]` (a current-thread runtime): the tasks of the test run on the
/// test's thread, and so do the ones the libraries spawn. The subscriber is installed once, as the
/// global default, and filters nothing: a scoped one (`tracing::subscriber::set_default`) can miss
/// events when tests run in threads of one process, because a callsite caches whether anybody
/// listens, and a thread that has no subscriber yet makes it cache "nobody".
pub struct LogCapture {
    buffer: Buffer,
}

impl LogCapture {
    /// Start capturing on this thread.
    pub fn start() -> Self {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            // A test binary that installed its own global subscriber keeps it; captures are then
            // empty, and the tests that read them fail loudly rather than pass silently.
            let _ = tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::TRACE)
                    .with_ansi(false)
                    .with_writer(PerThread)
                    .finish(),
            );
        });
        let buffer = Buffer::default();
        CURRENT.with(|current| *current.borrow_mut() = Some(buffer.clone()));
        Self { buffer }
    }

    /// What was logged so far.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.buffer.0.lock().unwrap_or_else(PoisonError::into_inner))
            .into_owned()
    }
}

impl Drop for LogCapture {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = None);
    }
}

/// Wait until `check` says `true`, and fail the test when it takes more than 20 seconds. A
/// condition poll with a deadline: the tests never sleep for a fixed time and hope.
pub async fn wait_until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + DEADLINE;
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
