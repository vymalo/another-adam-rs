//! What each tool of the screen shares with the other calls of a turn (ADR 0049).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use adam_llm_agent::ToolAccess;
use adam_mcp::McpPolicy;
use adam_ui::Ui;
use serde_json::json;

/// Whether two calls with these accesses must keep the model's order.
fn conflict(a: &ToolAccess, b: &ToolAccess) -> bool {
    match (a.uses(), b.uses()) {
        (Some(a), Some(b)) => a.iter().any(|x| b.iter().any(|y| x.conflicts(y))),
        _ => true,
    }
}

fn access(name: &str) -> ToolAccess {
    let tools = Ui::new(McpPolicy::default()).tools();
    tools.get(name).unwrap().access(&json!({}))
}

#[test]
fn the_catalog_is_read_beside_anything_and_show_keeps_its_order() {
    let catalog = access("ui_catalog");
    assert_eq!(catalog, ToolAccess::Independent);
    assert!(!conflict(&catalog, &catalog), "two catalog reads overlap");
    let show = access("show");
    assert!(conflict(&show, &show), "two shows keep the model's order");
    assert!(
        !conflict(&show, &catalog),
        "a show and a catalog read overlap"
    );
}

#[test]
fn asking_and_the_plan_run_alone() {
    for name in ["ask_user", "plan_work", "plan_check", "plan_add"] {
        assert_eq!(access(name), ToolAccess::Alone, "{name}");
    }
}

/// The orchestration layer's tools (`ask_agent` and the rest the endpoint lists) may ask other agents
/// and wait: the source says every call of them runs alone.
#[test]
fn the_thread_tools_run_alone() {
    use adam_llm_agent::ToolSource;
    let ui = Ui::new(McpPolicy::default());
    for source in [
        ui.source(),
        adam_ui::ThreadTools::new(std::sync::Arc::new(adam_ui::ThreadToolsClient::new(
            McpPolicy::default(),
        ))),
    ] {
        for name in ["ask_agent", "turn_output", "show"] {
            assert_eq!(
                source.access(name, &json!({"agent": "researcher"})),
                ToolAccess::Alone,
                "{name}"
            );
        }
    }
}
