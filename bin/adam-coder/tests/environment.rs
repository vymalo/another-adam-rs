//! The tools that run a process (`run_command`, `run_checks`, `delegate_to_opencode`) go through
//! the run's environment: the command is the one its session prepared, a timeout or a cancel tells
//! the session, what the environment says while it is made is shown as steps, and a failure to make
//! it is a result for the model. (The janitor's part is in `tests/janitor.rs`; the default,
//! `Local`, is what every other test runs on.)
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use adam_coder::opencode::OpenCodeLaunch;
use adam_coder::tools::checks::RunChecks;
use adam_coder::tools::delegate::DelegateToOpenCode;
use adam_coder::tools::inspect::RunCommand;
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_coder::tools::prepare::PrepareWorkspace;
use adam_llm_agent::{Tool, ToolCtx, ToolError};
use adam_runtime::{CancelToken, CollectingSink, RunEvent, StepKind, StepState};
use adam_workspace::{EnvError, EnvStep, EnvStepState, Program};
use common::{FakeEnvironment, Fixture};
use serde_json::json;

struct Rig {
    fx: Fixture,
    fake: Arc<FakeEnvironment>,
    sink: CollectingSink,
    ctx: ToolCtx,
}

impl Rig {
    async fn new() -> Self {
        Self::from(Fixture::new("hello\n").await).await
    }

    async fn from(fx: Fixture) -> Self {
        let fake = FakeEnvironment::new(Some(&fx.root));
        let fx = fx.using(fake.clone());
        let sink = CollectingSink::new();
        let ctx =
            ToolCtx::detached("tool", "call-1", Arc::new(sink.clone())).with_state(fx.env.clone());
        Self {
            fx,
            fake,
            sink,
            ctx,
        }
    }

    /// The same rig, with a token that cancels the run.
    fn cancellable(mut self) -> (Self, CancelToken) {
        let token = CancelToken::new();
        self.ctx = self.ctx.with_cancel_token(token.clone());
        (self, token)
    }

    /// The person names the repository and the workspace is made.
    async fn prepare(&self) {
        let run = self.ctx.run_id().to_string();
        let mut notes = self.fx.env.notes.load(&run).await.unwrap();
        notes.name_repos(named_in(
            &without_untrusted(&self.fx.remote_url()),
            &self.fx.env.settings.default_repo_host,
        ));
        self.fx.env.notes.save(&run, &notes).await.unwrap();
        let out = PrepareWorkspace
            .call(
                &self.ctx,
                json!({"repo_url": self.fx.remote_url(), "base_branch": "main"}),
            )
            .await
            .expect("prepare");
        assert!(!out.is_error, "{}", out.content);
    }

    fn worktree(&self) -> PathBuf {
        common::slot_dir(&self.fx.root, &self.ctx.run_id().to_string())
    }

    fn steps(&self) -> Vec<adam_runtime::StepEvent> {
        self.sink
            .events()
            .into_iter()
            .filter_map(|e| match e.event {
                RunEvent::Step(step) => Some(step),
                _ => None,
            })
            .collect()
    }
}

/// Everything the specs carry, as text: no secret may be in it.
fn text_of(rig: &Rig) -> String {
    format!("{:?}", rig.fake.session.specs.lock().unwrap())
}

#[tokio::test]
async fn a_command_and_a_check_run_as_the_environment_prepared_them() {
    let rig = Rig::new().await;
    rig.prepare().await;

    let looked = RunCommand
        .call(&rig.ctx, json!({"command": "echo looking-in=$FAKE_ENV"}))
        .await
        .unwrap();
    assert!(!looked.is_error, "{}", looked.content);
    assert!(
        looked.content.contains("looking-in=fake"),
        "{}",
        looked.content
    );
    let checked = RunChecks
        .call(&rig.ctx, json!({"command": "echo checking-in=$FAKE_ENV"}))
        .await
        .unwrap();
    assert!(!checked.is_error, "{}", checked.content);
    assert!(
        checked.content.contains("checking-in=fake"),
        "{}",
        checked.content
    );

    let run = rig.ctx.run_id().to_string();
    assert_eq!(
        *rig.fake.ensured.lock().unwrap(),
        [run.clone(), run],
        "each tool asks for the session of its run (the environment makes it once)"
    );
    let specs = rig.fake.session.specs.lock().unwrap().clone();
    assert_eq!(specs.len(), 2, "{specs:?}");
    let worktree = rig.worktree().canonicalize().unwrap();
    for (spec, command) in specs
        .iter()
        .zip(["echo looking-in=$FAKE_ENV", "echo checking-in=$FAKE_ENV"])
    {
        assert_eq!(spec.program, Program::Shell(command.to_owned()));
        assert_eq!(spec.cwd, worktree, "the same path in every environment");
        // A command of the project's is repository code: it must not see this process's secrets.
        for name in [
            "GITHUB_TOKEN",
            "DATABASE_URL",
            "A2A_BEARER_TOKENS",
            "MODEL_API_KEY",
        ] {
            assert!(
                spec.hide.iter().any(|hidden| hidden == name),
                "{name}: {spec:?}"
            );
        }
        assert!(
            spec.env.is_empty(),
            "no secret is ever given as a value: {spec:?}"
        );
    }
    let said = text_of(&rig);
    for secret in common::SECRETS {
        assert!(!said.contains(secret), "{said}");
    }
}

