//! The L1 key: deployment, caller, agent and kind. No L1 entry crosses a caller.

use serde::{Deserialize, Serialize};

use crate::{MemoryError, MemoryResult};

/// The kind of an L1 scope: the agent itself, or one repository it works in.
///
/// A closed set on purpose: a new kind must fail to compile in every host and adapter.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The agent's own memory, served whatever repository a run prepared.
    Agent,
    /// One repository's memory, served only to a run that prepared it.
    Repository(Repository),
}

impl Kind {
    fn key(&self) -> String {
        match self {
            Self::Agent => "agent".to_owned(),
            Self::Repository(repository) => repository.key(),
        }
    }
}

/// A repository an L1 scope is keyed by: `repository:<host>/<owner>/<name>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Repository {
    /// The host, such as `github.com`.
    pub host: String,
    /// The owner or organisation.
    pub owner: String,
    /// The repository's name.
    pub name: String,
}

impl Repository {
    /// A repository.
    #[must_use]
    pub fn new(host: impl Into<String>, owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
        }
    }

    fn key(&self) -> String {
        format!("repository:{}/{}/{}", self.host, self.owner, self.name)
    }
}

/// What an L1 entry is keyed by: the deployment, the caller, the agent and the kind.
///
/// A caller is the bearer index of the request that started the run (ADR 0072), not a person: no
/// L1 entry crosses a caller. The key does not name a worker, so two workers that mount one folder
/// share one L1, and two workers with two folders have two.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Scope {
    deployment: String,
    caller: String,
    agent: String,
    kind: Kind,
}

impl Scope {
    /// An agent-scoped key.
    #[must_use]
    pub fn agent(
        deployment: impl Into<String>,
        caller: impl Into<String>,
        agent: impl Into<String>,
    ) -> Self {
        Self {
            deployment: deployment.into(),
            caller: caller.into(),
            agent: agent.into(),
            kind: Kind::Agent,
        }
    }

    /// A repository-scoped key.
    #[must_use]
    pub fn repository(
        deployment: impl Into<String>,
        caller: impl Into<String>,
        agent: impl Into<String>,
        repository: Repository,
    ) -> Self {
        Self {
            deployment: deployment.into(),
            caller: caller.into(),
            agent: agent.into(),
            kind: Kind::Repository(repository),
        }
    }

    /// The deployment.
    #[must_use]
    pub fn deployment(&self) -> &str {
        &self.deployment
    }

    /// The caller.
    #[must_use]
    pub fn caller(&self) -> &str {
        &self.caller
    }

    /// The agent.
    #[must_use]
    pub fn agent_name(&self) -> &str {
        &self.agent
    }

    /// The kind.
    #[must_use]
    pub fn kind(&self) -> &Kind {
        &self.kind
    }

    /// The canonical key an adapter stores under. Components are separated by a unit separator, so
    /// no component can forge another's boundary.
    pub(crate) fn key(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.deployment,
            self.caller,
            self.agent,
            self.kind.key()
        )
    }

    /// A human-readable stem for a file or directory name, before its hash.
    pub(crate) fn slug(&self) -> String {
        let text = match &self.kind {
            Kind::Agent => self.agent.clone(),
            Kind::Repository(repository) => format!(
                "{}-{}-{}",
                repository.host, repository.owner, repository.name
            ),
        };
        crate::slug::slug(&text, 40)
    }

    pub(crate) fn validate(&self) -> MemoryResult<()> {
        for (field, value) in [
            ("deployment", &self.deployment),
            ("caller", &self.caller),
            ("agent", &self.agent),
        ] {
            if value.trim().is_empty() {
                return Err(MemoryError::EmptyScope { field });
            }
        }
        if let Kind::Repository(repository) = &self.kind {
            for (field, value) in [
                ("repository host", &repository.host),
                ("repository owner", &repository.owner),
                ("repository name", &repository.name),
            ] {
                if value.trim().is_empty() {
                    return Err(MemoryError::EmptyScope { field });
                }
            }
        }
        Ok(())
    }
}
