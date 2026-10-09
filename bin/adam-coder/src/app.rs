//! Composition: the coder agent, a runtime with workers, and the A2A server. The generic half
//! (the runtime options, the live signals, the runtime and the A2A backend over one store) is
//! [`adam_service`]; this module puts the coder's agent on it.

use std::future::Future;

use adam::AssemblyError;
use adam_a2a::{AgentCardConfig, AuthConfig};
use adam_a2a_runtime::{InboundFn, RuntimeTaskBackend, vymalo_inbound};
use adam_core::DynStore;
use adam_runtime::{Runtime, RuntimeError};
use adam_service::Service;
use axum::Router;
use url::Url;

use crate::agent::{AGENT_NAME, CoderAgent, CoderStarter};
use crate::files::AgentFiles;

pub use adam_service::{LiveSignals, RuntimeOptions};

/// How the coder reads an A2A message: as one from the person's screen ([`vymalo_inbound`]): their
/// answers through a form are the answer to the question, and the catalog and the grant for the
/// conversation's tools reach the run as its context. For a message from anything else it is the
/// default reading.
pub(crate) fn screen_inbound() -> Option<InboundFn> {
    Some(std::sync::Arc::new(vymalo_inbound))
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
    ///
    /// The subagents of the agent's folder ([`CoderAgent::subagents`]) are registered beside it,
    /// so that a subagent tool finds the agent it starts a child run of.
    pub fn new_with(
        store: DynStore,
        agent: CoderAgent,
        options: &RuntimeOptions,
        live: LiveSignals,
    ) -> Self {
        let builder = agent.register(Runtime::builder(store));
        Self::from_service(
            Service::new_with(builder, AGENT_NAME, options, live).with_inbound(screen_inbound()),
        )
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
        let builder = Runtime::builder(store).starter(CoderStarter);
        Self::from_service(
            Service::new_with(builder, AGENT_NAME, options, live).with_inbound(screen_inbound()),
        )
    }

    fn from_service(service: Service) -> Self {
        let Service { runtime, backend } = service;
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

    /// The A2A router (agent card, JSON-RPC, `/healthz`) with `auth`, serving the card of the
    /// embedded agent ([`agent_card`]). A process that runs on a folder serves
    /// [`router_with_card`](Self::router_with_card) with the card of that folder.
    pub fn router(&self, public_url: &Url, auth: AuthConfig) -> Router {
        self.router_with_card(agent_card(public_url), auth)
    }

    /// [`router`](Self::router) with the card the caller made, usually
    /// [`agent_card_from`] over the files the process runs.
    pub fn router_with_card(&self, card: AgentCardConfig, auth: AuthConfig) -> Router {
        adam_service::router(&self.backend, card, auth)
    }
}

/// The agent card the coder serves. `public_url` is where clients POST
/// JSON-RPC.
///
/// The card is declared in `agent/instructions.md` (the `card:` frontmatter) and read from the
/// embedded agent, so a control plane, which has no model, tools or credentials, serves the same
/// card as `Assembly::card` gives for the assembled agent. A process that runs on a folder
/// uses [`agent_card_from`].
///
/// # Panics
///
/// Never for the embedded files, which a unit test reads; the `expect` guards a mismatch between
/// this crate and `adam-agent-fs`, which the build would already have refused.
#[allow(clippy::expect_used)] // see `# Panics`
pub fn agent_card(public_url: &Url) -> AgentCardConfig {
    agent_card_from(&AgentFiles::Embedded, public_url)
        .expect("the coder's embedded agent declares a card")
}

/// The card the agent `files` declare, with `public_url` where clients POST JSON-RPC: the same
/// card [`Assembly::card`](adam::Assembly::card) gives for the agent assembled from them, with no
/// model, tools or credentials.
///
/// # Errors
///
/// [`AssemblyError::MissingCardDescription`] when a folder declares neither `description` nor
/// `card.description`; the embedded files cannot fail.
pub fn agent_card_from(
    files: &AgentFiles,
    public_url: &Url,
) -> Result<AgentCardConfig, Box<AssemblyError>> {
    card_for_build(files, public_url, BUILD_REVISION)
}

/// The revision this binary was built from: the build argument `ADAM_BUILD_REVISION` of the image
/// (the commit), `None` for a build that was not given one (a local `cargo build`).
pub const BUILD_REVISION: Option<&str> = option_env!("ADAM_BUILD_REVISION");

/// The card's `version`: the crate's version and the build's revision as semver build metadata,
/// `0.1.0+6478fbc`, or `0.1.0+unknown` when the build was not given one.
pub fn build_version() -> String {
    adam_a2a::build_version(env!("CARGO_PKG_VERSION"), BUILD_REVISION)
}

/// [`agent_card_from`] for a build of revision `revision`: the version carries it, and the
/// `build/v1` extension says it with the digest of `files`.
fn card_for_build(
    files: &AgentFiles,
    public_url: &Url,
    revision: Option<&str>,
) -> Result<AgentCardConfig, Box<AssemblyError>> {
    let version = adam_a2a::build_version(env!("CARGO_PKG_VERSION"), revision);
    files
        .def()?
        .card(public_url.clone(), version)
        .map(with_extensions)
        .map(|card| {
            card.with_extension(adam_a2a::ExtensionConfig::build(
                revision,
                files.describe().digest,
            ))
        })
        .map_err(Box::new)
}

/// `card` with the extensions the coder speaks: the screen's (A2UI, `ui-catalog/v1`,
/// `thread-tools/v1`, `mentions/v1`, `steer/v1`), `steps/v1` (every tool call, and OpenCode's, is reported as a step to a
/// client that activates it) and `text-stream/v1` (its answers are sent as the model writes them to a
/// client that activates it: the agent streams its model calls) and `usage/v1` (the tokens of every model
/// call to a client that activates it, and a task's totals on the task). `build/v1`, which says the build
/// and the files, is added by [`card_for_build`].
fn with_extensions(card: AgentCardConfig) -> AgentCardConfig {
    adam_ui::with_card_extensions(card)
        .with_extension(adam_a2a::ExtensionConfig::steps())
        .with_extension(adam_a2a::ExtensionConfig::text_stream())
        .with_extension(adam_a2a::ExtensionConfig::usage())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// The card is the golden `tests/fixtures/agent/card.json`: the one the Rust literal used to
    /// build, with the name `Adam` (the golden changes in the same commit as the file);
    /// only what a build decides follows it: the version (the crate's and the revision's), and the
    /// revision and the files' digest of `build/v1`.
    #[test]
    fn the_card_from_the_agent_file_equals_the_golden() {
        let mut golden: Value =
            serde_json::from_str(include_str!("../tests/fixtures/agent/card.json"))
                .expect("the golden card is JSON");
        golden["version"] = json!(build_version());
        let build = golden["extensions"]
            .as_array_mut()
            .and_then(|all| all.last_mut())
            .expect("the card lists extensions");
        build["params"]["revision"] = json!(adam_a2a::revision_of(BUILD_REVISION));
        build["params"]["folderDigest"] = json!(crate::agent::AGENT.digest);
        let url: Url = "https://agents.example.com/coder/".parse().expect("a URL");
        assert_eq!(render(&agent_card(&url)), golden);
    }

    /// A build with a revision says it twice: as semver build metadata of the card's version, and,
    /// whole, with the digest of the agent files, in `build/v1`. Without one it says `unknown`.
    #[test]
    fn the_card_says_which_build_and_which_files_answer() {
        let url: Url = "https://agents.example.com/coder/".parse().expect("a URL");
        let sha = "6478fbc1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a7";
        let card = card_for_build(&AgentFiles::Embedded, &url, Some(sha)).expect("a card");
        assert_eq!(
            card.version,
            format!("{}+6478fbc", env!("CARGO_PKG_VERSION"))
        );
        let build = card
            .extensions
            .iter()
            .find(|e| e.uri == adam_a2a::BUILD_EXTENSION)
            .expect("the card declares build/v1");
        assert!(!build.required);
        assert_eq!(build.params["revision"], sha);
        assert_eq!(build.params["folderDigest"], crate::agent::AGENT.digest);
        assert!(
            build.params["folderDigest"]
                .as_str()
                .is_some_and(|d| d.starts_with("sha256:"))
        );

        let card = card_for_build(&AgentFiles::Embedded, &url, None).expect("a card");
        assert_eq!(
            card.version,
            format!("{}+unknown", env!("CARGO_PKG_VERSION"))
        );
        let revision = card
            .extensions
            .iter()
            .find(|e| e.uri == adam_a2a::BUILD_EXTENSION)
            .map(|e| e.params["revision"].clone());
        assert_eq!(revision, Some(json!("unknown")));
    }
}
