//! A `RuntimeSpec` into the Kubernetes objects it stands for. Pure: no client, no I/O, so the
//! objects can be read in golden files and checked without a cluster.
//!
//! | Object | When |
//! |---|---|
//! | `StatefulSet` `<workload>` | the workload has a stable identity (`Workload::stable_identity`) |
//! | `Deployment` `<workload>` | it has not |
//! | `PodDisruptionBudget` `<workload>` | `Workload::min_available` is set |
//! | `Service` `<service>` | always, selecting `Network::selects` |
//! | `NetworkPolicy` `<service>` | `Network::allow_from` is not empty |
//! | `ConfigMap` `<file set>` | one per `FileSet` |
//! | `PersistentVolumeClaim` `<service>-<volume>` | a `Shared` persistent volume |
//!
//! The claims of a `PerReplica` volume are the StatefulSet's `volumeClaimTemplates`. **Data objects
//! carry no owner reference**; compute objects carry the owner reference the `OwnerHandle` stands for.

use std::collections::BTreeMap;

use adam_operator_ports::{
    Container, DeletionPolicy, EnvValue, FileSet, FsGroupChangePolicy, Network, OwnerHandle,
    PersistentVolume, Probe, ProbeAction, Resources, Role, RuntimeId, RuntimeSpec, Security,
    Sharing, VolumeSource, VolumeSpec, Workload,
};
use k8s_openapi::api::apps::v1::{
    Deployment, DeploymentSpec, StatefulSet, StatefulSetPersistentVolumeClaimRetentionPolicy,
    StatefulSetSpec, StatefulSetUpdateStrategy,
};
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::api::networking::v1 as net;
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::names::{
    self, DELETION_POLICY_ANNOTATION, DIGEST_ANNOTATION, VOLUME_LABEL, service_labels,
};

/// The name of the container port every agent serves.
pub const PORT_NAME: &str = "http";

/// The workload of a spec, as the object that runs it.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkloadObject {
    /// Stable pod names and a claim per replica.
    StatefulSet(Box<StatefulSet>),
    /// Interchangeable pods.
    Deployment(Box<Deployment>),
}

impl WorkloadObject {
    /// The object's name.
    pub fn name(&self) -> &str {
        let meta = match self {
            Self::StatefulSet(s) => &s.metadata,
            Self::Deployment(d) => &d.metadata,
        };
        meta.name.as_deref().unwrap_or_default()
    }

    /// `StatefulSet` or `Deployment`.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::StatefulSet(_) => "StatefulSet",
            Self::Deployment(_) => "Deployment",
        }
    }
}

/// Everything a spec stands for, in the order it is applied: file sets and claims first (pods mount
/// them), then the Service, the workloads, the budgets and the policy.
#[derive(Clone, Debug, PartialEq)]
pub struct Rendered {
    /// One per `FileSet`.
    pub config_maps: Vec<core::ConfigMap>,
    /// One per `Shared` persistent volume.
    pub claims: Vec<core::PersistentVolumeClaim>,
    /// The Service.
    pub service: core::Service,
    /// The workloads and the role each runs, in the order of the spec.
    pub workloads: Vec<(Role, WorkloadObject)>,
    /// One per workload with `min_available`.
    pub disruption_budgets: Vec<PodDisruptionBudget>,
    /// Only when someone is allowed from.
    pub network_policy: Option<net::NetworkPolicy>,
}

impl Rendered {
    /// Every object as JSON, in the order it is applied. The golden files are this, as YAML.
    pub fn documents(&self) -> Vec<serde_json::Value> {
        fn doc<T: Serialize>(o: &T) -> serde_json::Value {
            serde_json::to_value(o).unwrap_or(serde_json::Value::Null)
        }
        let mut out: Vec<serde_json::Value> = Vec::new();
        out.extend(self.config_maps.iter().map(doc));
        out.extend(self.claims.iter().map(doc));
        out.push(doc(&self.service));
        for (_, w) in &self.workloads {
            out.push(match w {
                WorkloadObject::StatefulSet(s) => doc(s.as_ref()),
                WorkloadObject::Deployment(d) => doc(d.as_ref()),
            });
        }
        out.extend(self.disruption_budgets.iter().map(doc));
        out.extend(self.network_policy.iter().map(doc));
        out
    }
}

