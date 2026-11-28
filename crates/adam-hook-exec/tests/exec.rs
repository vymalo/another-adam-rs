//! The adapter against the protocol's test program (ADR 0050, slice P1): a verdict round-trips,
//! garbage and a dead program never let a call through, a rewrite over a scrubbed span is refused,
//! the child sees only the safe variables, and a program that dies is restarted after its backoff.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use adam_error::{Classify, ErrorClass};
use adam_hook_exec::{Config, DecidePoint, Error, HookExec, Points, SAFE_VARS};
use adam_plugin::{
    Decision, EventHeader, EventKind, HookEvent, HookSet, HookedCall, HookedCompaction, HookedEnd,
    HookedMessage, HookedModelRequest, HookedText, MessageFile, ModelCallKind, Plugin,
};
use serde_json::{Value, json};

/// The protocol's test program, as cargo built it for these tests.
fn program() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_adam-hook-exec-test-program"))
}

fn config(mode: &str) -> Config {
    Config::new("test", program()).args([mode])
}

fn only(point: DecidePoint) -> Points {
    Points {
        decide: vec![point],
        observe: Vec::new(),
    }
}

/// How many requests the program logged: its `event` lines. The program logs every line it reads,
/// so the hello and the shutdown are in the file too, and are not requests.
fn requests(log: &std::path::Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line.get("type").and_then(Value::as_str) == Some("event"))
        .count()
}

fn header() -> EventHeader {
    EventHeader {
        key: "run/step/tool.completed/0".to_owned(),
        run: "run".to_owned(),
        root_run: "run".to_owned(),
        agent: "agent".to_owned(),
        at: "now".to_owned(),
    }
}

#[test]
fn a_rewrite_of_a_tool_call_is_applied() {
    let exec = HookExec::start(config("rewrite").points(only(DecidePoint::ToolBefore))).unwrap();
    let mut arguments = json!({"path": "src/lib.rs"});
    let verdict = exec
        .before_tool(&mut HookedCall {
            tool: "read",
            arguments: &mut arguments,
        })
        .unwrap();
    assert_eq!(verdict, Decision::Continue);
    assert_eq!(arguments, json!({"rewritten": true}));
}

#[test]
fn a_deny_an_ask_and_a_stop_carry_their_reason() {
    for (mode, expected) in [
        ("deny", Decision::Deny("the test program denies".to_owned())),
        ("ask", Decision::Ask("the test program asks".to_owned())),
        ("stop", Decision::Stop("the test program stops".to_owned())),
    ] {
        let exec = HookExec::start(config(mode).points(only(DecidePoint::ToolBefore))).unwrap();
        let mut arguments = json!({});
        let verdict = exec
            .before_tool(&mut HookedCall {
                tool: "read",
                arguments: &mut arguments,
            })
            .unwrap();
        assert_eq!(verdict, expected, "the {mode} verdict");
    }
}

#[test]
fn a_message_and_a_tool_answer_are_rewritten() {
    let exec = HookExec::start(config("rewrite").points(Points {
        decide: vec![DecidePoint::MessageReceived, DecidePoint::ToolAfter],
        observe: Vec::new(),
    }))
    .unwrap();
    let mut text = "the person's words".to_owned();
    exec.message_received(&mut HookedMessage {
        text: &mut text,
        files: &[MessageFile {
            name: Some("a.txt".to_owned()),
            media_type: Some("text/plain".to_owned()),
            size: 12,
            reference: None,
        }],
    })
    .unwrap();
    assert_eq!(text, "rewritten by the test program");

    let mut answer = "the tool's answer".to_owned();
    exec.after_tool(
        &HookedCall {
            tool: "read",
            arguments: &mut json!({}),
        },
        &mut HookedText { text: &mut answer },
    )
    .unwrap();
    assert_eq!(answer, "rewritten by the test program");
}

#[test]
fn a_compaction_context_and_a_model_request_are_rewritten() {
    let exec = HookExec::start(config("rewrite").points(Points {
        decide: vec![DecidePoint::CompactionBefore, DecidePoint::ModelRequest],
        observe: Vec::new(),
    }))
    .unwrap();
    let mut context = String::new();
    exec.compaction_before(&mut HookedCompaction {
        context: &mut context,
    })
    .unwrap();
    assert_eq!(context, "added by the test program");

    let (mut temperature, mut top_p, mut max_output_tokens) = (None, None, Some(4096));
    let (mut headers, mut extra_body, mut metadata) = (BTreeMap::new(), None, BTreeMap::new());
    exec.model_request(&mut HookedModelRequest {
        kind: ModelCallKind::Turn,
        temperature: &mut temperature,
        top_p: &mut top_p,
        headers: &mut headers,
        extra_body: &mut extra_body,
        max_output_tokens: &mut max_output_tokens,
        metadata: &mut metadata,
    })
    .unwrap();
    assert_eq!(temperature, Some(0.25));
    assert_eq!(
        max_output_tokens,
        Some(4096),
        "a member it did not rewrite stays"
    );
}

