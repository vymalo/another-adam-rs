//! The A2A card of the root agent (feature `a2a`).
#![cfg(feature = "a2a")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::path::Path;
use std::sync::Arc;

use adam_a2a::AgentCardConfig;
use adam_agent_fixture::AGENT;
use adam_assembly::{AgentDef, Error, Url};
use adam_llm_agent::ToolSet;
use adam_model::MockModel;
use common::{def, instructions, tools, with_fixture_token};
use serde_json::{Value, json};

fn assembly_of(def: AgentDef, set: ToolSet) -> adam_assembly::Assembly {
    with_fixture_token(def)
        .bind(set)
        .unwrap()
        .model(Arc::new(MockModel::new()), "m")
        .unwrap()
}

/// The config as JSON: everything the config holds, in a stable shape for the golden file.
fn render(card: &AgentCardConfig) -> Value {
    json!({
        "name": card.name,
        "description": card.description,
        "url": card.url.as_str(),
        "version": card.version,
        "skills": card.skills.iter().map(|s| json!({
            "id": s.id,
            "name": s.name,
            "description": s.description,
            "tags": s.tags,
            "examples": s.examples,
        })).collect::<Vec<_>>(),
        "extensions": card.extensions.iter().map(|e| json!({
            "uri": e.uri,
            "description": e.description,
            "required": e.required,
            "params": e.params,
        })).collect::<Vec<_>>(),
    })
}

