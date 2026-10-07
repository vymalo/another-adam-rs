//! What a pass observed, as the status of an `AgentService`: conditions and state.
//! Pure: no clock (the time is an argument), no I/O.

use adam_operator_api::{
    AgentServiceStatus, ConfigStatus, Endpoints, RuntimePhase, ServiceState, condition_type as ty,
    reason,
};
use adam_operator_ports::{Issue, IssueReason, Phase, RuntimeStatus, StoreState};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};

use crate::options::RegistryMode;

/// `ConfigResolved`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigOutcome {
    /// The config exists and `resolve` made a spec from it.
    Resolved,
    /// `spec.configRef` names an `AgentConfig` that does not exist.
    NotFound {
        /// The name that was looked up.
        name: String,
    },
    /// The config failed validation, or a provider refused what was resolved from it.
    Invalid {
        /// What is wrong, for a person. Never a secret's value: none is in the objects.
        message: String,
    },
}

/// `StoreReady`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreOutcome {
    /// The pass stopped before the store (the config did not resolve).
    NotEvaluated,
    /// The provisioner answered.
    Answered(StoreState),
    /// The backend of the kind is absent: CloudNativePG is not installed, or this operator was built
    /// without the provisioner for it.
    NotInstalled {
        /// Why, for a person.
        message: String,
    },
}

/// Everything one pass learned about a service.
#[derive(Clone, Debug)]
pub struct Observed {
    /// `metadata.generation` of the service.
    pub generation: Option<i64>,
    /// `spec.suspend`.
    pub suspend: bool,
    /// `spec.interfaces.a2a.enabled`.
    pub a2a_enabled: bool,
    /// Whether this operator serves a registry.
    pub registry: RegistryMode,
    /// The registry refuses to list anything for now: its document would pass a limit (`RegistryFull`).
    pub registry_full: bool,
    /// `RuntimeProvider::name`.
    pub provider: &'static str,
    /// The config.
    pub config: ConfigOutcome,
    /// The store.
    pub store: StoreOutcome,
    /// The runtime, as the provider reported it. `None`: not observed.
    pub runtime: Option<RuntimeStatus>,
    /// `status.config`: set when the config resolved, and kept as it was while the service is blocked
    /// (what runs was resolved from it).
    pub config_status: Option<ConfigStatus>,
    /// `status.endpoints`, kept the same way.
    pub endpoints: Option<Endpoints>,
}

/// The status of a pass. `previous` is the conditions the object had, so a condition whose status
/// did not change keeps its `lastTransitionTime`.
pub fn derive(obs: &Observed, previous: &[Condition], now: &Time) -> AgentServiceStatus {
    let config = config_condition(&obs.config);
    let store = store_condition(&obs.store);
    let runtime = runtime_condition(obs.runtime.as_ref());

    let state = state(obs, &config, &store, &runtime);
    let listed = listed_condition(obs, state);
    let ready = ready_condition([&config, &store, &runtime]);

    let conditions = [
        (ty::CONFIG_RESOLVED, config),
        (ty::STORE_READY, store),
        (ty::RUNTIME_READY, runtime),
        (ty::LISTED, listed),
        (ty::READY, ready),
    ]
    .into_iter()
    .map(|(type_, c)| c.into_condition(type_, obs.generation, previous, now))
    .collect();

    AgentServiceStatus {
        observed_generation: obs.generation,
        state: Some(state),
        config: obs.config_status.clone(),
        runtime: obs
            .runtime
            .as_ref()
            .map(|r| adam_operator_api::RuntimeStatus {
                provider: obs.provider.to_owned(),
                phase: phase(r.phase),
                replicas: i32::try_from(r.replicas).unwrap_or(i32::MAX),
            }),
        endpoints: obs.endpoints.clone(),
        conditions,
    }
}

/// A condition before it has a type and a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Draft {
    pub status: Truth,
    pub reason: String,
    pub message: String,
}

/// The three values of a condition's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    fn as_str(self) -> &'static str {
        match self {
            Self::True => "True",
            Self::False => "False",
            Self::Unknown => "Unknown",
        }
    }
}

