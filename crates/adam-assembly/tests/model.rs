//! Models, state and the sources an agent can come from.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_agent_fs::{Dir, Strictness};
use adam_assembly::{AgentDef, AliasProblem, Assembly, Error};
use adam_llm_agent::{StateKey, Tool, ToolCtx, ToolError, ToolOutput, ToolSet};
use adam_model::{MockModel, ToolSpec};
use async_trait::async_trait;
use common::{def, instructions, manifests, runtime, spawn_worker, tools, wait_done, write};
use serde_json::{Value, json};

const INSTRUCTIONS: &str = "agent/instructions.md";

fn mock() -> Arc<MockModel> {
    Arc::new(MockModel::new())
}

fn alias_of(assembly: &Assembly, name: &str) -> String {
    assembly
        .info()
        .iter()
        .find(|i| i.name == name)
        .unwrap()
        .model_alias
        .clone()
}

// --- model aliases -------------------------------------------------------------------------

/// A root with `root_model`, a child with `child_model` and a grandchild with `grand_model`.
fn family(root_model: &str, child_model: &str, grand_model: &str) -> AgentDef {
    let line = |model: &str| {
        if model.is_empty() {
            String::new()
        } else {
            format!("\nmodel: {model}")
        }
    };
    def(&[
        (
            INSTRUCTIONS,
            &instructions(&format!("name: coder{}", line(root_model)), "Root."),
        ),
        (
            "agent/subagents/child/instructions.md",
            &instructions(
                &format!("description: Child.{}", line(child_model)),
                "Child.",
            ),
        ),
        (
            "agent/subagents/child/subagents/grand.md",
            &instructions(
                &format!("description: Grand.{}", line(grand_model)),
                "Grand.",
            ),
        ),
    ])
}

#[test]
fn an_agent_that_names_no_model_uses_the_alias_the_code_gives() {
    let assembly = family("", "", "")
        .bind(ToolSet::new())
        .unwrap()
        .model(mock(), "gateway")
        .unwrap();
    for name in ["coder", "coder/child", "coder/child/grand"] {
        assert_eq!(alias_of(&assembly, name), "gateway", "{name}");
    }
}

#[test]
fn a_named_alias_wins_and_inherit_takes_the_parents() {
    let assembly = family("big", "inherit", "small")
        .bind(ToolSet::new())
        .unwrap()
        .model(mock(), "gateway")
        .unwrap();
    assert_eq!(alias_of(&assembly, "coder"), "big");
    assert_eq!(alias_of(&assembly, "coder/child"), "big");
    assert_eq!(alias_of(&assembly, "coder/child/grand"), "small");

    // A child that names a model passes it down to a grandchild that names none.
    let assembly = family("", "mid", "")
        .bind(ToolSet::new())
        .unwrap()
        .model(mock(), "gateway")
        .unwrap();
    assert_eq!(alias_of(&assembly, "coder"), "gateway");
    assert_eq!(alias_of(&assembly, "coder/child"), "mid");
    assert_eq!(alias_of(&assembly, "coder/child/grand"), "mid");
}

#[test]
fn a_bad_alias_from_the_code_names_the_agent_that_uses_it() {
    for (alias, problem) in [
        ("", AliasProblem::Empty),
        ("   ", AliasProblem::Empty),
        ("two words", AliasProblem::NotAToken),
        ("tab\there", AliasProblem::NotAToken),
    ] {
        let error = family("", "", "")
            .bind(ToolSet::new())
            .unwrap()
            .model(mock(), alias)
            .unwrap_err();
        let Error::ModelAlias {
            origin,
            alias: got,
            problem: got_problem,
        } = &error
        else {
            panic!("{error}");
        };
        assert_eq!(origin.agent, "coder", "{error}");
        assert_eq!(got, alias);
        assert_eq!(*got_problem, problem);
    }
    // An alias the code gives but nobody uses is not checked: every agent names its own.
    family("big", "big", "big")
        .bind(ToolSet::new())
        .unwrap()
        .model(mock(), "")
        .unwrap();
}

