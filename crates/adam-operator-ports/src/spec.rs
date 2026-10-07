//! The neutral description of a runtime: what a provider is asked to make. Nothing in it is a
//! Kubernetes type, and nothing in it can hold a secret's value.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{OwnerHandle, RuntimeError, SecretRef};

/// What happens to data when the service is deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeletionPolicy {
    /// The compute goes; work volumes and an operator-owned database stay.
    #[default]
    Retain,
    /// The data goes too.
    Delete,
}

/// Which part of an adam process a workload runs: adam's `ROLE`. Closed on purpose: a new role
/// must fail to compile in every provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Role {
    /// The A2A server and the workers in one process (`ROLE` unset).
    All,
    /// The A2A server over a runtime that only starts, delivers to and cancels runs.
    ControlPlane,
    /// The workers.
    Worker,
}

impl Role {
    /// Whether this role steps runs (and so is the workload `replicas` of a status counts).
    pub const fn runs_workers(self) -> bool {
        matches!(self, Self::All | Self::Worker)
    }
}

/// Everything a provider needs to make one agent's runtime. It is the same on every `ensure` of
/// the same configuration, and it is what the [`digest`](Self::digest) is of.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSpec {
    /// Whose runtime it is. Not part of the digest.
    pub owner: OwnerHandle,
    /// What a delete does to data. Not part of the digest.
    pub deletion: DeletionPolicy,
    /// `sha256:<hex>` of what the pods run (see the digest rules in `adam-operator-domain`), which a provider
    /// writes on the pods (`agents.vymalo.com/config-digest`) so that a changed folder, file, image
    /// or variable is a rollout and nothing else is. Empty when the spec was not resolved.
    pub digest: String,
    /// Scale to zero. Not part of the digest: scale is applied, it does not roll pods.
    pub suspend: bool,
    /// The workloads: one for `combined`, two for `split` (the workers, then the front). The
    /// first is the one `replicas` of a [`RuntimeStatus`](crate::RuntimeStatus) counts.
    pub workloads: Vec<Workload>,
    /// Files a provider materialises by name (on Kubernetes: ConfigMaps).
    pub file_sets: Vec<FileSet>,
    /// How the agent is reached.
    pub network: Network,
}

impl RuntimeSpec {
    /// The replicas that step runs when everything runs: the sum over the workloads whose role
    /// [`runs_workers`](Role::runs_workers).
    pub fn worker_replicas(&self) -> u32 {
        self.workloads
            .iter()
            .filter(|w| w.role.runs_workers())
            .map(|w| w.replicas)
            .sum()
    }

    /// The invariants every provider relies on, so each rejects the same malformed spec the same
    /// way: at least one workload, unique names, every mount names a volume of its workload, every
    /// file volume names a file set of the spec, the network selects a workload, no empty names.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidSpec`] naming the first broken invariant.
    pub fn check(&self) -> Result<(), RuntimeError> {
        let bad = |what: String| Err(RuntimeError::InvalidSpec(what));
        if self.workloads.is_empty() {
            return bad("no workload".into());
        }
        let mut workloads = BTreeSet::new();
        for w in &self.workloads {
            if w.name.is_empty() {
                return bad("a workload has no name".into());
            }
            if !workloads.insert(w.name.as_str()) {
                return bad(format!("workload {} appears twice", w.name));
            }
            if w.container.image.is_empty() {
                return bad(format!("workload {}: the container has no image", w.name));
            }
            let mut volumes = BTreeSet::new();
            for v in &w.volumes {
                if v.name.is_empty() || !volumes.insert(v.name.as_str()) {
                    return bad(format!(
                        "workload {}: volume {:?} is empty or repeated",
                        w.name, v.name
                    ));
                }
                match &v.source {
                    VolumeSource::Files { file_set, .. } => {
                        if !self.file_sets.iter().any(|f| &f.name == file_set) {
                            return bad(format!(
                                "workload {}: volume {} names the file set {file_set}, which the spec lacks",
                                w.name, v.name
                            ));
                        }
                    }
                    VolumeSource::SecretFile { secret, file, .. } => {
                        if secret.name.is_empty() || secret.key.is_empty() || file.is_empty() {
                            return bad(format!(
                                "workload {}: volume {} has an empty secret reference",
                                w.name, v.name
                            ));
                        }
                    }
                    VolumeSource::Persistent(p) => {
                        if p.size.is_empty() {
                            return bad(format!(
                                "workload {}: volume {} has no size",
                                w.name, v.name
                            ));
                        }
                    }
                    VolumeSource::ExternalFiles { name, .. } => {
                        if name.is_empty() {
                            return bad(format!(
                                "workload {}: volume {} names no file set",
                                w.name, v.name
                            ));
                        }
                    }
                }
            }
            for c in std::iter::once(&w.container).chain(&w.sidecars) {
                if c.name.is_empty() {
                    return bad(format!("workload {}: a container has no name", w.name));
                }
                for m in &c.mounts {
                    if !volumes.contains(m.volume.as_str()) {
                        return bad(format!(
                            "workload {}: container {} mounts {}, which is no volume of the workload",
                            w.name, c.name, m.volume
                        ));
                    }
                }
                let mut names = BTreeSet::new();
                for e in &c.env {
                    if e.name.is_empty() || !names.insert(e.name.as_str()) {
                        return bad(format!(
                            "workload {}: container {} sets the variable {:?} twice or with no name",
                            w.name, c.name, e.name
                        ));
                    }
                    if let EnvValue::Secret(s) = &e.value
                        && (s.name.is_empty() || s.key.is_empty())
                    {
                        return bad(format!(
                            "workload {}: container {}: {} has an empty secret reference",
                            w.name, c.name, e.name
                        ));
                    }
                }
            }
        }
        let mut sets = BTreeSet::new();
        for f in &self.file_sets {
            if f.name.is_empty() || !sets.insert(f.name.as_str()) {
                return bad(format!("file set {:?} is empty or repeated", f.name));
            }
        }
        if !workloads.contains(self.network.selects.as_str()) {
            return bad(format!(
                "the network selects {:?}, which is no workload",
                self.network.selects
            ));
        }
        if self.network.port == 0 {
            return bad("the network port is 0".into());
        }
        Ok(())
    }
}

