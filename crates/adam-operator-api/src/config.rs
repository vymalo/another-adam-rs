//! `AgentConfig`: what an agent is made of.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::{Empty, SecretKeyRef};
use crate::status::AgentConfigStatus;

/// The configuration of an agent: its harness, model, tools, environment and security. A
/// secret is never a value here: a field that needs one names a Secret and a key.
// kube-derive's repeated `printcolumn(type_ = …)` trips this lint (a false positive).
#[allow(clippy::duplicated_attributes)]
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "agents.vymalo.com",
    version = "v1alpha1",
    kind = "AgentConfig",
    plural = "agentconfigs",
    namespaced,
    status = "AgentConfigStatus",
    derive = "PartialEq",
    doc = "What an adam-rs agent is made of. Referenced by an AgentService; the operator validates it and resolves it into a runtime.",
    printcolumn(
        name = "Binary",
        type_ = "string",
        json_path = ".spec.harness.adam.binary"
    ),
    printcolumn(
        name = "Image",
        type_ = "string",
        json_path = ".spec.environment.image.ref",
        priority = 1
    ),
    printcolumn(
        name = "Age",
        type_ = "date",
        json_path = ".metadata.creationTimestamp"
    )
)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfigSpec {
    /// The agent framework that runs.
    pub harness: Harness,

    /// The model the agent talks to.
    pub model: Model,

    /// The tools the agent may use.
    #[serde(default)]
    pub tools: Tools,

    /// The image, resources and volumes of the agent's pods.
    pub environment: Environment,

    /// The pod's security context. Everything else of it is fixed by the operator.
    #[serde(default)]
    pub security: Security,

    /// Literal environment variables. Names only the adam binaries do not own; never a secret.
    #[serde(default)]
    pub extra_env: BTreeMap<String, String>,
}

// ---------------------------------------------------------------- harness

/// The agent framework. Only `adam-rs` exists in v0.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Harness {
    /// The harness type.
    #[serde(rename = "type")]
    pub kind: HarnessType,

    /// The adam-rs settings.
    pub adam: Adam,
}

/// The harness types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum HarnessType {
    /// adam-rs: `adam-coder` or `adam-agent`, one image.
    #[serde(rename = "adam-rs")]
    AdamRs,
}

/// The adam-rs settings: which binary runs, where the agent comes from, and the coder's tunables.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = Rule::new("self.binary != 'adam-coder' || (has(self.agent.embedded) && has(self.coder))").message("binary adam-coder needs agent.embedded and the coder block"))]
#[x_kube(validation = Rule::new("self.binary != 'adam-agent' || (has(self.agent.folder) && !has(self.coder))").message("binary adam-agent needs agent.folder and no coder block"))]
pub struct Adam {
    /// The binary the container runs.
    pub binary: Binary,

    /// Where the agent comes from.
    pub agent: AgentSource,

    /// The coder's settings. Only with `adam-coder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coder: Option<Coder>,
}

/// The two binaries of the adam-rs image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Binary {
    /// A coding task to a verified pull request.
    #[serde(rename = "adam-coder")]
    AdamCoder,
    /// Serves any agent folder.
    #[serde(rename = "adam-agent")]
    AdamAgent,
}

/// Where the agent comes from: a folder, or the one compiled into the binary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = Rule::new("has(self.folder) != has(self.embedded)").message("exactly one of agent.folder and agent.embedded"))]
pub struct AgentSource {
    /// An agent that is only a folder, served by `adam-agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<Folder>,

    /// The agent compiled into the binary (`adam-coder`). No settings: `{}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedded: Option<Empty>,
}

/// An agent folder, mounted read-only at `/etc/adam/agent` (`ADAM_AGENT_DIR`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = Rule::new("has(self.files) != has(self.configMapRef)").message("exactly one of agent.folder.files and agent.folder.configMapRef"))]
pub struct Folder {
    /// The whole folder inline: relative path to content. A ConfigMap holds 1 MiB, which is the
    /// folder's size limit in v0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<BTreeMap<String, String>>,

    /// A ConfigMap of the same namespace that holds the folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map_ref: Option<crate::common::NameRef>,
}