// ---------------------------------------------------------------- the owner

/// What an [`OwnerHandle`] holds for this provider: the object that owns the runtime.
#[derive(Debug, Serialize, Deserialize)]
struct OwnerToken {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    name: String,
    uid: String,
}

/// The handle of an owner: `agents.vymalo.com/v1alpha1`, `AgentService`, its name and its uid, which
/// the controller reads from the object it reconciles. The controller never looks inside it.
pub fn owner_handle(
    api_version: impl Into<String>,
    kind: impl Into<String>,
    name: impl Into<String>,
    uid: impl Into<String>,
) -> OwnerHandle {
    let token = OwnerToken {
        api_version: api_version.into(),
        kind: kind.into(),
        name: name.into(),
        uid: uid.into(),
    };
    // A struct of four strings always serialises.
    OwnerHandle::new(serde_json::to_string(&token).unwrap_or_default())
}

/// The owner references a handle stands for. A handle that is not this provider's encoding (empty,
/// or a test's) stands for no owner: nothing is garbage collected on its account, and the
/// finalizer's explicit `delete` remains what removes the compute.
fn owner_references(handle: &OwnerHandle) -> Option<Vec<OwnerReference>> {
    let token: OwnerToken = serde_json::from_str(handle.token()).ok()?;
    if token.uid.is_empty() || token.name.is_empty() || token.kind.is_empty() {
        return None;
    }
    Some(vec![OwnerReference {
        api_version: token.api_version,
        kind: token.kind,
        name: token.name,
        uid: token.uid,
        controller: Some(true),
        block_owner_deletion: None,
    }])
}

// ---------------------------------------------------------------- the render

/// Make the objects of `spec` for the runtime `id`.
///
/// # Errors
///
/// [`Error::InvalidSpec`] when the spec asks for what Kubernetes cannot do and
/// `RuntimeSpec::check` does not cover: a probe on a port the container does not serve, a
/// per-replica volume on pods with no stable identity, a claim defined twice differently, a number
/// out of range, a file path that cannot be a ConfigMap key.
pub fn render(id: &RuntimeId, spec: &RuntimeSpec) -> Result<Rendered, Error> {
    if id.scope().is_empty() || id.name().is_empty() {
        return Err(Error::InvalidSpec(
            "the runtime id needs a namespace and a name".to_owned(),
        ));
    }
    let cx = Cx {
        id,
        spec,
        owner: owner_references(&spec.owner),
    };
    let config_maps = spec
        .file_sets
        .iter()
        .map(|f| cx.config_map(f))
        .collect::<Result<Vec<_>, _>>()?;

    let mut claims: BTreeMap<String, core::PersistentVolumeClaim> = BTreeMap::new();
    let mut workloads = Vec::new();
    let mut disruption_budgets = Vec::new();
    for w in &spec.workloads {
        let (object, shared) = cx.workload(w)?;
        for claim in shared {
            let name = claim.metadata.name.clone().unwrap_or_default();
            if let Some(existing) = claims.get(&name)
                && existing != &claim
            {
                return Err(Error::InvalidSpec(format!(
                    "the claim {name} is defined twice, differently"
                )));
            }
            claims.insert(name, claim);
        }
        if let Some(min) = w.min_available {
            disruption_budgets.push(cx.disruption_budget(w, min)?);
        }
        workloads.push((w.role, object));
    }
    Ok(Rendered {
        config_maps,
        claims: claims.into_values().collect(),
        service: cx.service(),
        workloads,
        disruption_budgets,
        network_policy: cx.network_policy(),
    })
}

fn invalid(what: impl Into<String>) -> Error {
    Error::InvalidSpec(what.into())
}

