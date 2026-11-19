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
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use adam_a2a::push::{PushDeliverer, PushDeliveryOptions, PushSupport};
use adam_a2a::{A2aServer, AgentCardConfig, AuthConfig, ServerOptions};
use adam_a2a_runtime::InboundFn;
use adam_core::{ClaimScope, DynStore, StoreError};
use adam_error::BoxError;
use adam_host::{Host, HostError, Placement};
use adam_notify_postgres::PgNotify;
use adam_runtime::{BroadcastSink, Runtime, RuntimeBuilder};
use adam_store_postgres::PgStore;
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
            commands: None,
            child_sessions: None,
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
            .field("commands", &self.commands.as_ref().map(|c| !c.is_empty()))
            .field("child_sessions", &self.child_sessions.is_some())
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

/// Why [`serve`] ended with an error. Each message says which step failed and never contains a
/// credential; the cause is the [`source`](std::error::Error::source), so an error chain printed
/// whole says it once.
///
/// [`exit_code`](crate::exit_code) maps the variants: a store that cannot be reached is 69, an
/// address that cannot be bound 71, a missing card or an unregistered child-sessions helper 78, a component that stopped 70.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// Postgres cannot be reached, or refuses the credentials.
    #[error("connecting to Postgres")]
    Connect(#[source] StoreError),
    /// The schema cannot be migrated.
    #[error("migrating the schema")]
    Migrate(#[source] StoreError),
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
    let role = config.role;
    let Agents {
        name,
        card,
        register,
        options,
        inbound,
        access_tiers,
        commands,
        child_sessions,
        components,
    } = agents;
    if role.runs_control_plane() && card.is_none() {
        return Err(ServeError::NoCard);
    }
    // The store connects lazily so that the process's own configuration is checked before it
    // depends on the database: a mistake of the binary is exit 78 whether or not Postgres answers.
    let pg =
        PgStore::connect_lazy(config.database_url.expose_secret()).map_err(ServeError::Connect)?;
    let pool = pg.pool().clone();
    let store: DynStore = Arc::new(pg.clone());

    // Events and wake-up/cancel signals cross processes over `NOTIFY` on the store's own pool. The
    // listener (`notify.run`) is a host component below; the runtime only holds the two halves.
    let broadcast = BroadcastSink::default();
    let notify = PgNotify::new(pool, broadcast.clone()).local_only(config.notify_local_only);
    let push_events = broadcast.clone();
    let live = LiveSignals {
        broadcast,
        sink: Arc::new(notify.event_sink()),
        notifier: Some(Arc::new(notify.notifier())),
    };
    let options = options_for(config, options);
    let workers = role.runs_workers().then_some(options.concurrency);
    let worker_id = options.worker_id.clone();
    let component_store = store.clone();
    let service = Service::new_with(register(Runtime::builder(store)), name, &options, live)
        .with_inbound(inbound)
        .with_access_tiers(access_tiers)
        .with_commands(commands)
        .with_child_sessions(child_sessions.clone());
    // The card lists the extension if and only if this process serves it.
    let mut card = card;
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

    pg.ping().await.map_err(ServeError::Connect)?;
    adam_core::Store::migrate(&pg)
        .await
        .map_err(ServeError::Migrate)?;

    // Bind before anything runs: an address that cannot be bound fails the process at once.
    let listener = TcpListener::bind(config.listen_addr)
        .await
        .map_err(|source| ServeError::Bind {
            addr: config.listen_addr,
            source,
        })?;
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
        host.worker("worker", |stop| async move {
            let _done = done;
            runtime
                .run_worker(stop.cancelled_owned())
                .await
                .map_err(Into::into)
        })
        .worker("notify", |_| {
            run_notify(notify, notify_stop.cancelled_owned())
        })
    } else {
        host.control_plane("notify", |stop| run_notify(notify, stop.cancelled_owned()))
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
        let down = ServeError::Connect(StoreError::unavailable(std::io::Error::other("refused")));
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
