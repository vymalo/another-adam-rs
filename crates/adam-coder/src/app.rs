//! Composition: the coder agent, a runtime with workers, and the A2A server.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use adam_a2a::{A2aServer, AgentCardConfig, AuthConfig, SkillConfig};
use adam_a2a_runtime::RuntimeTaskBackend;
use adam_core::DynStore;
use adam_runtime::{BroadcastSink, Runtime, RuntimeBuilder, RuntimeError};
use axum::Router;
use url::Url;

use crate::agent::{AGENT_NAME, CoderAgent, CoderStarter};

/// How the runtime that advances runs is set up.
#[derive(Debug, Clone)]
pub struct RuntimeOptions {
    /// Lease identity; unique per process. `None`: random.
    pub worker_id: Option<String>,
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
            concurrency: 4,
            lease_ttl: Duration::from_secs(30),
            poll_interval: Duration::from_millis(250),
        }
    }
}

/// The coder, composed: runtime (workers) and A2A backend over one store.
///
/// By default one process serves A2A *and* runs workers; replicas over the same
/// database scale horizontally through leases. With `ROLE` the two halves run in
/// separate processes: a control plane ([`Coder::control_plane`]) uses [`Coder::router`] and
/// never calls [`Coder::run_worker`], a worker ([`Coder::new`]) does the opposite. The halves
/// meet only in the store, and the backend of a control plane learns what a worker did by
/// polling.
pub struct Coder {
    /// The runtime; call [`Coder::run_worker`] to advance runs.
    pub runtime: Runtime,
    /// The A2A backend over the runtime.
    pub backend: RuntimeTaskBackend,
}

impl Coder {
    /// Compose `agent` over `store`: the A2A backend and workers that step runs.
    pub fn new(store: DynStore, agent: CoderAgent, options: &RuntimeOptions) -> Self {
        Self::compose(Runtime::builder(store).agent(agent), options)
    }

    /// Compose the control plane over `store`: the A2A backend, with the agent registered as a
    /// [`CoderStarter`] only. It starts, delivers to, cancels and views runs, and needs no model,
    /// GitHub client or workspaces; nothing in it steps a run, so a
    /// [`run_worker`](Self::run_worker) here claims nothing. A process built with
    /// [`Coder::new`] over the same store does the stepping.
    pub fn control_plane(store: DynStore, options: &RuntimeOptions) -> Self {
        Self::compose(Runtime::builder(store).starter(CoderStarter), options)
    }

    /// The runtime settings and the A2A backend, common to both compositions.
    fn compose(builder: RuntimeBuilder, options: &RuntimeOptions) -> Self {
        let events = BroadcastSink::default();
        let mut builder = builder
            .event_sink(events.clone())
            .concurrency(options.concurrency)
            .lease_ttl(options.lease_ttl)
            .poll_interval(options.poll_interval);
        if let Some(id) = &options.worker_id {
            builder = builder.worker_id(id.clone());
        }
        let runtime = builder.build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), events, AGENT_NAME)
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
pub fn agent_card(public_url: &Url) -> AgentCardConfig {
    let mut skill = SkillConfig::new(
        "coding-task",
        "Coding task to pull request",
        "Given a repository and a task, makes the change in a private worktree with OpenCode, \
         runs the project's own checks, and opens a pull request. Reports the pull request as an \
         artifact and asks the caller when it needs an answer.",
    );
    skill.tags = vec!["code".into(), "git".into(), "pull-request".into()];
    skill.examples = vec![
        "In https://github.com/acme/widgets (base branch main), add a hello.txt containing hi."
            .into(),
    ];
    AgentCardConfig::new(
        "adam-coder",
        "Coder agent: turns a coding task into a verified pull request.",
        public_url.clone(),
        env!("CARGO_PKG_VERSION"),
    )
    .with_skill(skill)
}
