//! Composition: the coder agent, a runtime with workers, and the A2A server.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use adam::AgentDef;
use adam_a2a::{A2aServer, AgentCardConfig, AuthConfig};
use adam_a2a_runtime::RuntimeTaskBackend;
use adam_core::{ClaimScope, DynStore};
use adam_runtime::{
    BroadcastSink, DynEventSink, DynNotifier, Runtime, RuntimeBuilder, RuntimeError,
};
use axum::Router;
use url::Url;

use crate::agent::{AGENT, AGENT_NAME, CoderAgent, CoderStarter};

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
/// other processes are found by polling the store. `serve` builds the Postgres one
/// (`adam-notify-postgres`), which also carries events and wake-up/cancel signals across processes.
/// A composition of your own may pass any [`EventSink`](adam_runtime::EventSink) and `Notifier`;
/// `sink` should deliver to `broadcast` first, or streams see nothing of this process's runs.
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

/// The coder, composed: runtime (workers) and A2A backend over one store.
///
/// By default one process serves A2A *and* runs workers; replicas over the same
/// database scale horizontally through leases. With `ROLE` the two halves run in
/// separate processes: a control plane ([`Coder::control_plane`]) uses [`Coder::router`] and
/// never calls [`Coder::run_worker`], a worker ([`Coder::new`]) does the opposite. The halves
/// meet in the store, and the backend of a control plane learns what a worker did by polling. With
/// [`LiveSignals`] over Postgres `NOTIFY` (what the binary uses) they also meet in live events and
/// wake-up signals, which only make that faster.
pub struct Coder {
    /// The runtime; call [`Coder::run_worker`] to advance runs.
    pub runtime: Runtime,
    /// The A2A backend over the runtime.
    pub backend: RuntimeTaskBackend,
}

impl Coder {
    /// Compose `agent` over `store`: the A2A backend and workers that step runs.
    pub fn new(store: DynStore, agent: CoderAgent, options: &RuntimeOptions) -> Self {
        Self::new_with(store, agent, options, LiveSignals::local())
    }

    /// [`Coder::new`] with `live` in place of the in-process signals: events and wake-up signals
    /// that cross processes.
    pub fn new_with(
        store: DynStore,
        agent: CoderAgent,
        options: &RuntimeOptions,
        live: LiveSignals,
    ) -> Self {
        Self::compose(Runtime::builder(store).agent(agent), options, live)
    }

    /// Compose the control plane over `store`: the A2A backend, with the agent registered as a
    /// [`CoderStarter`] only. It starts, delivers to, cancels and views runs, and needs no model,
    /// GitHub client or workspaces; nothing in it steps a run, so a
    /// [`run_worker`](Self::run_worker) here claims nothing. A process built with
    /// [`Coder::new`] over the same store does the stepping.
    pub fn control_plane(store: DynStore, options: &RuntimeOptions) -> Self {
        Self::control_plane_with(store, options, LiveSignals::local())
    }

    /// [`Coder::control_plane`] with `live` in place of the in-process signals.
    pub fn control_plane_with(
        store: DynStore,
        options: &RuntimeOptions,
        live: LiveSignals,
    ) -> Self {
        Self::compose(Runtime::builder(store).starter(CoderStarter), options, live)
    }

    /// The runtime settings and the A2A backend, common to both compositions.
    fn compose(builder: RuntimeBuilder, options: &RuntimeOptions, live: LiveSignals) -> Self {
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
        let backend = RuntimeTaskBackend::new(runtime.clone(), broadcast, AGENT_NAME)
            .with_poll_interval(options.poll_interval);
        Self { runtime, backend }
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

    /// The A2A router (agent card, JSON-RPC, `/healthz`) with `auth`.
    pub fn router(&self, public_url: &Url, auth: AuthConfig) -> Router {
        A2aServer::router(agent_card(public_url), Arc::new(self.backend.clone()), auth)
    }
}

/// The agent card the coder serves. `public_url` is where clients POST
/// JSON-RPC.
///
/// The card is declared in `agent/instructions.md` (the `card:` frontmatter) and read from the
/// embedded agent, so a control plane, which has no model, tools or credentials, serves the same
/// card as `Assembly::card` gives for the assembled agent.
///
/// # Panics
///
/// Never for the embedded files, which a unit test reads; the `expect` guards a mismatch between
/// this crate and `adam-agent-fs`, which the build would already have refused.
#[allow(clippy::expect_used)] // see `# Panics`
pub fn agent_card(public_url: &Url) -> AgentCardConfig {
    AgentDef::from_manifest(AGENT)
        .map_err(Box::new)
        .and_then(|def| {
            def.card(public_url.clone(), env!("CARGO_PKG_VERSION"))
                .map_err(Box::new)
        })
        .expect("the coder's embedded agent declares a card")
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_core::RunId;
    use adam_runtime::{EventSink as _, RunEvent};
    use serde_json::{Value, json};

    /// The card as JSON, everything it holds, in the shape of `tests/fixtures/agent/card.json`.
    fn render(card: &AgentCardConfig) -> Value {
        json!({
            "name": card.name,
            "description": card.description,
            "url": card.url.as_str(),
            "version": card.version,
            "skills": card.skills.iter().map(|s| json!({
                "id": s.id,
                "name": s.name,
                "description": s.description,
                "tags": s.tags,
                "examples": s.examples,
            })).collect::<Vec<_>>(),
            "extensions": card.extensions.iter().map(|e| json!({
                "uri": e.uri,
                "description": e.description,
                "required": e.required,
                "params": e.params,
            })).collect::<Vec<_>>(),
        })
    }

    /// The card is the one the Rust literal used to build. The golden file was captured from that
    /// literal before it was deleted; only the version follows the crate's.
    #[test]
    fn the_card_from_the_agent_file_equals_the_old_literal() {
        let mut golden: Value =
            serde_json::from_str(include_str!("../tests/fixtures/agent/card.json"))
                .expect("the golden card is JSON");
        golden["version"] = json!(env!("CARGO_PKG_VERSION"));
        let url: Url = "https://agents.example.com/coder/".parse().expect("a URL");
        assert_eq!(render(&agent_card(&url)), golden);
    }

    #[tokio::test]
    async fn local_signals_deliver_events_to_the_broadcast_and_have_no_notifier() {
        let live = LiveSignals::local();
        assert!(live.notifier.is_none());
        let run = RunId::new();
        let mut sub = live.broadcast.subscribe_run(run);
        live.sink
            .emit(
                run,
                AGENT_NAME,
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
                AGENT_NAME,
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
}