#[test]
fn a_run_ending_may_be_kept_going_but_never_rewritten() {
    let exec = HookExec::start(config("deny").points(only(DecidePoint::RunEnding))).unwrap();
    let verdict = exec
        .run_ending(&HookedEnd {
            text: "the answer",
            continuations: 0,
        })
        .unwrap();
    assert_eq!(
        verdict,
        Decision::Deny("the test program denies".to_owned())
    );

    let rewriting =
        HookExec::start(config("rewrite").points(only(DecidePoint::RunEnding))).unwrap();
    let error = rewriting
        .run_ending(&HookedEnd {
            text: "the answer",
            continuations: 0,
        })
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::Invalid);
}

#[test]
fn garbage_denies() {
    let exec = HookExec::start(config("garbage").points(only(DecidePoint::ToolBefore))).unwrap();
    let mut arguments = json!({});
    let error = exec
        .before_tool(&mut HookedCall {
            tool: "read",
            arguments: &mut arguments,
        })
        .unwrap_err();
    assert_eq!(
        error.class,
        ErrorClass::Invalid,
        "a broken line never lets the call through: {error}"
    );
}

#[test]
fn a_dead_program_is_transient_and_the_set_gives_up() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("requests.log");
    // The program answers the hello, then exits on its first event.
    let exec = HookExec::start(
        config("die-after:0")
            .points(only(DecidePoint::ToolBefore))
            .env("ADAM_HOOK_TEST_LOG", log.to_string_lossy()),
    )
    .unwrap();
    let mut arguments = json!({});
    let error = exec
        .before_tool(&mut HookedCall {
            tool: "read",
            arguments: &mut arguments,
        })
        .unwrap_err();
    assert_eq!(
        error.class,
        ErrorClass::Transient,
        "a dead program is transient, never a continue: {error}"
    );

    // The set retries it once (its default) and then refuses the call. The retry finds the
    // program down, so the program itself was asked exactly once.
    let set = HookSet::new().with(Arc::new(exec));
    let mut arguments = json!({});
    let refused = set
        .before_tool(&mut HookedCall {
            tool: "read",
            arguments: &mut arguments,
        })
        .unwrap_err();
    assert_eq!(
        refused.class,
        ErrorClass::Transient,
        "the set reports the program's class: {refused}"
    );
    assert_eq!(
        requests(&log),
        1,
        "the dead program was asked once, and the retry found it down"
    );
}

#[test]
fn a_dead_program_fails_the_start() {
    let error = HookExec::start(config("dead"))
        .err()
        .expect("the start fails");
    assert_eq!(error.class(), ErrorClass::Invalid);
    assert!(
        matches!(
            error,
            Error::Spawn { .. }
                | Error::Hello { .. }
                | Error::Malformed { .. }
                | Error::Protocol { .. }
        ),
        "{error}"
    );
}

#[test]
fn another_protocol_fails_the_start() {
    let error = HookExec::start(config("badhello"))
        .err()
        .expect("the start fails");
    assert!(matches!(error, Error::Protocol { .. }), "{error}");
    assert_eq!(error.class(), ErrorClass::Invalid);
}

#[test]
fn the_child_sees_only_the_safe_variables() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("env.json");
    let exec = HookExec::start(
        config("continue")
            .points(only(DecidePoint::ToolBefore))
            .env("ADAM_HOOK_TEST_ENV", dump.to_string_lossy())
            .env("ADAM_HOOK_TEST_ADDED", "yes"),
    )
    .unwrap();
    // The program writes its environment before it says hello; a call keeps it alive.
    let mut arguments = json!({});
    exec.before_tool(&mut HookedCall {
        tool: "read",
        arguments: &mut arguments,
    })
    .unwrap();
    let text = std::fs::read_to_string(&dump).expect("the program wrote its environment");
    let vars: BTreeMap<String, String> = serde_json::from_str(&text).expect("a map");
    assert_eq!(
        vars.get("ADAM_HOOK_TEST_ADDED").map(String::as_str),
        Some("yes"),
        "the configuration's variables reach the program"
    );
    // Nothing else reaches it: every variable is a safe one or one the configuration added. This
    // is the assertion that fails when `env_clear` is removed, not a single absent name.
    for name in vars.keys() {
        assert!(
            SAFE_VARS.contains(&name.as_str())
                || name == "ADAM_HOOK_TEST_ENV"
                || name == "ADAM_HOOK_TEST_ADDED",
            "the child inherited {name}: {vars:?}"
        );
    }
    if std::env::var_os("PATH").is_some() {
        assert!(
            vars.contains_key("PATH"),
            "a safe variable is kept: {vars:?}"
        );
    }
}

#[test]
fn a_rewrite_over_a_scrubbed_span_is_refused() {
    let exec = HookExec::start(
        config("rewrite")
            .points(only(DecidePoint::ToolBefore))
            .scrub(|text| text.replace("s3cret", "[redacted]")),
    )
    .unwrap();
    let mut arguments = json!({"token": "s3cret"});
    let error = exec
        .before_tool(&mut HookedCall {
            tool: "read",
            arguments: &mut arguments,
        })
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::Invalid, "{error}");
    assert_eq!(
        arguments,
        json!({"token": "s3cret"}),
        "the real call is untouched"
    );
}

