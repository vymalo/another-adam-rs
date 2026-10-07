//! In-memory implementations of the three traits. They pass their own conformance suites
//! (`tests/memory.rs`), serve the controller's unit tests, and are the reference a real provider is
//! read against. Cheap to clone: every clone is a handle on the same state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::broadcast;

use crate::{
    AgentDirectory, Capabilities, DeleteOutcome, DirectoryEntry, DirectoryError, Endpoint,
    EnvValue, Issue, IssueReason, Phase, ReleaseOutcome, RuntimeError, RuntimeId, RuntimeProvider,
    RuntimeSpec, RuntimeStatus, StoreCapabilities, StoreError, StoreId, StoreKind,
    StoreProvisioner, StoreSpec, StoreState, StoreStatus, Surface, VolumeSource, cnpg_connection,
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Every plain-text value a provider would write to its backend for `spec`: literal variables,
/// commands, arguments, file contents and names. **A secret reference is not plain text**: it is how
/// a secret travels, so its name and key are not in the result.
pub fn plain_text(spec: &RuntimeSpec) -> Vec<String> {
    let mut out = Vec::new();
    for w in &spec.workloads {
        out.push(w.name.clone());
        for c in std::iter::once(&w.container).chain(&w.sidecars) {
            out.push(c.image.clone());
            out.extend(c.command.iter().cloned());
            out.extend(c.args.iter().cloned());
            for e in &c.env {
                if let EnvValue::Literal(v) = &e.value {
                    out.push(v.clone());
                }
            }
            for m in &c.mounts {
                out.push(m.path.clone());
            }
        }
        for v in &w.volumes {
            out.push(v.name.clone());
            if let VolumeSource::SecretFile { file, .. } = &v.source {
                out.push(file.clone());
            }
        }
    }
    for f in &spec.file_sets {
        out.push(f.name.clone());
        for (path, content) in &f.files {
            out.push(path.clone());
            out.push(content.clone());
        }
    }
    out
}

fn persistent_volumes(spec: &RuntimeSpec) -> Vec<String> {
    let mut names = BTreeSet::new();
    for w in &spec.workloads {
        for v in &w.volumes {
            if matches!(v.source, VolumeSource::Persistent(_)) {
                names.insert(v.name.clone());
            }
        }
    }
    names.into_iter().collect()
}

// ---------------------------------------------------------------- runtime

struct RuntimeEntry {
    spec: RuntimeSpec,
    suspended: bool,
    forced: Option<(Phase, Vec<Issue>)>,
}

struct RuntimeState {
    runtimes: BTreeMap<RuntimeId, RuntimeEntry>,
    foreign: BTreeSet<RuntimeId>,
    unavailable: bool,
    can_suspend: bool,
}

struct RuntimeInner {
    state: Mutex<RuntimeState>,
    changes: broadcast::Sender<RuntimeId>,
}

/// A [`RuntimeProvider`] that keeps the specs it is given. It is instantly ready.
#[derive(Clone)]
pub struct MemoryRuntime {
    inner: Arc<RuntimeInner>,
}

impl Default for MemoryRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryRuntime {
    /// An empty provider that can suspend.
    pub fn new() -> Self {
        let (changes, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(RuntimeInner {
                state: Mutex::new(RuntimeState {
                    runtimes: BTreeMap::new(),
                    foreign: BTreeSet::new(),
                    unavailable: false,
                    can_suspend: true,
                }),
                changes,
            }),
        }
    }

    /// A provider without the `suspend` capability.
    pub fn without_suspend(self) -> Self {
        lock(&self.inner.state).can_suspend = false;
        self
    }

    /// Every call fails with `Unavailable` while this is on.
    pub fn set_unavailable(&self, on: bool) {
        lock(&self.inner.state).unavailable = on;
    }

    /// An object that is not ours holds the name of `id`: `ensure` changes nothing and reports
    /// `NameConflict` (the adoption guard).
    pub fn set_foreign(&self, id: &RuntimeId, foreign: bool) {
        let mut s = lock(&self.inner.state);
        if foreign {
            s.foreign.insert(id.clone());
        } else {
            s.foreign.remove(id);
        }
    }

    /// Report this phase and these issues for `id` instead of the computed ones, as a backend
    /// does when a pod crash-loops. `None` goes back to computing.
    pub fn force_status(&self, id: &RuntimeId, forced: Option<(Phase, Vec<Issue>)>) {
        if let Some(e) = lock(&self.inner.state).runtimes.get_mut(id) {
            e.forced = forced;
        }
        let _ = self.inner.changes.send(id.clone());
    }

    /// The spec of the last `ensure` of `id`.
    pub fn spec(&self, id: &RuntimeId) -> Option<RuntimeSpec> {
        lock(&self.inner.state)
            .runtimes
            .get(id)
            .map(|e| e.spec.clone())
    }

    /// The ids that exist.
    pub fn ids(&self) -> Vec<RuntimeId> {
        lock(&self.inner.state).runtimes.keys().cloned().collect()
    }

    fn status_of(s: &RuntimeState, id: &RuntimeId) -> RuntimeStatus {
        let Some(e) = s.runtimes.get(id) else {
            return RuntimeStatus::absent();
        };
        if let Some((phase, issues)) = &e.forced {
            return RuntimeStatus {
                phase: *phase,
                replicas: 0,
                issues: issues.clone(),
            };
        }
        if e.suspended {
            return RuntimeStatus {
                phase: Phase::Suspended,
                replicas: 0,
                issues: Vec::new(),
            };
        }
        RuntimeStatus {
            phase: Phase::Ready,
            replicas: e.spec.worker_replicas(),
            issues: Vec::new(),
        }
    }

    fn up(s: &RuntimeState) -> Result<(), RuntimeError> {
        if s.unavailable {
            return Err(RuntimeError::Unavailable(
                "the in-memory runtime is switched off".into(),
            ));
        }
        Ok(())
    }

    fn notify(&self, id: &RuntimeId) {
        // No subscriber is not a failure: a signal nobody waits for is dropped.
        let _ = self.inner.changes.send(id.clone());
    }
}

impl RuntimeProvider for MemoryRuntime {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            suspend: lock(&self.inner.state).can_suspend,
        }
    }

    async fn ensure(
        &self,
        id: &RuntimeId,
        spec: &RuntimeSpec,
    ) -> Result<RuntimeStatus, RuntimeError> {
        spec.check()?;
        let status = {
            let mut s = lock(&self.inner.state);
            Self::up(&s)?;
            if spec.suspend && !s.can_suspend {
                return Err(RuntimeError::Unsupported("suspend"));
            }
            if s.foreign.contains(id) {
                let mut status = Self::status_of(&s, id);
                status.issues.push(Issue {
                    role: spec.workloads.first().map_or(crate::Role::All, |w| w.role),
                    reason: IssueReason::NameConflict,
                    message: format!(
                        "an object named {} exists and is not managed by the operator",
                        id.name()
                    ),
                });
                return Ok(status);
            }
            s.runtimes.insert(
                id.clone(),
                RuntimeEntry {
                    spec: spec.clone(),
                    suspended: spec.suspend,
                    forced: None,
                },
            );
            Self::status_of(&s, id)
        };
        self.notify(id);
        Ok(status)
    }

    async fn suspend(&self, id: &RuntimeId) -> Result<RuntimeStatus, RuntimeError> {
        let status = {
            let mut s = lock(&self.inner.state);
            Self::up(&s)?;
            if !s.can_suspend {
                return Err(RuntimeError::Unsupported("suspend"));
            }
            let Some(e) = s.runtimes.get_mut(id) else {
                return Err(RuntimeError::NotFound(id.clone()));
            };
            e.suspended = true;
            Self::status_of(&s, id)
        };
        self.notify(id);
        Ok(status)
    }

    async fn delete(&self, id: &RuntimeId) -> Result<DeleteOutcome, RuntimeError> {
        let outcome = {
            let mut s = lock(&self.inner.state);
            Self::up(&s)?;
            match s.runtimes.remove(id) {
                None => DeleteOutcome::default(),
                Some(e) => DeleteOutcome {
                    existed: true,
                    retained_volumes: match e.spec.deletion {
                        crate::DeletionPolicy::Retain => persistent_volumes(&e.spec),
                        crate::DeletionPolicy::Delete => Vec::new(),
                    },
                },
            }
        };
        if outcome.existed {
            self.notify(id);
        }
        Ok(outcome)
    }

    async fn status(&self, id: &RuntimeId) -> Result<RuntimeStatus, RuntimeError> {
        let s = lock(&self.inner.state);
        Self::up(&s)?;
        Ok(Self::status_of(&s, id))
    }

    async fn endpoint(&self, id: &RuntimeId, surface: Surface) -> Result<Endpoint, RuntimeError> {
        let s = lock(&self.inner.state);
        Self::up(&s)?;
        let Some(e) = s.runtimes.get(id) else {
            return Err(RuntimeError::NotFound(id.clone()));
        };
        let base = format!(
            "http://{}.{}.svc:{}/",
            id.name(),
            id.scope(),
            e.spec.network.port
        );
        Ok(Endpoint {
            url: match surface {
                Surface::A2a => base,
                Surface::AgentCard => format!("{base}.well-known/agent-card.json"),
            },
        })
    }

    fn watch(&self) -> BoxStream<'static, RuntimeId> {
        let rx = self.inner.changes.subscribe();
        futures::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(id) => return Some((id, rx)),
                    // Missed signals are fine: a signal is never the truth.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
        .boxed()
    }
}

