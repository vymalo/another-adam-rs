//! The provider: every call that reaches the API server.

use std::collections::BTreeSet;
use std::fmt::Debug;

use adam_operator_ports::{
    Capabilities, DeleteOutcome, DeletionPolicy, Endpoint, Issue, IssueReason, Role, RuntimeError,
    RuntimeId, RuntimeProvider, RuntimeSpec, RuntimeStatus, Surface,
};
use futures::stream::BoxStream;
use k8s_openapi::NamespaceResourceScope;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Pod, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use kube::{Api, Client, Resource};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::error::Error;
use crate::names::{
    self, DELETION_POLICY_ANNOTATION, FIELD_MANAGER, INSTANCE_LABEL, MANAGED_BY_LABEL,
    MANAGED_BY_VALUE, VOLUME_LABEL,
};
use crate::render::{self, Rendered, WorkloadObject, parse_policy};
use crate::status::{self, WorkloadObservation};

type Result<T, E = Error> = std::result::Result<T, E>;

/// Name the process's TLS provider (rustls' default, aws-lc-rs) unless one is named already.
///
/// `kube` builds its TLS configuration with the process default, and rustls refuses to guess one when the
/// dependency tree of a binary enables more than one provider, which any tree that also holds an HTTP client
/// does. [`KubernetesRuntime::try_default`] calls it; a caller that makes its own `kube::Client` calls it
/// first. Idempotent. The same three lines as `adam_env_kubernetes::install_crypto_provider`, which this
/// crate does not depend on.
pub fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Another thread may have installed one in between: that is as good.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

/// A namespaced kind this provider reads and writes.
pub trait Kind:
    Resource<Scope = NamespaceResourceScope, DynamicType = ()>
    + Clone
    + Debug
    + Serialize
    + DeserializeOwned
    + Send
    + Sync
    + 'static
{
}

impl<K> Kind for K where
    K: Resource<Scope = NamespaceResourceScope, DynamicType = ()>
        + Clone
        + Debug
        + Serialize
        + DeserializeOwned
        + Send
        + Sync
        + 'static
{
}

fn what<K: Kind>(name: &str) -> String {
    format!("{} {name}", K::kind(&()))
}

/// Whether an object is this service's and ours: the adoption guard's test.
fn is_ours(meta: &ObjectMeta, id: &RuntimeId) -> bool {
    let labels = meta.labels.as_ref();
    let label = |k: &str| labels.and_then(|l| l.get(k)).map(String::as_str);
    label(MANAGED_BY_LABEL) == Some(MANAGED_BY_VALUE) && label(INSTANCE_LABEL) == Some(id.name())
}

/// The runtime provider for native Kubernetes.
///
/// Cheap to clone: every clone shares one client. It needs the right to `get`, `list`, `watch`,
/// `patch` and `delete` the kinds it owns (StatefulSets, Deployments, Services, NetworkPolicies,
/// PodDisruptionBudgets, ConfigMaps, claims) and to `list` and `watch` pods. **It never touches a
/// Secret**: a Secret is a reference in a pod spec, and the kubelet is who reads it.
#[derive(Clone)]
pub struct KubernetesRuntime {
    client: Client,
    watch_namespace: Option<String>,
}

impl KubernetesRuntime {
    /// A provider on this client. [`watch`](RuntimeProvider::watch) covers every namespace; see
    /// [`watching_namespace`](Self::watching_namespace).
    pub fn new(client: Client) -> Self {
        Self {
            client,
            watch_namespace: None,
        }
    }

    /// A provider on the client the environment gives: the in-cluster configuration, or the
    /// kubeconfig of `KUBECONFIG`.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when no configuration can be inferred.
    pub async fn try_default() -> Result<Self> {
        install_crypto_provider();
        let client = Client::try_default()
            .await
            .map_err(|e| Error::from_kube("the client configuration", e))?;
        Ok(Self::new(client))
    }

