//! `plugins:` of an agent file (ADR 0050, slice H6a): a name the deployment registered, inherited
//! by subagents; a name nobody registered is a bind error, which every host reports as an invalid
//! configuration (exit 78).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_assembly::Error;
use adam_error::{Classify, ErrorClass};
use adam_model::MockModel;
use adam_plugin_testkit::Allows;
use common::{def, instructions, tools};

const INSTRUCTIONS: &str = "agent/instructions.md";

#[test]
fn a_plugin_nobody_registered_stops_the_agent_with_a_suggestion() {
    let error = def(&[(
        INSTRUCTIONS,
        &instructions("name: coder\nplugins: [audt]", "Hi."),
    )])
    .plugin("audit", Arc::new(Allows::default()))
    .bind(tools(&[]))
    .unwrap_err();
    let Error::UnknownPlugin {
        plugin, suggestion, ..
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(
        (plugin.as_str(), suggestion.as_deref()),
        ("audt", Some("audit"))
    );
    // An invalid configuration: the hosts exit 78 on this class.
    assert_eq!(error.class(), ErrorClass::Invalid);
    // No plugin registered at all is the same refusal.
    let error = def(&[(
        INSTRUCTIONS,
        &instructions("name: coder\nplugins: [audit]", "Hi."),
    )])
    .bind(tools(&[]))
    .unwrap_err();
    assert!(matches!(error, Error::UnknownPlugin { .. }), "{error}");
}

#[test]
fn a_subagent_runs_its_parents_plugins_and_its_own() {
    let assembly = def(&[
        (
            INSTRUCTIONS,
            &instructions("name: coder\nplugins: [audit]", "Root."),
        ),
        (
            "agent/subagents/child.md",
            &instructions("description: Child.\nplugins: [snapshot]", "Child."),
        ),
        (
            "agent/subagents/plain.md",
            &instructions("description: Plain.", "Plain."),
        ),
    ])
    .plugin("audit", Arc::new(Allows::default()))
    .plugin("snapshot", Arc::new(Allows::default()))
    .bind(tools(&[]))
    .unwrap()
    .model(Arc::new(MockModel::new()), "m")
    .unwrap();
    let plugins_of = |name: &str| {
        assembly
            .info()
            .iter()
            .find(|i| i.name == name)
            .unwrap()
            .plugins
            .clone()
    };
    assert_eq!(plugins_of("coder"), ["audit"]);
    assert_eq!(plugins_of("coder/child"), ["audit", "snapshot"]);
    assert_eq!(plugins_of("coder/plain"), ["audit"]);
}

#[test]
fn an_agent_with_no_plugins_key_runs_none() {
    let assembly = def(&[(INSTRUCTIONS, &instructions("name: coder", "Hi."))])
        .plugin("audit", Arc::new(Allows::default()))
        .bind(tools(&[]))
        .unwrap()
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    assert!(assembly.info()[0].plugins.is_empty());
}
