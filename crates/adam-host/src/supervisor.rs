use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use adam_error::{BoxError, Classify, ErrorClass, report};
use tokio::task::{AbortHandle, Id, JoinError, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::Role;

type BoxFuture = Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send + 'static>>;
type Start = Box<dyn FnOnce(CancellationToken) -> BoxFuture + Send + 'static>;

/// Why the host stopped with a failure.
///
/// Every variant names the component. A message describes its own layer only; the cause of
/// [`Stopped`](Self::Stopped) and [`Panicked`](Self::Panicked) is the `source`, printed once by
/// [`adam_error::report`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HostError {
    /// A component returned an error.
    #[error("component `{component}` stopped")]
    Stopped {
        /// The name the host gave the component.
        component: String,
        /// What the component returned.
        #[source]
        source: BoxError,
    },
    /// A component panicked.
    #[error("component `{component}` panicked")]
    Panicked {
        /// The name the host gave the component.
        component: String,
        /// The join error; it carries the panic message.
        #[source]
        source: BoxError,
    },
    /// A component returned `Ok` while the host was still meant to run.
    #[error("component `{component}` ended before shutdown")]
    EndedEarly {
        /// The name the host gave the component.
        component: String,
    },
    /// No registered component matches the role, so there is nothing to run.
    #[error("no registered component matches the role")]
    NothingToRun,
}

impl Classify for HostError {
    fn class(&self) -> ErrorClass {
        match self {
            HostError::Stopped { .. }
            | HostError::Panicked { .. }
            | HostError::EndedEarly { .. }
            | HostError::NothingToRun => ErrorClass::Internal,
        }
    }
}

/// A shared, data-only view of the host's state.
///
/// Clone it freely. The host app decides how to show it: a `/readyz` route, a gauge, a log line.
/// Before [`Host::run`] starts nothing is ready and nothing is shutting down.
#[derive(Debug, Clone, Default)]
pub struct Health {
    state: Arc<HealthState>,
}

#[derive(Debug, Default)]
struct HealthState {
    expected: AtomicUsize,
    live: AtomicUsize,
    shutting_down: AtomicBool,
}

impl Health {
    /// Every started component is still running, and the host is not shutting down.
    pub fn ready(&self) -> bool {
        let expected = self.state.expected.load(Ordering::SeqCst);
        expected > 0 && self.state.live.load(Ordering::SeqCst) == expected && !self.shutting_down()
    }

    /// The host has begun to stop: a signal arrived, or a component ended on its own.
    pub fn shutting_down(&self) -> bool {
        self.state.shutting_down.load(Ordering::SeqCst)
    }
}

/// Decrements the live count when its component's task ends, is aborted or panics.
struct LiveGuard(Arc<HealthState>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Component {
    name: String,
    start: Start,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    ControlPlane,
    Worker,
}

struct Running {
    name: String,
    tier: Tier,
    abort: AbortHandle,
}

/// A role-aware supervisor.
///
/// The host registers its components with [`control_plane`](Self::control_plane) and
/// [`worker`](Self::worker). [`run`](Self::run) starts only those the [`Role`] asks for.
///
/// Stop order, the same in every host:
///
/// 1. The first component to end on its own, or the shutdown future, marks the host as shutting
///    down ([`Health::shutting_down`]).
/// 2. The control-plane components are cancelled, and given the drain time to finish.
/// 3. The workers are cancelled, and given the grace time to finish.
/// 4. A component still running when its time is up is aborted.
///
/// `run` returns the first failure: an error, a panic, or a component that ended before
/// shutdown, reported by name.
///
/// # Example
///
/// The host maps its own components onto the two halves. The same list serves every role.
///
/// ```
/// use std::time::Duration;
/// use adam_host::{Host, Role};
///
/// # let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build()?;
/// # runtime.block_on(async {
/// // The host reads its own variable (here: not set); `None` or blank means `all`.
/// let raw: Option<String> = None; // std::env::var("ROLE").ok()
/// let role = Role::from_optional(raw.as_deref())?;
///
/// let host = Host::new(role)
///     .control_plane("http", |stop| async move {
///         // serve until `stop` is cancelled
///         stop.cancelled().await;
///         Ok(())
///     })
///     .worker("dispatcher", |stop| async move {
///         // claim and run work until `stop` is cancelled
///         stop.cancelled().await;
///         Ok(())
///     })
///     .control_plane_drain(Some(Duration::from_secs(10)))
///     .worker_grace(None);
/// let health = host.health(); // expose it on /readyz, or ignore it
///
/// // A real host passes its SIGTERM future here.
/// host.run(tokio::time::sleep(Duration::from_millis(10))).await?;
/// assert!(health.shutting_down());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// # })?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Host {
    role: Role,
    control_plane: Vec<Component>,
    workers: Vec<Component>,
    drain: Option<Duration>,
    grace: Option<Duration>,
    health: Health,
}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = |components: &[Component]| {
            components
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
        };
        f.debug_struct("Host")
            .field("role", &self.role)
            .field("control_plane", &names(&self.control_plane))
            .field("workers", &names(&self.workers))
            .field("control_plane_drain", &self.drain)
            .field("worker_grace", &self.grace)
            .finish_non_exhaustive()
    }
}

