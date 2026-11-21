//! The output cap of an agent's model calls (ADR 0064, slice 2): the folder's
//! `limits.max_output_tokens`, else the preset its `model_preset` names, else the profile's
//! `maxOutputTokens`, else the profile's base preset, else 128,000; then the smallest of that, the
//! model's `maxOutputTokensLimit` and what the window leaves after the margin and the history.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_assembly::{AgentDef, Assembly, CapSource, Error};
use adam_llm_agent::ToolSet;
use adam_model::{MockModel, ModelProfiles};
use common::{def, instructions, runtime, spawn_worker, wait_done};

const INSTRUCTIONS: &str = "agent/instructions.md";

fn profiles(json: &str) -> ModelProfiles {
    ModelProfiles::parse_json(json).expect("valid profiles")
}

/// A preset with a cap, a profile with its own cap and limit, and a profile that takes the
/// preset's cap only.
fn with_profile() -> ModelProfiles {
    profiles(
        r#"{
            "presets": {
                "quick": {"maxOutputTokens": 50000},
                "base": {"maxOutputTokens": 40000}
            },
            "profiles": {
                "large": {
                    "maxOutputTokens": 60000,
                    "maxOutputTokensLimit": 131072,
                    "preset": "base"
                },
                "base-only": {"preset": "base"},
                "small": {"maxOutputTokensLimit": 65536},
                "plain": {}
            }
        }"#,
    )
}

fn agent(frontmatter: &str) -> AgentDef {
    def(&[(INSTRUCTIONS, &instructions(frontmatter, "Root."))])
}

fn assembly_with(
    mock: MockModel,
    frontmatter: &str,
    model: &str,
    profiles: ModelProfiles,
) -> Result<Assembly, Error> {
    agent(frontmatter)
        .bind(ToolSet::new())
        .unwrap()
        .model_profiles(profiles)
        .model(Arc::new(mock), model)
}

fn assembly(frontmatter: &str, model: &str, profiles: ModelProfiles) -> Result<Assembly, Error> {
    assembly_with(MockModel::new(), frontmatter, model, profiles)
}

fn cap_of(frontmatter: &str, model: &str, profiles: ModelProfiles) -> u32 {
    assembly(frontmatter, model, profiles).unwrap().info()[0]
        .limits
        .max_output_tokens
}

#[test]
fn the_folder_wins_over_the_preset_and_the_profile() {
    assert_eq!(
        cap_of(
            "name: coder\nmodel: large\nlimits:\n  max_output_tokens: 7000",
            "large",
            with_profile()
        ),
        7_000
    );
}

#[test]
fn the_agents_preset_wins_over_the_profile() {
    assert_eq!(
        cap_of(
            "name: coder\nmodel: large\nmodel_preset: quick",
            "large",
            with_profile()
        ),
        50_000
    );
}

#[test]
fn the_profile_wins_over_its_base_preset() {
    assert_eq!(
        cap_of("name: coder\nmodel: large", "large", with_profile()),
        60_000
    );
    // A profile with no cap of its own takes its base preset's.
    assert_eq!(
        cap_of("name: coder\nmodel: base-only", "base-only", with_profile()),
        40_000
    );
}

#[test]
fn no_value_anywhere_is_the_128_000_fallback() {
    assert_eq!(
        cap_of("name: coder\nmodel: plain", "plain", with_profile()),
        128_000
    );
    assert_eq!(
        cap_of(
            "name: coder\nmodel: plain",
            "plain",
            ModelProfiles::default()
        ),
        128_000
    );
}

#[test]
fn the_models_limit_clamps_the_fallback_and_the_folders_value() {
    // A model whose documented maximum is below the fallback.
    assert_eq!(
        cap_of("name: coder\nmodel: small", "small", with_profile()),
        65_536
    );
    // The folder's own value is clamped, not refused.
    assert_eq!(
        cap_of(
            "name: coder\nmodel: large\nlimits:\n  max_output_tokens: 200000",
            "large",
            with_profile()
        ),
        131_072
    );
}

#[test]
fn a_known_window_clamps_the_cap_to_what_the_history_leaves() {
    let cap = |frontmatter: &str| {
        assembly_with(
            MockModel::new().with_context_window("plain", 131_072),
            frontmatter,
            "plain",
            with_profile(),
        )
        .unwrap()
        .info()[0]
            .limits
            .max_output_tokens
    };
    // The coder's history: 131,072 - 20,000 - 100,000.
    assert_eq!(cap("name: coder\nmodel: plain"), 11_072);
    // A shorter history leaves more.
    assert_eq!(
        cap("name: coder\nmodel: plain\nlimits:\n  max_history_tokens: 50000"),
        61_072
    );
    // No history: half the window.
    assert_eq!(
        cap("name: coder\nmodel: plain\nlimits:\n  max_history_tokens: 0"),
        45_536
    );
}

#[test]
fn a_window_with_no_room_asks_for_one_token() {
    let assembly = assembly_with(
        MockModel::new().with_context_window("large", 131_072),
        "name: coder\nmodel: large\nlimits:\n  max_history_tokens: 200000",
        "large",
        with_profile(),
    )
    .unwrap();
    assert_eq!(assembly.info()[0].limits.max_output_tokens, 1);
}

#[test]
fn a_preset_above_the_models_limit_is_refused() {
    // The loader refuses such a preset only when a profile names it; an agent may name any preset.
    let profiles = profiles(
        r#"{
            "presets": {"huge": {"maxOutputTokens": 200000}},
            "profiles": {"large": {"maxOutputTokensLimit": 131072}}
        }"#,
    );
    let error = assembly(
        "name: coder\nmodel: large\nmodel_preset: huge",
        "large",
        profiles,
    )
    .unwrap_err();
    let Error::OutputCap {
        origin,
        set_by,
        cap,
        limit,
        model,
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(origin.agent, "coder");
    assert_eq!(origin.file.to_string_lossy(), INSTRUCTIONS);
    assert_eq!(*set_by, CapSource::Preset("huge".into()));
    assert_eq!(*cap, 200_000);
    assert_eq!(*limit, 131_072);
    assert_eq!(model, "large");
    assert_eq!(
        error.to_string(),
        "agent `coder` (agent/instructions.md): the preset `huge` asks 200000 output tokens, \
         above the 131072 that model `large` allows: lower the preset's `maxOutputTokens`, or the \
         model's `maxOutputTokensLimit`"
    );
}

#[test]
fn the_resolved_cap_and_the_folders_limits_are_what_the_info_reports() {
    let assembly = assembly(
        "name: coder\nmodel: small\nlimits:\n  max_turns: 9\n  max_history_tokens: 50000",
        "small",
        with_profile(),
    )
    .unwrap();
    let limits = assembly.info()[0].limits;
    assert_eq!(limits.max_output_tokens, 65_536);
    assert_eq!(limits.max_turns, 9, "the folder's own limits are kept");
    assert_eq!(limits.max_history_tokens, 50_000);
}

#[tokio::test]
async fn the_resolved_cap_reaches_the_request_of_a_run() {
    let model = Arc::new(MockModel::new());
    model.push_text("done");
    let assembly = agent("name: coder\nmodel: small")
        .bind(ToolSet::new())
        .unwrap()
        .model_profiles(with_profile())
        .model(model.clone(), "small")
        .unwrap();
    let rt = runtime(&assembly);
    let worker = spawn_worker(&rt);
    let run = rt
        .start("coder", adam_llm_agent::user_message("go"), None)
        .await
        .unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(model.requests()[0].max_output_tokens, Some(65_536));
}