impl crate::testkit::RuntimeUnderTest for MemoryRuntime {
    async fn materialised(&self, id: &RuntimeId) -> Vec<String> {
        lock(&self.inner.state)
            .runtimes
            .get(id)
            .map(|e| plain_text(&e.spec))
            .unwrap_or_default()
    }
}

// ------------------------------------------------------------------ store

struct StoreInner {
    stores: BTreeMap<StoreId, StoreSpec>,
    can_cnpg: bool,
    cnpg_installed: bool,
    cluster_ready: bool,
}

/// A [`StoreProvisioner`] that keeps the specs it is given. A cluster is ready at once.
#[derive(Clone)]
pub struct MemoryStore {
    state: Arc<Mutex<StoreInner>>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    /// An empty provisioner that serves both kinds.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(StoreInner {
                stores: BTreeMap::new(),
                can_cnpg: true,
                cnpg_installed: true,
                cluster_ready: true,
            })),
        }
    }

    /// A provisioner that serves the Secret kind only.
    pub fn without_cnpg(self) -> Self {
        lock(&self.state).can_cnpg = false;
        self
    }

    /// Whether the CloudNativePG API is there: without it `ensure` of a cluster is `NotInstalled`.
    pub fn set_cnpg_installed(&self, installed: bool) {
        lock(&self.state).cnpg_installed = installed;
    }

    /// Whether a cluster reports ready.
    pub fn set_cluster_ready(&self, ready: bool) {
        lock(&self.state).cluster_ready = ready;
    }

    /// The spec of the last `ensure` of `id`.
    pub fn spec(&self, id: &StoreId) -> Option<StoreSpec> {
        lock(&self.state).stores.get(id).cloned()
    }
}