#[test]
fn a_deployment_can_list_the_aliases_it_serves() {
    let bound = || family("coder-larg", "", "").bind(ToolSet::new()).unwrap();
    let error = bound()
        .model_aliases(["coder-large", "coder-small"])
        .model(mock(), "coder-small")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "agent `coder` (agent/instructions.md): model alias `coder-larg`: not one of the aliases \
         this deployment serves; did you mean `coder-large`?; allowed: `coder-large`, `coder-small`"
    );

    // The code's own default is checked too, for the agent that falls back to it.
    let error = family("", "", "")
        .bind(ToolSet::new())
        .unwrap()
        .model_aliases(["a"])
        .model(mock(), "b")
        .unwrap_err();
    let Error::ModelAlias { problem, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(
        *problem,
        AliasProblem::NotAllowed {
            suggestion: None,
            allowed: vec!["a".into()]
        }
    );

    // Listed aliases pass.
    family("a", "b", "")
        .bind(ToolSet::new())
        .unwrap()
        .model_aliases(vec![String::from("a"), String::from("b")])
        .model(mock(), "a")
        .unwrap();
}

// --- state ---------------------------------------------------------------------------------

struct Env;

/// A tool that reads shared state.
struct NeedsEnv;

#[async_trait]
impl Tool for NeedsEnv {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "needs_env".into(),
            description: "Reads the environment.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }
    fn required_state(&self) -> Vec<StateKey> {
        vec![StateKey::of::<Env>()]
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        ctx.require_state::<Env>()?;
        Ok(ToolOutput::text("saw the environment"))
    }
}

#[test]
fn a_tool_whose_state_is_missing_fails_when_the_agents_are_built() {
    let d = || def(&[(INSTRUCTIONS, &instructions("name: coder", "Hi."))]);
    let error = d()
        .bind(ToolSet::new().tool(NeedsEnv))
        .unwrap()
        .model(mock(), "m")
        .unwrap_err();
    let Error::Build { origin, source } = &error else {
        panic!("{error}");
    };
    assert_eq!(origin.agent, "coder");
    assert_eq!(origin.file.to_string_lossy(), INSTRUCTIONS);
    assert!(source.to_string().contains("needs shared state"), "{error}");
    assert!(
        error
            .to_string()
            .starts_with("agent `coder` (agent/instructions.md): tool `needs_env`"),
        "{error}"
    );

    let assembly = d()
        .bind(ToolSet::new().tool(NeedsEnv))
        .unwrap()
        .state(Arc::new(Env))
        .model(mock(), "m")
        .unwrap();
    assert_eq!(assembly.info()[0].tools, ["needs_env"]);
}

#[tokio::test]
async fn state_reaches_the_tools_of_a_running_agent() {
    let model = mock();
    model.push_tool_calls(vec![adam_model::ToolCall {
        id: "c1".into(),
        name: "needs_env".into(),
        arguments: json!({}),
    }]);
    model.push_text("done");
    let assembly = def(&[(INSTRUCTIONS, &instructions("name: coder", "Hi."))])
        .bind(ToolSet::new().tool(NeedsEnv))
        .unwrap()
        .state(Arc::new(Env))
        .model(model.clone(), "m")
        .unwrap();
    let rt = runtime(&assembly);
    let worker = spawn_worker(&rt);
    let run = rt
        .start("coder", adam_llm_agent::user_message("go"), None)
        .await
        .unwrap();
    wait_done(&rt, run).await;
    assert_eq!(
        model.requests()[1].messages.last().unwrap().text(),
        "saw the environment"
    );
    worker.stop().await;
}

// --- sources -------------------------------------------------------------------------------

#[test]
fn a_manifest_by_value_or_by_reference_is_the_same_definition() {
    let (_dir, agents) = manifests(&[(
        INSTRUCTIONS,
        &instructions("name: coder\nvars: { a: 1 }", "{{a}}"),
    )]);
    let by_ref = AgentDef::from_manifest(&agents[0]).unwrap();
    let by_value = AgentDef::from_manifest(agents[0].clone()).unwrap();
    assert_eq!(by_ref.name(), "coder");
    assert_eq!(by_ref.manifest(), by_value.manifest());
    assert_eq!(by_value.manifest(), &agents[0]);
}