#[tokio::test]
async fn opencode_is_started_from_the_command_the_environment_prepared() {
    let rig = Rig::new().await;
    rig.fake.session.extra_env.lock().unwrap().insert(
        "FAKE_ACP_WRITE_CONTENT".to_owned(),
        "written under the environment\n".to_owned(),
    );
    rig.prepare().await;

    let out = DelegateToOpenCode
        .call(&rig.ctx, json!({"instructions": "add hello.txt"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(rig.worktree().join("hello.txt")).unwrap(),
        "written under the environment\n",
        "the process that ran had the environment the session prepared"
    );

    let specs = rig.fake.session.specs.lock().unwrap().clone();
    assert_eq!(specs.len(), 1, "{specs:?}");
    let spec = &specs[0];
    assert_eq!(
        spec.program,
        Program::Argv(vec![common::fake_agent().as_os_str().to_owned()])
    );
    assert_eq!(
        spec.cwd,
        rig.worktree().canonicalize().unwrap_or(rig.worktree())
    );
    // OpenCode reads the model key by reference; the three secrets it has no use for are hidden.
    assert_eq!(
        spec.hide,
        ["GITHUB_TOKEN", "DATABASE_URL", "A2A_BEARER_TOKENS"]
    );
    assert_eq!(
        spec.env.get("FAKE_ACP_SCENARIO").map(String::as_str),
        Some("write-file")
    );
    let said = text_of(&rig);
    for secret in common::SECRETS {
        assert!(!said.contains(secret), "{said}");
    }
    assert!(
        rig.fake.session.killed.lock().unwrap().is_empty(),
        "an OpenCode that ends its turn is not killed"
    );
}

#[tokio::test]
async fn a_check_that_times_out_is_killed_in_the_environment_too() {
    let fx = Fixture::with("hello\n", |s| {
        s.check_timeout = Duration::from_millis(500);
    })
    .await;
    let rig = Rig::from(fx).await;
    rig.prepare().await;

    let out = RunChecks
        .call(&rig.ctx, json!({"command": "sleep 30"}))
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("TIMED OUT"),
        "{}",
        out.content
    );
    let prepared = rig.fake.session.prepared.lock().unwrap().clone();
    let killed = rig.fake.session.killed.lock().unwrap().clone();
    assert_eq!(prepared.len(), 1);
    assert_eq!(
        killed, prepared,
        "the session is told which command timed out"
    );
}

#[tokio::test]
async fn a_cancelled_opencode_is_killed_in_the_environment_too() {
    let agent_dir = tempfile::tempdir().unwrap();
    let pid_file = agent_dir.path().join("agent.pid");
    let child_file = agent_dir.path().join("child.pid");
    let fx = Fixture::with("hello\n", |s| {
        s.opencode = OpenCodeLaunch::program(common::fake_agent())
            .env("FAKE_ACP_SCENARIO", "stubborn")
            .env("FAKE_ACP_PID_FILE", pid_file.to_string_lossy())
            .env("FAKE_ACP_CHILD_PID_FILE", child_file.to_string_lossy());
    })
    .await;
    let (rig, token) = Rig::from(fx).await.cancellable();
    rig.prepare().await;

    let cancel_when_running = async {
        let grandchild = common::wait_for_pid(&child_file).await;
        token.cancel();
        grandchild
    };
    let (out, grandchild) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(
            DelegateToOpenCode.call(&rig.ctx, json!({"instructions": "never finish"})),
            cancel_when_running
        )
    })
    .await
    .expect("the tool returns after a cancel");

    assert!(
        matches!(&out, Err(ToolError::Permanent(m)) if m.contains("cancelled")),
        "{out:?}"
    );
    let prepared = rig.fake.session.prepared.lock().unwrap().clone();
    let killed = rig.fake.session.killed.lock().unwrap().clone();
    assert_eq!(prepared.len(), 1);
    assert_eq!(
        killed, prepared,
        "the session is told which command was cancelled"
    );
    assert!(common::wait_gone(grandchild, false, Duration::from_secs(5)).await);
}