/// One set of identical pods.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workload {
    /// The object name: `<svc>`, or `<svc>-front`.
    pub name: String,
    /// The part of adam it runs.
    pub role: Role,
    /// Pods when running. Not part of the digest.
    pub replicas: u32,
    /// The pods keep their name across restarts and each owns its volumes (a StatefulSet on
    /// Kubernetes). True when a volume is per replica, and when a pod name is a worker's identity
    /// (`EnvValue::PodName`: runs are pinned to a worker by it, so a new name strands them).
    pub stable_identity: bool,
    /// The agent container.
    pub container: Container,
    /// Containers that start before the agent, run beside it and stop after it (native sidecars).
    pub sidecars: Vec<Container>,
    /// The volumes of the pods.
    pub volumes: Vec<VolumeSpec>,
    /// The security context the pod asks for. Everything else is the provider's fixed hardening:
    /// no service-account token, non-root, the runtime's default seccomp profile, no capabilities,
    /// no privilege escalation.
    pub security: Security,
    /// Seconds a pod has to stop.
    pub termination_grace_secs: Option<u32>,
    /// Pods that must stay up through a voluntary disruption (a PodDisruptionBudget); `None`: no
    /// budget. Not part of the digest.
    pub min_available: Option<u32>,
}

/// One container.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Container {
    /// Its name in the pod.
    pub name: String,
    /// The image reference, a tag or a tag and a digest.
    pub image: String,
    /// Replaces the image's entrypoint. Empty: the image's own.
    pub command: Vec<String>,
    /// Arguments.
    pub args: Vec<String>,
    /// The environment, in order. Unique names.
    pub env: Vec<EnvVar>,
    /// The port it serves, named `http`. `None`: it serves none.
    pub port: Option<u16>,
    /// Where the volumes are mounted.
    pub mounts: Vec<Mount>,
    /// The probes.
    pub probes: Probes,
    /// CPU and memory.
    pub resources: Resources,
}

/// One environment variable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    /// Its name.
    pub name: String,
    /// Where its value comes from.
    pub value: EnvValue,
}

impl EnvVar {
    /// A variable with a literal value.
    pub fn literal(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: EnvValue::Literal(value.into()),
        }
    }

    /// A variable whose value is a key of a Secret.
    pub fn secret(name: impl Into<String>, secret: SecretRef) -> Self {
        Self {
            name: name.into(),
            value: EnvValue::Secret(secret),
        }
    }
}

/// Where the value of a variable comes from. Closed on purpose: a new source must fail to compile
/// in every provider, and a secret is a reference and never a value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvValue {
    /// A value written in the spec. Never a secret.
    Literal(String),
    /// A key of a Secret the provider hands to the process without reading it.
    Secret(SecretRef),
    /// The name of the pod itself (the downward API's `metadata.name`).
    PodName,
}

/// A volume mounted into a container.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// The volume's name, one of the workload's.
    pub volume: String,
    /// Where it is mounted.
    pub path: String,
    /// Mounted read-only.
    pub read_only: bool,
}

/// A probe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    /// What it does.
    pub action: ProbeAction,
    /// Seconds between probes.
    pub period_secs: u32,
    /// Seconds a probe may take. `None`: the provider's default.
    pub timeout_secs: Option<u32>,
    /// Failures before it counts as failed. `None`: the provider's default.
    pub failure_threshold: Option<u32>,
}

/// What a probe does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeAction {
    /// `GET` this path on the container's port.
    Http {
        /// The path.
        path: String,
    },
    /// Run this command in the container; success is exit code 0.
    Exec {
        /// The command and its arguments.
        command: Vec<String>,
    },
}

