//! `AgentDirectory`: the agents the operator runs, as the registry lists them.

use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::DirectoryError;

/// One agent service, as far as a registry cares.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    /// The scope of the service (its namespace).
    pub scope: String,
    /// The service's name: the `service` of a registry item.
    pub name: String,
    /// Display title (`spec.registry.title`).
    pub title: Option<String>,
    /// What the agent does, in a sentence (`spec.description`).
    pub description: Option<String>,
    /// Free-form tags (`spec.registry.tags`).
    pub tags: Vec<String>,
    /// The agent card's URL (`status.endpoints.agentCard`). `None` until the runtime has one.
    pub agent_card: Option<String>,
    /// The service serves A2A (`spec.interfaces.a2a.enabled`).
    pub a2a_enabled: bool,
    /// The service is `Blocked`: the operator did not apply the desired state.
    pub blocked: bool,
}

impl DirectoryEntry {
    /// Whether a registry lists it: A2A is enabled, the service is not blocked, and there is a card
    /// to point at. The reason it is not is the `Listed` condition's.
    pub fn listed(&self) -> bool {
        self.a2a_enabled && !self.blocked && self.agent_card.is_some()
    }
}

/// Reads the agents the operator runs. A reflector's cache backs the v0 implementation, so a read
/// does not touch the API server.
pub trait AgentDirectory: Send + Sync {
    /// Every service, listed or not, ordered by scope and then name, so a document built from it
    /// is the same until something changes (its `ETag` depends on that).
    ///
    /// # Errors
    ///
    /// [`DirectoryError::NotReady`] before the first sync: an answer built from nothing would be
    /// an empty list that looks true.
    fn list(&self) -> impl Future<Output = Result<Vec<DirectoryEntry>, DirectoryError>> + Send;

    /// One service.
    ///
    /// # Errors
    ///
    /// As [`list`](Self::list).
    fn get(
        &self,
        scope: &str,
        name: &str,
    ) -> impl Future<Output = Result<Option<DirectoryEntry>, DirectoryError>> + Send;
}
