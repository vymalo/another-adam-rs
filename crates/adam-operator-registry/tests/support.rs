//! Shared by the test files: directory entries, and the consumer's reader of the document.
#![allow(dead_code)]

#[path = "vendored/consumer_linkset.rs"]
pub mod consumer;

use adam_operator_ports::DirectoryEntry;

/// A service that is listed: A2A on, not blocked, with a card URL in its namespace.
pub fn entry(scope: &str, name: &str) -> DirectoryEntry {
    DirectoryEntry {
        scope: scope.to_owned(),
        name: name.to_owned(),
        title: None,
        description: None,
        tags: Vec::new(),
        agent_card: Some(format!(
            "http://{name}.{scope}.svc:8080/.well-known/agent-card.json"
        )),
        a2a_enabled: true,
        blocked: false,
    }
}

pub fn titled(scope: &str, name: &str, title: &str, tags: &[&str]) -> DirectoryEntry {
    DirectoryEntry {
        title: Some(title.to_owned()),
        tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        ..entry(scope, name)
    }
}