impl Host {
    /// A host for this role, with no components. Drain and grace wait for ever.
    pub fn new(role: Role) -> Self {
        Self {
            role,
            control_plane: Vec::new(),
            workers: Vec::new(),
            drain: None,
            grace: None,
            health: Health::default(),
        }
    }

    /// The role this host runs.
    pub fn role(&self) -> Role {
        self.role
    }

    /// Register a control-plane component (an HTTP server, say). It runs only when
    /// [`Role::runs_control_plane`] is true; it is always registered, so the host builds one
    /// list for every role.
    ///
    /// The component must return when the token is cancelled. It receives it when the host
    /// begins to stop.
    pub fn control_plane<F, Fut>(mut self, name: impl Into<String>, component: F) -> Self
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), BoxError>> + Send + 'static,
    {
        self.control_plane.push(Component::new(name, component));
        self
    }

    /// Register a worker component (a dispatcher loop, say). It runs only when
    /// [`Role::runs_workers`] is true.
    ///
    /// The component must return when the token is cancelled. It receives it after the
    /// control-plane components have stopped.
    pub fn worker<F, Fut>(mut self, name: impl Into<String>, component: F) -> Self
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), BoxError>> + Send + 'static,
    {
        self.workers.push(Component::new(name, component));
        self
    }

    /// How long the control-plane components get to finish after they are cancelled.
    /// `None` waits for ever (the default). Open connections that never close (SSE) need a bound.
    pub fn control_plane_drain(mut self, drain: Option<Duration>) -> Self {
        self.drain = drain;
        self
    }

    /// How long the workers get to finish after they are cancelled. `None` waits for ever (the
    /// default), so in-flight steps finish and commit. `Some` aborts the workers past it.
    pub fn worker_grace(mut self, grace: Option<Duration>) -> Self {
        self.grace = grace;
        self
    }

    /// A handle to the host's state. Take it before [`run`](Self::run), which consumes the host.
    pub fn health(&self) -> Health {
        self.health.clone()
    }

    /// Run the components the role asks for until `shutdown` resolves or one ends on its own,
    /// then stop them in order (see the type docs).
    ///
    /// Returns [`HostError::NothingToRun`] at once when no registered component matches the
    /// role, and the first failure otherwise.
    pub async fn run<S>(self, shutdown: S) -> Result<(), HostError>
    where
        S: Future<Output = ()>,
    {
        let Host {
            role,
            control_plane,
            workers,
            drain,
            grace,
            health,
        } = self;
        let control_plane = if role.runs_control_plane() {
            control_plane
        } else {
            Vec::new()
        };
        let workers = if role.runs_workers() {
            workers
        } else {
            Vec::new()
        };
        let total = control_plane.len() + workers.len();
        if total == 0 {
            return Err(HostError::NothingToRun);
        }

        let stop_control_plane = CancellationToken::new();
        let stop_workers = CancellationToken::new();
        health.state.expected.store(total, Ordering::SeqCst);
        health.state.live.store(total, Ordering::SeqCst);

        let mut set = JoinSet::new();
        let mut running: HashMap<Id, Running> = HashMap::new();
        for (tier, components, token) in [
            (Tier::ControlPlane, control_plane, &stop_control_plane),
            (Tier::Worker, workers, &stop_workers),
        ] {
            for Component { name, start } in components {
                let live = LiveGuard(Arc::clone(&health.state));
                let token = token.clone();
                let abort = set.spawn(async move {
                    let _live = live;
                    start(token).await
                });
                running.insert(abort.id(), Running { name, tier, abort });
            }
        }
        tracing::info!(role = %role, components = total, "host started");

        let mut first: Option<HostError> = None;
        let mut shutdown = std::pin::pin!(shutdown);
        tokio::select! {
            biased;
            () = &mut shutdown => tracing::info!("shutdown requested"),
            joined = set.join_next_with_id() => {
                if let Some(joined) = joined {
                    record(&mut first, settle(&mut running, joined, true));
                }
                tracing::info!("a component ended on its own; shutting down");
            }
        }
        health.state.shutting_down.store(true, Ordering::SeqCst);

        stop_control_plane.cancel();
        wait_for(
            Tier::ControlPlane,
            drain,
            &mut set,
            &mut running,
            &mut first,
        )
        .await;
        stop_workers.cancel();
        wait_for(Tier::Worker, grace, &mut set, &mut running, &mut first).await;

        tracing::info!("host stopped");
        first.map_or(Ok(()), Err)
    }
}

