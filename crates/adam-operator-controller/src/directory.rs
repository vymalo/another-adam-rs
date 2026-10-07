//! An `AgentDirectory` over the reflector of the `AgentService` controller.
//!
//! The controller "feeds" the directory by doing what it already does: it patches each service's
//! status, the watch brings the change into the reflector's cache, and the directory reads the cache.
//! A read never touches the API server.

use adam_operator_api::{AgentService, ServiceState};
use adam_operator_ports::{AgentDirectory, DirectoryEntry, DirectoryError};
use futures::FutureExt;
use kube::ResourceExt;
use kube::runtime::reflector::Store;

/// The agents of the cluster, as the controller's cache has them.
#[derive(Clone)]
pub struct ReflectorDirectory {
    store: Store<AgentService>,
}

impl ReflectorDirectory {
    /// A directory over the reader of the `AgentService` controller
    /// ([`Operator::directory`](crate::Operator::directory) makes one).
    pub fn new(store: Store<AgentService>) -> Self {
        Self { store }
    }

    /// The reflector has listed the cluster at least once.
    pub fn is_synced(&self) -> bool {
        matches!(self.store.wait_until_ready().now_or_never(), Some(Ok(())))
    }
}

/// One service as a directory entry. `blocked` is true until the controller has reconciled it:
/// a registry must not list what the operator has not applied.
fn entry(svc: &AgentService) -> DirectoryEntry {
    let status = svc.status.as_ref();
    DirectoryEntry {
        scope: svc.namespace().unwrap_or_default(),
        name: svc.name_any(),
        title: svc.spec.registry.title.clone(),
        description: svc.spec.description.clone(),
        tags: svc.spec.registry.tags.clone(),
        agent_card: status
            .and_then(|s| s.endpoints.as_ref())
            .and_then(|e| e.agent_card.clone()),
        a2a_enabled: svc.spec.interfaces.a2a.enabled,
        blocked: !matches!(
            status.and_then(|s| s.state),
            Some(ServiceState::Ready | ServiceState::Degraded | ServiceState::Suspended)
        ),
    }
}

impl AgentDirectory for ReflectorDirectory {
    async fn list(&self) -> Result<Vec<DirectoryEntry>, DirectoryError> {
        if !self.is_synced() {
            return Err(DirectoryError::NotReady);
        }
        let mut entries: Vec<_> = self
            .store
            .state()
            .iter()
            .filter(|s| s.metadata.deletion_timestamp.is_none())
            .map(|s| entry(s))
            .collect();
        entries.sort_by(|a, b| (&a.scope, &a.name).cmp(&(&b.scope, &b.name)));
        Ok(entries)
    }

    async fn get(&self, scope: &str, name: &str) -> Result<Option<DirectoryEntry>, DirectoryError> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .find(|e| e.scope == scope && e.name == name))
    }
}