#[test]
fn a_slow_program_misses_its_deadline() {
    let exec = HookExec::start(
        config("slow")
            .points(only(DecidePoint::ToolBefore))
            .deadline(Duration::from_millis(100)),
    )
    .unwrap();
    let mut arguments = json!({});
    let error = exec
        .before_tool(&mut HookedCall {
            tool: "read",
            arguments: &mut arguments,
        })
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::Transient, "{error}");
}

#[test]
fn three_deadlines_in_a_row_kill_the_program() {
    let exec = HookExec::start(
        config("slow")
            .points(only(DecidePoint::ToolBefore))
            .deadline(Duration::from_millis(50)),
    )
    .unwrap();
    for _ in 0..3 {
        let mut arguments = json!({});
        let error = exec
            .before_tool(&mut HookedCall {
                tool: "read",
                arguments: &mut arguments,
            })
            .unwrap_err();
        assert_eq!(error.class, ErrorClass::Transient, "{error}");
    }
    // The program is down for its backoff: its deciding points are transient, with no new request.
    let mut arguments = json!({});
    let error = exec
        .before_tool(&mut HookedCall {
            tool: "read",
            arguments: &mut arguments,
        })
        .unwrap_err();
    assert!(
        error.reason.contains("restarting"),
        "the program is killed and restarting: {error}"
    );
}

#[test]
fn a_program_that_dies_is_restarted_after_its_backoff() {
    let exec =
        HookExec::start(config("die-after:1").points(only(DecidePoint::ToolBefore))).unwrap();
    let mut arguments = json!({});
    exec.before_tool(&mut HookedCall {
        tool: "read",
        arguments: &mut arguments,
    })
    .unwrap();
    // The second event kills it: the request fails transiently, and it is restarted with backoff.
    let mut arguments = json!({});
    let error = exec
        .before_tool(&mut HookedCall {
            tool: "read",
            arguments: &mut arguments,
        })
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::Transient, "{error}");
    std::thread::sleep(Duration::from_millis(1200));
    let mut arguments = json!({});
    let verdict = exec
        .before_tool(&mut HookedCall {
            tool: "read",
            arguments: &mut arguments,
        })
        .unwrap();
    assert_eq!(verdict, Decision::Continue, "the restarted program answers");
}

#[test]
fn an_observer_that_decides_is_refused() {
    let exec = HookExec::start(config("deny").points(Points {
        decide: Vec::new(),
        observe: vec!["tool.completed".to_owned()],
    }))
    .unwrap();
    let event = HookEvent::new(EventKind::ToolCompleted, header());
    let error = exec.observe(&event).unwrap_err();
    assert_eq!(error.class, ErrorClass::Invalid, "{error}");

    let quiet = HookExec::start(config("continue").points(Points {
        decide: Vec::new(),
        observe: vec!["tool.completed".to_owned()],
    }))
    .unwrap();
    quiet.observe(&event).unwrap();
}

#[test]
fn a_configuration_may_not_override_a_safe_variable_or_add_a_dangerous_one() {
    for (name, value) in [
        ("PATH", "/tmp"),
        ("HOME", "/tmp"),
        ("LD_PRELOAD", "/tmp/evil.so"),
        ("BASH_ENV", "/tmp/evil.sh"),
        ("lower_case", "x"),
        ("1NUMBER", "x"),
    ] {
        let error = HookExec::start(config("continue").env(name, value))
            .err()
            .unwrap_or_else(|| panic!("{name} must be refused"));
        assert!(
            matches!(error, Error::Config { .. }),
            "{name} is a configuration error: {error}"
        );
    }
}

#[test]
fn sixteen_calls_at_once_each_get_their_own_reply() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("requests.log");
    let exec = Arc::new(
        HookExec::start(
            config("continue")
                .points(only(DecidePoint::ToolBefore))
                .env("ADAM_HOOK_TEST_LOG", log.to_string_lossy()),
        )
        .unwrap(),
    );
    let mut handles = Vec::new();
    for n in 0..16 {
        let exec = Arc::clone(&exec);
        handles.push(std::thread::spawn(move || {
            let mut arguments = json!({"n": n});
            exec.before_tool(&mut HookedCall {
                tool: "read",
                arguments: &mut arguments,
            })
        }));
    }
    for handle in handles {
        assert_eq!(handle.join().unwrap().unwrap(), Decision::Continue);
    }
    assert_eq!(requests(&log), 16, "each call was its own request");
}

#[test]
fn an_unusable_configuration_is_refused() {
    let error = HookExec::start(config("continue").deadline(Duration::ZERO))
        .err()
        .expect("a zero deadline is refused");
    assert!(matches!(error, Error::Config { .. }), "{error}");
    let error = HookExec::start(config("continue").points(Points {
        decide: Vec::new(),
        observe: vec!["not a pattern".to_owned()],
    }))
    .err()
    .expect("a bad pattern is refused");
    assert!(matches!(error, Error::Config { .. }), "{error}");
}
