//! The default reasoning effort of an agent (ADR 0063 decision 4, ADR 0064 decision 3): the
//! folder's `reasoning_effort`, else the preset its `model_preset` names, else the profile's own
//! `reasoningEffort`, else the profile's base preset, each checked against the model's levels.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_assembly::{AgentDef, Assembly, EffortSource, Error};
use adam_llm_agent::ToolSet;
use adam_model::{MockModel, ModelProfiles, ReasoningEffort};
use common::{def, instructions, runtime, spawn_worker, wait_done};

const INSTRUCTIONS: &str = "agent/instructions.md";

fn profiles(json: &str) -> ModelProfiles {
    ModelProfiles::parse_json(json).expect("valid profiles")
}

fn with_profile() -> ModelProfiles {
    profiles(
        r#"{
            "presets": {
                "quick": {"reasoningEffort": "low"},
                "deep": {"reasoningEffort": "high"}
            },
            "profiles": {
                "large": {
                    "reasoningEfforts": ["none", "low", "medium", "high"],
                    "reasoningWire": "reasoning_effort",
                    "reasoningEffort": "medium",
                    "preset": "deep"
                },
                "base": {
                    "reasoningEfforts": ["low", "high"],
                    "reasoningWire": "reasoning_effort",
                    "preset": "deep"
                }
            }
        }"#,
    )
}

fn agent(frontmatter: &str) -> AgentDef {
    def(&[(INSTRUCTIONS, &instructions(frontmatter, "Root."))])
}

fn assembly(frontmatter: &str, model: &str, profiles: ModelProfiles) -> Result<Assembly, Error> {
    agent(frontmatter)
        .bind(ToolSet::new())
        .unwrap()
        .model_profiles(profiles)
        .model(Arc::new(MockModel::new()), model)
}

fn default_of(frontmatter: &str, model: &str) -> Option<ReasoningEffort> {
    assembly(frontmatter, model, with_profile()).unwrap().info()[0].reasoning_effort
}

#[test]
fn the_folder_wins_over_the_preset_and_the_profile() {
    assert_eq!(
        default_of("name: coder\nmodel: large\nreasoning_effort: high", "large"),
        Some(ReasoningEffort::High)
    );
}

#[test]
fn the_agents_preset_wins_over_the_profile() {
    assert_eq!(
        default_of("name: coder\nmodel: large\nmodel_preset: quick", "large"),
        Some(ReasoningEffort::Low)
    );
}

#[test]
fn the_profile_wins_over_its_base_preset() {
    assert_eq!(
        default_of("name: coder\nmodel: large", "large"),
        Some(ReasoningEffort::Medium)
    );
    // A profile with no value of its own takes its base preset's.
    assert_eq!(
        default_of("name: coder\nmodel: base", "base"),
        Some(ReasoningEffort::High)
    );
}

#[test]
fn no_folder_preset_or_profile_value_sends_nothing() {
    let profiles = profiles(
        r#"{"profiles": {"plain": {"reasoningEfforts": ["low"], "reasoningWire": "reasoning_effort"}}}"#,
    );
    let assembly = assembly("name: coder\nmodel: plain", "plain", profiles).unwrap();
    assert_eq!(assembly.info()[0].reasoning_effort, None);
}

#[test]
fn the_resolved_effort_reaches_the_request_of_a_run() {
    let model = Arc::new(MockModel::new());
    model.push_text("done");
    let assembly = agent("name: coder\nmodel: large\nreasoning_effort: high")
        .bind(ToolSet::new())
        .unwrap()
        .model_profiles(with_profile())
        .model(model.clone(), "large")
        .unwrap();
    let rt = runtime(&assembly);
    let worker = spawn_worker(&rt);
    let run = rt
        .start("coder", adam_llm_agent::user_message("go"), None)
        .await
        .unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(
        model.requests()[0].reasoning_effort,
        Some(ReasoningEffort::High)
    );
}

#[test]
fn a_level_the_model_does_not_take_names_the_agent_and_the_value() {
    let error = assembly(
        "name: coder\nmodel: large\nreasoning_effort: max",
        "large",
        with_profile(),
    )
    .unwrap_err();
    let Error::ReasoningEffort {
        origin,
        effort,
        set_by,
        model,
        suggestion,
        allowed,
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(origin.agent, "coder");
    assert_eq!(origin.file.to_string_lossy(), INSTRUCTIONS);
    assert_eq!(effort, "max");
    assert_eq!(*set_by, EffortSource::Folder);
    assert_eq!(model, "large");
    assert_eq!(*suggestion, None);
    let allowed: Vec<&str> = allowed.iter().map(String::as_str).collect();
    assert_eq!(allowed, ["none", "low", "medium", "high"]);
    assert_eq!(
        error.to_string(),
        "agent `coder` (agent/instructions.md): `reasoning_effort` sets reasoning effort `max`, \
         which model `large` does not take; it takes: `none`, `low`, `medium`, `high`"
    );
}

#[test]
fn a_folder_effort_on_a_model_with_no_profile_is_refused() {
    let error = assembly(
        "name: coder\nmodel: unknown\nreasoning_effort: low",
        "unknown",
        ModelProfiles::default(),
    )
    .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::ReasoningEffort {
                set_by: EffortSource::Folder,
                allowed,
                ..
            } if allowed.is_empty()
        ),
        "{error}"
    );
    assert!(error.to_string().ends_with("it takes: none"), "{error}");
}

#[test]
fn a_word_outside_the_vocabulary_is_refused_with_the_seven_levels() {
    let error = assembly(
        "name: coder\nmodel: large\nreasoning_effort: ultra",
        "large",
        with_profile(),
    )
    .unwrap_err();
    assert!(
        matches!(&error, Error::InvalidReasoningEffort { origin, .. } if origin.agent == "coder"),
        "{error}"
    );
    assert!(error.to_string().contains("`ultra`"), "{error}");
    assert!(error.to_string().contains("xhigh"), "{error}");
}

#[test]
fn a_preset_the_profiles_do_not_declare_is_refused_with_a_suggestion() {
    let error = assembly(
        "name: coder\nmodel: large\nmodel_preset: quik",
        "large",
        with_profile(),
    )
    .unwrap_err();
    let Error::UnknownModelPreset {
        origin,
        preset,
        suggestion,
        known,
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(origin.agent, "coder");
    assert_eq!(preset, "quik");
    assert_eq!(suggestion.as_deref(), Some("quick"));
    assert_eq!(known, &["deep", "quick"]);
}

#[test]
fn an_agents_preset_effort_outside_the_models_levels_is_refused() {
    // The loader refuses such a preset only when a profile names it; an agent may name any preset.
    let profiles = profiles(
        r#"{
            "presets": {"wide": {"reasoningEffort": "max"}},
            "profiles": {"narrow": {"reasoningEfforts": ["low"], "reasoningWire": "reasoning_effort"}}
        }"#,
    );
    let error = assembly(
        "name: coder\nmodel: narrow\nmodel_preset: wide",
        "narrow",
        profiles,
    )
    .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::ReasoningEffort {
                set_by: EffortSource::Preset(name),
                effort,
                ..
            } if name == "wide" && effort == "max"
        ),
        "{error}"
    );
    assert!(error.to_string().contains("the preset `wide`"), "{error}");
}
