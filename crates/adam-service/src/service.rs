//! The service, composed: a runtime (workers) and an A2A backend over one store.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use adam_a2a::{A2aServer, AgentCardConfig, AuthConfig};
use adam_a2a_runtime::{InboundFn, RuntimeTaskBackend};
use adam_core::ClaimScope;
use adam_runtime::{
    BroadcastSink, DynEventSink, DynNotifier, Runtime, RuntimeBuilder, RuntimeError,
};
use axum::Router;

/// How the runtime that advances runs is set up.
#[derive(Debug, Clone)]
pub struct RuntimeOptions {
    /// Lease identity; unique per process. `None`: random. With
    /// [`ClaimScope::Pinned`] it is also the run owner, so it must be stable across restarts.
    pub worker_id: Option<String>,
    /// Whose runs the worker claims: any run (default), or only its own
    /// ([`ClaimScope::Pinned`], for the `affinity` and `isolated` placements).
    pub claim_scope: ClaimScope,
    /// Runs advanced at the same time by this process.
    pub concurrency: usize,
    /// How long a claimed run stays leased without renewal.
    pub lease_ttl: Duration,
    /// How often an idle worker polls for due runs, and a subscription re-reads
    /// a run.
    pub poll_interval: Duration,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            worker_id: None,
            claim_scope: ClaimScope::Any,
            concurrency: 4,
            lease_ttl: Duration::from_secs(30),
            poll_interval: Duration::from_millis(250),
        }
    }
}

/// How a process learns of what other processes do, and tells them: the runtime's live event sink
/// and (optionally) its [`Notifier`](adam_runtime::Notifier), plus the in-process
/// [`BroadcastSink`] the A2A backend streams from.
///
/// [`LiveSignals::local`] is the default and needs nothing: events reach only this process, and
/// other processes are found by polling the store. [`serve`](crate::serve) builds the Postgres
/// one (`adam-notify-postgres`), which also carries events and wake-up/cancel signals across
/// processes. A composition of your own may pass any [`EventSink`](adam_runtime::EventSink) and
/// `Notifier`; `sink` should deliver to `broadcast` first, or streams see nothing of this
/// process's runs.
#[derive(Clone)]
pub struct LiveSignals {
    /// What the A2A backend subscribes to, for SSE.
    pub broadcast: BroadcastSink,
    /// The runtime's event sink; it delivers to `broadcast` (and, across processes, beyond).
    pub sink: DynEventSink,
    /// The runtime's notifier, if any. `None`: only polling crosses a process boundary.
    pub notifier: Option<DynNotifier>,
}

impl LiveSignals {
    /// In-process only: a fresh [`BroadcastSink`] as both the sink and the backend's source, and
    /// no notifier.
    pub fn local() -> Self {
        let broadcast = BroadcastSink::default();
        Self {
            sink: Arc::new(broadcast.clone()),
            broadcast,
            notifier: None,
        }
    }
}

impl std::fmt::Debug for LiveSignals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveSignals")
            .field("notifier", &self.notifier.is_some())
            .finish_non_exhaustive()
    }
}

/// An agent service, composed: runtime (workers) and A2A backend over one store.
///
/// By default one process serves A2A *and* runs workers; replicas over the same database scale
/// horizontally through leases. With `ROLE` the two halves run in separate processes: a control
/// plane builds its service from a runtime that knows the agent as a starter only and uses
/// [`Service::router`], never [`Service::run_worker`]; a worker registers the whole agent and does
/// the opposite. The halves meet in the store, and the backend of a control plane learns what a
/// worker did by polling. With [`LiveSignals`] over Postgres `NOTIFY` (what [`serve`](crate::serve)
/// uses) they also meet in live events and wake-up signals, which only make that faster.
///
/// The backend serves the one agent `name` the service is built for: a task is a run of that
/// agent, and a run of another agent in the same database is refused (see
/// `adam_a2a_runtime::RuntimeTaskBackend`).
pub struct Service {
    /// The runtime; call [`Service::run_worker`] to advance runs.
    pub runtime: Runtime,
    /// The A2A backend over the runtime.
    pub backend: RuntimeTaskBackend,
}