fn int32(what: &str, n: u32) -> Result<i32, Error> {
    i32::try_from(n).map_err(|_| invalid(format!("{what} {n} is out of range")))
}

fn quantities(map: &BTreeMap<String, String>) -> Option<BTreeMap<String, Quantity>> {
    (!map.is_empty()).then(|| {
        map.iter()
            .map(|(k, v)| (k.clone(), Quantity(v.clone())))
            .collect()
    })
}

struct Cx<'a> {
    id: &'a RuntimeId,
    spec: &'a RuntimeSpec,
    owner: Option<Vec<OwnerReference>>,
}

impl Cx<'_> {
    /// Metadata of an object of the service. `owned`: a compute object, which the owner's garbage
    /// collection may take with it. Data objects are not.
    fn meta(
        &self,
        name: &str,
        labels: BTreeMap<String, String>,
        owned: bool,
        policy: bool,
    ) -> ObjectMeta {
        let mut annotations = BTreeMap::new();
        if !self.spec.digest.is_empty() {
            annotations.insert(DIGEST_ANNOTATION.to_owned(), self.spec.digest.clone());
        }
        if policy {
            annotations.insert(
                DELETION_POLICY_ANNOTATION.to_owned(),
                policy_name(self.spec.deletion).to_owned(),
            );
        }
        ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(self.id.scope().to_owned()),
            labels: Some(labels),
            annotations: (!annotations.is_empty()).then_some(annotations),
            owner_references: if owned { self.owner.clone() } else { None },
            ..ObjectMeta::default()
        }
    }

    fn config_map(&self, set: &FileSet) -> Result<core::ConfigMap, Error> {
        let mut data = BTreeMap::new();
        for (path, content) in &set.files {
            let key = names::config_map_key(path).ok_or_else(|| {
                invalid(format!(
                    "file set {}: the path {path} is too long to be a ConfigMap key",
                    set.name
                ))
            })?;
            data.insert(key, content.clone());
        }
        Ok(core::ConfigMap {
            metadata: self.meta(&set.name, service_labels(self.id), true, false),
            data: Some(data),
            immutable: set.immutable.then_some(true),
            ..core::ConfigMap::default()
        })
    }

    fn service(&self) -> core::Service {
        let Network { port, selects, .. } = &self.spec.network;
        core::Service {
            metadata: self.meta(self.id.name(), service_labels(self.id), true, false),
            spec: Some(core::ServiceSpec {
                type_: Some("ClusterIP".to_owned()),
                selector: Some(names::selector_labels(selects, self.id)),
                ports: Some(vec![core::ServicePort {
                    name: Some(PORT_NAME.to_owned()),
                    port: i32::from(*port),
                    target_port: Some(IntOrString::String(PORT_NAME.to_owned())),
                    protocol: Some("TCP".to_owned()),
                    ..core::ServicePort::default()
                }]),
                ..core::ServiceSpec::default()
            }),
            ..core::Service::default()
        }
    }

    /// Ingress on the container port from the peers, and no egress rule: the agent needs git hosts,
    /// the model gateway and registries.
    fn network_policy(&self) -> Option<net::NetworkPolicy> {
        let network = &self.spec.network;
        if network.allow_from.is_empty() {
            return None;
        }
        let peers = network.allow_from.iter().map(peer).collect();
        Some(net::NetworkPolicy {
            metadata: self.meta(self.id.name(), service_labels(self.id), true, false),
            spec: Some(net::NetworkPolicySpec {
                // Every pod of the service, whatever its workload.
                pod_selector: Some(LabelSelector {
                    match_labels: Some(BTreeMap::from([
                        (
                            names::MANAGED_BY_LABEL.to_owned(),
                            names::MANAGED_BY_VALUE.to_owned(),
                        ),
                        (names::INSTANCE_LABEL.to_owned(), self.id.name().to_owned()),
                    ])),
                    match_expressions: None,
                }),
                policy_types: Some(vec!["Ingress".to_owned()]),
                ingress: Some(vec![net::NetworkPolicyIngressRule {
                    from: Some(peers),
                    ports: Some(vec![net::NetworkPolicyPort {
                        protocol: Some("TCP".to_owned()),
                        port: Some(IntOrString::Int(i32::from(network.port))),
                        end_port: None,
                    }]),
                }]),
                ..net::NetworkPolicySpec::default()
            }),
        })
    }

    fn disruption_budget(&self, w: &Workload, min: u32) -> Result<PodDisruptionBudget, Error> {
        Ok(PodDisruptionBudget {
            metadata: self.meta(&w.name, service_labels(self.id), true, false),
            spec: Some(PodDisruptionBudgetSpec {
                min_available: Some(IntOrString::Int(int32("minAvailable", min)?)),
                selector: Some(LabelSelector {
                    match_labels: Some(names::selector_labels(&w.name, self.id)),
                    match_expressions: None,
                }),
                ..PodDisruptionBudgetSpec::default()
            }),
            ..PodDisruptionBudget::default()
        })
    }

    /// The workload's object, and the shared claims its volumes need.
    fn workload(
        &self,
        w: &Workload,
    ) -> Result<(WorkloadObject, Vec<core::PersistentVolumeClaim>), Error> {
        let selector = LabelSelector {
            match_labels: Some(names::selector_labels(&w.name, self.id)),
            match_expressions: None,
        };
        let labels = names::workload_labels(&w.name, w.role, self.id);
        let replicas = if self.spec.suspend {
            0
        } else {
            int32("replicas", w.replicas)?
        };

        let mut volumes = Vec::new();
        let mut templates = Vec::new();
        let mut shared = Vec::new();
        for v in &w.volumes {
            match &v.source {
                VolumeSource::Persistent(p) => match p.sharing {
                    Sharing::PerReplica => {
                        if !w.stable_identity {
                            return Err(invalid(format!(
                                "workload {}: the volume {} is per replica, which needs pods with a stable identity",
                                w.name, v.name
                            )));
                        }
                        templates.push(self.claim_template(v, p));
                    }
                    Sharing::Shared => {
                        let claim = self.shared_claim(v, p);
                        volumes.push(core::Volume {
                            name: v.name.clone(),
                            persistent_volume_claim: Some(
                                core::PersistentVolumeClaimVolumeSource {
                                    claim_name: claim.metadata.name.clone().unwrap_or_default(),
                                    read_only: None,
                                },
                            ),
                            ..core::Volume::default()
                        });
                        shared.push(claim);
                    }
                },
                _ => volumes.push(self.volume(w, v)?),
            }
        }

        let pod = core::PodTemplateSpec {
            metadata: Some(ObjectMeta {
                labels: Some(labels.clone()),
                annotations: (!self.spec.digest.is_empty()).then(|| {
                    BTreeMap::from([(DIGEST_ANNOTATION.to_owned(), self.spec.digest.clone())])
                }),
                ..ObjectMeta::default()
            }),
            spec: Some(core::PodSpec {
                automount_service_account_token: Some(false),
                termination_grace_period_seconds: w.termination_grace_secs.map(i64::from),
                security_context: Some(pod_security(&w.security)),
                init_containers: (!w.sidecars.is_empty())
                    .then(|| {
                        w.sidecars
                            .iter()
                            .map(|c| {
                                let mut k = container(c)?;
                                // A native sidecar: starts before the agent, is probed before the
                                // agent starts, restarts on its own and stops after it.
                                k.restart_policy = Some("Always".to_owned());
                                Ok::<_, Error>(k)
                            })
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .transpose()?,
                containers: vec![container(&w.container)?],
                volumes: (!volumes.is_empty()).then_some(volumes),
                ..core::PodSpec::default()
            }),
        };

        let meta = self.meta(&w.name, labels, true, true);
        let object = if w.stable_identity {
            WorkloadObject::StatefulSet(Box::new(StatefulSet {
                metadata: meta,
                spec: Some(StatefulSetSpec {
                    replicas: Some(replicas),
                    service_name: Some(self.id.name().to_owned()),
                    pod_management_policy: Some("OrderedReady".to_owned()),
                    update_strategy: Some(StatefulSetUpdateStrategy {
                        type_: Some("RollingUpdate".to_owned()),
                        rolling_update: None,
                    }),
                    // Claims are data: they get no owner reference, so deleting the set or scaling
                    // it down never takes them (the deletion policy decides, in `delete`).
                    persistent_volume_claim_retention_policy: Some(
                        StatefulSetPersistentVolumeClaimRetentionPolicy {
                            when_deleted: Some("Retain".to_owned()),
                            when_scaled: Some("Retain".to_owned()),
                        },
                    ),
                    selector,
                    template: pod,
                    volume_claim_templates: (!templates.is_empty()).then_some(templates),
                    ..StatefulSetSpec::default()
                }),
                ..StatefulSet::default()
            }))
        } else {
            WorkloadObject::Deployment(Box::new(Deployment {
                metadata: meta,
                spec: Some(DeploymentSpec {
                    replicas: Some(replicas),
                    selector,
                    template: pod,
                    ..DeploymentSpec::default()
                }),
                ..Deployment::default()
            }))
        };
        Ok((object, shared))
    }

    /// The claim a StatefulSet makes for each replica. Its metadata is fixed once the set exists
    /// (the API server refuses a change of a template), so it holds nothing that moves: no digest, no
    /// deletion policy.
    fn claim_template(&self, v: &VolumeSpec, p: &PersistentVolume) -> core::PersistentVolumeClaim {
        let mut labels = service_labels(self.id);
        labels.insert(VOLUME_LABEL.to_owned(), v.name.clone());
        core::PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(v.name.clone()),
                labels: Some(labels),
                ..ObjectMeta::default()
            },
            spec: Some(claim_spec(p, "ReadWriteOnce")),
            ..core::PersistentVolumeClaim::default()
        }
    }

    /// The one claim every replica mounts: ReadWriteMany, named `<service>-<volume>`.
    fn shared_claim(&self, v: &VolumeSpec, p: &PersistentVolume) -> core::PersistentVolumeClaim {
        let mut labels = service_labels(self.id);
        labels.insert(VOLUME_LABEL.to_owned(), v.name.clone());
        core::PersistentVolumeClaim {
            metadata: self.meta(&names::shared_claim(self.id, &v.name), labels, false, true),
            spec: Some(claim_spec(p, "ReadWriteMany")),
            ..core::PersistentVolumeClaim::default()
        }
    }

    /// A volume that is not persistent.
    fn volume(&self, w: &Workload, v: &VolumeSpec) -> Result<core::Volume, Error> {
        let mut volume = core::Volume {
            name: v.name.clone(),
            ..core::Volume::default()
        };
        match &v.source {
            VolumeSource::Persistent(_) => {}
            VolumeSource::Files { file_set, mode } => {
                let set = self
                    .spec
                    .file_sets
                    .iter()
                    .find(|f| &f.name == file_set)
                    .ok_or_else(|| {
                        invalid(format!(
                            "workload {}: volume {} names the file set {file_set}, which the spec lacks",
                            w.name, v.name
                        ))
                    })?;
                let mut items = Vec::new();
                for path in set.files.keys() {
                    let key = names::config_map_key(path).ok_or_else(|| {
                        invalid(format!("file set {file_set}: {path} is too long"))
                    })?;
                    items.push(core::KeyToPath {
                        key,
                        path: path.clone(),
                        mode: None,
                    });
                }
                volume.config_map = Some(core::ConfigMapVolumeSource {
                    name: file_set.clone(),
                    default_mode: Some(file_mode(*mode)?),
                    items: Some(items),
                    optional: None,
                });
            }
            VolumeSource::ExternalFiles { name, mode } => {
                volume.config_map = Some(core::ConfigMapVolumeSource {
                    name: name.clone(),
                    default_mode: Some(file_mode(*mode)?),
                    items: None,
                    optional: None,
                });
            }
            VolumeSource::SecretFile { secret, file, mode } => {
                // One key as one file: a private key is a file and never a variable.
                volume.secret = Some(core::SecretVolumeSource {
                    secret_name: Some(secret.name.clone()),
                    default_mode: Some(file_mode(*mode)?),
                    items: Some(vec![core::KeyToPath {
                        key: secret.key.clone(),
                        path: file.clone(),
                        mode: None,
                    }]),
                    optional: None,
                });
            }
        }
        Ok(volume)
    }
}