#[test]
fn a_package_of_several_agents_gives_one_definition_each() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("agents/alpha/instructions.md", "Alpha."),
            ("agents/beta/instructions.md", "Beta."),
        ],
    );
    let defs = AgentDef::from_source(&Dir::new(dir.path()), Strictness::Lenient).unwrap();
    let names: Vec<&str> = defs.iter().map(AgentDef::name).collect();
    assert_eq!(names, ["alpha", "beta"]);
}

#[test]
fn a_source_with_errors_is_refused_with_its_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    // A subagent without a description is an error of the files.
    write(
        dir.path(),
        &[
            (INSTRUCTIONS, "Root."),
            ("agent/subagents/x.md", "---\nmodel: inherit\n---\nBody.\n"),
        ],
    );
    let error = AgentDef::from_source(&Dir::new(dir.path()).default_name("r"), Strictness::Lenient)
        .unwrap_err();
    assert!(
        matches!(error, Error::Manifest(adam_agent_fs::Error::Invalid { .. })),
        "{error}"
    );
    assert!(
        error.to_string().contains("needs a `description`"),
        "{error}"
    );

    // A directory that is not there cannot be read.
    let missing =
        AgentDef::from_source(&Dir::new(dir.path().join("nope")), Strictness::Lenient).unwrap_err();
    assert!(matches!(missing, Error::Manifest(_)), "{missing}");

    // Warnings pass a lenient load and fail a strict one.
    write(
        dir.path(),
        &[(
            "agent/subagents/x.md",
            "---\ndescription: X.\ncolor: red\n---\nBody.\n",
        )],
    );
    let source = Dir::new(dir.path()).default_name("r");
    assert!(AgentDef::from_source(&source, Strictness::Lenient).is_ok());
    assert!(AgentDef::from_source(&source, Strictness::Strict).is_err());
}

#[test]
fn a_registered_runtime_knows_every_agent() {
    let assembly = family("", "", "")
        .bind(tools(&[]))
        .unwrap()
        .model(mock(), "m")
        .unwrap();
    let rt = runtime(&assembly);
    assert_eq!(
        rt.agent_names(),
        ["coder", "coder/child", "coder/child/grand"]
    );
    assert_eq!(assembly.agents().len(), 3);
    assert_eq!(assembly.root().limits().max_turns, 50);
    let text = format!("{assembly:?}");
    assert!(text.contains("coder/child"), "{text}");
}

#[test]
fn the_bound_stage_can_be_printed() {
    let bound = family("", "", "")
        .bind(tools(&[]))
        .unwrap()
        .model_aliases(["a"]);
    let text = format!("{bound:?}");
    assert!(
        text.contains("coder/child/grand") && text.contains("\"a\""),
        "{text}"
    );
    let def = family("", "", "");
    assert!(format!("{def:?}").contains("AgentDef"));
}

// --- tool sources ----------------------------------------------------------------------------

/// A source that offers `ping` to every agent and answers it.
struct PingSource;

#[async_trait]
impl adam_llm_agent::ToolSource for PingSource {
    async fn specs(&self, _ctx: &adam_llm_agent::SourceCtx) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "ping".into(),
            description: "ping".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }]
    }

    async fn call(
        &self,
        _ctx: &ToolCtx,
        name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        (name == "ping").then(|| Ok(ToolOutput::text("pong")))
    }
}

#[tokio::test]
async fn a_tool_source_is_given_to_every_agent_and_the_model_is_offered_its_tools() {
    let model = mock();
    model
        .push_tool_calls(vec![adam_model::ToolCall {
            id: "c1".into(),
            name: "ping".into(),
            arguments: json!({}),
        }])
        .push_text("done");
    let assembly = def(&[(INSTRUCTIONS, &instructions("name: coder", "Root."))])
        .bind(tools(&["own"]))
        .unwrap()
        .tool_source(PingSource)
        .model(model.clone(), "gateway")
        .unwrap();
    let rt = runtime(&assembly);
    let worker = spawn_worker(&rt);
    let run = rt
        .start("coder", adam_llm_agent::user_message("go"), None)
        .await
        .unwrap();
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let requests = model.requests();
    let offered: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        offered,
        ["own", "ping"],
        "its own tools first, then the source's"
    );
    assert_eq!(view.output.unwrap()["text"], "done");
    // The source's answer was the tool result.
    assert_eq!(
        requests[1].messages.last(),
        Some(&adam_model::Message::tool_result("c1", "pong"))
    );
}