impl Service {
    /// The service for the agent `name`, registered on `builder` (the root and its subagents with
    /// `Assembly::register`, or a starter only for a control plane), over [`LiveSignals::local`].
    pub fn new(builder: RuntimeBuilder, name: impl Into<String>, options: &RuntimeOptions) -> Self {
        Self::new_with(builder, name, options, LiveSignals::local())
    }

    /// [`Service::new`] with `live` in place of the in-process signals: events and wake-up
    /// signals that cross processes.
    pub fn new_with(
        builder: RuntimeBuilder,
        name: impl Into<String>,
        options: &RuntimeOptions,
        live: LiveSignals,
    ) -> Self {
        let LiveSignals {
            broadcast,
            sink,
            notifier,
        } = live;
        let mut builder = builder
            .event_sink(sink)
            .claim_scope(options.claim_scope)
            .concurrency(options.concurrency)
            .lease_ttl(options.lease_ttl)
            .poll_interval(options.poll_interval);
        if let Some(id) = &options.worker_id {
            builder = builder.worker_id(id.clone());
        }
        if let Some(notifier) = notifier {
            builder = builder.notifier(notifier);
        }
        let runtime = builder.build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), broadcast, name)
            .with_poll_interval(options.poll_interval);
        Self { runtime, backend }
    }

    /// Read A2A messages with `inbound` instead of the default
    /// ([`default_inbound`](adam_a2a_runtime::default_inbound)); `None` changes nothing.
    #[must_use]
    pub fn with_inbound(mut self, inbound: Option<InboundFn>) -> Self {
        if let Some(inbound) = inbound {
            self.backend = self.backend.with_inbound(move |message| inbound(message));
        }
        self
    }

    /// Advance runs until `shutdown` resolves; in-flight steps finish first.
    ///
    /// # Errors
    ///
    /// Whatever `Runtime::run_worker` reports.
    pub async fn run_worker(
        &self,
        shutdown: impl Future<Output = ()> + Send,
    ) -> Result<(), RuntimeError> {
        self.runtime.run_worker(shutdown).await
    }

    /// The A2A router (agent card, JSON-RPC, `/healthz`) for `card`, with `auth`.
    pub fn router(&self, card: AgentCardConfig, auth: AuthConfig) -> Router {
        router(&self.backend, card, auth)
    }
}

/// The A2A router (agent card, JSON-RPC, `/healthz`) over `backend`, for a composition that holds
/// the runtime and the backend itself.
pub fn router(backend: &RuntimeTaskBackend, card: AgentCardConfig, auth: AuthConfig) -> Router {
    A2aServer::router(card, Arc::new(backend.clone()), auth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_core::RunId;
    use adam_runtime::{EventSink as _, RunEvent};

    #[tokio::test]
    async fn local_signals_deliver_events_to_the_broadcast_and_have_no_notifier() {
        let live = LiveSignals::local();
        assert!(live.notifier.is_none());
        let run = RunId::new();
        let mut sub = live.broadcast.subscribe_run(run);
        live.sink
            .emit(
                run,
                "agent",
                RunEvent::Progress {
                    message: "hi".into(),
                },
            )
            .await;
        let got = tokio::time::timeout(Duration::from_secs(1), sub.recv())
            .await
            .expect("the sink delivers to the broadcast");
        assert_eq!(
            got,
            Some(RunEvent::Progress {
                message: "hi".into()
            })
        );
        // Two `local()` values share nothing.
        let unrelated = LiveSignals::local();
        let mut other = unrelated.broadcast.subscribe_run(run);
        live.sink
            .emit(
                run,
                "agent",
                RunEvent::Progress {
                    message: "again".into(),
                },
            )
            .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), other.recv())
                .await
                .is_err()
        );
    }

    #[test]
    fn the_defaults_are_four_runs_at_once_and_any_runs_claim_scope() {
        let options = RuntimeOptions::default();
        assert_eq!(options.concurrency, 4);
        assert_eq!(options.claim_scope, ClaimScope::Any);
        assert_eq!(options.worker_id, None);
        assert!(format!("{:?}", LiveSignals::local()).contains("notifier: false"));
    }
}