    /// Watch one namespace only, for a deployment whose operator has a namespaced `Role`.
    #[must_use]
    pub fn watching_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.watch_namespace = Some(namespace.into());
        self
    }

    fn api<K: Kind>(&self, namespace: &str) -> Api<K> {
        Api::namespaced(self.client.clone(), namespace)
    }

    async fn get_opt<K: Kind>(&self, namespace: &str, name: &str) -> Result<Option<K>> {
        self.api::<K>(namespace)
            .get_opt(name)
            .await
            .map_err(|e| Error::from_kube(what::<K>(name), e))
    }

    /// Server-side apply. `force`: the operator is the one writer of the fields it sets, and it
    /// also moves `replicas` itself (`suspend`), so a field another manager (or its own earlier
    /// patch) holds is taken back instead of stalling the reconcile. What protects objects that
    /// are not ours is the adoption guard, which runs before any apply.
    async fn apply<K: Kind>(&self, namespace: &str, object: &K) -> Result<K> {
        let name = object.meta().name.clone().unwrap_or_default();
        self.api::<K>(namespace)
            .patch(
                &name,
                &PatchParams::apply(FIELD_MANAGER).force(),
                &Patch::Apply(object),
            )
            .await
            .map_err(|e| Error::from_kube(what::<K>(&name), e))
    }

    /// The objects of a kind that are this service's and ours, not those already being deleted.
    async fn list_ours<K: Kind>(&self, id: &RuntimeId) -> Result<Vec<K>> {
        let list = self
            .api::<K>(id.scope())
            .list(&ListParams::default().labels(&names::ours_selector(id)))
            .await
            .map_err(|e| Error::from_kube(what::<K>("(list)"), e))?;
        Ok(list
            .items
            .into_iter()
            .filter(|o| o.meta().deletion_timestamp.is_none())
            .collect())
    }

    /// Delete by name, in the background (the object is gone at once, its pods are collected
    /// after). `false` when there was nothing to delete.
    async fn delete_named<K: Kind>(&self, namespace: &str, name: &str) -> Result<bool> {
        match self
            .api::<K>(namespace)
            .delete(name, &DeleteParams::background())
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                let e = Error::from_kube(what::<K>(name), e);
                if e.is_not_found() { Ok(false) } else { Err(e) }
            }
        }
    }

    /// The adoption guard for one object: an object of this name that is not ours.
    async fn foreign<K: Kind>(&self, id: &RuntimeId, name: &str) -> Result<Option<String>> {
        Ok(self
            .get_opt::<K>(id.scope(), name)
            .await?
            .filter(|o| !is_ours(o.meta(), id))
            .map(|_| what::<K>(name)))
    }

    // ------------------------------------------------------------ ensure

    async fn ensure_objects(&self, id: &RuntimeId, spec: &RuntimeSpec) -> Result<RuntimeStatus> {
        let rendered = render::render(id, spec)?;
        let conflicts = self.conflicts(id, &rendered).await?;
        if !conflicts.is_empty() {
            // Nothing is changed. What ours runs is reported as it is, with the reason it is not
            // being updated.
            let mut status = self.observe(id).await?;
            status.issues.extend(conflicts);
            return Ok(status);
        }
        self.apply_all(id, &rendered).await?;
        self.remove_stale(id, &rendered).await?;
        let status = self.observe(id).await?;
        if matches!(
            status.phase,
            adam_operator_ports::Phase::Ready | adam_operator_ports::Phase::Suspended
        ) {
            // The rollout is done: no pod mounts a superseded file set any more.
            self.remove_stale_config_maps(id, &rendered).await?;
        }
        Ok(status)
    }

    /// Every object the spec needs whose name is held by something that is not ours, as a
    /// `NameConflict` issue each.
    async fn conflicts(&self, id: &RuntimeId, r: &Rendered) -> Result<Vec<Issue>> {
        let first_role = r.workloads.first().map_or(Role::All, |(role, _)| *role);
        let mut taken: Vec<(Role, String)> = Vec::new();
        let mut note = |role: Role, what: Option<String>| {
            if let Some(what) = what {
                taken.push((role, what));
            }
        };
        for c in &r.config_maps {
            let name = c.metadata.name.clone().unwrap_or_default();
            note(first_role, self.foreign::<ConfigMap>(id, &name).await?);
        }
        for c in &r.claims {
            let name = c.metadata.name.clone().unwrap_or_default();
            note(
                first_role,
                self.foreign::<PersistentVolumeClaim>(id, &name).await?,
            );
        }
        note(first_role, self.foreign::<Service>(id, id.name()).await?);
        for (role, w) in &r.workloads {
            let found = match w {
                WorkloadObject::StatefulSet(_) => self.foreign::<StatefulSet>(id, w.name()).await?,
                WorkloadObject::Deployment(_) => self.foreign::<Deployment>(id, w.name()).await?,
            };
            note(*role, found);
        }
        for b in &r.disruption_budgets {
            let name = b.metadata.name.clone().unwrap_or_default();
            note(
                first_role,
                self.foreign::<PodDisruptionBudget>(id, &name).await?,
            );
        }
        if r.network_policy.is_some() {
            note(
                first_role,
                self.foreign::<NetworkPolicy>(id, id.name()).await?,
            );
        }
        Ok(taken
            .into_iter()
            .map(|(role, what)| Issue {
                role,
                reason: IssueReason::NameConflict,
                message: format!(
                    "{what} exists and is not managed by {MANAGED_BY_VALUE} for this service; nothing was changed"
                ),
            })
            .collect())
    }

    async fn apply_all(&self, id: &RuntimeId, r: &Rendered) -> Result<()> {
        let ns = id.scope();
        for o in &r.config_maps {
            self.apply(ns, o).await?;
        }
        for o in &r.claims {
            self.apply(ns, o).await?;
        }
        self.apply(ns, &r.service).await?;
        for (_, w) in &r.workloads {
            match w {
                WorkloadObject::StatefulSet(s) => self.apply(ns, s.as_ref()).await.map(drop)?,
                WorkloadObject::Deployment(d) => self.apply(ns, d.as_ref()).await.map(drop)?,
            }
        }
        for o in &r.disruption_budgets {
            self.apply(ns, o).await?;
        }
        if let Some(o) = &r.network_policy {
            self.apply(ns, o).await?;
        }
        Ok(())
    }

    /// Delete what an earlier spec made and this one does not: a workload that is gone or of the
    /// other kind, a budget, a policy. File sets wait for the rollout.
    async fn remove_stale(&self, id: &RuntimeId, r: &Rendered) -> Result<()> {
        let ns = id.scope();
        let want = |kind: &str| -> BTreeSet<&str> {
            r.workloads
                .iter()
                .filter(|(_, w)| w.kind() == kind)
                .map(|(_, w)| w.name())
                .collect()
        };
        let (want_sts, want_deploy) = (want("StatefulSet"), want("Deployment"));
        for s in self.list_ours::<StatefulSet>(id).await? {
            let name = s.metadata.name.unwrap_or_default();
            if !want_sts.contains(name.as_str()) {
                self.delete_named::<StatefulSet>(ns, &name).await?;
            }
        }
        for d in self.list_ours::<Deployment>(id).await? {
            let name = d.metadata.name.unwrap_or_default();
            if !want_deploy.contains(name.as_str()) {
                self.delete_named::<Deployment>(ns, &name).await?;
            }
        }
        let budgets: BTreeSet<&str> = r
            .disruption_budgets
            .iter()
            .filter_map(|b| b.metadata.name.as_deref())
            .collect();
        for b in self.list_ours::<PodDisruptionBudget>(id).await? {
            let name = b.metadata.name.unwrap_or_default();
            if !budgets.contains(name.as_str()) {
                self.delete_named::<PodDisruptionBudget>(ns, &name).await?;
            }
        }
        if r.network_policy.is_none() {
            for p in self.list_ours::<NetworkPolicy>(id).await? {
                let name = p.metadata.name.unwrap_or_default();
                self.delete_named::<NetworkPolicy>(ns, &name).await?;
            }
        }
        Ok(())
    }

    async fn remove_stale_config_maps(&self, id: &RuntimeId, r: &Rendered) -> Result<()> {
        let wanted: BTreeSet<&str> = r
            .config_maps
            .iter()
            .filter_map(|c| c.metadata.name.as_deref())
            .collect();
        for c in self.list_ours::<ConfigMap>(id).await? {
            let name = c.metadata.name.unwrap_or_default();
            if !wanted.contains(name.as_str()) {
                self.delete_named::<ConfigMap>(id.scope(), &name).await?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ status

    /// The workloads of the runtime that are ours, as observed.
    async fn workloads(&self, id: &RuntimeId) -> Result<Vec<WorkloadObservation>> {
        let sts = self.list_ours::<StatefulSet>(id).await?;
        let deploys = self.list_ours::<Deployment>(id).await?;
        Ok(sts
            .iter()
            .filter_map(status::observe_stateful_set)
            .chain(deploys.iter().filter_map(status::observe_deployment))
            .collect())
    }

    /// The status of what is ours. A runtime with nothing of ours is `Absent`, and says
    /// `NameConflict` when something that is not ours holds the name it would need.
    async fn observe(&self, id: &RuntimeId) -> Result<RuntimeStatus> {
        let workloads = self.workloads(id).await?;
        if workloads.is_empty() {
            let mut status = RuntimeStatus::absent();
            let taken = [
                self.foreign::<StatefulSet>(id, id.name()).await?,
                self.foreign::<Deployment>(id, id.name()).await?,
                self.foreign::<Service>(id, id.name()).await?,
            ];
            for what in taken.into_iter().flatten() {
                status.issues.push(Issue {
                    role: Role::All,
                    reason: IssueReason::NameConflict,
                    message: format!(
                        "{what} exists and is not managed by {MANAGED_BY_VALUE} for this service"
                    ),
                });
            }
            return Ok(status);
        }
        let pods = self.list_ours::<Pod>(id).await?;
        Ok(status::compute(&workloads, &pods))
    }

    // ------------------------------------------------------------ suspend

    async fn scale_to_zero(&self, id: &RuntimeId) -> Result<RuntimeStatus> {
        let ns = id.scope();
        let sts = self.list_ours::<StatefulSet>(id).await?;
        let deploys = self.list_ours::<Deployment>(id).await?;
        if sts.is_empty() && deploys.is_empty() {
            return Err(Error::NotFound {
                what: format!("runtime {id}"),
            });
        }
        // A merge patch of one field: a server-side apply of a partial object would drop every
        // other field this manager owns.
        let patch = Patch::Merge(json!({"spec": {"replicas": 0}}));
        let params = PatchParams {
            field_manager: Some(FIELD_MANAGER.to_owned()),
            ..PatchParams::default()
        };
        for s in sts {
            let name = s.metadata.name.unwrap_or_default();
            self.api::<StatefulSet>(ns)
                .patch(&name, &params, &patch)
                .await
                .map_err(|e| Error::from_kube(what::<StatefulSet>(&name), e))?;
        }
        for d in deploys {
            let name = d.metadata.name.unwrap_or_default();
            self.api::<Deployment>(ns)
                .patch(&name, &params, &patch)
                .await
                .map_err(|e| Error::from_kube(what::<Deployment>(&name), e))?;
        }
        self.observe(id).await
    }

    // ------------------------------------------------------------ delete

    async fn delete_objects(&self, id: &RuntimeId) -> Result<DeleteOutcome> {
        let ns = id.scope();
        let sts = self.list_ours::<StatefulSet>(id).await?;
        let deploys = self.list_ours::<Deployment>(id).await?;
        let claims = self.list_ours::<PersistentVolumeClaim>(id).await?;

        // What the last `ensure` asked for is on the workloads; the shared claims say it too.
        let policy = sts
            .iter()
            .map(|s| &s.metadata)
            .chain(deploys.iter().map(|d| &d.metadata))
            .chain(claims.iter().map(|c| &c.metadata))
            .find_map(|m| m.annotations.as_ref()?.get(DELETION_POLICY_ANNOTATION))
            .map_or(DeletionPolicy::Retain, |p| parse_policy(Some(p)));

        // The volumes that exist, and those a StatefulSet will make for its next replica.
        let mut volumes: BTreeSet<String> = claims
            .iter()
            .filter_map(|c| c.metadata.labels.as_ref()?.get(VOLUME_LABEL).cloned())
            .collect();
        for s in &sts {
            for t in s
                .spec
                .iter()
                .flat_map(|s| s.volume_claim_templates.iter().flatten())
            {
                volumes.extend(t.metadata.name.clone());
            }
        }

        let mut existed = !sts.is_empty() || !deploys.is_empty();
        // Compute first: nothing is left to make a claim again.
        for s in &sts {
            self.delete_named::<StatefulSet>(ns, s.metadata.name.as_deref().unwrap_or_default())
                .await?;
        }
        for d in &deploys {
            self.delete_named::<Deployment>(ns, d.metadata.name.as_deref().unwrap_or_default())
                .await?;
        }
        existed |= self.delete_all::<PodDisruptionBudget>(id).await?;
        existed |= self.delete_all::<NetworkPolicy>(id).await?;
        existed |= self.delete_all::<Service>(id).await?;
        existed |= self.delete_all::<ConfigMap>(id).await?;

        // The claims are listed again: a StatefulSet may have made one since the first list.
        let claims = self.list_ours::<PersistentVolumeClaim>(id).await?;
        match policy {
            DeletionPolicy::Delete => {
                for c in &claims {
                    self.delete_named::<PersistentVolumeClaim>(
                        ns,
                        c.metadata.name.as_deref().unwrap_or_default(),
                    )
                    .await?;
                }
                Ok(DeleteOutcome {
                    existed,
                    retained_volumes: Vec::new(),
                })
            }
            DeletionPolicy::Retain => {
                for c in &claims {
                    self.strip_owners(ns, &c.metadata).await?;
                }
                volumes.extend(
                    claims
                        .iter()
                        .filter_map(|c| c.metadata.labels.as_ref()?.get(VOLUME_LABEL).cloned()),
                );
                Ok(DeleteOutcome {
                    existed,
                    // Nothing was retained if nothing was there.
                    retained_volumes: if existed {
                        volumes.into_iter().collect()
                    } else {
                        Vec::new()
                    },
                })
            }
        }
    }

    /// Delete every object of a kind that is this service's. `true` when there was one.
    async fn delete_all<K: Kind>(&self, id: &RuntimeId) -> Result<bool> {
        let mut any = false;
        for o in self.list_ours::<K>(id).await? {
            let name = o.meta().name.clone().unwrap_or_default();
            any |= self.delete_named::<K>(id.scope(), &name).await?;
        }
        Ok(any)
    }

    /// A kept claim keeps nothing that would let the garbage collector take it: the owner references
    /// of a workload or of the service are removed.
    async fn strip_owners(&self, namespace: &str, meta: &ObjectMeta) -> Result<()> {
        let Some(owners) = meta.owner_references.as_ref().filter(|o| !o.is_empty()) else {
            return Ok(());
        };
        let ours = |kind: &str, api_version: &str| {
            matches!(kind, "StatefulSet" | "Deployment")
                || api_version.split('/').next() == Some(names::OWNER_GROUP)
        };
        let kept: Vec<_> = owners
            .iter()
            .filter(|o| !ours(&o.kind, &o.api_version))
            .collect();
        if kept.len() == owners.len() {
            return Ok(());
        }
        let name = meta.name.clone().unwrap_or_default();
        let params = PatchParams {
            field_manager: Some(FIELD_MANAGER.to_owned()),
            ..PatchParams::default()
        };
        self.api::<PersistentVolumeClaim>(namespace)
            .patch(
                &name,
                &params,
                &Patch::Merge(json!({"metadata": {"ownerReferences": kept}})),
            )
            .await
            .map_err(|e| Error::from_kube(what::<PersistentVolumeClaim>(&name), e))?;
        Ok(())
    }

    // ------------------------------------------------------------ endpoint

    async fn endpoint_of(&self, id: &RuntimeId, surface: Surface) -> Result<Endpoint> {
        let service = self
            .get_opt::<Service>(id.scope(), id.name())
            .await?
            .filter(|s| is_ours(&s.metadata, id) && s.metadata.deletion_timestamp.is_none())
            .ok_or_else(|| Error::NotFound {
                what: format!("runtime {id}"),
            })?;
        let port = service
            .spec
            .and_then(|s| s.ports?.first().map(|p| p.port))
            .ok_or_else(|| Error::NotFound {
                what: format!("the port of runtime {id}"),
            })?;
        let base = format!("http://{}.{}.svc:{port}/", id.name(), id.scope());
        Ok(Endpoint {
            url: match surface {
                Surface::A2a => base,
                Surface::AgentCard => format!("{base}.well-known/agent-card.json"),
            },
        })
    }

    // ------------------------------------------------------------ audit

    /// Every plain-text value of the objects this provider applied for `id`, as the conformance
    /// suite reads them: names, labels, annotations, literal variables, commands, file contents.
    /// **A secret reference is how a secret travels and is not plain text**: `secretKeyRef` and
    /// the `secret` of a volume are left out, and so is `managedFields`.
    ///
    /// # Errors
    ///
    /// Any failure of the API server.
    pub async fn plain_text(&self, id: &RuntimeId) -> Result<Vec<String>> {
        let mut docs: Vec<Value> = Vec::new();
        fn push<K: Kind>(docs: &mut Vec<Value>, items: Vec<K>) {
            docs.extend(items.iter().filter_map(|o| serde_json::to_value(o).ok()));
        }
        push(&mut docs, self.list_ours::<ConfigMap>(id).await?);
        push(&mut docs, self.list_ours::<Service>(id).await?);
        push(&mut docs, self.list_ours::<StatefulSet>(id).await?);
        push(&mut docs, self.list_ours::<Deployment>(id).await?);
        push(&mut docs, self.list_ours::<NetworkPolicy>(id).await?);
        push(&mut docs, self.list_ours::<PodDisruptionBudget>(id).await?);
        push(
            &mut docs,
            self.list_ours::<PersistentVolumeClaim>(id).await?,
        );
        let mut out = Vec::new();
        for mut doc in docs {
            strip_references(&mut doc);
            collect_strings(&doc, &mut out);
        }
        Ok(out)
    }
}

/// Remove what is a reference to a Secret and not a value: `secretKeyRef`, the `secret` source of a
/// volume, and the server's bookkeeping.
fn strip_references(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.remove("secretKeyRef");
            map.remove("managedFields");
            // A volume's source is an object that names the volume next to it.
            if map.contains_key("name") {
                map.remove("secret");
            }
            for child in map.values_mut() {
                strip_references(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_references),
        _ => {}
    }
}

fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|i| collect_strings(i, out)),
        Value::Object(map) => {
            for (k, child) in map {
                out.push(k.clone());
                collect_strings(child, out);
            }
        }
        _ => {}
    }
}