/// The same JSON with every object's keys in sorted order, whether or not another crate of the
/// build turned on serde_json's `preserve_order` (feature unification decides that, not us).
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            let mut map = map;
            Value::Object(
                keys.into_iter()
                    .filter_map(|k| map.remove(&k).map(|v| (k, sorted(v))))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

fn url() -> Url {
    "https://agents.example.com/coder/".parse().unwrap()
}

#[test]
fn the_fixture_card_matches_the_golden_file() {
    let fixture_tools = tools(&[
        "prepare_workspace",
        "run_checks",
        "ask_user",
        "read_diff",
        "list_files",
        "fetch_page",
    ]);
    let assembly = assembly_of(
        common::with_fixture_mcp(AgentDef::from_manifest(AGENT).unwrap()),
        fixture_tools,
    );
    let card = assembly.card(url(), "1.2.3").unwrap();
    let got = serde_json::to_string_pretty(&sorted(render(&card))).unwrap() + "\n";

    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/card.json");
    if std::env::var_os("ADAM_UPDATE_GOLDEN").is_some() {
        std::fs::write(&golden, &got).unwrap();
    }
    let want = std::fs::read_to_string(&golden).unwrap();
    assert_eq!(got, want, "regenerate with ADAM_UPDATE_GOLDEN=1");
}

#[test]
fn without_a_card_the_agent_describes_itself() {
    let assembly = assembly_of(
        def(&[(
            "agent/instructions.md",
            &instructions("name: helper\ndescription: Helps out.", "Hi."),
        )]),
        ToolSet::new(),
    );
    let card = assembly.card(url(), "0.1.0").unwrap();
    assert_eq!(card.name, "helper");
    assert_eq!(card.description, "Helps out.");
    assert_eq!(card.version, "0.1.0");
    assert!(card.skills.is_empty() && card.extensions.is_empty());
}

#[test]
fn the_card_description_wins_over_the_agents() {
    let assembly = assembly_of(
        def(&[(
            "agent/instructions.md",
            &instructions(
                "name: helper\ndescription: Internal.\ncard:\n  description: Public.",
                "Hi.",
            ),
        )]),
        ToolSet::new(),
    );
    assert_eq!(assembly.card(url(), "1").unwrap().description, "Public.");
}

#[test]
fn a_card_needs_a_description() {
    let assembly = assembly_of(
        def(&[(
            "agent/instructions.md",
            &instructions("name: helper", "Hi."),
        )]),
        ToolSet::new(),
    );
    let error = assembly.card(url(), "1").unwrap_err();
    assert!(
        matches!(error, Error::MissingCardDescription { .. }),
        "{error}"
    );
    assert!(
        error.to_string().starts_with(
            "agent `helper` (agent/instructions.md): the A2A card needs a description"
        ),
        "{error}"
    );
    // Blanks are no description either.
    let blank = assembly_of(
        def(&[(
            "agent/instructions.md",
            &instructions("name: helper\ndescription: '  '", "Hi."),
        )]),
        ToolSet::new(),
    );
    assert!(blank.card(url(), "1").is_err());
}

/// The card is a fact about the files: a definition gives it before any tool, state or model is
/// bound, and it is the card the assembled agent gives.
#[test]
fn a_definition_gives_the_card_before_it_is_bound() {
    let fixture_tools = tools(&[
        "prepare_workspace",
        "run_checks",
        "ask_user",
        "read_diff",
        "list_files",
        "fetch_page",
    ]);
    let def = common::with_fixture_mcp(AgentDef::from_manifest(AGENT).unwrap());
    let early = def.card(url(), "1.2.3").unwrap();
    let assembly = assembly_of(def, fixture_tools);
    let bound = assembly.card(url(), "1.2.3").unwrap();
    assert_eq!(
        serde_json::to_string(&sorted(render(&early))).unwrap(),
        serde_json::to_string(&sorted(render(&bound))).unwrap()
    );
    assert_eq!(early.skills.len(), bound.skills.len());
    assert!(!early.skills.is_empty());

    // The same refusal, from the definition.
    let bare = def_of_name_only();
    assert!(matches!(
        bare.card(url(), "1").unwrap_err(),
        Error::MissingCardDescription { .. }
    ));
}

fn def_of_name_only() -> AgentDef {
    def(&[(
        "agent/instructions.md",
        &instructions("name: helper", "Hi."),
    )])
}

/// `card.extended` is what an authenticated caller sees on top of the public card: it becomes the
/// config's extended card, and a card without it, or with one that adds nothing, has none.
#[test]
fn card_extended_becomes_the_extended_card_of_the_config() {
    let assembly = assembly_of(
        def(&[(
            "agent/instructions.md",
            &instructions(
                "name: helper\n\
                 description: Helps out.\n\
                 card:\n  \
                   skills:\n    - { id: help, name: Help, description: Public help }\n  \
                   extended:\n    \
                     description: Helps out, and audits for the signed in.\n    \
                     skills:\n      - { id: audit, name: Audit, description: Only for you, tags: [internal] }",
                "Hi.",
            ),
        )]),
        ToolSet::new(),
    );
    let card = assembly.card(url(), "1").unwrap();
    assert_eq!(
        card.skills.len(),
        1,
        "the public card lists the public skill only"
    );
    let extended = card.extended.as_ref().expect("an extended card");
    assert_eq!(
        extended.description.as_deref(),
        Some("Helps out, and audits for the signed in.")
    );
    assert_eq!(extended.skills.len(), 1);
    assert_eq!(extended.skills[0].id, "audit");
    assert_eq!(extended.skills[0].tags, ["internal"]);
    assert!(
        extended.extensions.is_empty(),
        "extensions come from code, not from files"
    );

    // Declared and empty: nothing is served.
    let empty = assembly_of(
        def(&[(
            "agent/instructions.md",
            &instructions(
                "name: helper\ndescription: Helps out.\ncard:\n  extended: {}",
                "Hi.",
            ),
        )]),
        ToolSet::new(),
    );
    assert!(empty.card(url(), "1").unwrap().extended.is_none());
    // And without the key, none.
    let none = assembly_of(
        def(&[(
            "agent/instructions.md",
            &instructions("name: helper\ndescription: Helps out.", "Hi."),
        )]),
        ToolSet::new(),
    );
    assert!(none.card(url(), "1").unwrap().extended.is_none());
}