impl Draft {
    fn new(status: Truth, reason: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.to_owned(),
            message: message.into(),
        }
    }

    fn into_condition(
        self,
        type_: &str,
        generation: Option<i64>,
        previous: &[Condition],
        now: &Time,
    ) -> Condition {
        let status = self.status.as_str();
        let since = previous
            .iter()
            .find(|c| c.type_ == type_ && c.status == status)
            .map_or_else(|| now.clone(), |c| c.last_transition_time.clone());
        Condition {
            type_: type_.to_owned(),
            status: status.to_owned(),
            reason: self.reason,
            message: self.message,
            observed_generation: generation,
            last_transition_time: since,
        }
    }
}

fn config_condition(config: &ConfigOutcome) -> Draft {
    match config {
        ConfigOutcome::Resolved => Draft::new(
            Truth::True,
            reason::RESOLVED,
            "the AgentConfig exists and resolved",
        ),
        ConfigOutcome::NotFound { name } => Draft::new(
            Truth::False,
            reason::CONFIG_NOT_FOUND,
            format!("the AgentConfig {name:?} does not exist in this namespace"),
        ),
        ConfigOutcome::Invalid { message } => {
            Draft::new(Truth::False, reason::CONFIG_INVALID, message.clone())
        }
    }
}

fn store_condition(store: &StoreOutcome) -> Draft {
    match store {
        StoreOutcome::NotEvaluated => Draft::new(
            Truth::Unknown,
            "ConfigNotResolved",
            "the store is looked at once the config resolves",
        ),
        StoreOutcome::Answered(StoreState::SecretReferenced) => Draft::new(
            Truth::True,
            reason::SECRET_REFERENCED,
            "a Secret key holds the connection string; the operator does not read Secrets, so the kubelet tells whether it exists",
        ),
        StoreOutcome::Answered(StoreState::ClusterReady) => Draft::new(
            Truth::True,
            reason::CLUSTER_READY,
            "the CloudNativePG cluster is ready",
        ),
        StoreOutcome::Answered(StoreState::ClusterNotReady) => Draft::new(
            Truth::False,
            reason::CLUSTER_NOT_READY,
            "the CloudNativePG cluster exists and is not ready yet",
        ),
        StoreOutcome::NotInstalled { message } => {
            Draft::new(Truth::False, reason::CNPG_NOT_INSTALLED, message.clone())
        }
    }
}

/// The order in which one issue is chosen to speak for a workload that has several.
fn rank(reason: &IssueReason) -> u8 {
    match reason {
        IssueReason::NameConflict => 0,
        IssueReason::MissingSecret { .. } => 1,
        IssueReason::ConfigRejected => 2,
        IssueReason::DependencyUnavailable => 3,
        IssueReason::ImagePull => 4,
        IssueReason::CrashLoop => 5,
    }
}

fn main_issue(issues: &[Issue]) -> Option<&Issue> {
    issues.iter().min_by_key(|i| rank(&i.reason))
}

fn issue_reason(reason: &IssueReason) -> &'static str {
    match reason {
        IssueReason::NameConflict => reason::NAME_CONFLICT,
        IssueReason::MissingSecret { .. } => reason::MISSING_SECRET,
        IssueReason::ConfigRejected => reason::CONFIG_REJECTED,
        IssueReason::DependencyUnavailable => reason::DEPENDENCY_UNAVAILABLE,
        IssueReason::ImagePull => reason::IMAGE_PULL,
        IssueReason::CrashLoop => reason::CRASH_LOOP,
    }
}

