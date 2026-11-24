//! The service as one process: [`serve`] connects the Postgres store from a [`ServiceConfig`]
//! and runs the halves its [`Role`](adam_host::Role) asks for until told to stop.
//!
//! The binary is `serve(&config, agents, sigterm)` and nothing else; tests drive it with their
//! own shutdown future. The halves are components of an [`adam_host::Host`], which starts only the
//! ones the role runs and stops them in a fixed order.
//!
//! The sequence and the lifecycle are in the [crate README](https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-service/README.md#the-process).
//!
//! | Role | Components | Also |
//! |---|---|---|
//! | `all` (default) | `a2a-server` (control plane), `worker`, `notify` | `push-delivery` (control plane) when `A2A_PUSH_ALLOWED_URLS` is set |
//! | `control-plane` | `a2a-server`, `notify` | the runtime knows the agent as a starter only: no model, no tools; `push-delivery` when `A2A_PUSH_ALLOWED_URLS` is set |
//! | `worker` | `worker`, `health`, `notify` | `/healthz` on [`ServiceConfig::listen_addr`], no A2A |
//!
//! `notify` is the [`adam_notify_postgres::PgNotify`] listener and publisher: live events and
//! wake-up/cancel signals cross processes over Postgres `LISTEN`/`NOTIFY`, so a worker takes a
//! run another process started at once instead of at its next poll, and a control plane streams
//! the progress of a run a worker steps as it happens. It is a latency optimisation: polling
//! stays on and correctness never depends on a notification (see `adam-notify-postgres`). With a
//! worker it stops only after the worker has finished, then sends what is still queued for up to
//! `adam_notify_postgres::DRAIN_ON_STOP`, so the last step's events and signals normally still
//! reach other processes (best effort, like any notification).

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use adam_a2a::push::{PushDeliverer, PushDeliveryOptions, PushSupport};
use adam_a2a::{A2aServer, AgentCardConfig, AuthConfig, ServerOptions};
use adam_a2a_runtime::InboundFn;
use adam_core::{ClaimScope, DynStore, Store, StoreError};
use adam_error::BoxError;
use adam_host::{Host, HostError, Placement, Role};
#[cfg(feature = "store-postgres")]
use adam_notify_postgres::PgNotify;
#[cfg(feature = "store-postgres")]
use adam_postgres::PgConfig;
use adam_runtime::{BroadcastSink, Runtime, RuntimeBuilder};
#[cfg(feature = "store-mongodb")]
use adam_store_mongodb::MongoStore;
#[cfg(feature = "store-postgres")]
use adam_store_postgres::{PgStore, pool_size_from_env};
#[cfg(feature = "store-sqlite")]
use adam_store_sqlite::SqliteStore;
use secrecy::ExposeSecret as _;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::config::ServiceConfig;
use crate::service::{LiveSignals, RuntimeOptions, Service};

/// How long open connections (SSE streams never end on their own) get to
/// finish after the shutdown signal before the server is dropped.
const SERVER_DRAIN: Duration = Duration::from_secs(10);

/// How an agent is put on the runtime: given the builder (over the store [`serve`] connected), it
/// returns it with the agent registered.
pub type Register = Box<dyn FnOnce(RuntimeBuilder) -> RuntimeBuilder + Send>;

/// The component that carries live events and wake-up/cancel signals across processes, when the
/// chosen store has one: the Postgres listener. SQLite uses the in-process notifier and MongoDB
/// has none, so both return no component.
type StartNotify = Box<
    dyn FnOnce(CancellationToken) -> Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send>>
        + Send
        + Sync,
>;

/// The store a `DATABASE_URL` selects, before anything connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoreKind {
    /// `postgres://` or `postgresql://`.
    Postgres,
    /// `sqlite://<path>`: one process per file.
    Sqlite {
        /// The file (or `:memory:`).
        path: PathBuf,
    },
    /// `mongodb://` or `mongodb+srv://`.
    Mongo {
        /// The database name, from the URL's path (`adam` when it has none).
        database: String,
    },
}

/// The store [`serve`] built, with what it needs before the process starts its halves.
struct ChosenStore {
    store: DynStore,
    live: LiveSignals,
    notify: Option<StartNotify>,
    /// The store's name for [`ServeError::Connect`]: what `connecting to …` says.
    backend: &'static str,
    /// The Postgres store, whose connection is opened after the process's own checks so a
    /// misconfiguration is exit 78 whether or not the server answers.
    #[cfg(feature = "store-postgres")]
    postgres: Option<PgStore>,
}

impl ChosenStore {
    /// Open the connection and migrate, for a store that connects lazily. SQLite and MongoDB were
    /// opened and migrated when they were chosen.
    async fn ready(&self) -> Result<(), ServeError> {
        #[cfg(feature = "store-postgres")]
        if let Some(pg) = &self.postgres {
            pg.ping().await.map_err(|source| ServeError::Connect {
                backend: self.backend,
                source,
            })?;
            Store::migrate(pg).await.map_err(ServeError::Migrate)?;
        }
        Ok(())
    }
}

impl StoreKind {
    /// The store's name in an error: `Postgres`, `MongoDB` or `SQLite`.
    fn backend(&self) -> &'static str {
        match self {
            Self::Postgres => "Postgres",
            Self::Sqlite { .. } => "SQLite",
            Self::Mongo { .. } => "MongoDB",
        }
    }
}