impl Component {
    fn new<F, Fut>(name: impl Into<String>, component: F) -> Self
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), BoxError>> + Send + 'static,
    {
        Self {
            name: name.into(),
            start: Box::new(move |stop| Box::pin(component(stop))),
        }
    }
}

/// Wait until every component of `tier` has ended. Past `limit`, abort the ones left.
async fn wait_for(
    tier: Tier,
    limit: Option<Duration>,
    set: &mut JoinSet<Result<(), BoxError>>,
    running: &mut HashMap<Id, Running>,
    first: &mut Option<HostError>,
) {
    let mut deadline = limit.map(|limit| Instant::now() + limit);
    while running.values().any(|r| r.tier == tier) {
        let next = set.join_next_with_id();
        let joined = match deadline {
            None => next.await,
            Some(at) => match tokio::time::timeout_at(at, next).await {
                Ok(joined) => joined,
                Err(_elapsed) => {
                    for r in running.values().filter(|r| r.tier == tier) {
                        tracing::warn!(component = %r.name, "did not stop in time; aborting it");
                        r.abort.abort();
                    }
                    // Aborted tasks end at once; reap them without a limit.
                    deadline = None;
                    continue;
                }
            },
        };
        let Some(joined) = joined else { break };
        record(first, settle(running, joined, false));
    }
}

/// What a finished task means. `early` is true while the host is still meant to run.
fn settle(
    running: &mut HashMap<Id, Running>,
    joined: Result<(Id, Result<(), BoxError>), JoinError>,
    early: bool,
) -> Option<HostError> {
    let (id, outcome) = match joined {
        Ok((id, outcome)) => (id, Ok(outcome)),
        Err(join) => (join.id(), Err(join)),
    };
    let component = running.remove(&id)?.name;
    match outcome {
        Ok(Ok(())) if early => Some(HostError::EndedEarly { component }),
        Ok(Ok(())) => None,
        Ok(Err(source)) => Some(HostError::Stopped { component, source }),
        Err(join) if join.is_panic() => Some(HostError::Panicked {
            component,
            source: Box::new(join),
        }),
        // Only the host aborts a task, after its time is up. Not a failure of the component.
        Err(_cancelled) => None,
    }
}