impl RuntimeProvider for KubernetesRuntime {
    fn name(&self) -> &'static str {
        "kubernetes"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { suspend: true }
    }

    async fn ensure(
        &self,
        id: &RuntimeId,
        spec: &RuntimeSpec,
    ) -> Result<RuntimeStatus, RuntimeError> {
        spec.check()?;
        self.ensure_objects(id, spec)
            .await
            .map_err(|e| e.into_runtime(id))
    }

    async fn suspend(&self, id: &RuntimeId) -> Result<RuntimeStatus, RuntimeError> {
        self.scale_to_zero(id).await.map_err(|e| e.into_runtime(id))
    }

    async fn delete(&self, id: &RuntimeId) -> Result<DeleteOutcome, RuntimeError> {
        self.delete_objects(id)
            .await
            .map_err(|e| e.into_runtime(id))
    }

    async fn status(&self, id: &RuntimeId) -> Result<RuntimeStatus, RuntimeError> {
        self.observe(id).await.map_err(|e| e.into_runtime(id))
    }

    async fn endpoint(&self, id: &RuntimeId, surface: Surface) -> Result<Endpoint, RuntimeError> {
        self.endpoint_of(id, surface)
            .await
            .map_err(|e| e.into_runtime(id))
    }

    fn watch(&self) -> BoxStream<'static, RuntimeId> {
        crate::watch::changes(self.client.clone(), self.watch_namespace.clone())
    }
}

