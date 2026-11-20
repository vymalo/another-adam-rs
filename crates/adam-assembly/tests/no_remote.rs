//! A build without the feature `remote` (ADR 0047 decision 2): no A2A client, and a subagent file
//! with `a2a:` is refused at bind, before any secret is read.
#![cfg(not(feature = "remote"))]
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use adam_assembly::Error;
use adam_core::{SecretError, SecretSource};
use adam_error::{Classify, ErrorClass};
use adam_llm_agent::ToolSet;
use secrecy::SecretString;

use common::{def, instructions, tools};

/// A source that counts the names it is asked for and gives none.
#[derive(Default)]
struct Counting(AtomicUsize);

impl SecretSource for Counting {
    fn secret(&self, _name: &str) -> Result<SecretString, SecretError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(SecretError::Refused)
    }
}

fn root() -> String {
    instructions("name: coder\ntools: []", "You are the root.")
}

const BILLING: &str = "---\ndescription: Handles billing questions.\n\
                       a2a: https://billing.example.com/card\nauth: bearer:T\n---\n";

#[test]
fn an_a2a_subagent_is_refused_at_bind_before_any_secret_is_read() {
    let root = root();
    let asked = Arc::new(Counting::default());
    let error = def(&[
        ("agent/instructions.md", root.as_str()),
        ("agent/subagents/billing.md", BILLING),
    ])
    .secrets(asked.clone())
    .bind(ToolSet::new())
    .unwrap_err();
    let Error::RemoteNotEnabled { origin } = &error else {
        panic!("wrong variant: {error}");
    };
    assert_eq!(origin.agent, "coder/billing");
    assert_eq!(origin.file.to_string_lossy(), "agent/subagents/billing.md");
    let text = error.to_string();
    assert!(text.contains("the feature `remote`"), "{text}");
    assert_eq!(error.class(), ErrorClass::Invalid);
    assert_eq!(asked.0.load(Ordering::SeqCst), 0, "no secret is asked for");

    // `NoSecrets` and a value given in code change nothing: the file is refused for what it is.
    let error = def(&[
        ("agent/instructions.md", root.as_str()),
        ("agent/subagents/billing.md", BILLING),
    ])
    .secrets(Arc::new(adam_core::NoSecrets))
    .env("T", "token")
    .bind(ToolSet::new())
    .unwrap_err();
    assert!(matches!(error, Error::RemoteNotEnabled { .. }), "{error}");
}

#[test]
fn a_name_clash_is_still_refused_as_a_clash_first() {
    // The refusal comes after the name checks: the file's real mistake is reported, as it is in a
    // build with the feature. A root with no `tools:` has every registered tool, `billing` among them.
    let root = instructions("name: coder", "You are the root.");
    let asked = Arc::new(Counting::default());
    let error = def(&[
        ("agent/instructions.md", root.as_str()),
        ("agent/subagents/billing.md", BILLING),
    ])
    .secrets(asked.clone())
    .bind(tools(&["billing"]))
    .unwrap_err();
    assert!(
        matches!(&error, Error::SubagentToolClash { tool, .. } if tool == "billing"),
        "{error}"
    );
    assert_eq!(asked.0.load(Ordering::SeqCst), 0, "no secret is asked for");
}

#[test]
fn the_same_root_without_the_file_binds() {
    let root = root();
    def(&[("agent/instructions.md", root.as_str())])
        .secrets(Arc::new(adam_core::NoSecrets))
        .bind(ToolSet::new())
        .unwrap();
}