/// What `DATABASE_URL`'s scheme says, and whether the process's role may use it.
///
/// `sqlite://` holds one process per file (ADR 0053, decision 3), so a `worker` or a
/// `control-plane` is refused: a split deployment needs Postgres, MongoDB or the store service.
pub(crate) fn store_kind(url: &str, role: Role) -> Result<StoreKind, ServeError> {
    let Some((scheme, rest)) = url.split_once(':') else {
        return Err(ServeError::UnsupportedScheme {
            scheme: url.to_owned(),
        });
    };
    match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => Ok(StoreKind::Postgres),
        "sqlite" => {
            if role != Role::All {
                return Err(ServeError::SplitRoleOnSqlite {
                    role: role.as_str().to_owned(),
                });
            }
            let path = rest.strip_prefix("//").unwrap_or(rest);
            if path.is_empty() {
                return Err(ServeError::SqliteWithoutPath);
            }
            Ok(StoreKind::Sqlite {
                path: PathBuf::from(path),
            })
        }
        "mongodb" | "mongodb+srv" => Ok(StoreKind::Mongo {
            database: mongo_database(url),
        }),
        other => Err(ServeError::UnsupportedScheme {
            scheme: other.to_owned(),
        }),
    }
}

/// The database name of a MongoDB URL: its path, or `adam` when it names none.
fn mongo_database(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|url| {
            let path = url.path().trim_start_matches('/');
            let database = path.split('/').next().unwrap_or_default();
            (!database.is_empty()).then(|| database.to_owned())
        })
        .unwrap_or_else(|| "adam".to_owned())
}

/// A worker-tier component of a binary (see [`Agents::worker_component`]): given the store [`serve`]
/// connected and the token that says "stop", the future that is the component.
type StartComponent = Box<
    dyn FnOnce(
            DynStore,
            CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send>>
        + Send,
>;

/// The agent a process serves, as [`serve`] needs it. The binary builds it from its own files and
/// configuration, before anything connects, so a mistake in them stops the process first.
#[non_exhaustive]
pub struct Agents {
    /// The agent's registered name: the key of its stored runs, and the one agent the A2A backend
    /// serves.
    pub name: String,
    /// The agent card the A2A server serves. Required by the roles that serve A2A
    /// ([`Role::runs_control_plane`](adam_host::Role::runs_control_plane)), unused by a worker.
    pub card: Option<AgentCardConfig>,
    /// Registers the agent. A role that runs workers registers the whole agent (the root and its
    /// subagents: `Assembly::register`); one that does not registers the start-only half (the
    /// starter), which needs no model or tools.
    pub register: Register,
    /// How the runtime is set up: worker id, claim scope, concurrency.
    pub options: RuntimeOptions,
    /// How an A2A message becomes the agent's input. `None`: the default reading
    /// ([`default_inbound`](adam_a2a_runtime::default_inbound)). An agent that serves a screen
    /// sets [`vymalo_inbound`](adam_a2a_runtime::vymalo_inbound). Only the roles that serve A2A
    /// use it.
    pub inbound: Option<InboundFn>,
    /// The access tiers the agent's card declares (`access-tier/v1`), when it does: the backend
    /// then reads the tier of a message that activated the extension, refuses a bad one and says
    /// the tier a task works under. `None`: the extension is never read or said. Only the roles
    /// that serve A2A use it.
    pub access_tiers: Option<adam_a2a::AccessTiers>,
    /// The models the agent offers a message to pick (`model-pick/v1`), when its card lists them:
    /// the backend then reads the pick of a message that activated the extension and refuses one
    /// outside the choices. `None`: the extension is never read. Only the roles that serve A2A use
    /// it.
    pub model_choices: Option<adam_a2a::ModelChoices>,
    /// The reasoning levels the agent's card lists (`reasoning/v1`), when it lists any: the
    /// backend then reads the effort of a message that activated the extension and refuses one the
    /// agent's model does not take. `None`: the extension is never read. Only the roles that serve
    /// A2A use it.
    pub reasoning_efforts: Option<adam_a2a::ReasoningEfforts>,
    /// The commands the agent's card lists (`commands/v1`), when it lists any: the backend then
    /// expands a message that names one. `None`: the extension is never read. Only the roles that
    /// serve A2A use it.
    pub commands: Option<adam_a2a_runtime::Commands>,
    /// The helpers the agent serves `child-sessions/v1` for, when it has any
    /// (`AgentDef::child_sessions`): the backend then starts, continues and closes a person's
    /// sessions on them, and [`serve`] adds the extension to the card **only when the backend
    /// accepted them**. Every helper must be registered on the runtime ([`Agents::register`]: the
    /// full agent or its starter), or [`serve`] stops with [`ServeError::ChildSessions`]. `None`:
    /// the extension is never listed or read. Only the roles that serve A2A use it.
    pub child_sessions: Option<adam_a2a_runtime::ChildSessions>,
    /// The largest upload the card says (`uploads/v1`'s `params.maxBytes`), from the agent
    /// folder's `limits.max_upload_bytes`. It wins over `UPLOADS_MAX_BYTES`
    /// ([`ServiceConfig::uploads_max_bytes`]), which wins over
    /// [`DEFAULT_UPLOADS_MAX_BYTES`](adam_a2a::DEFAULT_UPLOADS_MAX_BYTES). Only the roles that
    /// serve A2A use it.
    pub uploads_max_bytes: Option<u64>,
    /// The components the binary adds to the worker tier ([`Agents::worker_component`]).
    components: Vec<(String, StartComponent)>,
}

impl Agents {
    /// The agent `name`, put on the runtime by `register`, with the default options and no card.
    pub fn new(
        name: impl Into<String>,
        register: impl FnOnce(RuntimeBuilder) -> RuntimeBuilder + Send + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            card: None,
            register: Box::new(register),
            options: RuntimeOptions::default(),
            inbound: None,
            access_tiers: None,
            model_choices: None,
            reasoning_efforts: None,
            commands: None,
            child_sessions: None,
            uploads_max_bytes: None,
            components: Vec::new(),
        }
    }

    /// Serve `card`.
    #[must_use]
    pub fn card(mut self, card: AgentCardConfig) -> Self {
        self.card = Some(card);
        self
    }

    /// Serve `card`, if there is one (a process that only runs workers has none).
    #[must_use]
    pub fn card_if(mut self, card: Option<AgentCardConfig>) -> Self {
        self.card = card;
        self
    }

    /// Set up the runtime with `options`.
    #[must_use]
    pub fn options(mut self, options: RuntimeOptions) -> Self {
        self.options = options;
        self
    }

    /// Add a component of the binary to the worker tier of the [`Host`](adam_host::Host), beside
    /// the runtime's worker: `component` is given the store [`serve`] connected and the token the
    /// host cancels when it stops the workers, and is the future of the component. It runs in the
    /// roles that run workers (`all` and `worker`) and never in `control-plane`, and it follows the
    /// rules of any host component: it must return when the token is cancelled, and a component
    /// that returns before that, or with an error, stops the process (a host error, exit 70). A
    /// component that has nothing to do waits for the token.
    ///
    /// What a binary does beside its agent that must not wait for a run: the coder's sweep of the
    /// workspaces of finished runs.
    #[must_use]
    pub fn worker_component<F, Fut>(mut self, name: impl Into<String>, component: F) -> Self
    where
        F: FnOnce(DynStore, CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), BoxError>> + Send + 'static,
    {
        self.components.push((
            name.into(),
            Box::new(move |store, stop| Box::pin(component(store, stop))),
        ));
        self
    }

    /// Read A2A messages with `f` instead of the default: how a message becomes the agent's
    /// input, for the roles that serve A2A. See [`Agents::inbound`](struct@Agents#structfield.inbound).
    #[must_use]
    pub fn inbound(
        mut self,
        f: impl Fn(&a2a::Message) -> Result<adam_runtime::Inbound, String> + Send + Sync + 'static,
    ) -> Self {
        self.inbound = Some(std::sync::Arc::new(f));
        self
    }
}