fn file_mode(mode: u32) -> Result<i32, Error> {
    if mode > 0o777 {
        return Err(invalid(format!(
            "the file mode {mode:o} is not a permission"
        )));
    }
    int32("file mode", mode)
}

const fn policy_name(policy: DeletionPolicy) -> &'static str {
    match policy {
        DeletionPolicy::Retain => "Retain",
        DeletionPolicy::Delete => "Delete",
    }
}

/// What a `deletion-policy` annotation says. Anything else is `Retain`: data stays unless the
/// policy clearly says otherwise.
pub fn parse_policy(annotation: Option<&str>) -> DeletionPolicy {
    match annotation {
        Some("Delete") => DeletionPolicy::Delete,
        _ => DeletionPolicy::Retain,
    }
}

fn claim_spec(p: &PersistentVolume, access: &str) -> core::PersistentVolumeClaimSpec {
    core::PersistentVolumeClaimSpec {
        access_modes: Some(vec![access.to_owned()]),
        storage_class_name: p.storage_class.clone(),
        resources: Some(core::VolumeResourceRequirements {
            requests: Some(BTreeMap::from([(
                "storage".to_owned(),
                Quantity(p.size.clone()),
            )])),
            limits: None,
        }),
        ..core::PersistentVolumeClaimSpec::default()
    }
}

