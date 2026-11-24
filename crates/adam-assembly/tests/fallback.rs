//! The model a run continues on when its own is locked (ADR 0064, amended 2026-11-23): where the
//! alias comes from (the folder, its preset, the model's profile, in that order) and that the
//! fallback's cap and level come from its **own** profile, not from the agent's settings for its
//! own model.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_assembly::{AgentDef, Assembly, Error};
use adam_llm_agent::ToolSet;
use adam_model::{MockModel, ModelError, ModelProfiles};
use common::{def, instructions, runtime, spawn_worker, wait_done};

const INSTRUCTIONS: &str = "agent/instructions.md";

fn profiles() -> ModelProfiles {
    ModelProfiles::parse_json(
        r#"{
            "presets": {
                "careful": {"fallback": "mid"}
            },
            "profiles": {
                "big": {"fallback": "lite", "maxOutputTokens": 90000},
                "lite": {"maxOutputTokens": 4096, "reasoningEfforts": ["low"],
                         "reasoningWire": "reasoning_effort", "reasoningEffort": "low"},
                "mid": {"maxOutputTokens": 8192},
                "alone": {}
            }
        }"#,
    )
    .expect("valid profiles")
}

fn agent(frontmatter: &str) -> AgentDef {
    def(&[(INSTRUCTIONS, &instructions(frontmatter, "Root."))])
}

fn bind(
    mock: Arc<MockModel>,
    frontmatter: &str,
    model: &str,
    profiles: ModelProfiles,
) -> Result<Assembly, Error> {
    agent(frontmatter)
        .bind(ToolSet::new())
        .unwrap()
        .model_profiles(profiles)
        .model(mock, model)
}

fn fallback_of(frontmatter: &str, model: &str) -> Option<String> {
    bind(Arc::new(MockModel::new()), frontmatter, model, profiles())
        .unwrap()
        .info()[0]
        .model_fallback
        .clone()
}

/// The profile is the seat; a preset the agent names wins over it; the folder wins over both.
#[test]
fn the_fallback_is_the_folders_then_the_presets_then_the_profiles() {
    assert_eq!(
        fallback_of("name: coder\nmodel: big", "big").as_deref(),
        Some("lite"),
        "the profile's"
    );
    assert_eq!(
        fallback_of("name: coder\nmodel: big\nmodel_preset: careful", "big").as_deref(),
        Some("mid"),
        "the agent's preset wins over the profile"
    );
    assert_eq!(
        fallback_of(
            "name: coder\nmodel: big\nmodel_preset: careful\nmodel_fallback: alone",
            "big"
        )
        .as_deref(),
        Some("alone"),
        "the folder wins over both"
    );
    assert_eq!(
        fallback_of("name: coder\nmodel: alone", "alone"),
        None,
        "a model with no seat has no fallback"
    );
}

/// A profile's fallback is another model of the document: the file is refused whole otherwise.
#[test]
fn a_profile_may_not_name_itself_or_an_unprofiled_model_as_its_fallback() {
    let own = ModelProfiles::parse_json(r#"{"profiles": {"a": {"fallback": "a"}}}"#).unwrap_err();
    assert!(
        own.iter().any(|p| p.contains("its own fallback")),
        "{own:?}"
    );
    let gone = ModelProfiles::parse_json(r#"{"profiles": {"a": {"fallback": "b"}}}"#).unwrap_err();
    assert!(
        gone.iter().any(|p| p.contains("has no profile")),
        "{gone:?}"
    );
}

/// A fallback that is the agent's own model, from any seat, stops the build.
#[test]
fn a_fallback_from_the_profile_that_is_the_agents_own_model_is_refused() {
    let error = bind(
        Arc::new(MockModel::new()),
        "name: coder\nmodel: lite\nmodel_preset: careful",
        "lite",
        ModelProfiles::parse_json(
            r#"{"presets": {"careful": {"fallback": "lite"}},
                "profiles": {"lite": {}}}"#,
        )
        .unwrap(),
    )
    .unwrap_err();
    assert!(matches!(error, Error::ModelAlias { .. }), "{error}");
}

/// The cap and the level of the fallback's calls come from its own profile: the folder's cap for
/// its own model (90,000 here) is not carried over.
#[tokio::test]
async fn the_fallback_takes_its_own_profiles_cap_and_level() {
    let model = Arc::new(MockModel::new());
    model
        .push_error(ModelError::ModelLocked {
            model: "big".into(),
        })
        .push_text("done");
    let assembly = bind(
        model.clone(),
        "name: coder\nmodel: big\nlimits:\n  max_output_tokens: 70000",
        "big",
        profiles(),
    )
    .unwrap();
    let rt = runtime(&assembly);
    let worker = spawn_worker(&rt);
    let run = rt
        .start("coder", adam_llm_agent::user_message("go"), None)
        .await
        .unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;
    let requests = model.requests();
    assert_eq!(requests[0].model, "big");
    assert_eq!(requests[0].max_output_tokens, Some(70_000));
    assert_eq!(requests[1].model, "lite");
    assert_eq!(requests[1].max_output_tokens, Some(4_096));
    assert_eq!(
        requests[1].reasoning_effort,
        Some(adam_model::ReasoningEffort::Low)
    );
}

/// A level the fallback's profile does not take (a hand-built preset's) stops the build, naming it.
#[test]
fn a_fallback_level_its_profile_does_not_take_is_refused() {
    let mut profiles = profiles();
    profiles.profiles.get_mut("mid").unwrap().preset = Some("high".to_owned());
    profiles.presets.insert(
        "high".to_owned(),
        adam_model::Preset {
            reasoning_effort: Some(adam_model::ReasoningEffort::High),
            ..Default::default()
        },
    );
    let error = bind(
        Arc::new(MockModel::new()),
        "name: coder\nmodel: big\nmodel_fallback: mid",
        "big",
        profiles,
    )
    .unwrap_err();
    assert!(matches!(error, Error::ReasoningEffort { .. }), "{error}");
}