impl Agents {
    /// The agent honours `tiers` of `access-tier/v1`: declare them on the card
    /// ([`ExtensionConfig::access_tier`](adam_a2a::ExtensionConfig::access_tier)) and the backend
    /// reads and says them. See [`Agents::access_tiers`](struct@Agents#structfield.access_tiers).
    #[must_use]
    pub fn access_tiers(mut self, tiers: adam_a2a::AccessTiers) -> Self {
        self.access_tiers = Some(tiers);
        self
    }
}

impl Agents {
    /// The agent's folder says the largest upload the card lists (`uploads/v1`). See
    /// [`Agents::uploads_max_bytes`](struct@Agents#structfield.uploads_max_bytes).
    #[must_use]
    pub fn uploads_max_bytes(mut self, bytes: u64) -> Self {
        self.uploads_max_bytes = Some(bytes);
        self
    }
}

impl Agents {
    /// The agent offers `choices` of `model-pick/v1`: list them on the card
    /// ([`ExtensionConfig::model_pick`](adam_a2a::ExtensionConfig::model_pick)) and the backend
    /// reads the pick of a message that activated it. See
    /// [`Agents::model_choices`](struct@Agents#structfield.model_choices).
    #[must_use]
    pub fn model_choices(mut self, choices: adam_a2a::ModelChoices) -> Self {
        self.model_choices = Some(choices);
        self
    }
}

impl Agents {
    /// The agent takes `efforts` of `reasoning/v1`: list them on the card
    /// ([`ExtensionConfig::reasoning`](adam_a2a::ExtensionConfig::reasoning)) and the backend reads
    /// the effort of a message that activated it. See
    /// [`Agents::reasoning_efforts`](struct@Agents#structfield.reasoning_efforts).
    #[must_use]
    pub fn reasoning_efforts(mut self, efforts: adam_a2a::ReasoningEfforts) -> Self {
        self.reasoning_efforts = Some(efforts);
        self
    }
}

impl Agents {
    /// The agent serves `commands` (`commands/v1`): list them on the card
    /// ([`ExtensionConfig::commands`](adam_a2a::ExtensionConfig::commands)) and the backend
    /// expands a message that names one. See [`Agents::commands`](struct@Agents#structfield.commands).
    #[must_use]
    pub fn commands(mut self, commands: adam_a2a_runtime::Commands) -> Self {
        self.commands = Some(commands);
        self
    }
}

impl Agents {
    /// The agent serves `child-sessions/v1` for `sessions`: see
    /// [`Agents::child_sessions`](struct@Agents#structfield.child_sessions).
    #[must_use]
    pub fn child_sessions(mut self, sessions: adam_a2a_runtime::ChildSessions) -> Self {
        self.child_sessions = Some(sessions);
        self
    }
}