impl StoreProvisioner for MemoryStore {
    fn capabilities(&self) -> StoreCapabilities {
        StoreCapabilities {
            cnpg: lock(&self.state).can_cnpg,
        }
    }

    async fn ensure(&self, id: &StoreId, spec: &StoreSpec) -> Result<StoreStatus, StoreError> {
        spec.check()?;
        let mut s = lock(&self.state);
        let status = match &spec.kind {
            StoreKind::Secret(secret) => StoreStatus {
                state: StoreState::SecretReferenced,
                connection: secret.clone(),
            },
            StoreKind::Cnpg(_) => {
                if !s.can_cnpg {
                    return Err(StoreError::Unsupported("a CloudNativePG cluster"));
                }
                if !s.cnpg_installed {
                    return Err(StoreError::NotInstalled {
                        what: "CloudNativePG",
                    });
                }
                StoreStatus {
                    state: if s.cluster_ready {
                        StoreState::ClusterReady
                    } else {
                        StoreState::ClusterNotReady
                    },
                    connection: cnpg_connection(id.name()),
                }
            }
        };
        s.stores.insert(id.clone(), spec.clone());
        Ok(status)
    }

    async fn release(&self, id: &StoreId) -> Result<ReleaseOutcome, StoreError> {
        let mut s = lock(&self.state);
        Ok(match s.stores.remove(id) {
            None => ReleaseOutcome::default(),
            Some(spec) => ReleaseOutcome {
                existed: true,
                // Only an operator-owned cluster holds data of ours.
                retained: matches!(spec.kind, StoreKind::Cnpg(_))
                    && spec.deletion == crate::DeletionPolicy::Retain,
            },
        })
    }
}

