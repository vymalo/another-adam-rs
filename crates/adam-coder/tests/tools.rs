//! The tools one by one, against real worktrees over a local bare remote.

mod common;

use std::sync::Arc;

use adam_coder::opencode::OpenCodeLaunch;
use adam_coder::tools::checks::RunChecks;
use adam_coder::tools::delegate::DelegateToOpenCode;
use adam_coder::tools::prepare::PrepareWorkspace;
use adam_coder::tools::publish::{CommitAndPush, OpenPullRequest};
use adam_coder::tools::{ToolEnv, ask::AskUser};
use adam_llm_agent::{Tool, ToolCtx, ToolError, ToolOutput};
use adam_runtime::{CollectingSink, RunEvent};
use common::{Fixture, PR_URL};
use serde_json::{Value, json};

struct Rig {
    fx: Fixture,
    sink: CollectingSink,
    ctx: ToolCtx,
}

impl Rig {
    async fn new() -> Self {
        Self::from(Fixture::new("hello\n").await)
    }

    fn from(fx: Fixture) -> Self {
        let sink = CollectingSink::new();
        let ctx = ToolCtx::detached("tool", "call-1", Arc::new(sink.clone()));
        Self { fx, sink, ctx }
    }

    fn env(&self) -> Arc<ToolEnv> {
        self.fx.env.clone()
    }

    async fn prepare(&self) -> ToolOutput {
        PrepareWorkspace::new(self.env())
            .call(
                &self.ctx,
                json!({"repo_url": self.fx.remote_url(), "base_branch": "main"}),
            )
            .await
            .expect("prepare")
    }

    fn worktree(&self) -> std::path::PathBuf {
        self.fx
            .root
            .join("worktrees")
            .join(self.ctx.run_id().to_string())
    }

    fn progress(&self) -> Vec<String> {
        self.sink
            .events()
            .into_iter()
            .filter_map(|e| match e.event {
                RunEvent::Progress { message } => Some(message),
                _ => None,
            })
            .collect()
    }
}

fn is_error(out: &Result<ToolOutput, ToolError>) -> bool {
    matches!(out, Ok(o) if o.is_error)
}

fn text(out: Result<ToolOutput, ToolError>) -> String {
    out.expect("tool result").content
}

#[tokio::test]
async fn prepare_workspace_is_idempotent_per_run_and_keeps_changes() {
    let rig = Rig::new().await;
    let first = rig.prepare().await;
    assert!(!first.is_error, "{}", first.content);
    assert!(
        first.content.contains("branch: agent/"),
        "{}",
        first.content
    );
    assert!(rig.worktree().join("README.md").is_file());

    std::fs::write(rig.worktree().join("wip.txt"), "uncommitted").unwrap();
    let again = rig.prepare().await;
    assert_eq!(again.content, first.content, "same worktree, same branch");
    assert!(
        rig.worktree().join("wip.txt").is_file(),
        "work in progress survives"
    );
}

#[tokio::test]
async fn prepare_workspace_reports_a_bad_repository_to_the_model() {
    let rig = Rig::new().await;
    let out = PrepareWorkspace::new(rig.env())
        .call(&rig.ctx, json!({"repo_url": "", "base_branch": "main"}))
        .await;
    assert!(is_error(&out), "{out:?}");

    let out = PrepareWorkspace::new(rig.env())
        .call(
            &rig.ctx,
            json!({"repo_url": rig.fx.remote_url(), "base_branch": "no-such-branch"}),
        )
        .await;
    assert!(
        is_error(&out),
        "a missing branch is the model's problem: {out:?}"
    );
}

#[tokio::test]
async fn tools_that_need_a_workspace_say_so() {
    let rig = Rig::new().await;
    let env = rig.env();
    let cases: Vec<(&str, Result<ToolOutput, ToolError>)> = vec![
        (
            "delegate",
            DelegateToOpenCode::new(env.clone())
                .call(&rig.ctx, json!({"instructions": "x"}))
                .await,
        ),
        (
            "checks",
            RunChecks::new(env.clone())
                .call(&rig.ctx, json!({"command": "true"}))
                .await,
        ),
        (
            "commit",
            CommitAndPush::new(env.clone())
                .call(&rig.ctx, json!({"message": "m"}))
                .await,
        ),
        (
            "pr",
            OpenPullRequest::new(env)
                .call(&rig.ctx, json!({"title": "t", "body": "b"}))
                .await,
        ),
    ];
    for (name, out) in cases {
        assert!(is_error(&out), "{name}: {out:?}");
        assert!(text(out).contains("prepare_workspace"), "{name}");
    }
}