/// The probes of a container.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probes {
    /// Ready to be probed for liveness: the process has started.
    pub startup: Option<Probe>,
    /// Alive: restart it when this fails.
    pub liveness: Option<Probe>,
    /// Ready: take it out of the Service when this fails.
    pub readiness: Option<Probe>,
}

/// CPU and memory, as quantity strings (`500m`, `1Gi`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    /// What the container is guaranteed.
    pub requests: BTreeMap<String, String>,
    /// What it may not exceed.
    pub limits: BTreeMap<String, String>,
}

/// The pod's security context. The ids are applied as given.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Security {
    /// `runAsUser`.
    pub run_as_user: i64,
    /// `runAsGroup`.
    pub run_as_group: i64,
    /// `fsGroup`.
    pub fs_group: i64,
    /// `fsGroupChangePolicy`. `None`: the provider's default.
    pub fs_group_change_policy: Option<FsGroupChangePolicy>,
}

/// When the group ownership of a volume is changed to `fsGroup`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsGroupChangePolicy {
    /// Only when the root of the volume has the wrong owner.
    OnRootMismatch,
    /// Always.
    Always,
}

/// A volume of a workload's pods.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSpec {
    /// Its name, unique in the workload. A persistent volume's claim is named after it.
    pub name: String,
    /// What backs it.
    pub source: VolumeSource,
}

/// What backs a volume. Closed on purpose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VolumeSource {
    /// Durable storage. **Data**: not removed with the service unless the deletion policy says so.
    Persistent(PersistentVolume),
    /// A file set of the spec, mounted read-only, each file at its path.
    Files {
        /// The name of one of [`RuntimeSpec::file_sets`].
        file_set: String,
        /// The permission bits of the files (`0o444`).
        mode: u32,
    },
    /// A file set that someone else owns (on Kubernetes: a ConfigMap the user made), mounted as it
    /// is. The operator cannot read it, so a change of its content is not a rollout.
    ExternalFiles {
        /// Its name.
        name: String,
        /// The permission bits of the files.
        mode: u32,
    },
    /// One key of a Secret, mounted as one file: a private key is a file and never a variable.
    SecretFile {
        /// The Secret key.
        secret: SecretRef,
        /// The file's name in the mount.
        file: String,
        /// The permission bits (`0o440`).
        mode: u32,
    },
}

/// A persistent volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistentVolume {
    /// Requested size, a quantity string (`20Gi`).
    pub size: String,
    /// Storage class. `None`: the cluster's default.
    pub storage_class: Option<String>,
    /// Who shares it.
    pub sharing: Sharing,
}

/// Who shares a persistent volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sharing {
    /// One volume per replica (a `volumeClaimTemplate`), mounted by that replica alone
    /// (ReadWriteOnce).
    PerReplica,
    /// One volume for the service, mounted by every replica at once (ReadWriteMany). Named
    /// `<service>-<volume name>`.
    Shared,
}

/// Files a provider materialises by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSet {
    /// Its name, unique in the spec.
    pub name: String,
    /// Relative path to content. Paths may contain `/`; a provider whose storage cannot
    /// (ConfigMap keys) maps them and mounts each file at its path.
    pub files: BTreeMap<String, String>,
    /// The name carries the content, so the set is never edited: a new content is a new set, and a
    /// provider removes the superseded ones once the rollout is done.
    pub immutable: bool,
}

/// How the agent is reached: a Service, and when there are peers, who may reach it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    /// The port the Service listens on and the container serves.
    pub port: u16,
    /// The name of the workload the Service sends to (the front, with `split`).
    pub selects: String,
    /// Who may reach the port. Empty: nobody is restricted and no policy is made. Ingress only: the
    /// agent needs git hosts, the model gateway and registries, so there is no egress rule.
    pub allow_from: Vec<Peer>,
}

/// One peer of an ingress rule: what a Kubernetes `NetworkPolicyPeer` says, in neutral terms. At
/// least one field is set, and `cidr` is not combined with the selectors.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    /// Peers in the namespaces these labels select.
    pub namespaces: Option<Selector>,
    /// Peers among the pods these labels select.
    pub pods: Option<Selector>,
    /// Peers at these addresses.
    pub cidr: Option<IpBlock>,
}

/// A label selector.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selector {
    /// Labels that must equal.
    pub match_labels: BTreeMap<String, String>,
    /// Further requirements.
    pub match_expressions: Vec<Requirement>,
}

/// One requirement of a [`Selector`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    /// The label key.
    pub key: String,
    /// How it is compared.
    pub operator: SelectorOperator,
    /// The values, for `In` and `NotIn`.
    pub values: Vec<String>,
}

/// How a [`Requirement`] compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelectorOperator {
    /// The label's value is one of `values`.
    In,
    /// The label's value is none of `values`.
    NotIn,
    /// The label exists.
    Exists,
    /// The label does not exist.
    DoesNotExist,
}

/// A range of addresses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpBlock {
    /// A CIDR, e.g. `10.0.0.0/8`.
    pub cidr: String,
    /// Sub-ranges left out.
    pub except: Vec<String>,
}