impl crate::testkit::StoreUnderTest for MemoryStore {
    async fn materialised(&self, id: &StoreId) -> Vec<String> {
        // A store writes references and sizes; a cluster's name is the only plain text.
        lock(&self.state)
            .stores
            .get(id)
            .map(|spec| match &spec.kind {
                StoreKind::Secret(_) => Vec::new(),
                StoreKind::Cnpg(c) => {
                    let mut v = vec![format!("{}-db", id.name()), c.size.clone()];
                    v.extend(c.storage_class.clone());
                    v
                }
            })
            .unwrap_or_default()
    }
}

// -------------------------------------------------------------- directory

struct DirectoryInner {
    entries: BTreeMap<(String, String), DirectoryEntry>,
    ready: bool,
}

/// An [`AgentDirectory`] over entries it is given.
#[derive(Clone)]
pub struct MemoryDirectory {
    inner: Arc<Mutex<DirectoryInner>>,
}

impl Default for MemoryDirectory {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryDirectory {
    /// An empty, ready directory.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(DirectoryInner {
                entries: BTreeMap::new(),
                ready: true,
            })),
        }
    }

    /// Whether the directory has synced: before it has, reads fail with `NotReady`.
    pub fn set_ready(&self, ready: bool) {
        lock(&self.inner).ready = ready;
    }

    /// Add or replace an entry.
    pub fn put(&self, entry: DirectoryEntry) {
        lock(&self.inner)
            .entries
            .insert((entry.scope.clone(), entry.name.clone()), entry);
    }

    /// Remove an entry.
    pub fn remove(&self, scope: &str, name: &str) {
        lock(&self.inner)
            .entries
            .remove(&(scope.to_owned(), name.to_owned()));
    }
}

impl AgentDirectory for MemoryDirectory {
    async fn list(&self) -> Result<Vec<DirectoryEntry>, DirectoryError> {
        let s = lock(&self.inner);
        if !s.ready {
            return Err(DirectoryError::NotReady);
        }
        Ok(s.entries.values().cloned().collect())
    }

    async fn get(&self, scope: &str, name: &str) -> Result<Option<DirectoryEntry>, DirectoryError> {
        let s = lock(&self.inner);
        if !s.ready {
            return Err(DirectoryError::NotReady);
        }
        Ok(s.entries.get(&(scope.to_owned(), name.to_owned())).cloned())
    }
}

impl crate::testkit::DirectoryUnderTest for MemoryDirectory {
    async fn put(&self, entry: DirectoryEntry) {
        Self::put(self, entry);
    }

    async fn remove(&self, scope: &str, name: &str) {
        Self::remove(self, scope, name);
    }
}