#[cfg(feature = "testkit")]
impl adam_operator_ports::testkit::RuntimeUnderTest for KubernetesRuntime {
    #[allow(clippy::panic)]
    async fn materialised(&self, id: &RuntimeId) -> Vec<String> {
        self.plain_text(id)
            .await
            .unwrap_or_else(|e| panic!("reading the objects of {id}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn references_to_secrets_are_not_plain_text() {
        let mut doc = json!({
            "metadata": {"name": "w", "managedFields": [{"manager": "x"}]},
            "spec": {"template": {"spec": {
                "containers": [{"name": "agent", "env": [
                    {"name": "K", "value": "literal"},
                    {"name": "S", "valueFrom": {"secretKeyRef": {"name": "the-secret", "key": "k"}}}
                ]}],
                "volumes": [
                    {"name": "key", "secret": {"secretName": "the-key"}},
                    {"name": "cm", "configMap": {"name": "files"}}
                ]
            }}}
        });
        strip_references(&mut doc);
        let mut text = Vec::new();
        collect_strings(&doc, &mut text);
        assert!(text.contains(&"literal".to_owned()));
        assert!(text.contains(&"files".to_owned()));
        for secret in ["the-secret", "the-key", "manager"] {
            assert!(!text.iter().any(|t| t == secret), "{secret}");
        }
    }

    #[test]
    fn an_object_is_ours_only_with_both_labels() {
        let id = RuntimeId::new("ns", "svc");
        let meta = |labels: &[(&str, &str)]| ObjectMeta {
            labels: Some(
                labels
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            ),
            ..ObjectMeta::default()
        };
        assert!(is_ours(
            &meta(&[(MANAGED_BY_LABEL, "adam-operator"), (INSTANCE_LABEL, "svc")]),
            &id
        ));
        assert!(!is_ours(
            &meta(&[(MANAGED_BY_LABEL, "Helm"), (INSTANCE_LABEL, "svc")]),
            &id
        ));
        assert!(!is_ours(
            &meta(&[
                (MANAGED_BY_LABEL, "adam-operator"),
                (INSTANCE_LABEL, "other")
            ]),
            &id
        ));
        assert!(!is_ours(&meta(&[(MANAGED_BY_LABEL, "adam-operator")]), &id));
        assert!(!is_ours(&ObjectMeta::default(), &id));
    }
}