impl std::fmt::Debug for Agents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agents")
            .field("name", &self.name)
            .field("card", &self.card.is_some())
            .field("options", &self.options)
            .field("inbound", &self.inbound.is_some())
            .field("access_tiers", &self.access_tiers)
            .field("model_choices", &self.model_choices)
            .field("reasoning_efforts", &self.reasoning_efforts)
            .field("commands", &self.commands.as_ref().map(|c| !c.is_empty()))
            .field("child_sessions", &self.child_sessions.is_some())
            .field("uploads_max_bytes", &self.uploads_max_bytes)
            .field(
                "components",
                &self
                    .components
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// `card` with the `reasoning/v1` entry of `efforts` for `agent`, when the card lists none yet: the
/// card of a process says the levels its backend reads, and a card that already lists the entry (a
/// composition of its own) is kept as it is. An agent whose model takes no level gets no entry.
#[must_use]
pub fn card_with_reasoning(
    mut card: AgentCardConfig,
    efforts: &adam_a2a::ReasoningEfforts,
    agent: &str,
) -> AgentCardConfig {
    if card
        .extensions
        .iter()
        .any(|extension| extension.uri == adam_a2a::REASONING_EXTENSION)
    {
        return card;
    }
    if let Some(extension) = adam_a2a::ExtensionConfig::reasoning(efforts, agent) {
        card.extensions.push(extension);
    }
    card
}

/// Why [`serve`] ended with an error. Each message says which step failed and never contains a
/// credential; the cause is the [`source`](std::error::Error::source), so an error chain printed
/// whole says it once.
///
/// [`exit_code`](crate::exit_code) maps the variants: a store that cannot be reached is 69, an
/// address that cannot be bound 71, a missing card or an unregistered child-sessions helper 78, a component that stopped 70.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// The store cannot be reached, or refuses its credentials.
    #[error("connecting to {backend}")]
    Connect {
        /// The store's name: `Postgres`, `MongoDB` or `SQLite`.
        backend: &'static str,
        /// What the store said.
        #[source]
        source: StoreError,
    },
    /// The schema cannot be migrated.
    #[error("migrating the schema")]
    Migrate(#[source] StoreError),
    /// `DATABASE_URL`'s scheme is not one this binary knows.
    #[error(
        "DATABASE_URL scheme {scheme:?} is not supported: use postgres://, postgresql://, \
         mongodb://, mongodb+srv:// or sqlite://"
    )]
    UnsupportedScheme {
        /// The scheme as written.
        scheme: String,
    },
    /// The binary was built without the adapter the scheme names.
    #[error(
        "this binary was built without the {scheme} store: rebuild it with the store-{scheme} feature"
    )]
    StoreNotBuilt {
        /// The scheme as written.
        scheme: String,
    },
    /// `sqlite://` with no file path.
    #[error("DATABASE_URL sqlite:// needs a file path, e.g. sqlite:///var/lib/adam/adam.sqlite")]
    SqliteWithoutPath,
    /// A role that needs several processes on one store, on a file that holds one.
    #[error(
        "ROLE={role} cannot use sqlite://: a SQLite file is opened by one process; use \
         postgres://, mongodb:// or the store service, or ROLE=all"
    )]
    SplitRoleOnSqlite {
        /// The role as written.
        role: String,
    },
    /// A role that serves A2A was given no agent card: a mistake of the binary, not of the
    /// deployment, but one only a restart with other code fixes.
    #[error("no agent card was given for a role that serves A2A")]
    NoCard,
    /// `Agents::child_sessions` names a helper that is not registered on the runtime: a mistake of
    /// the binary, found at start, so the card never lists an extension the process does not serve.
    #[error("the child-sessions helper `{agent}` is not registered on the runtime")]
    ChildSessions {
        /// The registered name of the first helper that is missing.
        agent: String,
    },
    /// The listener cannot be bound.
    #[error("binding {addr}")]
    Bind {
        /// The address from `LISTEN_ADDR`.
        addr: SocketAddr,
        /// What the operating system said.
        #[source]
        source: std::io::Error,
    },
    /// The bound address cannot be read back.
    #[error("reading the bound address")]
    LocalAddr(#[source] std::io::Error),
    /// A component of the process stopped, panicked or ended while still needed. The message is
    /// the host's own.
    #[error(transparent)]
    Host(#[from] HostError),
    /// Push notifications are on and their webhook client cannot be built (no TLS backend), or
    /// the allow-list cannot be read. A mistake of the deployment (or the build): a restart with
    /// other code or configuration fixes it.
    #[error("setting up push notifications")]
    Push(#[source] BoxError),
}

/// Deliver push notifications until `stop`, and let a status event of any run start a round at
/// once. The events are only a hint: the loop polls the store, and the store decides.
async fn run_push_delivery(
    deliverer: PushDeliverer,
    events: BroadcastSink,
    stop: CancellationToken,
) -> Result<(), adam_error::BoxError> {
    use adam_runtime::RunEvent;
    use tokio::sync::broadcast::error::RecvError;

    let nudge = deliverer.nudger();
    let mut received = events.subscribe();
    let hint = async move {
        loop {
            match received.recv().await {
                Ok(event) if matches!(event.event, RunEvent::Status { .. }) => nudge.notify_one(),
                Ok(_) => {}
                // Missed some: a round is the way to catch up.
                Err(RecvError::Lagged(_)) => nudge.notify_one(),
                // No more events (the sink is gone): polling alone goes on.
                Err(RecvError::Closed) => std::future::pending::<()>().await,
            }
        }
    };
    tokio::select! {
        () = deliverer.run(stop.cancelled_owned()) => Ok(()),
        () = hint => Ok(()),
    }
}

/// The runtime options [`serve`] builds the runtime with: the binary's, with the deployment's idle
/// limit (`SESSION_IDLE_DAYS`), which is the environment's and the same on every role.
pub(crate) fn options_for(config: &ServiceConfig, options: RuntimeOptions) -> RuntimeOptions {
    RuntimeOptions {
        session_idle_ttl: config.sessions.idle_ttl,
        ..options
    }
}

/// What the deployment turned on in the A2A server: the options of the router, and the delivery
/// loop of push notifications (which runs beside the server, in the control plane).
fn a2a_features(
    config: &ServiceConfig,
    service: &Service,
) -> Result<(ServerOptions, Option<PushDeliverer>), ServeError> {
    let mut options = ServerOptions::default().with_docs(config.a2a.docs);
    let mut deliverer = None;
    if let Some(push) = &config.a2a.push {
        let policy = push.policy().map_err(|e| ServeError::Push(Box::new(e)))?;
        // The durable store: configs and delivery progress live in the run store.
        let support = PushSupport::new(Arc::new(service.backend.push_store()), policy);
        let delivery = PushDeliveryOptions::new()
            .with_give_up_after(push.give_up_after)
            .with_request_timeout(push.request_timeout);
        deliverer = Some(
            support
                .deliverer(Arc::new(service.backend.clone()), delivery)
                .map_err(|e| ServeError::Push(Box::new(e)))?,
        );
        options = options.with_push(support);
    }
    if let Some(signing) = &config.a2a.card_signing {
        options = options.with_card_signer(signing.signer.clone());
    }
    Ok((options, deliverer))
}

/// Drive the notifier until `stop`, and log once `LISTEN` is active.
#[cfg(feature = "store-postgres")]
async fn run_notify(
    notify: PgNotify,
    stop: impl Future<Output = ()> + Send,
) -> Result<(), adam_error::BoxError> {
    let listening = async {
        notify.wait_listening().await;
        tracing::info!("listening for notifications");
        std::future::pending::<Result<(), adam_notify_postgres::NotifyError>>().await
    };
    tokio::select! {
        result = notify.run(stop) => result.map_err(Into::into),
        result = listening => result.map_err(Into::into),
    }
}

/// The listener that turns Nagle off on every connection it accepts: a small write (a response
/// head, an SSE frame) then goes out at once instead of waiting for the previous one to be
/// acknowledged. The failure to set it is the connection's, not the process's: it is logged.
fn no_delay(
    listener: TcpListener,
) -> impl axum::serve::Listener<Io = tokio::net::TcpStream, Addr = SocketAddr> {
    use axum::serve::ListenerExt as _;
    listener.tap_io(|stream| {
        if let Err(e) = stream.set_nodelay(true) {
            tracing::debug!(error = %e, "cannot set TCP_NODELAY on an accepted connection");
        }
    })
}

/// Pinned runs for the placements that keep a run's files on one worker, any run otherwise: what a
/// binary that has a [`Placement`] puts in [`RuntimeOptions::claim_scope`].
pub fn claim_scope_for(placement: Placement) -> ClaimScope {
    if placement.pins_runs() {
        ClaimScope::Pinned
    } else {
        ClaimScope::Any
    }
}

/// Build the store `kind` names, and the signals that go with it.
///
/// Postgres is built lazily: the connection is opened by [`ChosenStore::ready`] after the
/// process's own configuration is checked. SQLite opens its file and takes the process lock now,
/// and MongoDB connects now, because both answer the store before the service is built.
async fn choose_store(config: &ServiceConfig, kind: StoreKind) -> Result<ChosenStore, ServeError> {
    let backend = kind.backend();
    match kind {
        #[cfg(feature = "store-postgres")]
        StoreKind::Postgres => {
            let pg_config = PgConfig::parse(config.database_url.expose_secret())
                .map_err(|source| ServeError::Connect { backend, source })?;
            let pool_size =
                pool_size_from_env().map_err(|source| ServeError::Connect { backend, source })?;
            let pg = PgStore::from_config(pg_config.clone(), pool_size)
                .map_err(|source| ServeError::Connect { backend, source })?;
            let pool = pg.pool().clone();
            let store: DynStore = Arc::new(pg.clone());
            // Events and wake-up/cancel signals cross processes over `NOTIFY` on the store's own
            // pool. The listener opens its own connection; the runtime holds the two halves.
            let broadcast = adam_runtime::BroadcastSink::default();
            let notify = PgNotify::new(pool, pg_config, broadcast.clone())
                .local_only(config.notify_local_only);
            let live = LiveSignals {
                broadcast: broadcast.clone(),
                sink: Arc::new(notify.event_sink()),
                notifier: Some(Arc::new(notify.notifier())),
            };
            let start: StartNotify =
                Box::new(move |stop| Box::pin(run_notify(notify, stop.cancelled_owned())));
            Ok(ChosenStore {
                store,
                live,
                notify: Some(start),
                backend,
                postgres: Some(pg),
            })
        }
        #[cfg(not(feature = "store-postgres"))]
        StoreKind::Postgres => Err(ServeError::StoreNotBuilt {
            scheme: "postgres".into(),
        }),
        #[cfg(feature = "store-sqlite")]
        StoreKind::Sqlite { path } => {
            let store = SqliteStore::open(&path)
                .map_err(|source| ServeError::Connect { backend, source })?;
            store.migrate().await.map_err(ServeError::Migrate)?;
            // One process, one file: the notifier is in-process, and there is no listener to run.
            let broadcast = adam_runtime::BroadcastSink::default();
            let notifier = adam_runtime::LocalNotifier::default();
            let live = LiveSignals {
                broadcast: broadcast.clone(),
                sink: Arc::new(broadcast),
                notifier: Some(Arc::new(notifier)),
            };
            Ok(ChosenStore {
                store: Arc::new(store),
                live,
                notify: None,
                backend,
                #[cfg(feature = "store-postgres")]
                postgres: None,
            })
        }
        #[cfg(not(feature = "store-sqlite"))]
        StoreKind::Sqlite { .. } => Err(ServeError::StoreNotBuilt {
            scheme: "sqlite".into(),
        }),
        #[cfg(feature = "store-mongodb")]
        StoreKind::Mongo { database } => {
            let store = MongoStore::connect(config.database_url.expose_secret(), &database)
                .await
                .map_err(|source| ServeError::Connect { backend, source })?;
            store.migrate().await.map_err(ServeError::Migrate)?;
            // MongoDB has no cross-process signal: workers poll.
            Ok(ChosenStore {
                store: Arc::new(store),
                live: LiveSignals::local(),
                notify: None,
                backend,
                #[cfg(feature = "store-postgres")]
                postgres: None,
            })
        }
        #[cfg(not(feature = "store-mongodb"))]
        StoreKind::Mongo { .. } => Err(ServeError::StoreNotBuilt {
            scheme: "mongodb".into(),
        }),
    }
}

/// Run the service until `shutdown` resolves (SIGTERM in a binary).
///
/// Which components run depends on [`ServiceConfig::role`]; see the module docs. On shutdown the
/// server stops taking connections (open ones get ten seconds to finish), and the workers finish
/// and commit the steps they are in before this returns; a step cut short by a hard kill is
/// picked up by another replica when its lease expires. If a component stops on its own the
/// others are stopped the same way and the error is returned.
///
/// # Errors
///
/// [`ServeError`]: connecting to or migrating Postgres, a missing card, a child-sessions helper that is not registered, binding
/// [`ServiceConfig::listen_addr`], or a component stopping unexpectedly.
pub async fn serve(
    config: &ServiceConfig,
    agents: Agents,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), ServeError> {
    run(config, agents, Listen::Addr(config.listen_addr), shutdown).await
}

/// [`serve`] on a listener the caller has already bound; [`ServiceConfig::listen_addr`] is not read.
///
/// For a host that must know its address before the process starts (a test, a supervisor handing
/// over a socket): binding first and handing the listener over leaves no moment in which another
/// process can take the port. Everything else, the order of the checks included, is [`serve`]'s.
///
/// # Errors
///
/// As [`serve`], except [`ServeError::Bind`], which this never returns.
pub async fn serve_on(
    config: &ServiceConfig,
    agents: Agents,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), ServeError> {
    run(config, agents, Listen::Bound(listener), shutdown).await
}

/// Where the server listens: an address to bind once the store is ready, or a bound listener.
enum Listen {
    Addr(SocketAddr),
    Bound(TcpListener),
}

async fn run(
    config: &ServiceConfig,
    agents: Agents,
    listen: Listen,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), ServeError> {
    let role = config.role;
    let Agents {
        name,
        card,
        register,
        options,
        inbound,
        access_tiers,
        model_choices,
        reasoning_efforts,
        commands,
        child_sessions,
        uploads_max_bytes,
        components,
    } = agents;
    if role.runs_control_plane() && card.is_none() {
        return Err(ServeError::NoCard);
    }
    // `DATABASE_URL`'s scheme picks the store (ADR 0053, decision 3). A scheme or a role the
    // binary cannot serve is a configuration error before anything connects.
    let kind = store_kind(config.database_url.expose_secret(), role)?;
    let chosen = choose_store(config, kind).await?;
    let store = chosen.store.clone();
    let push_events = chosen.live.broadcast.clone();
    let options = options_for(config, options);
    let workers = role.runs_workers().then_some(options.concurrency);
    let worker_id = options.worker_id.clone();
    let component_store = store.clone();
    let service = Service::new_with(
        register(Runtime::builder(store)),
        name,
        &options,
        chosen.live.clone(),
    )
    .with_inbound(inbound)
    .with_access_tiers(access_tiers)
    .with_model_choices(model_choices)
    .with_reasoning_efforts(reasoning_efforts)
    .with_commands(commands)
    .with_child_sessions(child_sessions.clone());
    // The card lists the extension if and only if this process serves it.
    let mut card = card;
    if let (Some(card), Some(locale)) = (card.as_mut(), &config.default_locale) {
        card.extensions
            .push(adam_a2a::ExtensionConfig::locale(locale));
    }
    // The largest upload the card says: the folder's, else the environment's, else the default.
    if let Some(card) = card.as_mut()
        && !card
            .extensions
            .iter()
            .any(|listed| listed.uri == adam_a2a::UPLOADS_EXTENSION)
    {
        card.extensions.push(adam_a2a::ExtensionConfig::uploads(
            uploads_max_bytes
                .or(config.uploads_max_bytes)
                .unwrap_or(adam_a2a::DEFAULT_UPLOADS_MAX_BYTES),
        ));
    }
    if let (Some(card), Some(choices)) = (card.as_mut(), service.model_choices())
        && let Some(extension) = adam_a2a::ExtensionConfig::model_pick(choices)
    {
        card.extensions.push(extension);
    }
    // The levels of `reasoning/v1`, the root agent's model's: `None` when it takes none, and the
    // card keeps whatever it already says (a composition of its own listed the entry).
    if let Some(efforts) = service.reasoning_efforts()
        && let Some(listed) = card.take()
    {
        card = Some(card_with_reasoning(
            listed,
            efforts,
            service.backend.agent(),
        ));
    }
    if role.runs_control_plane()
        && let Some(sessions) = &child_sessions
    {
        if service.child_sessions().is_none() {
            let registered = service.runtime.agent_names();
            let missing = sessions
                .agents()
                .iter()
                .find(|helper| !registered.contains(&helper.agent))
                .map_or_else(String::new, |helper| helper.agent.clone());
            return Err(ServeError::ChildSessions { agent: missing });
        }
        if let (Some(card), Some(extension)) = (card.as_mut(), sessions.card_extension()) {
            card.extensions.push(extension);
        }
    }

    // The process's own configuration is checked; now the store may connect (Postgres) and
    // migrate. SQLite and MongoDB were opened and migrated when they were chosen.
    chosen.ready().await?;
    let notify = chosen.notify;

    // Bind before anything runs: an address that cannot be bound fails the process at once.
    let listener = match listen {
        Listen::Addr(addr) => TcpListener::bind(addr)
            .await
            .map_err(|source| ServeError::Bind { addr, source })?,
        Listen::Bound(listener) => listener,
    };
    // The bound address, not the configured one: `LISTEN_ADDR=…:0` picks a
    // free port, and this line is how a supervisor (or a test) learns it.
    let addr = listener.local_addr().map_err(ServeError::LocalAddr)?;
    // Nagle holds a small write (a response head, an SSE frame) until the previous one is
    // acknowledged, which a client that is not sending delays by up to 40 ms.
    let listener = no_delay(listener);
    tracing::info!(%addr, %role, workers, worker_id = worker_id.as_deref(), "listening");

    let host = Host::new(role)
        .control_plane_drain(Some(SERVER_DRAIN))
        // Workers are never cut short here; the orchestrator's grace period bounds the wait.
        .worker_grace(None);
    let mut push_delivery = None;
    let host = match card {
        Some(card) if role.runs_control_plane() => {
            let (options, deliverer) = a2a_features(config, &service)?;
            push_delivery = deliverer;
            let app = service.router_with_options(
                card,
                AuthConfig::BearerTokens(config.a2a_bearer_tokens.clone()),
                options,
            );
            host.control_plane("a2a-server", |stop| async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(stop.cancelled_owned())
                    .await
                    .map_err(Into::into)
            })
        }
        // No A2A here, but probes still need an answer: the same `/healthz` the A2A router serves.
        _ => host.worker("health", |stop| async move {
            axum::serve(listener, A2aServer::health_router())
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
                .map_err(Into::into)
        }),
    };
    // With workers, `notify` is a worker component that stops when the `worker` component is done
    // (or gone): the host cancels the components of a tier together, and a step finishing after
    // the cancel still emits events and signals that should be sent. Without workers it stops with
    // the control plane.
    let notify_stop = CancellationToken::new();
    let host = if role.runs_workers() {
        let runtime = service.runtime.clone();
        let done = notify_stop.clone().drop_guard();
        let host = host.worker("worker", |stop| async move {
            let _done = done;
            runtime
                .run_worker(stop.cancelled_owned())
                .await
                .map_err(Into::into)
        });
        // Postgres has a listener; SQLite's notifier is in-process and MongoDB has none.
        match notify {
            Some(start) => host.worker("notify", move |_| start(notify_stop)),
            None => host,
        }
    } else {
        match notify {
            Some(start) => host.control_plane("notify", start),
            None => host,
        }
    };

    // Push notifications are delivered beside the server that accepts them, in the control plane:
    // it needs the store and the backend, not the agent.
    let host = match push_delivery {
        Some(deliverer) => host.control_plane("push-delivery", |stop| {
            run_push_delivery(deliverer, push_events, stop)
        }),
        None => host,
    };

    // The binary's own components: they run in the worker tier, so the host stops them with the
    // workers (and never starts them in a control plane).
    let host = components.into_iter().fold(host, |host, (name, start)| {
        let store = component_store.clone();
        host.worker(name, move |stop| start(store, stop))
    });

    host.run(shutdown).await?;
    tracing::info!("stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The connection the helper accepts writes at once: `TCP_NODELAY` is on it.
    ///
    /// This proves the helper, not that [`serve`] uses it: the server's own socket is not
    /// reachable from a test, so that wiring is the one `no_delay(listener)` call in `serve`,
    /// reviewed by eye.
    #[tokio::test]
    async fn an_accepted_connection_has_nagle_off() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut listener = no_delay(listener);
        let client = tokio::spawn(async move { tokio::net::TcpStream::connect(addr).await });
        let (accepted, _peer) = axum::serve::Listener::accept(&mut listener).await;
        assert!(
            accepted.nodelay().unwrap(),
            "the accepted socket has TCP_NODELAY set"
        );
        client.await.unwrap().unwrap();
    }

    #[test]
    fn the_database_url_scheme_picks_the_store_and_refuses_a_split_sqlite_role() {
        assert_eq!(
            store_kind("postgres://u@h/d", Role::All).unwrap(),
            StoreKind::Postgres
        );
        assert_eq!(
            store_kind("postgres://u@h/d", Role::All).unwrap().backend(),
            "Postgres",
            "the error says which store it could not reach"
        );
        assert_eq!(
            store_kind("postgresql://u@h/d", Role::All).unwrap(),
            StoreKind::Postgres
        );
        assert_eq!(
            store_kind("mongodb://h/db", Role::All).unwrap().backend(),
            "MongoDB"
        );
        assert_eq!(
            store_kind("sqlite:///tmp/adam.sqlite", Role::All)
                .unwrap()
                .backend(),
            "SQLite"
        );
        assert_eq!(
            store_kind("mongodb://h/db", Role::All).unwrap(),
            StoreKind::Mongo {
                database: "db".into()
            }
        );
        assert_eq!(
            store_kind("mongodb+srv://h/db?retryWrites=true", Role::All).unwrap(),
            StoreKind::Mongo {
                database: "db".into()
            }
        );
        assert_eq!(
            store_kind("mongodb://h", Role::All).unwrap(),
            StoreKind::Mongo {
                database: "adam".into()
            }
        );
        assert_eq!(
            store_kind("sqlite:///tmp/adam.sqlite", Role::All).unwrap(),
            StoreKind::Sqlite {
                path: "/tmp/adam.sqlite".into()
            }
        );
        assert_eq!(
            store_kind("sqlite://relative.db", Role::All).unwrap(),
            StoreKind::Sqlite {
                path: "relative.db".into()
            }
        );
        assert_eq!(
            store_kind("sqlite://./x.db", Role::All).unwrap(),
            StoreKind::Sqlite {
                path: "./x.db".into()
            }
        );

        // A split role needs several processes on one store, which a SQLite file refuses.
        for role in [Role::Worker, Role::ControlPlane] {
            let error = store_kind("sqlite:///tmp/adam.sqlite", role).unwrap_err();
            assert!(
                matches!(&error, ServeError::SplitRoleOnSqlite { role: written } if written == role.as_str()),
                "{role}: {error:?}"
            );
            assert!(error.to_string().contains("one process"), "{error}");
            assert_eq!(crate::exit_code(&error), 78, "{role}");
        }

        // An unknown scheme is a configuration error that names the scheme and the accepted ones.
        for url in ["mysql://h/d", "sqlite", "memory:"] {
            let error = store_kind(url, Role::All).unwrap_err();
            assert!(
                matches!(&error, ServeError::UnsupportedScheme { .. }),
                "{url}: {error:?}"
            );
            assert!(error.to_string().contains("postgres://"), "{error}");
            assert_eq!(crate::exit_code(&error), 78, "{url}");
        }
        let error = store_kind("sqlite://", Role::All).unwrap_err();
        assert!(matches!(error, ServeError::SqliteWithoutPath), "{error:?}");
        assert_eq!(crate::exit_code(&error), 78);
    }

    #[test]
    fn a_placement_that_pins_runs_claims_pinned_runs_and_the_others_any() {
        for placement in Placement::VALUES {
            let want = if placement.pins_runs() {
                ClaimScope::Pinned
            } else {
                ClaimScope::Any
            };
            assert_eq!(claim_scope_for(placement), want, "{placement}");
        }
    }

    #[test]
    fn the_errors_say_the_step_and_keep_the_cause_apart() {
        let down = ServeError::Connect {
            backend: "Postgres",
            source: StoreError::unavailable(std::io::Error::other("refused")),
        };
        assert_eq!(down.to_string(), "connecting to Postgres");
        assert!(std::error::Error::source(&down).is_some());
        let bind = ServeError::Bind {
            addr: "127.0.0.1:1".parse().unwrap(),
            source: std::io::Error::from(std::io::ErrorKind::AddrInUse),
        };
        assert_eq!(bind.to_string(), "binding 127.0.0.1:1");
        assert!(std::error::Error::source(&bind).is_some());
        assert!(std::error::Error::source(&ServeError::NoCard).is_none());
    }
    use std::time::Duration;

    #[test]
    fn serve_builds_the_runtime_with_the_configured_idle_limit() {
        let vars = |name: &str| match name {
            "DATABASE_URL" => Some("postgres://u:p@db/adam".to_owned()),
            "A2A_BEARER_TOKENS" => Some("t".to_owned()),
            "PUBLIC_URL" => Some("http://agent.svc:8080/".to_owned()),
            "SESSION_IDLE_DAYS" => Some("3".to_owned()),
            _ => None,
        };
        let mut problems = Vec::new();
        let config = ServiceConfig::parse(&vars, &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
        // Whatever the binary's own options say, the environment's value wins.
        let own = RuntimeOptions {
            session_idle_ttl: Duration::from_secs(60),
            ..RuntimeOptions::default()
        };
        let options = options_for(&config, own);
        assert_eq!(options.session_idle_ttl, Duration::from_secs(3 * 24 * 3600));
        let runtime = Service::new(
            adam_runtime::Runtime::builder(std::sync::Arc::new(adam_core::MemoryStore::new())),
            "x",
            &options,
        )
        .runtime;
        assert_eq!(
            runtime.session_idle_ttl(),
            Duration::from_secs(3 * 24 * 3600)
        );
    }
}
