//! [`CancelToken`]: how a running step learns that its run was cancelled.

use std::sync::Arc;

use tokio::sync::watch;

/// A cloneable signal that fires once when the run being stepped is
/// cancelled (or otherwise finished by someone else).
///
/// [`Runtime::cancel`](crate::Runtime::cancel) commits the run as `Failed`, so
/// the worker's own commit is rejected and its result dropped. That alone does
/// not stop work already under way: a step that shells out, streams from a
/// model or drives a child process keeps going until it ends by itself. A step
/// that wants to stop early hands this token to that work and reacts to
/// [`cancelled`](Self::cancelled):
///
/// ```ignore
/// tokio::select! {
///     out = run_the_child() => out,
///     () = ctx.cancelled() => { child.kill().await; return Err(...); }
/// }
/// ```
///
/// The runtime fires it in three ways: at once when `Runtime::cancel` is called
/// on the same [`Runtime`](crate::Runtime) that holds the run's lease; at once
/// when another process cancels and the two share a
/// [`Notifier`](crate::Notifier) (a [`Signal::Finished`](crate::Signal)); and
/// within one `poll_interval` when the run turns terminal in the store for any
/// other reason (a cancel whose signal was lost, a purge). The token is
/// per transition: the next transition of the run gets a fresh one. Reacting
/// is optional and never affects correctness, which rests on the version CAS;
/// it only saves work.
///
/// Firing is one way: once cancelled, a token stays cancelled. Only the
/// runtime fires the token it hands out through [`Ctx`](crate::Ctx); a
/// standalone token ([`CancelToken::new`]) is for testing code that takes one.
#[derive(Clone, Debug)]
pub struct CancelToken {
    tx: Arc<watch::Sender<bool>>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    /// A token that has not fired. Clones share the same signal.
    pub fn new() -> Self {
        Self {
            tx: Arc::new(watch::channel(false).0),
        }
    }

    /// Fire the token. Idempotent; wakes every task waiting in
    /// [`cancelled`](Self::cancelled).
    ///
    /// The runtime calls this for the tokens it hands to steps. Call it
    /// yourself only on a token you created with [`CancelToken::new`].
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    /// Whether the token has fired.
    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolves once the token has fired (immediately if it already has).
    /// Never resolves for a token that is never fired. Cancel-safe.
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        // The sender lives in `self`, so `wait_for` cannot see it closed.
        let _ = rx.wait_for(|fired| *fired).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn fires_once_for_every_clone_and_waiter() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        let waiters: Vec<_> = (0..3)
            .map(|_| {
                let t = token.clone();
                tokio::spawn(async move { t.cancelled().await })
            })
            .collect();
        // Nobody is released before the token fires.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), token.cancelled())
                .await
                .is_err()
        );
        token.clone().cancel();
        token.cancel(); // idempotent
        for w in waiters {
            tokio::time::timeout(Duration::from_secs(5), w)
                .await
                .expect("released")
                .expect("task");
        }
        assert!(token.is_cancelled());
        // Already fired: resolves at once, for old and new clones alike.
        token.cancelled().await;
        token.clone().cancelled().await;
    }
}