/// The settings of `adam-coder`. A field left out is not set, so the binary's own default applies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Coder {
    /// Runs advanced at once, per process (`WORKERS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub workers: Option<i32>,

    /// `MAX_CHECK_CYCLES`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0))]
    pub max_check_cycles: Option<i32>,

    /// `CHECK_TIMEOUT_SECS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub check_timeout_secs: Option<i64>,

    /// `WORKSPACE_SWEEP_SECS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub workspace_sweep_secs: Option<i64>,

    /// `WORKSPACE_PLACEMENT`: `""`, `shared`, `affinity` or `isolated`. Checked by the reconciler,
    /// which mirrors the adam-rs chart's rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_placement: Option<String>,

    /// `ALLOWED_REPO_HOSTS`.
    #[serde(default)]
    pub allowed_repo_hosts: Vec<String>,

    /// `GITHUB_API_URL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_api_url: Option<String>,

    /// `PR_DRAFT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_draft: Option<bool>,

    /// `GIT_AUTHOR_NAME` and `GIT_AUTHOR_EMAIL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_author: Option<GitAuthor>,

    /// `CREATE_REPO_OWNERS`. Empty: the tool stays off.
    #[serde(default)]
    pub create_repo_owners: Vec<String>,

    /// `OPENCODE_MODEL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode_model: Option<String>,

    /// How the coder authenticates to GitHub.
    pub github: Github,
}

/// The author of the coder's commits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitAuthor {
    /// `GIT_AUTHOR_NAME`.
    pub name: String,
    /// `GIT_AUTHOR_EMAIL`.
    pub email: String,
}

/// The coder's GitHub credential: a GitHub App or a token, never both.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = Rule::new("has(self.app) != has(self.token)").message("exactly one of github.app and github.token"))]
pub struct Github {
    /// A GitHub App.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<GithubApp>,

    /// A token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<GithubToken>,
}

/// A GitHub App. The private key is mounted as a file, never an environment variable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = Rule::new("has(self.installationId) != has(self.owners)").message("exactly one of github.app.installationId and github.app.owners"))]
pub struct GithubApp {
    /// The App's client ID: public, not a secret (`GITHUB_APP_ID`).
    #[schemars(length(min = 1))]
    pub id: String,

    /// The owners whose installation is looked up (`GITHUB_APP_OWNERS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub owners: Option<Vec<String>>,

    /// One pinned installation (`GITHUB_APP_INSTALLATION_ID`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<i64>,

    /// The Secret key that holds the private key (a PEM file).
    pub private_key_secret_ref: SecretKeyRef,
}

/// A GitHub token (`GITHUB_TOKEN`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GithubToken {
    /// The Secret key that holds the token.
    pub secret_ref: SecretKeyRef,
}

// ------------------------------------------------------------------ model

/// The model the agent talks to, over an OpenAI-compatible endpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    /// The model alias (`MODEL`).
    #[schemars(length(min = 1))]
    pub model: String,

    /// The endpoint (`MODEL_BASE_URL`): a value or a Secret key.
    pub base_url: BaseUrl,

    /// The Secret key that holds the API key (`MODEL_API_KEY`).
    pub api_key_secret_ref: SecretKeyRef,

    /// A JSON object merged into every chat-completions request (`MODEL_EXTRA_BODY`), to make a gateway
    /// or a model emit its reasoning, for example `{reasoning_effort: medium}`. Not a secret: it shows in
    /// the pod's environment. It may not set `model`, `messages`, `tools`, `tool_choice` or `stream`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "free_form_object")]
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,

    /// Send the reasoning of earlier turns back to the model, under this member name
    /// (`MODEL_ECHO_REASONING`). Unset: never sent, which is what almost every model wants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub echo_reasoning: Option<EchoReasoning>,
}

/// The member a model's earlier reasoning is sent back under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EchoReasoning {
    /// `reasoning_content`.
    ReasoningContent,
    /// `reasoning`.
    Reasoning,
}

impl EchoReasoning {
    /// The value of `MODEL_ECHO_REASONING`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReasoningContent => "reasoning_content",
            Self::Reasoning => "reasoning",
        }
    }
}

/// An object whose members the schema does not describe.
fn free_form_object(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "object", "x-kubernetes-preserve-unknown-fields": true })
}

/// The model endpoint: a plain value, or a Secret key when the deployment keeps it out of git.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = Rule::new("has(self.value) != has(self.secretRef)").message("exactly one of model.baseUrl.value and model.baseUrl.secretRef"))]
pub struct BaseUrl {
    /// The URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,

    /// The Secret key that holds the URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretKeyRef>,
}

// ------------------------------------------------------------------ tools

