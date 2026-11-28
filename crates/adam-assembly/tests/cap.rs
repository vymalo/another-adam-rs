//! The output cap of an agent's model calls (ADR 0064, slice 2): the folder's
//! `limits.max_output_tokens`, else the preset its `model_preset` names, else the profile's
//! `maxOutputTokens`, else the profile's base preset, else 128,000; then the smallest of that, the
//! model's `maxOutputTokensLimit` and what the window leaves after the margin and the history.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use adam_assembly::{AgentDef, Assembly, CapSource, Error};
use adam_llm_agent::ToolSet;
use adam_model::{MockModel, ModelProfile, ModelProfiles, Preset};
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
    // The default history (200,000) is more than half the window, so the window wins and keeps
    // half of itself for the history: 131,072 - 20,000 - 65,536.
    assert_eq!(cap("name: coder\nmodel: plain"), 45_536);
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
    // 30,000 - 20,000 - the 15,000 of history the window holds is nothing.
    let assembly = assembly_with(
        MockModel::new().with_context_window("large", 30_000),
        "name: coder\nmodel: large\nlimits:\n  max_history_tokens: 200000",
        "large",
        with_profile(),
    )
    .unwrap();
    assert_eq!(assembly.info()[0].limits.max_output_tokens, 1);
}

#[test]
fn a_known_window_clamps_the_folders_own_value_too() {
    // The decisive case: the folder's 60,000 is clamped by the window, not passed through.
    let assembly = assembly_with(
        MockModel::new().with_context_window("large", 131_072),
        "name: coder\nmodel: large\nlimits:\n  max_output_tokens: 60000",
        "large",
        with_profile(),
    )
    .unwrap();
    assert_eq!(assembly.info()[0].limits.max_output_tokens, 45_536);
}

/// ADR 0088 decision 4: the 200,000-token default, or a folder's history, never squeezes the
/// output below what half the window leaves; a window that holds it keeps it whole.
#[test]
fn a_history_the_window_cannot_hold_leaves_the_output_half_the_window() {
    let cap = |window: u64, frontmatter: &str| {
        assembly_with(
            MockModel::new().with_context_window("plain", window),
            frontmatter,
            "plain",
            with_profile(),
        )
        .unwrap()
        .info()[0]
            .limits
            .max_output_tokens
    };
    // 200,000 - 20,000 - 100,000, where the default history alone would leave one token.
    assert_eq!(cap(200_000, "name: coder\nmodel: plain"), 80_000);
    // A folder's own history over half the window gives way the same: 131,072 - 20,000 - 65,536.
    assert_eq!(
        cap(
            131_072,
            "name: coder\nmodel: plain\nlimits:\n  max_history_tokens: 100000"
        ),
        45_536
    );
    // A window that holds the default: 400,000 - 20,000 - 200,000 is over the 128,000 fallback.
    assert_eq!(cap(400_000, "name: coder\nmodel: plain"), 128_000);
}

#[test]
fn the_folder_wins_over_the_preset_too() {
    assert_eq!(
        cap_of(
            "name: coder\nmodel: large\nmodel_preset: quick\nlimits:\n  max_output_tokens: 7000",
            "large",
            with_profile()
        ),
        7_000
    );
}

#[test]
fn a_preset_with_no_profile_still_caps_the_calls() {
    let profiles = profiles(r#"{"presets": {"quick": {"maxOutputTokens": 50000}}}"#);
    assert_eq!(
        cap_of(
            "name: coder\nmodel: any\nmodel_preset: quick",
            "any",
            profiles
        ),
        50_000
    );
}

#[test]
fn a_hand_built_profile_with_a_limit_of_zero_asks_for_one_token() {
    let profiles = ModelProfiles {
        profiles: BTreeMap::from([(
            "large".to_owned(),
            ModelProfile {
                max_output_tokens_limit: Some(0),
                ..ModelProfile::default()
            },
        )]),
        presets: BTreeMap::new(),
    };
    assert_eq!(cap_of("name: coder\nmodel: large", "large", profiles), 1);
}

#[test]
fn a_hand_built_base_preset_above_the_limit_names_the_profiles_file() {
    let profiles = ModelProfiles {
        profiles: BTreeMap::from([(
            "large".to_owned(),
            ModelProfile {
                max_output_tokens_limit: Some(131_072),
                preset: Some("base".to_owned()),
                ..ModelProfile::default()
            },
        )]),
        presets: BTreeMap::from([(
            "base".to_owned(),
            Preset {
                reasoning_effort: None,
                max_output_tokens: Some(200_000),
                ..Preset::default()
            },
        )]),
    };
    let error = assembly("name: coder\nmodel: large", "large", profiles).unwrap_err();
    let Error::OutputCap {
        origin,
        set_by,
        cap,
        limit,
        ..
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(origin.file.to_string_lossy(), "MODEL_ALIASES_FILE");
    assert_eq!(*set_by, CapSource::Preset("base".into()));
    assert_eq!(*cap, 200_000);
    assert_eq!(*limit, 131_072);
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

/// ADR 0088 decisions 1 and 3: a subagent whose `model:` names another alias has its own window,
/// its profile's `contextWindow`, which the client answers for that alias (no `DEFAULT_MODEL_CONTEXT_WINDOW`
/// ever reached it); the window clamps that subagent's cap, and not the root's.
#[test]
fn a_subagent_on_another_alias_has_its_own_profiles_window() {
    let profiles = profiles(r#"{"profiles": {"plain": {}, "small": {"contextWindow": 131072}}}"#);
    let assembly = def(&[
        (
            INSTRUCTIONS,
            &instructions("name: coder\nmodel: plain", "Root."),
        ),
        (
            "agent/subagents/helper.md",
            &instructions("description: Helps.\nmodel: small", "Helper."),
        ),
    ])
    .bind(ToolSet::new())
    .unwrap()
    .model_profiles(profiles.clone())
    .model(Arc::new(MockModel::new().with_profiles(profiles)), "plain")
    .unwrap();
    let cap_of = |name: &str| {
        assembly
            .info()
            .iter()
            .find(|info| info.name == name)
            .unwrap()
            .limits
            .max_output_tokens
    };
    // The root's alias has no window: the 128,000 fallback.
    assert_eq!(cap_of("coder"), 128_000);
    // The subagent's: 131,072 - 20,000 - half its window, which the 200,000 default does not fit.
    assert_eq!(cap_of("coder/helper"), 45_536);
}