#[tokio::test]
async fn what_the_environment_says_while_it_is_made_is_shown_as_steps_of_the_tool_call() {
    let rig = Rig::new().await;
    *rig.fake.steps.lock().unwrap() = vec![
        EnvStep::new("pull", "Pulling the image", EnvStepState::Running),
        EnvStep::new("pull", "Pulling the image", EnvStepState::Completed)
            .with_detail(format!("authenticated with {}", common::MODEL_KEY)),
        EnvStep::new("build", "Building the image", EnvStepState::Failed),
    ];
    rig.prepare().await;

    let out = RunCommand
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);

    let run = rig.ctx.run_id().to_string();
    let steps: Vec<_> = rig
        .steps()
        .into_iter()
        .filter(|step| step.id.starts_with("env:"))
        .collect();
    assert_eq!(steps.len(), 3, "{steps:?}");
    assert_eq!(steps[0].id, format!("env:{run}:pull"));
    assert_eq!(steps[1].id, steps[0].id, "the same id updates the step");
    assert_eq!(steps[2].id, format!("env:{run}:build"));
    assert_eq!(
        steps.iter().map(|s| s.state).collect::<Vec<_>>(),
        [StepState::Running, StepState::Completed, StepState::Failed]
    );
    for step in &steps {
        assert_eq!(step.kind, StepKind::Command);
        assert_eq!(
            step.parent.as_deref(),
            Some("tool:call-1"),
            "under the tool call"
        );
    }
    assert_eq!(steps[0].label, "Pulling the image");
    let detail = steps[1].detail.clone().unwrap();
    assert!(!detail.contains(common::MODEL_KEY), "scrubbed: {detail}");
    assert!(detail.contains("[redacted]"), "{detail}");
}

#[tokio::test]
async fn an_environment_that_cannot_be_made_is_a_result_for_the_model_and_nothing_runs() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let marker = rig.worktree().join("ran.txt");
    let command = json!({"command": "touch ran.txt"});

    // A build that failed: the repository's own configuration is at fault; say what the build said.
    *rig.fake.ensure_fails.lock().unwrap() = Some(|| EnvError::Build {
        reason: "the image build exited with 1".to_owned(),
        log_tail: format!("npm ERR! 401 with {}", common::MODEL_KEY),
    });
    for out in [
        RunCommand.call(&rig.ctx, command.clone()).await,
        RunChecks.call(&rig.ctx, command.clone()).await,
        DelegateToOpenCode
            .call(&rig.ctx, json!({"instructions": "anything"}))
            .await,
    ] {
        let Err(ToolError::Permanent(message)) = out else {
            panic!("a permanent error was expected: {out:?}");
        };
        assert!(message.contains("the work environment"), "{message}");
        assert!(
            message.contains("the image build exited with 1"),
            "{message}"
        );
        assert!(
            message.contains("npm ERR! 401"),
            "the end of the build log: {message}"
        );
        assert!(!message.contains(common::MODEL_KEY), "scrubbed: {message}");
    }
    assert!(!marker.exists(), "nothing ran");
    let notes = rig
        .fx
        .env
        .notes
        .load(&rig.ctx.run_id().to_string())
        .await
        .unwrap();
    assert_eq!(notes.checks.failures, 0, "no check cycle was used");
    assert!(
        notes.checks.last.is_none(),
        "and nothing was recorded for the gate"
    );

    // A runtime that is down now: worth another try.
    *rig.fake.ensure_fails.lock().unwrap() =
        Some(|| EnvError::Unavailable("podman is not running".to_owned()));
    let out = RunCommand.call(&rig.ctx, command).await;
    assert!(
        matches!(&out, Err(ToolError::Transient(m)) if m.contains("podman is not running")),
        "{out:?}"
    );

    // And it is made again at the next call, once it can be.
    *rig.fake.ensure_fails.lock().unwrap() = None;
    let out = RunCommand
        .call(&rig.ctx, json!({"command": "echo made"}))
        .await
        .unwrap();
    assert!(
        !out.is_error && out.content.contains("made"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn a_cancel_stops_the_wait_for_an_environment() {
    let (rig, token) = Rig::new().await.cancellable();
    rig.prepare().await;
    rig.fake.hang.store(true, Ordering::SeqCst);

    let cancel = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        token.cancel();
    };
    let (out, ()) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(
            RunCommand.call(&rig.ctx, json!({"command": "true"})),
            cancel
        )
    })
    .await
    .expect("the tool stops waiting after a cancel");
    assert!(
        matches!(&out, Err(ToolError::Permanent(m)) if m.contains("cancelled")),
        "{out:?}"
    );
    assert!(
        rig.fake.session.specs.lock().unwrap().is_empty(),
        "nothing was prepared"
    );
}