/// The pod's security context: the ids of the spec and the fixed hardening.
fn pod_security(s: &Security) -> core::PodSecurityContext {
    core::PodSecurityContext {
        run_as_non_root: Some(true),
        run_as_user: Some(s.run_as_user),
        run_as_group: Some(s.run_as_group),
        fs_group: Some(s.fs_group),
        fs_group_change_policy: s.fs_group_change_policy.map(|p| {
            match p {
                FsGroupChangePolicy::OnRootMismatch => "OnRootMismatch",
                FsGroupChangePolicy::Always => "Always",
            }
            .to_owned()
        }),
        seccomp_profile: Some(core::SeccompProfile {
            type_: "RuntimeDefault".to_owned(),
            localhost_profile: None,
        }),
        ..core::PodSecurityContext::default()
    }
}

fn container(c: &Container) -> Result<core::Container, Error> {
    let env = c
        .env
        .iter()
        .map(|e| core::EnvVar {
            name: e.name.clone(),
            value: match &e.value {
                EnvValue::Literal(v) => Some(v.clone()),
                EnvValue::Secret(_) | EnvValue::PodName => None,
            },
            value_from: match &e.value {
                EnvValue::Literal(_) => None,
                EnvValue::Secret(s) => Some(core::EnvVarSource {
                    secret_key_ref: Some(core::SecretKeySelector {
                        name: s.name.clone(),
                        key: s.key.clone(),
                        optional: None,
                    }),
                    ..core::EnvVarSource::default()
                }),
                EnvValue::PodName => Some(core::EnvVarSource {
                    field_ref: Some(core::ObjectFieldSelector {
                        field_path: "metadata.name".to_owned(),
                        api_version: None,
                    }),
                    ..core::EnvVarSource::default()
                }),
            },
        })
        .collect::<Vec<_>>();

    let probe = |p: &Option<Probe>| -> Result<Option<core::Probe>, Error> {
        p.as_ref().map(|p| self::probe(c, p)).transpose()
    };
    let resources = resources(&c.resources);
    Ok(core::Container {
        name: c.name.clone(),
        image: Some(c.image.clone()),
        command: (!c.command.is_empty()).then(|| c.command.clone()),
        args: (!c.args.is_empty()).then(|| c.args.clone()),
        env: (!env.is_empty()).then_some(env),
        ports: c.port.map(|p| {
            vec![core::ContainerPort {
                name: Some(PORT_NAME.to_owned()),
                container_port: i32::from(p),
                protocol: Some("TCP".to_owned()),
                ..core::ContainerPort::default()
            }]
        }),
        volume_mounts: (!c.mounts.is_empty()).then(|| {
            c.mounts
                .iter()
                .map(|m| core::VolumeMount {
                    name: m.volume.clone(),
                    mount_path: m.path.clone(),
                    read_only: m.read_only.then_some(true),
                    ..core::VolumeMount::default()
                })
                .collect()
        }),
        startup_probe: probe(&c.probes.startup)?,
        liveness_probe: probe(&c.probes.liveness)?,
        readiness_probe: probe(&c.probes.readiness)?,
        resources,
        // Every container drops every capability and refuses privilege escalation.
        security_context: Some(core::SecurityContext {
            allow_privilege_escalation: Some(false),
            capabilities: Some(core::Capabilities {
                add: None,
                drop: Some(vec!["ALL".to_owned()]),
            }),
            ..core::SecurityContext::default()
        }),
        ..core::Container::default()
    })
}

