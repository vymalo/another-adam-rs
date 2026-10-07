//! The status of both kinds, and the condition types and reasons the controller writes.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What the controller observed of an AgentService.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentServiceStatus {
    /// The `metadata.generation` the controller last reconciled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,

    /// The state derived from the conditions and the runtime phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ServiceState>,

    /// The AgentConfig this status was resolved from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigStatus>,

    /// The runtime, as the provider reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeStatus>,

    /// Where the agent can be reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<Endpoints>,

    /// `ConfigResolved`, `StoreReady`, `RuntimeReady`, `Listed` and `Ready`.
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// The state of a service. `Suspended` is healthy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ServiceState {
    /// The runtime is ready.
    Ready,
    /// The desired state is applied and the runtime is not ready yet, or has an issue.
    Degraded,
    /// `spec.suspend` is set and the runtime is suspended.
    Suspended,
    /// The operator did not apply the desired state, and left what runs untouched.
    Blocked,
}

/// The AgentConfig a service was resolved from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigStatus {
    /// Name of the AgentConfig.
    pub name: String,
    /// Its `metadata.generation` when it was resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// `sha256:<hex>` of the resolved runtime: the seed of AgentRevision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// The runtime of a service, in the neutral terms of `RuntimeStatus`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    /// The provider that made it: `kubernetes`.
    pub provider: String,
    /// The phase of the runtime.
    pub phase: RuntimePhase,
    /// Replicas that are ready.
    #[serde(default)]
    pub replicas: i32,
}

/// The phase of a runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum RuntimePhase {
    /// Nothing exists.
    Absent,
    /// A rollout is in progress.
    Provisioning,
    /// Ready.
    Ready,
    /// Scaled to zero on purpose.
    Suspended,
    /// Failed.
    Failed,
}

/// Where the agent can be reached.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Endpoints {
    /// The A2A endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a: Option<String>,
    /// The agent card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_card: Option<String>,
}

/// What the controller observed of an AgentConfig.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfigStatus {
    /// The `metadata.generation` the controller last validated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,

    /// `Valid`.
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// Condition types.
pub mod condition_type {
    /// AgentConfig: it passed the validation of `adam-operator-domain`.
    pub const VALID: &str = "Valid";
    /// AgentService: the config exists and resolved.
    pub const CONFIG_RESOLVED: &str = "ConfigResolved";
    /// AgentService: the store is ready.
    pub const STORE_READY: &str = "StoreReady";
    /// AgentService: the runtime is ready.
    pub const RUNTIME_READY: &str = "RuntimeReady";
    /// AgentService: listed in the agent registry. Informs, never gates `Ready`.
    pub const LISTED: &str = "Listed";
    /// AgentService: the first three are true.
    pub const READY: &str = "Ready";
}

/// Condition reasons.
pub mod reason {
    /// `ConfigResolved` true.
    pub const RESOLVED: &str = "Resolved";
    /// `ConfigResolved` false: the named AgentConfig does not exist.
    pub const CONFIG_NOT_FOUND: &str = "ConfigNotFound";
    /// `ConfigResolved` false: the config failed validation.
    pub const CONFIG_INVALID: &str = "ConfigInvalid";
    /// `StoreReady` true: a Secret is referenced.
    pub const SECRET_REFERENCED: &str = "SecretReferenced";
    /// `StoreReady` true: the CloudNativePG cluster is ready.
    pub const CLUSTER_READY: &str = "ClusterReady";
    /// `StoreReady` false: the CloudNativePG API is absent.
    pub const CNPG_NOT_INSTALLED: &str = "CNPGNotInstalled";
    /// `StoreReady` false: the cluster is not ready.
    pub const CLUSTER_NOT_READY: &str = "ClusterNotReady";
    /// `RuntimeReady` and `Ready` true.
    pub const READY: &str = "Ready";
    /// `RuntimeReady` false: a rollout is in progress.
    pub const PROVISIONING: &str = "Provisioning";
    /// `RuntimeReady` false: suspended on purpose.
    pub const SUSPENDED: &str = "Suspended";
    /// `RuntimeReady` false: a referenced Secret or key does not exist.
    pub const MISSING_SECRET: &str = "MissingSecret";
    /// `RuntimeReady` false: the agent process refused its configuration (exit 78).
    pub const CONFIG_REJECTED: &str = "ConfigRejected";
    /// `RuntimeReady` false: a dependency is unreachable (exit 69).
    pub const DEPENDENCY_UNAVAILABLE: &str = "DependencyUnavailable";
    /// `RuntimeReady` false: the image cannot be pulled.
    pub const IMAGE_PULL: &str = "ImagePull";
    /// `RuntimeReady` false: the container keeps exiting.
    pub const CRASH_LOOP: &str = "CrashLoop";
    /// `RuntimeReady` false: an object the runtime needs exists and is not ours.
    pub const NAME_CONFLICT: &str = "NameConflict";
    /// `Listed` true.
    pub const LISTED: &str = "Listed";
    /// `Listed` false: A2A is not enabled.
    pub const A2A_DISABLED: &str = "A2ADisabled";
    /// `Listed` false: the service is `Blocked`.
    pub const SERVICE_BLOCKED: &str = "ServiceBlocked";
    /// `Listed` false: the registry is past its limits.
    pub const REGISTRY_FULL: &str = "RegistryFull";
    /// `Listed` false: the registry is not served.
    pub const REGISTRY_DISABLED: &str = "RegistryDisabled";
    /// `Ready` true.
    pub const RECONCILED: &str = "Reconciled";
    /// `Valid` true.
    pub const VALID: &str = "Valid";
}
