//! `AgentService`: the stable, addressable agent.

use k8s_openapi::api::networking::v1::NetworkPolicyPeer;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::{NameRef, SecretKeyRef};
use crate::status::AgentServiceStatus;

/// The desired state of an agent as a service: which config it runs, which interfaces it
/// serves, how it scales and where its ledger lives.
// kube-derive's repeated `printcolumn(type_ = …)` trips this lint (a false positive).
#[allow(clippy::duplicated_attributes)]
#[derive(CustomResource, KubeSchema, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[kube(
    group = "agents.vymalo.com",
    version = "v1alpha1",
    kind = "AgentService",
    plural = "agentservices",
    namespaced,
    status = "AgentServiceStatus",
    derive = "PartialEq",
    doc = "A durable, addressable adam-rs agent service. It names an AgentConfig and the operator makes the workload, Service, NetworkPolicy and storage for it.",
    printcolumn(name = "State", type_ = "string", json_path = ".status.state"),
    printcolumn(name = "Config", type_ = "string", json_path = ".spec.configRef.name"),
    printcolumn(
        name = "Replicas",
        type_ = "integer",
        json_path = ".status.runtime.replicas"
    ),
    printcolumn(
        name = "Age",
        type_ = "date",
        json_path = ".metadata.creationTimestamp"
    )
)]
#[serde(rename_all = "camelCase")]
pub struct AgentServiceSpec {
    /// What the agent does, in a sentence. Listed in the agent registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// The AgentConfig, in the same namespace, this service runs.
    pub config_ref: NameRef,

    /// The protocol surfaces the service serves. A2A is the only one in v0, and it is on unless said otherwise.
    #[serde(default)]
    pub interfaces: Interfaces,

    /// How many processes run, and in which topology.
    #[serde(default)]
    pub scaling: Scaling,

    /// Scale the workload to zero replicas. The only scale-to-zero of v0.
    #[serde(default)]
    pub suspend: bool,

    /// Where the agent's durable run ledger lives.
    pub store: Store,

    /// Who may reach the service.
    #[serde(default)]
    pub access: Access,

    /// How the agent is listed in the agent registry.
    #[serde(default)]
    pub registry: Registry,

    /// What happens to data (work volumes, an operator-owned database) when the service is deleted.
    #[serde(default)]
    pub deletion_policy: DeletionPolicy,
}

/// The protocol surfaces of a service. Only A2A is served in v0.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Interfaces {
    /// The A2A surface.
    #[serde(default)]
    pub a2a: A2aInterface,
    /// The Responses surface. Exists for the target design; v0 refuses `true`.
    #[serde(default)]
    pub responses: UnsupportedInterface,
    /// The MCP surface. Exists for the target design; v0 refuses `true`.
    #[serde(default)]
    pub mcp: UnsupportedInterface,
}

/// The A2A surface of a service. On by default: it is the only surface of v0, and the adam binaries serve nothing else.
// A missing `enabled` is true, so the rule counts it as true; the API server applies the schema default first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = Rule::new("(has(self.enabled) && !self.enabled) || has(self.bearerTokensSecretRef)").message("interfaces.a2a needs bearerTokensSecretRef: no token, no server"))]
pub struct A2aInterface {
    /// Serve A2A. The default is true; v0 refuses false (the agent would not start), and without A2A the agent is not listed.
    #[serde(default = "enabled")]
    pub enabled: bool,

    /// The Secret key that holds the bearer tokens (`A2A_BEARER_TOKENS`). No token, no server:
    /// the agent fails closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_tokens_secret_ref: Option<SecretKeyRef>,

    /// The URL clients reach the agent at (`PUBLIC_URL`). Empty: `http://<name>.<ns>.svc:8080/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
}

fn enabled() -> bool {
    true
}

impl Default for A2aInterface {
    fn default() -> Self {
        Self {
            enabled: enabled(),
            bearer_tokens_secret_ref: None,
            public_url: None,
        }
    }
}

/// A surface the v0 operator does not serve.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, KubeSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnsupportedInterface {
    /// Must be false in v0.
    #[serde(default)]
    #[x_kube(validation = Rule::new("self == false").message("v0 serves A2A only: this interface cannot be enabled"))]
    pub enabled: bool,
}

/// How many processes run, and in which topology.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = Rule::new("!has(self.front) || self.topology == 'split'").message("scaling.front is only allowed with topology: split"))]
pub struct Scaling {
    /// `combined`: one workload runs every role. `split`: a control plane (`<name>-front`) and workers.
    #[serde(default)]
    pub topology: Topology,

    /// Replicas of the workload `<name>` (the adam workers as pods).
    #[serde(default = "one")]
    #[schemars(range(min = 1))]
    pub workers: i32,

    /// The front of a `split` topology.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub front: Option<Front>,
}

fn one() -> i32 {
    1
}

impl Default for Scaling {
    fn default() -> Self {
        Self {
            topology: Topology::default(),
            workers: one(),
            front: None,
        }
    }
}

/// The topology of a service's processes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Topology {
    /// One workload runs every role.
    #[default]
    Combined,
    /// A control plane (`<name>-front`) and the workers (`<name>`).
    Split,
}

/// The front (control plane) of a `split` topology.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Front {
    /// Replicas of `<name>-front`. A PodDisruptionBudget is made when there is more than one.
    #[serde(default = "one")]
    #[schemars(range(min = 1))]
    pub replicas: i32,
}

impl Default for Front {
    fn default() -> Self {
        Self { replicas: one() }
    }
}

/// Where the agent's durable run ledger lives.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Store {
    /// A PostgreSQL ledger.
    pub postgres: PostgresStore,
}

/// A PostgreSQL ledger: a Secret someone else owns, or an operator-owned CloudNativePG cluster.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = Rule::new("has(self.secretRef) != has(self.cnpg)").message("exactly one of store.postgres.secretRef and store.postgres.cnpg"))]
pub struct PostgresStore {
    /// The Secret key that holds the connection string (`DATABASE_URL`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretKeyRef>,

    /// An operator-owned CloudNativePG `Cluster` `<name>-db`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cnpg: Option<Cnpg>,
}

/// An operator-owned CloudNativePG cluster.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Cnpg {
    /// Instances of the cluster.
    #[serde(default = "one")]
    #[schemars(range(min = 1))]
    pub instances: i32,

    /// Storage of each instance.
    pub storage: CnpgStorage,
}

/// Storage of a CloudNativePG instance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CnpgStorage {
    /// Size of the volume, e.g. `5Gi`.
    pub size: Quantity,

    /// Storage class. Unset: the cluster's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
}

/// Who may reach the service.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Access {
    /// Peers allowed to reach port 8080: the `from` of a NetworkPolicy ingress rule. There is no
    /// egress rule: the agent needs git hosts, the model gateway and registries.
    #[serde(default)]
    pub allow_from: Vec<NetworkPolicyPeer>,
}

/// How the agent is listed in the agent registry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Registry {
    /// Display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// What happens to data when an AgentService is deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum DeletionPolicy {
    /// The compute is removed; the work volumes and an operator-owned database stay.
    #[default]
    Retain,
    /// The data goes too.
    Delete,
}