fn resources(r: &Resources) -> Option<core::ResourceRequirements> {
    let (requests, limits) = (quantities(&r.requests), quantities(&r.limits));
    (requests.is_some() || limits.is_some()).then(|| core::ResourceRequirements {
        requests,
        limits,
        ..core::ResourceRequirements::default()
    })
}

fn probe(c: &Container, p: &Probe) -> Result<core::Probe, Error> {
    let mut probe = core::Probe {
        period_seconds: Some(int32("probe period", p.period_secs)?),
        timeout_seconds: p
            .timeout_secs
            .map(|n| int32("probe timeout", n))
            .transpose()?,
        failure_threshold: p
            .failure_threshold
            .map(|n| int32("probe failure threshold", n))
            .transpose()?,
        ..core::Probe::default()
    };
    match &p.action {
        ProbeAction::Http { path } => {
            if c.port.is_none() {
                return Err(invalid(format!(
                    "container {}: an HTTP probe needs the container to serve a port",
                    c.name
                )));
            }
            probe.http_get = Some(core::HTTPGetAction {
                path: Some(path.clone()),
                port: IntOrString::String(PORT_NAME.to_owned()),
                ..core::HTTPGetAction::default()
            });
        }
        ProbeAction::Exec { command } => {
            probe.exec = Some(core::ExecAction {
                command: Some(command.clone()),
            });
        }
    }
    Ok(probe)
}

fn selector(s: &adam_operator_ports::Selector) -> LabelSelector {
    use adam_operator_ports::SelectorOperator as Op;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement;
    LabelSelector {
        match_labels: (!s.match_labels.is_empty()).then(|| s.match_labels.clone()),
        match_expressions: (!s.match_expressions.is_empty()).then(|| {
            s.match_expressions
                .iter()
                .map(|r| LabelSelectorRequirement {
                    key: r.key.clone(),
                    operator: match r.operator {
                        Op::In => "In",
                        Op::NotIn => "NotIn",
                        Op::Exists => "Exists",
                        Op::DoesNotExist => "DoesNotExist",
                    }
                    .to_owned(),
                    values: (!r.values.is_empty()).then(|| r.values.clone()),
                })
                .collect()
        }),
    }
}

fn peer(p: &adam_operator_ports::Peer) -> net::NetworkPolicyPeer {
    net::NetworkPolicyPeer {
        namespace_selector: p.namespaces.as_ref().map(selector),
        pod_selector: p.pods.as_ref().map(selector),
        ip_block: p.cidr.as_ref().map(|b| net::IPBlock {
            cidr: b.cidr.clone(),
            except: (!b.except.is_empty()).then(|| b.except.clone()),
        }),
    }
}