#[tokio::test]
async fn run_checks_runs_in_the_worktree_and_never_outside_it() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let tool = RunChecks::new(rig.env());

    let ok = tool
        .call(&rig.ctx, json!({"command": "cat README.md"}))
        .await
        .unwrap();
    assert!(!ok.is_error, "{}", ok.content);
    assert!(ok.content.contains("exit code 0: passed") && ok.content.contains("widgets"));

    std::fs::create_dir_all(rig.worktree().join("sub")).unwrap();
    std::fs::write(rig.worktree().join("sub/only-here.txt"), "x").unwrap();
    let sub = tool
        .call(&rig.ctx, json!({"command": "ls", "cwd": "sub"}))
        .await
        .unwrap();
    assert!(sub.content.contains("only-here.txt"), "{}", sub.content);

    for cwd in ["..", "../..", "/etc", "sub/../.."] {
        let out = tool
            .call(&rig.ctx, json!({"command": "ls", "cwd": cwd}))
            .await;
        assert!(is_error(&out), "{cwd}: {out:?}");
        assert!(text(out).contains("cwd"), "{cwd}");
    }
}

#[tokio::test]
async fn run_checks_times_out_and_caps_its_output() {
    let fx = Fixture::with("hello\n", |s| {
        s.check_timeout = std::time::Duration::from_millis(500);
        s.check_output_tail = 256;
    })
    .await;
    let rig = Rig::from(fx);
    rig.prepare().await;
    let tool = RunChecks::new(rig.env());

    let out = tool
        .call(&rig.ctx, json!({"command": "echo before; sleep 30"}))
        .await;
    assert!(is_error(&out));
    let text = text(out);
    assert!(
        text.contains("TIMED OUT after 0s") || text.contains("TIMED OUT"),
        "{text}"
    );
    assert!(text.contains("before"), "{text}");

    let out = tool
        .call(
            &rig.ctx,
            json!({"command": "i=0; while [ $i -lt 500 ]; do echo line-$i; i=$((i+1)); done"}),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("output truncated"), "{}", out.content);
    assert!(
        out.content.contains("line-499") && !out.content.contains("line-1\n"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn a_replayed_failing_check_counts_one_cycle() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let tool = RunChecks::new(rig.env());
    for _ in 0..3 {
        let out = tool
            .call(&rig.ctx, json!({"command": "exit 2"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("failed check run 1 of 3"),
            "the same call id never costs a second cycle: {}",
            out.content
        );
    }
}

#[tokio::test]
async fn commit_and_push_is_idempotent_and_refuses_an_empty_branch() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let tool = CommitAndPush::new(rig.env());

    let nothing = tool.call(&rig.ctx, json!({"message": "feat: x"})).await;
    assert!(is_error(&nothing), "{nothing:?}");
    assert!(text(nothing).contains("Nothing to commit"));

    std::fs::write(rig.worktree().join("a.txt"), "a\n").unwrap();
    let first = tool
        .call(&rig.ctx, json!({"message": "feat: add a"}))
        .await
        .unwrap();
    assert!(!first.is_error, "{}", first.content);
    assert!(first.content.starts_with("Committed "));
    assert_eq!(first.artifacts[0].name, "branch");
    let branch = first.artifacts[0].data["branch"]
        .as_str()
        .unwrap()
        .to_owned();
    let sha = first.artifacts[0].data["commit"]
        .as_str()
        .unwrap()
        .to_owned();

    // Run again, as a replay after a crash would: same commit, nothing pushed.
    let again = tool
        .call(&rig.ctx, json!({"message": "feat: add a"}))
        .await
        .unwrap();
    assert!(
        again.content.contains("Nothing new to commit"),
        "{}",
        again.content
    );
    assert_eq!(again.artifacts[0].data["commit"], sha.as_str());
    assert_eq!(rig.fx.commits_ahead(&branch), 1);
    assert_eq!(rig.fx.ref_updates(&branch), 1);
    assert_eq!(rig.fx.file_on(&branch, "a.txt"), "a");

    let empty = tool.call(&rig.ctx, json!({"message": "  "})).await;
    assert!(is_error(&empty));
}

#[tokio::test]
async fn open_pull_request_needs_a_push_and_green_checks_and_is_idempotent() {
    let rig = Rig::new().await;
    rig.prepare().await;
    std::fs::write(rig.worktree().join("a.txt"), "a\n").unwrap();
    let pr = OpenPullRequest::new(rig.env());
    let args = json!({"title": "feat: a", "body": "Adds a.\n\n## Verification\n- ok"});

    let unpushed = pr.call(&rig.ctx, args.clone()).await;
    assert!(is_error(&unpushed));
    assert!(text(unpushed).contains("not pushed"));

    CommitAndPush::new(rig.env())
        .call(&rig.ctx, json!({"message": "feat: add a"}))
        .await
        .unwrap();
    let unchecked = pr.call(&rig.ctx, args.clone()).await;
    assert!(is_error(&unchecked));
    let unchecked = text(unchecked);
    assert!(
        unchecked.contains("Refusing") && unchecked.contains("no check was run"),
        "{unchecked}"
    );
    assert!(rig.fx.created_pulls().await.is_empty());

    // Checks pass on this code; then more code is committed and pushed
    // without re-running them: the pull request would contain unverified code.
    let checks = RunChecks::new(rig.env());
    assert!(
        !checks
            .call(&rig.ctx, json!({"command": "true"}))
            .await
            .unwrap()
            .is_error
    );
    std::fs::write(rig.worktree().join("b.txt"), "b\n").unwrap();
    CommitAndPush::new(rig.env())
        .call(&rig.ctx, json!({"message": "feat: add b"}))
        .await
        .unwrap();
    let stale = pr.call(&rig.ctx, args.clone()).await;
    assert!(is_error(&stale));
    assert!(
        text(stale).contains("code changed since the last passing check run"),
        "an unverified change must block the pull request"
    );
    // Re-verified on the pushed code: allowed. Uncommitted work is not part of
    // it, and the result says so.
    checks
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap();
    std::fs::write(rig.worktree().join("scratch.txt"), "x\n").unwrap();
    let opened = pr.call(&rig.ctx, args.clone()).await.unwrap();
    assert!(!opened.is_error, "{}", opened.content);
    assert!(opened.content.contains(PR_URL));
    assert!(
        opened.content.contains("scratch.txt") && opened.content.contains("not part of"),
        "{}",
        opened.content
    );
    assert_eq!(opened.artifacts[0].name, "pull_request");
    assert_eq!(opened.artifacts[0].data["url"], PR_URL);

    // Again (replay): the same PR, no second POST.
    let again = pr.call(&rig.ctx, args).await.unwrap();
    assert_eq!(again.artifacts[0].data["url"], PR_URL);
    assert_eq!(rig.fx.created_pulls().await.len(), 1);
}

#[tokio::test]
async fn delegate_to_opencode_streams_updates_and_returns_the_summary() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let out = DelegateToOpenCode::new(rig.env())
        .call(&rig.ctx, json!({"instructions": "add hello.txt"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("stop reason: end_turn"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("write: ok"),
        "the agent's own summary: {}",
        out.content
    );
    assert!(
        out.content.contains("hello.txt"),
        "changed files: {}",
        out.content
    );
    assert_eq!(
        std::fs::read_to_string(rig.worktree().join("hello.txt")).unwrap(),
        "hello\n"
    );

    let progress = rig.progress();
    assert!(
        progress
            .iter()
            .any(|p| p.starts_with("opencode: edit: Write ")),
        "{progress:?}"
    );
    assert!(
        progress.iter().any(|p| p.contains("opencode: write: ok")),
        "{progress:?}"
    );
    assert!(
        progress.iter().any(|p| p.contains("turn ended (end_turn)")),
        "{progress:?}"
    );
}

#[tokio::test]
async fn opencode_cannot_write_outside_the_worktree() {
    let fx = Fixture::with("x", |s| {
        s.opencode = OpenCodeLaunch::program(common::fake_agent())
            .env("FAKE_ACP_SCENARIO", "write-file")
            .env("FAKE_ACP_WRITE_PATH", "../escaped.txt")
            .env("FAKE_ACP_WRITE_CONTENT", "nope");
    })
    .await;
    let rig = Rig::from(fx);
    rig.prepare().await;
    let out = DelegateToOpenCode::new(rig.env())
        .call(&rig.ctx, json!({"instructions": "write outside"}))
        .await
        .unwrap();
    assert!(out.content.contains("write: "), "{}", out.content);
    assert!(
        !out.content.contains("write: ok"),
        "the write must be refused: {}",
        out.content
    );
    let escaped = rig.fx.root.join("worktrees/escaped.txt");
    assert!(!escaped.exists(), "{}", escaped.display());
}

#[tokio::test]
async fn a_crashing_opencode_is_a_transient_error_and_a_missing_one_is_permanent() {
    let fx = Fixture::with("x", |s| {
        s.opencode =
            OpenCodeLaunch::program(common::fake_agent()).env("FAKE_ACP_SCENARIO", "crash");
    })
    .await;
    let rig = Rig::from(fx);
    rig.prepare().await;
    let out = DelegateToOpenCode::new(rig.env())
        .call(&rig.ctx, json!({"instructions": "x"}))
        .await;
    assert!(matches!(out, Err(ToolError::Transient(_))), "{out:?}");

    let fx = Fixture::with("x", |s| {
        s.opencode = OpenCodeLaunch::program("/nonexistent/opencode");
    })
    .await;
    let rig = Rig::from(fx);
    rig.prepare().await;
    let out = DelegateToOpenCode::new(rig.env())
        .call(&rig.ctx, json!({"instructions": "x"}))
        .await;
    assert!(matches!(out, Err(ToolError::Permanent(_))), "{out:?}");
}

#[tokio::test]
async fn ask_user_needs_input_with_the_question() {
    let rig = Rig::new().await;
    let out = AskUser
        .call(&rig.ctx, json!({"question": " Which repo? "}))
        .await;
    assert_eq!(
        out,
        Err(ToolError::NeedsInput {
            question: "Which repo?".into()
        })
    );
    assert!(is_error(&AskUser.call(&rig.ctx, Value::Null).await));
}
