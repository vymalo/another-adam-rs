//! What a reconciler is given.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use adam_error::{Classify, ErrorClass};
use adam_operator_api::AgentService;
use adam_operator_ports::OwnerHandle;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::Client;
use kube::runtime::controller::Action;
use kube::runtime::events::{Recorder, Reporter};

use crate::metrics::Metrics;
use crate::options::Options;

/// The finalizer of an `AgentService`.
pub const FINALIZER: &str = "agents.vymalo.com/runtime";

/// The field manager of everything the controller applies: the status of both kinds.
pub const FIELD_MANAGER: &str = "adam-operator";

/// How the controller gets the opaque `OwnerHandle` of the object it reconciles. The handle's
/// encoding is the runtime provider's (`adam_operator_runtime_kubernetes::owner_handle` for the Kubernetes
/// one), so the composition root, which knows both, passes the function: the controller never looks
/// inside a handle and has no dependency on a provider.
pub type OwnerOf = Arc<dyn Fn(&AgentService) -> OwnerHandle + Send + Sync>;

/// The time of a pass. A function so a test can fix it.
pub type Clock = Arc<dyn Fn() -> Time + Send + Sync>;

/// The real clock.
pub fn system_clock() -> Clock {
    Arc::new(|| Time(k8s_openapi::jiff::Timestamp::now()))
}

/// Consecutive failures of each object, for the back-off of the error policy.
#[derive(Debug, Default)]
pub(crate) struct Retries {
    failures: Mutex<HashMap<String, u32>>,
}

impl Retries {
    /// Count one more failure of `key` and say how many in a row there have been.
    pub(crate) fn failed(&self, key: &str) -> u32 {
        let mut map = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        let n = map.entry(key.to_owned()).or_insert(0);
        *n = n.saturating_add(1);
        *n
    }

    /// A pass succeeded.
    pub(crate) fn succeeded(&self, key: &str) {
        self.failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }
}

/// How long to wait after the `attempt`th failure in a row with an error of this class.
///
/// * `Transient`, `RateLimited` and `Conflict`: 5 s, doubling, at most 5 minutes (the API server or the backend
///   is the next thing to change, so look soon, and not in a storm).
/// * `NotFound`: 5 s. Something we expected is not there yet.
/// * `Internal`, `Corrupt`: 30 s, doubling, at most 10 minutes: a bug or the operator's own RBAC, which a person
///   fixes, and retrying fast only fills the log.
/// * `Invalid`, `Unsupported`, `Rejected`, `Unauthenticated`: the same input never succeeds, so wait for the object to change, with
///   a long timer as the only other way out.
pub fn backoff(class: ErrorClass, attempt: u32) -> Duration {
    let doubled = |base: u64, cap: u64| {
        Duration::from_secs(
            base.saturating_mul(1u64 << attempt.saturating_sub(1).min(20))
                .min(cap),
        )
    };
    match class {
        ErrorClass::Transient | ErrorClass::RateLimited | ErrorClass::Conflict => doubled(5, 300),
        ErrorClass::NotFound => Duration::from_secs(5),
        ErrorClass::Internal | ErrorClass::Corrupt => doubled(30, 600),
        _ => Duration::from_secs(600),
    }
}

/// What the `AgentService` reconciler needs: the API, the two providers, and the settings.
///
/// Generic over the providers: the controller never names a provider type.
pub struct Context<R, S> {
    pub(crate) client: Client,
    pub(crate) runtime: R,
    pub(crate) store: S,
    pub(crate) owner: OwnerOf,
    pub(crate) options: Options,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) recorder: Recorder,
    pub(crate) clock: Clock,
    pub(crate) retries: Retries,
}

impl<R, S> Context<R, S> {
    /// A context with the real clock and a fresh set of counters.
    pub fn new(client: Client, runtime: R, store: S, owner: OwnerOf, options: Options) -> Self {
        let recorder = Recorder::new(
            client.clone(),
            Reporter {
                controller: FIELD_MANAGER.to_owned(),
                instance: options.instance.clone(),
            },
        );
        Self {
            client,
            runtime,
            store,
            owner,
            options,
            metrics: Arc::new(Metrics::new()),
            recorder,
            clock: system_clock(),
            retries: Retries::default(),
        }
    }

    /// Count into `metrics` instead of a private set.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Take the time from `clock`.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The counters.
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }
}

/// What the `AgentConfig` reconciler needs.
pub struct ConfigContext {
    pub(crate) client: Client,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) clock: Clock,
    pub(crate) retries: Retries,
}

impl ConfigContext {
    /// A context with the real clock.
    pub fn new(client: Client, metrics: Arc<Metrics>) -> Self {
        Self {
            client,
            metrics,
            clock: system_clock(),
            retries: Retries::default(),
        }
    }

    /// Take the time from `clock`.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }
}

/// The action for a failed pass, and the book-keeping that goes with it.
pub(crate) fn failed(
    controller: &'static str,
    key: &str,
    retries: &Retries,
    metrics: &Metrics,
    err: &(impl Classify + std::fmt::Display),
) -> Action {
    let class = err.class();
    let attempt = retries.failed(key);
    let wait = backoff(class, attempt);
    metrics.inc(
        crate::metrics::name::RECONCILES,
        &[("controller", controller), ("result", "error")],
    );
    metrics.inc(
        crate::metrics::name::ERRORS,
        &[
            ("controller", controller),
            ("class", crate::error::class_label(class)),
        ],
    );
    if class == ErrorClass::Conflict {
        // A lost race (a pass on a stale cache) is expected when objects change in bursts, and the
        // change that beat us is already on its way as a watch event.
        tracing::info!(controller, object = key, attempt, retry_in = ?wait, "reconcile lost a race: {err}");
    } else {
        tracing::warn!(controller, object = key, class = ?class, attempt, retry_in = ?wait, "reconcile failed: {err}");
    }
    Action::requeue(wait)
}
