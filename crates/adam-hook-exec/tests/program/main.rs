//! The `adam-hooks/1` test program (ADR 0050, slice P1).
//!
//! It is the child the adapter's integration tests start. Its mode is its first argument (or
//! `ADAM_HOOK_TEST_MODE`), and it writes what it receives where the tests can read it:
//!
//! | Mode | What it does |
//! |---|---|
//! | `continue` (default) | answers `continue` to everything |
//! | `deny` | answers `deny` with a fixed reason |
//! | `ask` | answers `ask` with a fixed question |
//! | `stop` | answers `stop` with a fixed reason |
//! | `rewrite` | rewrites the point's member (arguments, text, context, temperature) |
//! | `garbage` | writes a line that is not JSON instead of its first reply |
//! | `slow` | waits five seconds before every reply |
//! | `dead` | exits before the hello |
//! | `badhello` | says hello with another protocol |
//! | `die-after:<n>` | exits after `n` events |
//!
//! `ADAM_HOOK_TEST_LOG` names a file every line it reads is appended to, and `ADAM_HOOK_TEST_ENV`
//! names a file its whole environment is written to as JSON, before the hello.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let mode = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("ADAM_HOOK_TEST_MODE").ok())
        .unwrap_or_else(|| "continue".to_owned());
    if let Ok(path) = std::env::var("ADAM_HOOK_TEST_ENV") {
        let vars: BTreeMap<String, String> = std::env::vars().collect();
        let _ = std::fs::write(path, serde_json::to_string(&vars).unwrap_or_default());
    }
    if mode == "dead" {
        std::process::exit(3);
    }
    let die_after = mode
        .strip_prefix("die-after:")
        .and_then(|n| n.parse::<u32>().ok());
    let log = std::env::var("ADAM_HOOK_TEST_LOG").ok();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut events = 0u32;
    let mut garbage = mode == "garbage";
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return };
        if let Some(path) = &log
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        {
            let _ = writeln!(file, "{line}");
        }
        let value: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        match value.get("type").and_then(Value::as_str) {
            Some("hello") => {
                let protocol = if mode == "badhello" {
                    "adam-hooks/0"
                } else {
                    "adam-hooks/1"
                };
                reply(
                    &mut stdout,
                    json!({"type": "hello", "protocol": protocol, "name": "test-program", "version": 1}),
                );
            }
            Some("shutdown") => return,
            Some("event") => {
                events += 1;
                if let Some(limit) = die_after
                    && events > limit
                {
                    std::process::exit(4);
                }
                if garbage {
                    // One broken line: the adapter must refuse, never continue.
                    garbage = false;
                    let _ = writeln!(stdout, "this is not JSON");
                    let _ = stdout.flush();
                    continue;
                }
                if mode == "slow" {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                }
                let id = value.get("id").cloned().unwrap_or(Value::Null);
                let point = value
                    .get("point")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                reply(&mut stdout, answer(&mode, &id, &point));
            }
            _ => {}
        }
    }
}

/// What the program answers to one event.
fn answer(mode: &str, id: &Value, point: &str) -> Value {
    let verdict = match mode {
        "deny" => json!({"verdict": "deny", "reason": "the test program denies"}),
        "ask" => json!({"verdict": "ask", "reason": "the test program asks"}),
        "stop" => json!({"verdict": "stop", "reason": "the test program stops"}),
        "rewrite" => match point {
            "tool.before" => json!({"verdict": "rewrite", "arguments": {"rewritten": true}}),
            "compaction.before" => {
                json!({"verdict": "rewrite", "context": "added by the test program"})
            }
            "model.request" => json!({"verdict": "rewrite", "temperature": 0.25}),
            _ => json!({"verdict": "rewrite", "text": "rewritten by the test program"}),
        },
        _ => json!({"verdict": "continue"}),
    };
    let mut reply = verdict;
    reply["type"] = json!("reply");
    reply["id"] = id.clone();
    reply
}

fn reply(stdout: &mut std::io::Stdout, value: Value) {
    let _ = writeln!(stdout, "{value}");
    let _ = stdout.flush();
}
