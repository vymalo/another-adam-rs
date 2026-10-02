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
            "GITHUB_APP_PRIVATE_KEY",
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
    // OpenCode reads the model key by reference; the secrets it has no use for are hidden (the token,
    // the GitHub App's key, the database, the A2A tokens).
    assert_eq!(
        spec.hide,
        [
            "GITHUB_TOKEN",
            "GITHUB_APP_PRIVATE_KEY",
            "DATABASE_URL",
            "A2A_BEARER_TOKENS"
        ]
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

// --------------------------------------------------------------- a broken environment, and the way out

use adam_coder::tools::environment::RebuildEnvironment;
use adam_workspace::EnvKind;

fn broken_file() -> EnvError {
    EnvError::Refused(
        ".devcontainer/devcontainer.json: privileged is not allowed (a devcontainer may not ask for it)"
            .to_owned(),
    )
}

#[tokio::test]
async fn a_broken_environment_is_a_result_that_says_to_ask_the_person_for_every_tool_that_needs_it()
{
    let rig = Rig::new().await;
    rig.prepare().await;
    *rig.fake.ensure_fails.lock().unwrap() = Some(broken_file);

    let command = RunCommand
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap_err();
    let checks = RunChecks
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap_err();
    let opencode = DelegateToOpenCode
        .call(&rig.ctx, json!({"instructions": "change it"}))
        .await
        .unwrap_err();
    for (tool, err) in [
        ("run_command", command),
        ("run_checks", checks),
        ("delegate_to_opencode", opencode),
    ] {
        let ToolError::Permanent(text) = err else {
            panic!("{tool}: a broken environment is not worth retrying: {err:?}")
        };
        assert!(
            text.contains(".devcontainer/devcontainer.json") && text.contains("privileged"),
            "{tool}: names the file and the problem: {text}"
        );
        assert!(
            text.contains("ask_user")
                && text.contains("rebuild_environment")
                && text.contains("use_default: true")
                && text.contains("not something to work around"),
            "{tool}: says what to do: {text}"
        );
    }
    assert!(
        rig.fake.session.specs.lock().unwrap().is_empty(),
        "nothing was run, in this container or anywhere"
    );
    assert!(
        rig.fake.rebuilt.lock().unwrap().is_empty(),
        "the coder never rebuilds by itself: that is the person's decision"
    );
}

#[tokio::test]
async fn an_environment_that_is_not_there_for_now_is_worth_retrying_and_is_not_broken() {
    let rig = Rig::new().await;
    rig.prepare().await;
    *rig.fake.ensure_fails.lock().unwrap() =
        Some(|| EnvError::Unavailable("the Podman service does not answer".to_owned()));
    let err = RunCommand
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap_err();
    let ToolError::Transient(text) = err else {
        panic!("{err:?}")
    };
    assert!(!text.contains("rebuild_environment"), "{text}");
}

#[tokio::test]
async fn rebuilding_with_the_default_goes_on_and_the_choice_is_noted() {
    let rig = Rig::new().await;
    rig.prepare().await;
    *rig.fake.ensure_fails.lock().unwrap() = Some(broken_file);
    RunCommand
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap_err();

    let out = RebuildEnvironment
        .call(&rig.ctx, json!({"use_default": true}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("made again")
            && out.content.contains("not used for the rest of this run")
            && out.content.contains("Say so in your final answer"),
        "{}",
        out.content
    );
    let run = rig.ctx.run_id().to_string();
    assert_eq!(*rig.fake.rebuilt.lock().unwrap(), [(run.clone(), true)]);
    let notes = rig.fx.env.notes.load(&run).await.unwrap();
    assert!(notes.environment.use_default);

    // The environment is there again: the command runs.
    let looked = RunCommand
        .call(&rig.ctx, json!({"command": "echo in=$FAKE_ENV"}))
        .await
        .unwrap();
    assert!(
        !looked.is_error && looked.content.contains("in=fake"),
        "{}",
        looked.content
    );
}

#[tokio::test]
async fn rebuilding_without_use_default_makes_it_again_from_the_file_and_forgets_what_it_knew() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let run = rig.ctx.run_id().to_string();
    let mut notes = rig.fx.env.notes.load(&run).await.unwrap();
    notes.environment.opencode = Some(adam_coder::tools::notes::OpenCodeCheck {
        works: false,
        detail: "exec format error".to_owned(),
    });
    rig.fx.env.notes.save(&run, &notes).await.unwrap();

    let out = RebuildEnvironment.call(&rig.ctx, json!({})).await.unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(
        !out.content.contains("not used for the rest"),
        "{}",
        out.content
    );
    assert_eq!(*rig.fake.rebuilt.lock().unwrap(), [(run.clone(), false)]);
    let notes = rig.fx.env.notes.load(&run).await.unwrap();
    assert!(!notes.environment.use_default);
    assert_eq!(
        notes.environment.opencode, None,
        "a new environment is asked again whether OpenCode starts in it"
    );
}

#[tokio::test]
async fn rebuilding_where_commands_run_in_the_coders_own_environment_says_there_is_nothing_to_rebuild()
 {
    let rig = Rig::new().await;
    rig.prepare().await;
    rig.fake.rebuild_says.store(false, Ordering::SeqCst);
    let out = RebuildEnvironment
        .call(&rig.ctx, json!({"use_default": true}))
        .await
        .unwrap();
    assert!(
        !out.is_error && out.content.contains("Nothing was rebuilt"),
        "{}",
        out.content
    );
    assert!(
        rig.fake.ensured.lock().unwrap().is_empty(),
        "no environment was asked for"
    );
    let notes = rig
        .fx
        .env
        .notes
        .load(&rig.ctx.run_id().to_string())
        .await
        .unwrap();
    assert!(!notes.environment.use_default, "nothing was decided");
}

#[tokio::test]
async fn a_rebuild_that_fails_again_is_the_same_kind_of_result() {
    let rig = Rig::new().await;
    rig.prepare().await;
    // The rebuild clears the fake's failure; a build that fails on the way is reported as steps and
    // as the call's result, with the way out again.
    rig.fake
        .ensure_fails_after_rebuild
        .lock()
        .unwrap()
        .replace(|| EnvError::Build {
            reason: "the image does not build".to_owned(),
            log_tail: "step 3/4: no such package nonesuch".to_owned(),
        });
    let err = RebuildEnvironment
        .call(&rig.ctx, json!({}))
        .await
        .unwrap_err();
    let ToolError::Permanent(text) = err else {
        panic!("{err:?}")
    };
    assert!(
        text.contains("does not build")
            && text.contains("no such package nonesuch")
            && text.contains("rebuild_environment"),
        "{text}"
    );
}

// --------------------------------------------------------------- where OpenCode runs, and its key

fn scripted_opencode(dir: &std::path::Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("opencode-stub");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn opencode_in_a_container_is_the_mounted_copy_and_reads_the_key_from_a_file() {
    let launch = OpenCodeLaunch::from_command(
        &[common::fake_agent().to_string_lossy().into_owned()],
        "http://gateway.example/v1",
        "model-x",
    )
    .env("FAKE_ACP_SCENARIO", "write-file");
    let fx = Fixture::with("hello\n", |s| s.opencode = launch).await;
    let rig = Rig::from(fx).await;
    // The coder's own copy, which the environment mounts and names: the same binary here.
    *rig.fake.session.opencode_at.lock().unwrap() = Some(common::fake_agent().to_path_buf());
    rig.prepare().await;

    let out = DelegateToOpenCode
        .call(&rig.ctx, json!({"instructions": "add hello.txt"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);

    let specs = rig.fake.session.specs.lock().unwrap().clone();
    // `opencode --version` first (it is asked once per environment), then OpenCode itself.
    assert_eq!(specs.len(), 2, "{specs:?}");
    assert_eq!(
        specs[0].program,
        Program::Argv(vec![
            common::fake_agent().as_os_str().to_owned(),
            "--version".into()
        ])
    );
    let spec = &specs[1];
    assert_eq!(
        spec.program,
        Program::Argv(vec![common::fake_agent().as_os_str().to_owned()])
    );
    let config = spec
        .env
        .get("OPENCODE_CONFIG_CONTENT")
        .expect("the inline configuration");
    assert!(
        config.contains("{file:/run/secrets/model-key}"),
        "the key is a reference to the file the environment gives: {config}"
    );
    assert!(!config.contains("{env:MODEL_API_KEY}"), "{config}");
    let said = text_of(&rig);
    for secret in common::SECRETS {
        assert!(
            !said.contains(secret),
            "the value of a secret is in no argument or variable: {said}"
        );
    }
}

#[tokio::test]
async fn opencode_that_cannot_start_in_the_environment_is_refused_once_and_the_other_tools_work() {
    let dir = tempfile::tempdir().unwrap();
    let stub = scripted_opencode(
        dir.path(),
        "echo 'exec /opt/adam/bin/opencode: no such file or directory (musl)' >&2; exit 127",
    );
    let launch = OpenCodeLaunch::from_command(
        &[stub.to_string_lossy().into_owned()],
        "http://gateway.example/v1",
        "model-x",
    );
    let fx = Fixture::with("hello\n", |s| s.opencode = launch).await;
    let rig = Rig::from(fx).await;
    rig.prepare().await;

    for _ in 0..2 {
        let out = DelegateToOpenCode
            .call(&rig.ctx, json!({"instructions": "add hello.txt"}))
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        for expected in [
            "OpenCode cannot start in this workspace's environment",
            "exit code 127",
            "musl",
            "read_file, write_file and apply_patch",
        ] {
            assert!(
                out.content.contains(expected),
                "{expected}: {}",
                out.content
            );
        }
    }
    assert_eq!(
        rig.fake.session.specs.lock().unwrap().len(),
        1,
        "`opencode --version` ran once; the second call used the note"
    );
    // The other tools are not affected.
    let looked = RunCommand
        .call(&rig.ctx, json!({"command": "echo still=$FAKE_ENV"}))
        .await
        .unwrap();
    assert!(looked.content.contains("still=fake"), "{}", looked.content);
}

#[tokio::test]
async fn a_container_with_no_network_has_no_opencode_and_says_what_to_use() {
    let launch = OpenCodeLaunch::from_command(
        &[common::fake_agent().to_string_lossy().into_owned()],
        "http://gateway.example/v1",
        "model-x",
    );
    let fx = Fixture::with("hello\n", |s| {
        s.opencode = launch;
        s.container_network_none = true;
    })
    .await;
    let rig = Rig::from(fx).await;
    rig.prepare().await;
    let out = DelegateToOpenCode
        .call(&rig.ctx, json!({"instructions": "add hello.txt"}))
        .await
        .unwrap();
    assert!(
        out.is_error
            && out.content.contains("DEVCONTAINER_NETWORK=none")
            && out.content.contains("apply_patch"),
        "{}",
        out.content
    );
    assert!(rig.fake.session.specs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn in_the_coders_own_container_opencode_is_not_probed_and_the_network_setting_is_not_its_business()
 {
    let launch = OpenCodeLaunch::from_command(
        &[common::fake_agent().to_string_lossy().into_owned()],
        "http://gateway.example/v1",
        "model-x",
    )
    .env("FAKE_ACP_SCENARIO", "write-file");
    let fx = Fixture::with("hello\n", |s| {
        s.opencode = launch;
        s.container_network_none = true;
    })
    .await;
    let rig = Rig::from(fx).await;
    *rig.fake.session.kind.lock().unwrap() = Some(EnvKind::Local);
    rig.prepare().await;
    let out = DelegateToOpenCode
        .call(&rig.ctx, json!({"instructions": "add hello.txt"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    let specs = rig.fake.session.specs.lock().unwrap().clone();
    assert_eq!(specs.len(), 1, "no `--version` first: {specs:?}");
}

// --------------------------------------------------------------- what a check reports, and a missing tool

#[tokio::test]
async fn the_checks_artifact_says_where_it_ran_when_that_was_a_devcontainer() {
    let rig = Rig::new().await;
    *rig.fake.session.kind.lock().unwrap() = Some(EnvKind::DevContainer {
        source: Some(PathBuf::from(
            "/work/workspaces/r/remote/.devcontainer/devcontainer.json",
        )),
        image: "vsc-remote-abc".to_owned(),
    });
    rig.prepare().await;
    let out = RunChecks
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.artifacts[0].data["environment"],
        json!({
            "kind": "devcontainer",
            "source": ".devcontainer/devcontainer.json",
            "image": "vsc-remote-abc"
        })
    );

    // The default image has no source; the coder's own container has no `environment` at all.
    *rig.fake.session.kind.lock().unwrap() = Some(EnvKind::DevContainer {
        source: None,
        image: "default:1".to_owned(),
    });
    let out = RunChecks
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap();
    assert_eq!(
        out.artifacts[0].data["environment"],
        json!({"kind": "devcontainer", "image": "default:1"})
    );
    *rig.fake.session.kind.lock().unwrap() = Some(EnvKind::Local);
    let out = RunChecks
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap();
    assert!(
        out.artifacts[0].data.get("environment").is_none(),
        "{}",
        out.artifacts[0].data
    );
}

#[tokio::test]
async fn a_missing_tool_says_which_environment_lacks_it_and_how_it_gets_there() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let missing = |rig: &Rig| {
        let ctx = rig.ctx.clone();
        async move {
            RunChecks
                .call(&ctx, json!({"command": "devbox-tool --version"}))
                .await
                .unwrap()
        }
    };
    *rig.fake.session.kind.lock().unwrap() = Some(EnvKind::DevContainer {
        source: Some(PathBuf::from(
            "/work/workspaces/r/remote/.devcontainer/devcontainer.json",
        )),
        image: "vsc-remote-abc".to_owned(),
    });
    let out = missing(&rig).await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("no `devbox-tool`")
            && out.content.contains("`.devcontainer/devcontainer.json`")
            && out
                .content
                .contains("a devcontainer feature, or its Dockerfile")
            && out.content.contains("rebuild_environment"),
        "{}",
        out.content
    );
    assert!(out.artifacts.is_empty(), "a missing tool is not a check");

    *rig.fake.session.kind.lock().unwrap() = Some(EnvKind::DevContainer {
        source: None,
        image: "mcr.example/base:1".to_owned(),
    });
    let out = RunChecks
        .call(&rig.ctx, json!({"command": "devbox-tool --version"}))
        .await
        .unwrap();
    assert!(
        out.content.contains("has no devcontainer")
            && out
                .content
                .contains("default environment (mcr.example/base:1)")
            && out
                .content
                .contains("a `.devcontainer/devcontainer.json` in the repository can provide it"),
        "{}",
        out.content
    );

    *rig.fake.session.kind.lock().unwrap() = Some(EnvKind::Local);
    let out = RunChecks
        .call(&rig.ctx, json!({"command": "devbox-tool --version"}))
        .await
        .unwrap();
    assert!(
        out.content.contains("no `devbox-tool`") && !out.content.contains("devcontainer"),
        "this container's answer is what it always was: {}",
        out.content
    );
}

// --------------------------------------------------------------- the real environment, composed as the binary does

/// A coder configured for devcontainers whose Podman service does not answer: its client is a script
/// that records every call and says the service is down.
#[tokio::test]
async fn a_service_that_does_not_answer_gives_the_coders_own_container_with_one_step_per_run() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new("hello\n").await;
    let bin = tempfile::tempdir().unwrap();
    let calls = bin.path().join("calls");
    let client = bin.path().join("podman-remote");
    std::fs::write(
        &client,
        format!(
            "#!/bin/sh\necho \"$1\" >> {}\necho 'Cannot connect to Podman: connection refused' >&2\nexit 125\n",
            calls.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&client, std::fs::Permissions::from_mode(0o755)).unwrap();
    let opencode = bin.path().join("opencode");
    std::fs::write(&opencode, b"\x7fELF this is only a stub").unwrap();
    std::fs::set_permissions(&opencode, std::fs::Permissions::from_mode(0o755)).unwrap();

    let vars: std::collections::HashMap<&str, String> = [
        ("DATABASE_URL", "postgres://u:p@127.0.0.1:1/x".to_owned()),
        ("MODEL_BASE_URL", "http://127.0.0.1:9/v1".to_owned()),
        ("MODEL_API_KEY", "sk-from-the-gateway".to_owned()),
        ("MODEL", "m".to_owned()),
        ("GITHUB_TOKEN", "ghp_x".to_owned()),
        ("A2A_BEARER_TOKENS", "t".to_owned()),
        ("PUBLIC_URL", "http://coder.test/".to_owned()),
        ("DEVCONTAINER_RUNTIME", "podman".to_owned()),
        (
            "CONTAINER_HOST",
            "unix:///nonexistent/podman.sock".to_owned(),
        ),
        ("DEVCONTAINER_PODMAN", client.to_string_lossy().into_owned()),
        ("DEVCONTAINER_PREPULL", "false".to_owned()),
        ("OPENCODE_BINARY", opencode.to_string_lossy().into_owned()),
    ]
    .into();
    let config = adam_coder::Config::from_lookup(|k| vars.get(k).cloned()).unwrap();
    let worker = config.worker.expect("a worker");
    let environment = adam_coder::environment_for(&worker, &fx.root)
        .await
        .expect("a service that does not answer does not stop the start");
    assert!(
        std::fs::read_dir(fx.root.join("environments/.tools"))
            .unwrap()
            .flatten()
            .any(|dir| dir.path().join("adam-exec").is_file()),
        "the tools directory is written at start"
    );
    let asked = |calls: &std::path::Path| {
        std::fs::read_to_string(calls)
            .unwrap_or_default()
            .lines()
            .count()
    };
    assert!(asked(&calls) >= 1, "the service was probed at start");

    let fake = FakeEnvironment::new(None);
    let fx = fx.using(environment);
    let sink = CollectingSink::new();
    let ctx =
        ToolCtx::detached("tool", "call-1", Arc::new(sink.clone())).with_state(fx.env.clone());
    let rig = Rig {
        fx,
        fake,
        sink,
        ctx,
    };
    rig.prepare().await;

    let mut asked_after = Vec::new();
    for _ in 0..2 {
        let out = RunCommand
            .call(&rig.ctx, json!({"command": "echo here=$PWD"}))
            .await
            .unwrap();
        assert!(
            !out.is_error && out.content.contains("here="),
            "the command runs in the coder's own container: {}",
            out.content
        );
        asked_after.push(asked(&calls));
    }
    let steps: Vec<_> = rig
        .steps()
        .into_iter()
        .filter(|step| step.id.starts_with("env:"))
        .collect();
    assert_eq!(steps.len(), 1, "exactly one fallback step: {steps:?}");
    assert_eq!(
        steps[0].label,
        "The container runtime is not reachable: commands run in the coder's own environment"
    );
    assert_eq!(steps[0].state, StepState::Completed);
    let detail = steps[0].detail.clone().unwrap_or_default();
    assert!(
        detail.contains("connection refused") && !detail.contains("sk-from-the-gateway"),
        "the reason, without a secret: {detail}"
    );
    // The run stays where it fell back to: the second command did not ask the service again.
    assert_eq!(asked_after[0], asked_after[1], "{asked_after:?}");
}