/// The tools the agent may use.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Tools {
    /// The read-only GitHub MCP server, as a native sidecar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_mcp: Option<GithubMcp>,

    /// Extra MCP servers, by name. Written to a ConfigMap that holds `${VAR}` references only.
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, McpServer>,

    /// Allow a plain `http://` MCP server on another machine (`MCP_ALLOW_INSECURE`). Never automatic.
    #[serde(default)]
    pub allow_insecure_http: bool,
}

/// The `github-mcp` sidecar. It holds no credential: the coder sends the credentials of each call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GithubMcp {
    /// Run the sidecar.
    #[serde(default = "yes")]
    pub sidecar: bool,

    /// Its loopback port. Checked again by the reconciler.
    #[serde(default = "github_mcp_port")]
    #[schemars(range(min = 1, max = 65535))]
    pub port: i32,

    /// `GITHUB_HOST` of the sidecar, for GitHub Enterprise. Empty or unset: github.com.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

fn yes() -> bool {
    true
}

fn github_mcp_port() -> i32 {
    8082
}

impl Default for GithubMcp {
    fn default() -> Self {
        Self {
            sidecar: yes(),
            port: github_mcp_port(),
            host: None,
        }
    }
}

/// One extra MCP server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    /// Its `http` or `https` URL, with no user name, password or `${…}` in it (checked by the reconciler).
    pub url: String,

    /// Request headers, by header name. Each value is a Secret key.
    #[serde(default)]
    pub headers: BTreeMap<String, McpHeader>,

    /// The agent starts without it when it is unreachable.
    #[serde(default)]
    pub optional: bool,
}

/// A header whose value is a Secret key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpHeader {
    /// Plain text put in front of the secret, e.g. `Bearer `.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,

    /// The Secret key that holds the value. It is named by the variable that carries it.
    pub secret_ref: SecretKeyRef,
}

// ------------------------------------------------------------ environment

/// The image, resources and volumes of the agent's pods. It has the shape of the spec of the
/// later `AgentEnvironment`, so a `*Ref` can join it additively.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    /// The image. adam-rs ships one image for both binaries.
    pub image: Image,

    /// CPU and memory of the agent container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,

    /// Volumes. None: the operator renders a Deployment.
    #[serde(default)]
    pub volumes: Vec<Volume>,

    /// The pod's `terminationGracePeriodSeconds`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0))]
    pub termination_grace_period_seconds: Option<i64>,
}

/// An image.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Image {
    /// The reference: a tag, or a tag and a digest.
    #[serde(rename = "ref")]
    #[schemars(length(min = 1))]
    pub reference: String,
}

/// A volume of the agent's pods.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    /// The volume's name; the claim template of a StatefulSet is named after it.
    #[schemars(length(min = 1))]
    pub name: String,

    /// Who shares the volume. v0 uses `agent`; the reconciler refuses the others.
    pub scope: VolumeScope,

    /// Where it is mounted.
    #[schemars(length(min = 1))]
    pub mount_path: String,

    /// What backs it.
    pub source: VolumeSource,
}

/// Who shares a volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum VolumeScope {
    /// Destroyed after a run.
    Run,
    /// Shared between the runs of one AgentService.
    Agent,
    /// Shared by the agents of a project.
    Project,
}

/// What backs a volume. Only `persistent` exists in v0.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VolumeSource {
    /// A persistent volume claim.
    pub persistent: PersistentVolume,
}

/// A persistent volume claim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistentVolume {
    /// Requested size, e.g. `20Gi`.
    pub size: Quantity,

    /// Storage class. Unset: the cluster's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,

    /// One claim per replica (a StatefulSet's `volumeClaimTemplate`), or a single shared claim.
    #[serde(default)]
    pub per_replica: bool,
}

// --------------------------------------------------------------- security

/// The pod's security context. The rest of it is fixed: no service-account token, `runAsNonRoot`,
/// `seccompProfile: RuntimeDefault`, no capabilities, no privilege escalation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Security {
    /// `runAsUser`. Unset: 10001, the adam image's user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub run_as_user: Option<i64>,

    /// `runAsGroup`. Unset: 10001.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub run_as_group: Option<i64>,

    /// `fsGroup`. Unset: 10001.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub fs_group: Option<i64>,

    /// `fsGroupChangePolicy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_group_change_policy: Option<FsGroupChangePolicy>,
}

/// `fsGroupChangePolicy` of a pod.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum FsGroupChangePolicy {
    /// Only when the root of the volume has the wrong owner.
    OnRootMismatch,
    /// Always.
    Always,
}