fn runtime_condition(runtime: Option<&RuntimeStatus>) -> Draft {
    let Some(rt) = runtime else {
        return Draft::new(
            Truth::Unknown,
            "NotObserved",
            "the runtime provider has not been asked yet",
        );
    };
    if let Some(issue) = main_issue(&rt.issues) {
        // The name of a missing Secret is a name, not a value; the provider's message never holds it.
        let message = match &issue.reason {
            IssueReason::MissingSecret { name } => format!(
                "the Secret {name:?}, or a key of it, that the workload references does not exist ({})",
                issue.message
            ),
            _ => issue.message.clone(),
        };
        return Draft::new(Truth::False, issue_reason(&issue.reason), message);
    }
    match rt.phase {
        Phase::Ready => Draft::new(
            Truth::True,
            reason::READY,
            format!("{} replicas ready", rt.replicas),
        ),
        Phase::Suspended => Draft::new(
            Truth::False,
            reason::SUSPENDED,
            "scaled to zero on purpose (spec.suspend)",
        ),
        Phase::Provisioning => Draft::new(
            Truth::False,
            reason::PROVISIONING,
            "a rollout is in progress",
        ),
        Phase::Failed => Draft::new(
            Truth::False,
            reason::PROVISIONING,
            "the runtime cannot get ready, and the provider gave no reason",
        ),
        Phase::Absent => Draft::new(Truth::Unknown, "NotCreated", "no workload exists"),
    }
}

fn state(obs: &Observed, config: &Draft, store: &Draft, runtime: &Draft) -> ServiceState {
    let conflict = obs
        .runtime
        .as_ref()
        .is_some_and(RuntimeStatus::is_name_conflict);
    // The operator did not apply the desired state, and left what runs untouched.
    if config.status != Truth::True || store.status != Truth::True || conflict {
        return ServiceState::Blocked;
    }
    match obs.runtime.as_ref().map(|r| r.phase) {
        Some(Phase::Suspended) if obs.suspend => ServiceState::Suspended,
        Some(Phase::Ready) if runtime.status == Truth::True => ServiceState::Ready,
        _ => ServiceState::Degraded,
    }
}

fn listed_condition(obs: &Observed, state: ServiceState) -> Draft {
    match obs.registry {
        RegistryMode::Disabled => Draft::new(
            Truth::False,
            reason::REGISTRY_DISABLED,
            "this operator serves no agent registry",
        ),
        RegistryMode::Enabled => {
            if !obs.a2a_enabled {
                Draft::new(
                    Truth::False,
                    reason::A2A_DISABLED,
                    "A2A is not enabled, and the registry lists A2A agents",
                )
            } else if state == ServiceState::Blocked {
                Draft::new(
                    Truth::False,
                    reason::SERVICE_BLOCKED,
                    "the service is Blocked, so the registry does not list it",
                )
            } else if obs.registry_full {
                Draft::new(
                    Truth::False,
                    reason::REGISTRY_FULL,
                    "the registry would pass its limit of 500 items or 1 MiB and lists nothing rather than truncate",
                )
            } else if obs
                .endpoints
                .as_ref()
                .and_then(|e| e.agent_card.as_ref())
                .is_none()
            {
                Draft::new(
                    Truth::Unknown,
                    "NoEndpoint",
                    "the runtime has no agent card URL yet",
                )
            } else {
                Draft::new(Truth::True, reason::LISTED, "listed in the agent registry")
            }
        }
    }
}

/// `Ready`: the first three are true. The reason is that of the first one that is not.
fn ready_condition(first_three: [&Draft; 3]) -> Draft {
    match first_three.iter().find(|c| c.status != Truth::True) {
        None => Draft::new(Truth::True, reason::RECONCILED, "the agent is running"),
        Some(c) => Draft::new(
            if c.status == Truth::False {
                Truth::False
            } else {
                Truth::Unknown
            },
            &c.reason,
            c.message.clone(),
        ),
    }
}

fn phase(p: Phase) -> RuntimePhase {
    match p {
        Phase::Absent => RuntimePhase::Absent,
        Phase::Provisioning => RuntimePhase::Provisioning,
        Phase::Ready => RuntimePhase::Ready,
        Phase::Suspended => RuntimePhase::Suspended,
        Phase::Failed => RuntimePhase::Failed,
    }
}

/// The reason and message of the `Ready` condition of a status, for an Event.
pub(crate) fn ready_summary(status: &AgentServiceStatus) -> Option<(&str, &str)> {
    status
        .conditions
        .iter()
        .find(|c| c.type_ == ty::READY)
        .map(|c| (c.reason.as_str(), c.message.as_str()))
}