/// Keep the first failure; log the later ones, so none is lost.
fn record(first: &mut Option<HostError>, failure: Option<HostError>) {
    let Some(failure) = failure else { return };
    if first.is_none() {
        *first = Some(failure);
    } else {
        tracing::warn!(error = %report(&failure), "another component failed while stopping");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    type Log = Arc<Mutex<Vec<String>>>;

    fn log() -> Log {
        Arc::default()
    }

    fn note(log: &Log, line: impl Into<String>) {
        log.lock().unwrap().push(line.into());
    }

    fn lines(log: &Log) -> Vec<String> {
        log.lock().unwrap().clone()
    }

    const TICK: Duration = Duration::from_millis(10);

    /// Logs `<name>: started`, waits for the stop token, waits `stopping_takes`, logs
    /// `<name>: stopped`.
    fn polite(
        log: &Log,
        name: &'static str,
        stopping_takes: Duration,
    ) -> impl FnOnce(CancellationToken) -> BoxFuture + Send + 'static {
        let log = Arc::clone(log);
        move |stop| {
            Box::pin(async move {
                note(&log, format!("{name}: started"));
                stop.cancelled().await;
                note(&log, format!("{name}: stopping"));
                tokio::time::sleep(stopping_takes).await;
                note(&log, format!("{name}: stopped"));
                Ok(())
            })
        }
    }

    /// Never looks at the token. Sets `aborted` when it is dropped before it finished.
    fn deaf(
        aborted: &Arc<AtomicBool>,
        finishes_after: Option<Duration>,
    ) -> impl FnOnce(CancellationToken) -> BoxFuture + Send + 'static {
        struct SetOnDrop(Arc<AtomicBool>, bool);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                if self.1 {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
        }
        let aborted = Arc::clone(aborted);
        move |_stop| {
            Box::pin(async move {
                let mut flag = SetOnDrop(aborted, true);
                match finishes_after {
                    Some(after) => tokio::time::sleep(after).await,
                    None => std::future::pending::<()>().await,
                }
                // Finished by itself: not aborted.
                flag.1 = false;
                Ok(())
            })
        }
    }

    fn after_tick() -> impl Future<Output = ()> {
        tokio::time::sleep(TICK)
    }

    fn boom() -> BoxError {
        "boom".into()
    }

    // ---- role gating ----

    #[tokio::test(start_paused = true)]
    async fn only_the_components_of_the_role_run() {
        let table = [
            (Role::All, vec!["http", "dispatcher"]),
            (Role::ControlPlane, vec!["http"]),
            (Role::Worker, vec!["dispatcher"]),
        ];
        for (role, expected) in table {
            let log = log();
            let result = Host::new(role)
                .control_plane("http", polite(&log, "http", Duration::ZERO))
                .worker("dispatcher", polite(&log, "dispatcher", Duration::ZERO))
                .run(after_tick())
                .await;
            assert!(result.is_ok(), "{role}: {result:?}");
            let mut started: Vec<String> = lines(&log)
                .into_iter()
                .filter_map(|l| l.strip_suffix(": started").map(str::to_owned))
                .collect();
            started.sort();
            let mut expected: Vec<String> = expected.into_iter().map(str::to_owned).collect();
            expected.sort();
            assert_eq!(started, expected, "{role}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_component_of_another_role_is_never_called() {
        let called = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&called);
        let log = log();
        Host::new(Role::Worker)
            .control_plane("http", move |_stop| {
                flag.store(true, Ordering::SeqCst);
                async { Ok(()) }
            })
            .worker("dispatcher", polite(&log, "dispatcher", Duration::ZERO))
            .run(after_tick())
            .await
            .unwrap();
        assert!(!called.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_to_run_returns_at_once() {
        let log = log();
        // A worker role with only control-plane components.
        let result = Host::new(Role::Worker)
            .control_plane("http", polite(&log, "http", Duration::ZERO))
            .run(std::future::pending())
            .await;
        assert!(matches!(result, Err(HostError::NothingToRun)), "{result:?}");
        // A control-plane role with only workers.
        let result = Host::new(Role::ControlPlane)
            .worker("dispatcher", polite(&log, "dispatcher", Duration::ZERO))
            .run(std::future::pending())
            .await;
        assert!(matches!(result, Err(HostError::NothingToRun)), "{result:?}");
        // No components at all.
        let result = Host::new(Role::All).run(std::future::pending()).await;
        assert!(matches!(result, Err(HostError::NothingToRun)), "{result:?}");
        assert!(lines(&log).is_empty(), "nothing must have started");
    }

    // ---- stop order and bounds ----

    #[tokio::test(start_paused = true)]
    async fn a_clean_shutdown_stops_the_control_plane_before_the_workers() {
        let log = log();
        let result = Host::new(Role::All)
            .control_plane("http", polite(&log, "http", Duration::from_secs(1)))
            .worker("dispatcher", polite(&log, "dispatcher", Duration::ZERO))
            .run(after_tick())
            .await;
        assert!(result.is_ok(), "{result:?}");
        let stops: Vec<String> = lines(&log)
            .into_iter()
            .filter(|l| !l.ends_with(": started"))
            .collect();
        assert_eq!(
            stops,
            [
                "http: stopping",
                "http: stopped",
                "dispatcher: stopping",
                "dispatcher: stopped"
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_drain_bound_aborts_a_control_plane_that_ignores_cancel() {
        let log = log();
        let aborted = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        let result = Host::new(Role::All)
            .control_plane("http", deaf(&aborted, None))
            .worker("dispatcher", polite(&log, "dispatcher", Duration::ZERO))
            .control_plane_drain(Some(Duration::from_secs(5)))
            .run(after_tick())
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(aborted.load(Ordering::SeqCst), "the component was aborted");
        let took = start.elapsed();
        assert!(
            took >= Duration::from_secs(5) && took < Duration::from_secs(6),
            "{took:?}"
        );
        assert!(lines(&log).contains(&"dispatcher: stopped".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn no_drain_bound_waits_for_a_slow_control_plane() {
        let aborted = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        let result = Host::new(Role::ControlPlane)
            .control_plane("http", deaf(&aborted, Some(Duration::from_secs(3600))))
            .control_plane_drain(None)
            .run(after_tick())
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(start.elapsed() >= Duration::from_secs(3600));
        assert!(!aborted.load(Ordering::SeqCst), "it finished by itself");
    }

    #[tokio::test(start_paused = true)]
    async fn the_grace_bound_aborts_a_worker_that_ignores_cancel() {
        let aborted = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        let result = Host::new(Role::Worker)
            .worker("dispatcher", deaf(&aborted, None))
            .worker_grace(Some(Duration::from_secs(30)))
            .run(after_tick())
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(aborted.load(Ordering::SeqCst), "the worker was aborted");
        let took = start.elapsed();
        assert!(
            took >= Duration::from_secs(30) && took < Duration::from_secs(31),
            "{took:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_grace_bound_waits_for_a_slow_worker() {
        let log = log();
        let start = Instant::now();
        let result = Host::new(Role::Worker)
            .worker(
                "dispatcher",
                polite(&log, "dispatcher", Duration::from_secs(3600)),
            )
            .worker_grace(None)
            .run(after_tick())
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(start.elapsed() >= Duration::from_secs(3600));
        assert!(lines(&log).contains(&"dispatcher: stopped".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn workers_keep_running_while_the_control_plane_drains() {
        let log = log();
        // The worker logs the moment it is cancelled; the control plane needs 2s to stop.
        Host::new(Role::All)
            .control_plane("http", polite(&log, "http", Duration::from_secs(2)))
            .worker("dispatcher", polite(&log, "dispatcher", Duration::ZERO))
            .run(after_tick())
            .await
            .unwrap();
        let all = lines(&log);
        let http_stopped = all.iter().position(|l| l == "http: stopped").unwrap();
        let worker_stopping = all
            .iter()
            .position(|l| l == "dispatcher: stopping")
            .unwrap();
        assert!(http_stopped < worker_stopping, "{all:?}");
    }

    // ---- failures ----

    #[tokio::test(start_paused = true)]
    async fn a_component_error_ends_the_host_and_is_named() {
        let log = log();
        let result = Host::new(Role::All)
            .control_plane("http", polite(&log, "http", Duration::ZERO))
            .worker("dispatcher", |_stop| async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Err(boom())
            })
            .run(std::future::pending())
            .await;
        let err = result.unwrap_err();
        match &err {
            HostError::Stopped { component, source } => {
                assert_eq!(component, "dispatcher");
                assert_eq!(source.to_string(), "boom");
            }
            other => panic!("expected Stopped, got {other:?}"),
        }
        assert_eq!(report(&err), "component `dispatcher` stopped: boom");
        // The rest of the host was stopped too.
        assert!(lines(&log).contains(&"http: stopped".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_panic_is_reported_as_panicked() {
        let log = log();
        let result = Host::new(Role::All)
            .control_plane("http", polite(&log, "http", Duration::ZERO))
            .worker("dispatcher", |_stop| async {
                tokio::time::sleep(TICK).await;
                panic!("kaboom")
            })
            .run(std::future::pending())
            .await;
        let err = result.unwrap_err();
        match &err {
            HostError::Panicked { component, .. } => assert_eq!(component, "dispatcher"),
            other => panic!("expected Panicked, got {other:?}"),
        }
        assert!(report(&err).contains("kaboom"), "{}", report(&err));
        assert!(lines(&log).contains(&"http: stopped".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_component_that_returns_ok_early_is_ended_early() {
        let log = log();
        let result = Host::new(Role::All)
            .control_plane("http", |_stop| async { Ok(()) })
            .worker("dispatcher", polite(&log, "dispatcher", Duration::ZERO))
            .run(std::future::pending())
            .await;
        match result {
            Err(HostError::EndedEarly { component }) => assert_eq!(component, "http"),
            other => panic!("expected EndedEarly, got {other:?}"),
        }
        assert!(lines(&log).contains(&"dispatcher: stopped".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_failure_wins() {
        // `http` fails on its own; then the worker fails while stopping.
        let result = Host::new(Role::All)
            .control_plane("http", |_stop| async { Err(boom()) })
            .worker("dispatcher", |stop| async move {
                stop.cancelled().await;
                Err("late".into())
            })
            .run(std::future::pending())
            .await;
        match result {
            Err(HostError::Stopped { component, .. }) => assert_eq!(component, "http"),
            other => panic!("expected Stopped(http), got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_error_while_stopping_is_returned() {
        let result = Host::new(Role::Worker)
            .worker("dispatcher", |stop| async move {
                stop.cancelled().await;
                Err(boom())
            })
            .run(after_tick())
            .await;
        match result {
            Err(HostError::Stopped { component, .. }) => assert_eq!(component, "dispatcher"),
            other => panic!("expected Stopped, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_component_that_ends_ok_while_stopping_is_not_a_failure() {
        // Shutdown fires at 10ms. The worker returns Ok at 1s, while `http` still drains
        // (2s). The host is already stopping, so this is not `EndedEarly`.
        let log = log();
        let result = Host::new(Role::All)
            .control_plane("http", polite(&log, "http", Duration::from_secs(2)))
            .worker("dispatcher", |_stop| async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(())
            })
            .run(after_tick())
            .await;
        assert!(result.is_ok(), "{result:?}");
    }

    // ---- health ----

    #[tokio::test(start_paused = true)]
    async fn health_flips_to_shutting_down() {
        let log = log();
        let host = Host::new(Role::All)
            .control_plane("http", polite(&log, "http", Duration::from_secs(1)))
            .worker("dispatcher", polite(&log, "dispatcher", Duration::ZERO));
        let health = host.health();
        assert!(!health.ready() && !health.shutting_down(), "before run");

        let (signal, shutdown) = tokio::sync::oneshot::channel::<()>();
        let running = tokio::spawn(host.run(async move {
            let _ = shutdown.await;
        }));
        tokio::time::sleep(TICK).await;
        assert!(health.ready(), "all started, none stopping");
        assert!(!health.shutting_down());

        signal.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The control plane is still draining (1s), so the host is mid-stop.
        assert!(health.shutting_down());
        assert!(!health.ready());

        running.await.unwrap().unwrap();
        assert!(health.shutting_down());
        assert!(!health.ready());
    }

    #[tokio::test(start_paused = true)]
    async fn health_is_not_ready_once_a_component_has_ended() {
        let log = log();
        let host = Host::new(Role::All)
            .control_plane("http", polite(&log, "http", Duration::ZERO))
            .worker("dispatcher", |_stop| async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Err(boom())
            });
        let health = host.health();
        let running = tokio::spawn(host.run(std::future::pending()));
        tokio::time::sleep(TICK).await;
        assert!(health.ready());
        assert!(running.await.unwrap().is_err());
        assert!(
            health.shutting_down(),
            "an early end marks the host as stopping"
        );
        assert!(!health.ready());
    }

    // ---- the error enum ----

    #[test]
    fn host_error_classes() {
        let table = |e: &HostError| match e {
            HostError::Stopped { .. } => ErrorClass::Internal,
            HostError::Panicked { .. } => ErrorClass::Internal,
            HostError::EndedEarly { .. } => ErrorClass::Internal,
            HostError::NothingToRun => ErrorClass::Internal,
        };
        let all = [
            HostError::Stopped {
                component: "a".into(),
                source: boom(),
            },
            HostError::Panicked {
                component: "a".into(),
                source: boom(),
            },
            HostError::EndedEarly {
                component: "a".into(),
            },
            HostError::NothingToRun,
        ];
        for e in &all {
            assert_eq!(e.class(), table(e), "{e:?}");
            assert!(e.class().should_alert(), "{e:?}");
            assert!(!e.is_retryable(), "{e:?}");
        }
    }

    #[test]
    fn a_message_describes_its_own_layer_only() {
        let e = HostError::Stopped {
            component: "http".into(),
            source: boom(),
        };
        assert_eq!(e.to_string(), "component `http` stopped");
        assert_eq!(report(&e), "component `http` stopped: boom");
        assert!(std::error::Error::source(&e).is_some());
        assert!(
            std::error::Error::source(&HostError::EndedEarly {
                component: "http".into()
            })
            .is_none()
        );
    }
}
