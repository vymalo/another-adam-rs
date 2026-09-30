//! The tools one by one, against real worktrees over a local bare remote.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_coder::opencode::OpenCodeLaunch;
use adam_coder::tools::ask::AskUser;
use adam_coder::tools::checks::RunChecks;
use adam_coder::tools::delegate::DelegateToOpenCode;
use adam_coder::tools::prepare::PrepareWorkspace;
use adam_coder::tools::publish::{CommitAndPush, OpenPullRequest};
use adam_llm_agent::{Tool, ToolCtx, ToolError, ToolOutput};
use adam_runtime::{CancelToken, CollectingSink, RunEvent};
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
        let ctx =
            ToolCtx::detached("tool", "call-1", Arc::new(sink.clone())).with_state(fx.env.clone());
        Self { fx, sink, ctx }
    }

    async fn prepare(&self) -> ToolOutput {
        PrepareWorkspace
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
    let out = PrepareWorkspace
        .call(&rig.ctx, json!({"repo_url": "", "base_branch": "main"}))
        .await;
    assert!(is_error(&out), "{out:?}");

    let out = PrepareWorkspace
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

/// The production policy (only `github.com`, no local paths): a hostile or
/// careless `repo_url` is reported to the model before any git call, and the
/// GitHub token never leaves for the host the model named.
#[tokio::test]
async fn prepare_workspace_refuses_foreign_hosts_and_local_paths_in_production() {
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let rig = Rig::new().await;
    let evil = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .mount(&evil)
        .await;
    // The same run, with the production policy as the tools' state.
    let ctx = rig.ctx.clone().with_state(rig.fx.production_env());
    let hostile = [
        // Another host, the honest way and with look-alike names.
        format!("{}/octo/widgets.git", evil.uri()),
        "https://evil.example/octo/widgets.git".to_owned(),
        "https://github.com.evil.example/octo/widgets.git".to_owned(),
        "https://evil.example/github.com/octo/widgets.git".to_owned(),
        // Local paths, in both spellings, including the fixture's real remote.
        rig.fx.remote_url(),
        format!("file://{}", rig.fx.remote.display()),
        "/etc".to_owned(),
        // Credentials in the URL, to the allowed host and to another.
        "https://x-access-token:s3cr3t@github.com/octo/widgets.git".to_owned(),
        "https://user:s3cr3t@evil.example/octo/widgets.git".to_owned(),
        // Not URLs at all.
        "git@github.com:octo/widgets.git".to_owned(),
        "ssh://git@github.com/octo/widgets.git".to_owned(),
        "ext::sh -c 'touch /tmp/pwned'".to_owned(),
        "--upload-pack=touch /tmp/pwned".to_owned(),
    ];
    for url in &hostile {
        let out = PrepareWorkspace
            .call(&ctx, json!({"repo_url": url, "base_branch": "main"}))
            .await;
        assert!(
            is_error(&out),
            "{url} must be reported to the model as an error, got {out:?}"
        );
        let message = text(out);
        assert!(!message.contains("s3cr3t"), "{url}: {message}");
        assert!(!message.contains(common::GITHUB_TOKEN), "{url}: {message}");
    }

    let seen = evil.received_requests().await.unwrap();
    assert!(seen.is_empty(), "the foreign host was contacted: {seen:?}");
    let root = rig.fx.tmp.path().join("production-work");
    assert!(
        !root.join("git").exists() && !root.join("worktrees").exists(),
        "nothing was created for a refused repository"
    );

    // Whatever the reason, the model gets the reason as text to act on.
    let out = PrepareWorkspace
        .call(
            &ctx,
            json!({"repo_url": "https://evil.example/o/r.git", "base_branch": "main"}),
        )
        .await;
    let message = text(out);
    assert!(
        message.contains("not allowed") && message.contains("github.com"),
        "{message}"
    );
    let out = PrepareWorkspace
        .call(
            &ctx,
            json!({"repo_url": rig.fx.remote_url(), "base_branch": "main"}),
        )
        .await;
    assert!(text(out).contains("local"), "the reason names local paths");
}

#[tokio::test]
async fn tools_that_need_a_workspace_say_so() {
    let rig = Rig::new().await;
    let cases: Vec<(&str, Result<ToolOutput, ToolError>)> = vec![
        (
            "delegate",
            DelegateToOpenCode
                .call(&rig.ctx, json!({"instructions": "x"}))
                .await,
        ),
        (
            "checks",
            RunChecks.call(&rig.ctx, json!({"command": "true"})).await,
        ),
        (
            "commit",
            CommitAndPush.call(&rig.ctx, json!({"message": "m"})).await,
        ),
        (
            "pr",
            OpenPullRequest
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
    let tool = RunChecks;

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
    let tool = RunChecks;

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
    let tool = RunChecks;
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

/// The `checks` artifact of a tool result: its data.
fn checks_of(out: &ToolOutput) -> Value {
    let artifacts: Vec<_> = out
        .artifacts
        .iter()
        .filter(|a| a.name == "checks")
        .collect();
    assert_eq!(artifacts.len(), 1, "one checks artifact per run: {out:?}");
    assert_eq!(artifacts[0].mime_type.as_deref(), Some("application/json"));
    artifacts[0].data.clone()
}

fn head_of(dir: &std::path::Path) -> String {
    common::git(dir, &["rev-parse", "HEAD"])
}

#[tokio::test]
async fn a_passing_check_emits_a_passing_checks_artifact_for_head() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let out = RunChecks
        .call(&rig.ctx, json!({"command": "cat README.md"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    let data = checks_of(&out);
    let head = head_of(&rig.worktree());
    assert_eq!(head.len(), 40);
    assert_eq!(data["passed"], true);
    assert_eq!(data["commit"], head.as_str());
    assert_eq!(data["summary"], "`cat README.md` passed");
    assert!(data.get("findings").is_none(), "{data}");
    // Nothing changed: the tree checked is the tree of HEAD.
    let tree = common::git(&rig.worktree(), &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(data["tree"], tree.as_str());
}

#[tokio::test]
async fn a_failing_check_emits_findings_with_its_name_and_output_tail() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let command = "echo compiling; echo 'error[E0432]: unresolved import' >&2; exit 3";
    let out = RunChecks
        .call(&rig.ctx, json!({ "command": command }))
        .await
        .unwrap();
    assert!(out.is_error);
    let data = checks_of(&out);
    assert_eq!(data["passed"], false);
    assert_eq!(data["commit"], head_of(&rig.worktree()).as_str());
    assert!(data["summary"].as_str().unwrap().contains("exit code 3"));
    let findings = data["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{data}");
    assert_eq!(findings[0]["check"], command);
    let message = findings[0]["message"].as_str().unwrap();
    assert!(message.starts_with("exit code 3\n"), "{message}");
    assert!(message.contains("compiling") && message.contains("unresolved import"));
}

#[tokio::test]
async fn a_long_failing_output_is_capped_and_the_cut_is_marked() {
    let fx = Fixture::with("hello\n", |s| {
        s.check_output_tail = 64 * 1024;
    })
    .await;
    let rig = Rig::from(fx);
    rig.prepare().await;
    let out = RunChecks
        .call(
            &rig.ctx,
            json!({"command": "i=0; while [ $i -lt 6000 ]; do echo line-$i-padding; i=$((i+1)); done; exit 1"}),
        )
        .await
        .unwrap();
    let data = checks_of(&out);
    let findings = data["findings"].as_array().unwrap();
    assert!(findings.len() <= 20);
    let total: usize = findings
        .iter()
        .map(|f| f["check"].as_str().unwrap().len() + f["message"].as_str().unwrap().len())
        .sum();
    assert!(total <= 16 * 1024, "{total}");
    let message = findings[0]["message"].as_str().unwrap();
    assert!(message.starts_with("[cut: the last "), "{}", &message[..60]);
    assert!(
        message.trim_end().ends_with("line-5999-padding"),
        "the end is kept"
    );
}

#[tokio::test]
async fn no_commit_is_reported_as_a_failed_check_not_a_missing_artifact() {
    let rig = Rig::new().await;
    rig.prepare().await;
    // The branch is gone: HEAD points at nothing.
    common::git(&rig.worktree(), &["update-ref", "-d", "HEAD"]);
    let out = RunChecks
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap();
    assert!(!out.is_error, "the command itself passed: {}", out.content);
    let data = checks_of(&out);
    assert_eq!(data["passed"], false);
    assert_eq!(data["commit"], "");
    let findings = data["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{data}");
    assert_eq!(findings[0]["check"], "commit");
    assert!(
        findings[0]["message"]
            .as_str()
            .unwrap()
            .contains("cannot determine the commit")
    );
}

#[tokio::test]
async fn secrets_in_the_output_are_not_in_the_checks_artifact() {
    use base64::Engine as _;
    let rig = Rig::new().await;
    rig.prepare().await;
    let encoded = base64::engine::general_purpose::STANDARD.encode(common::GITHUB_TOKEN);
    let command = format!(
        "echo token={} key={} b64={encoded}; exit 1",
        common::GITHUB_TOKEN,
        common::MODEL_KEY
    );
    let out = RunChecks
        .call(&rig.ctx, json!({ "command": command }))
        .await
        .unwrap();
    let artifact = checks_of(&out).to_string();
    for secret in [common::GITHUB_TOKEN, common::MODEL_KEY, encoded.as_str()] {
        assert!(!artifact.contains(secret), "{secret} leaked: {artifact}");
        assert!(
            !out.content.contains(secret),
            "{secret} leaked to the model"
        );
    }
    assert!(artifact.contains("[redacted]"), "{artifact}");
}

/// A journaled step that is replayed returns the same result, so the artifact it emits is the
/// same one: same content, hence the same content-derived id, which a subscriber sees once. A call
/// that ran again (its result was never journaled) emits one artifact, not a second with another
/// id, because the run's own state did not change.
#[tokio::test]
async fn a_replayed_check_emits_the_same_artifact_id() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let mut ids = Vec::new();
    let mut first = None;
    for _ in 0..3 {
        let out = RunChecks
            .call(&rig.ctx, json!({"command": "exit 2"}))
            .await
            .unwrap();
        assert_eq!(out.artifacts.len(), 1);
        ids.push(adam_a2a_runtime::artifact_id(&out.artifacts[0]));
        let data = checks_of(&out);
        assert_eq!(*first.get_or_insert(data.clone()), data);
    }
    assert_eq!(ids[0], ids[1]);
    assert_eq!(ids[1], ids[2]);
    assert!(ids[0].starts_with("checks-"), "{}", ids[0]);
}

#[tokio::test]
async fn a_refused_check_run_emits_no_checks_artifact() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let out = RunChecks
        .call(&rig.ctx, json!({"command": "true", "cwd": ".."}))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.artifacts.is_empty(), "nothing ran: {out:?}");
}

/// The two artifacts of a `commit_and_push` result: the bound `checks` and `branch`.
fn commit_artifacts(out: &ToolOutput) -> (Value, Value) {
    let names: Vec<_> = out.artifacts.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["checks", "branch"], "{out:?}");
    (out.artifacts[0].data.clone(), out.artifacts[1].data.clone())
}

fn tree_of(dir: &std::path::Path, commit: &str) -> String {
    common::git(dir, &["rev-parse", &format!("{commit}^{{tree}}")])
}

/// The verdict is bound to what was pushed: the run checked the worktree as it would be committed
/// (tracked changes, untracked files, not the ignored ones), and the commit has that tree.
#[tokio::test]
async fn checks_then_commit_binds_a_passing_verdict_to_the_pushed_commit() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let wt = rig.worktree();
    std::fs::write(wt.join(".gitignore"), "ignored.log\n").unwrap();
    std::fs::write(wt.join("ignored.log"), "noise\n").unwrap();
    std::fs::write(wt.join("new.txt"), "new\n").unwrap();
    std::fs::write(wt.join("README.md"), "changed\n").unwrap();

    let checked = RunChecks
        .call(&rig.ctx, json!({"command": "test -f new.txt"}))
        .await
        .unwrap();
    let on_head = checks_of(&checked);
    assert_eq!(on_head["passed"], true);
    assert_eq!(
        on_head["commit"],
        head_of(&wt).as_str(),
        "still the old HEAD"
    );

    let out = CommitAndPush
        .call(&rig.ctx, json!({"message": "feat: add new"}))
        .await
        .unwrap();
    let (bound, branch) = commit_artifacts(&out);
    let pushed = branch["commit"].as_str().unwrap();
    assert_ne!(pushed, on_head["commit"].as_str().unwrap());
    assert_eq!(bound["passed"], true, "{bound}");
    assert_eq!(bound["commit"], pushed);
    assert_eq!(bound["tree"], tree_of(&wt, pushed).as_str());
    assert_eq!(
        bound["tree"], on_head["tree"],
        "the tree run_checks recorded is the tree commit_all committed"
    );
    assert!(
        bound["summary"]
            .as_str()
            .unwrap()
            .contains("checked on the identical tree before it was committed"),
        "{bound}"
    );
    assert!(!bound["summary"].as_str().unwrap().contains("uncommitted"));
    assert!(
        !common::git(&wt, &["ls-tree", "--name-only", pushed]).contains("ignored.log"),
        "ignored files are not committed"
    );
    assert_eq!(bound["commit"], branch["commit"]);
}

/// Checks on a clean tree, then a commit step that has nothing new to commit (the tree is
/// already the pushed one): still a bound, passing verdict.
#[tokio::test]
async fn checks_then_a_commit_with_no_changes_binds_to_the_pushed_head() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let wt = rig.worktree();
    std::fs::write(wt.join("a.txt"), "a\n").unwrap();
    CommitAndPush
        .call(&rig.ctx, json!({"message": "feat: add a"}))
        .await
        .unwrap();
    let checked = RunChecks
        .call(&rig.ctx, json!({"command": "test -f a.txt"}))
        .await
        .unwrap();
    assert_eq!(checks_of(&checked)["commit"], head_of(&wt).as_str());

    let again = CommitAndPush
        .call(&rig.ctx, json!({"message": "feat: add a"}))
        .await
        .unwrap();
    assert!(again.content.contains("Nothing new to commit"));
    let (bound, branch) = commit_artifacts(&again);
    assert_eq!(bound["passed"], true, "{bound}");
    assert_eq!(bound["commit"], head_of(&wt).as_str());
    assert_eq!(bound["commit"], branch["commit"]);
    assert_eq!(bound["tree"], tree_of(&wt, "HEAD").as_str());
}

#[tokio::test]
async fn edits_after_the_checks_leave_the_pushed_commit_unchecked() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let wt = rig.worktree();
    std::fs::write(wt.join("a.txt"), "a\n").unwrap();
    let checked = RunChecks
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap();
    let checked_tree = checks_of(&checked)["tree"].as_str().unwrap().to_owned();
    std::fs::write(wt.join("a.txt"), "a, edited after the checks\n").unwrap();

    let out = CommitAndPush
        .call(&rig.ctx, json!({"message": "feat: add a"}))
        .await
        .unwrap();
    let (bound, branch) = commit_artifacts(&out);
    let pushed_tree = tree_of(&wt, "HEAD");
    assert_eq!(bound["passed"], false, "{bound}");
    assert_eq!(bound["commit"], branch["commit"]);
    assert_eq!(bound["tree"], pushed_tree.as_str());
    let findings = bound["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["check"], "checks");
    assert_eq!(
        findings[0]["message"],
        format!(
            "the pushed tree was not checked: the last checks ran on {}, the commit has {}",
            &checked_tree[..10],
            &pushed_tree[..10]
        )
        .as_str()
    );
}

#[tokio::test]
async fn a_commit_without_any_checks_is_unchecked() {
    let rig = Rig::new().await;
    rig.prepare().await;
    std::fs::write(rig.worktree().join("a.txt"), "a\n").unwrap();
    let out = CommitAndPush
        .call(&rig.ctx, json!({"message": "feat: add a"}))
        .await
        .unwrap();
    let (bound, branch) = commit_artifacts(&out);
    assert_eq!(bound["passed"], false, "{bound}");
    assert_eq!(bound["commit"], branch["commit"]);
    let message = bound["findings"][0]["message"].as_str().unwrap();
    assert!(
        message.starts_with(
            "the pushed tree was not checked: no check ran in this run, the commit has "
        ),
        "{message}"
    );
}

/// Red checks on the tree that was pushed: bound, and still red, with the findings.
#[tokio::test]
async fn a_failed_check_on_the_pushed_tree_stays_failed_when_bound() {
    let rig = Rig::new().await;
    rig.prepare().await;
    std::fs::write(rig.worktree().join("a.txt"), "a\n").unwrap();
    RunChecks
        .call(&rig.ctx, json!({"command": "echo lint failed; exit 1"}))
        .await
        .unwrap();
    let out = CommitAndPush
        .call(&rig.ctx, json!({"message": "feat: add a"}))
        .await
        .unwrap();
    let (bound, branch) = commit_artifacts(&out);
    assert_eq!(bound["passed"], false);
    assert_eq!(bound["commit"], branch["commit"]);
    assert_eq!(bound["findings"][0]["check"], "echo lint failed; exit 1");
    assert!(
        bound["findings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("lint failed")
    );
}

#[tokio::test]
async fn commit_and_push_is_idempotent_and_refuses_an_empty_branch() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let tool = CommitAndPush;

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
    let names: Vec<_> = first.artifacts.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["checks", "branch"], "the verdict comes first");
    let branch = first.artifacts[1].data["branch"]
        .as_str()
        .unwrap()
        .to_owned();
    let sha = first.artifacts[1].data["commit"]
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
    assert_eq!(again.artifacts[1].data["commit"], sha.as_str());
    assert_eq!(
        again.artifacts, first.artifacts,
        "a repeated step emits the same artifacts"
    );
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
    let pr = OpenPullRequest;
    let args = json!({"title": "feat: a", "body": "Adds a.\n\n## Verification\n- ok"});

    let unpushed = pr.call(&rig.ctx, args.clone()).await;
    assert!(is_error(&unpushed));
    assert!(text(unpushed).contains("not pushed"));

    CommitAndPush
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
    let checks = RunChecks;
    assert!(
        !checks
            .call(&rig.ctx, json!({"command": "true"}))
            .await
            .unwrap()
            .is_error
    );
    std::fs::write(rig.worktree().join("b.txt"), "b\n").unwrap();
    CommitAndPush
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
    let out = DelegateToOpenCode
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

/// The run is cancelled while OpenCode ignores `session/cancel` (a hung tool):
/// the tool gives it a short grace, then kills it and what it started, waits
/// until it is reaped, and fails as cancelled, all without help from the
/// caller. Nothing is left running when `call` returns.
#[tokio::test]
async fn delegate_to_opencode_kills_a_stubborn_opencode_when_the_run_is_cancelled() {
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
    let mut rig = Rig::from(fx);
    let token = CancelToken::new();
    rig.ctx = rig.ctx.with_cancel_token(token.clone());
    rig.prepare().await;

    let tool = DelegateToOpenCode;
    let cancel_when_running = async {
        let grandchild = common::wait_for_pid(&child_file).await;
        token.cancel();
        grandchild
    };
    let started = Instant::now();
    let (out, grandchild) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(
            tool.call(&rig.ctx, json!({"instructions": "never finish"})),
            cancel_when_running
        )
    })
    .await
    .expect("the tool returns after a cancel");

    assert!(
        matches!(&out, Err(ToolError::Permanent(m)) if m.contains("cancelled")),
        "{out:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a cancel must not wait for the agent: {:?}",
        started.elapsed()
    );
    let agent = common::wait_for_pid(&pid_file).await;
    assert!(
        common::process_gone(agent, true),
        "OpenCode (pid {agent}) must be reaped when the tool returns"
    );
    assert!(
        common::wait_gone(grandchild, false, Duration::from_secs(5)).await,
        "OpenCode's child (pid {grandchild}) survived"
    );
}

/// A cancelled run must not publish, even if the model asked for several
/// calls in one turn and a later one is next in line: the cancelled tool's
/// result is only an error to the loop, which goes on to the next call.
#[tokio::test]
async fn publishing_tools_refuse_to_act_for_a_cancelled_run() {
    let mut rig = Rig::new().await;
    let token = CancelToken::new();
    rig.ctx = rig.ctx.with_cancel_token(token.clone());
    rig.prepare().await;
    std::fs::write(rig.worktree().join("a.txt"), "a\n").unwrap();
    token.cancel();

    let push = CommitAndPush
        .call(&rig.ctx, json!({"message": "feat: add a"}))
        .await;
    assert!(
        matches!(&push, Err(ToolError::Permanent(m)) if m.contains("cancelled")),
        "{push:?}"
    );
    let pr = OpenPullRequest
        .call(&rig.ctx, json!({"title": "feat: add a", "body": "b"}))
        .await;
    assert!(
        matches!(&pr, Err(ToolError::Permanent(m)) if m.contains("cancelled")),
        "{pr:?}"
    );
    assert!(rig.fx.agent_branches().is_empty(), "nothing was pushed");
    assert!(rig.fx.created_pulls().await.is_empty(), "no pull request");
    let log = common::git(&rig.worktree(), &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 1, "nothing was committed: {log}");
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
    let out = DelegateToOpenCode
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
    let out = DelegateToOpenCode
        .call(&rig.ctx, json!({"instructions": "x"}))
        .await;
    assert!(matches!(out, Err(ToolError::Transient(_))), "{out:?}");

    let fx = Fixture::with("x", |s| {
        s.opencode = OpenCodeLaunch::program("/nonexistent/opencode");
    })
    .await;
    let rig = Rig::from(fx);
    rig.prepare().await;
    let out = DelegateToOpenCode
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

/// Arguments the schema does not allow are the model's mistake, not the run's: it gets the reason
/// as a tool result and can correct itself (the `#[tool]` contract; the hand-written tools said
/// "X is required" for the same input).
#[tokio::test]
async fn malformed_arguments_are_reported_to_the_model() {
    let rig = Rig::new().await;
    for (tool, args) in [
        (&RunChecks as &dyn Tool, json!({})),
        (&RunChecks, json!({"command": 5})),
        (&CommitAndPush, json!({"message": ["a"]})),
        (&OpenPullRequest, json!({"title": "t"})),
        (&PrepareWorkspace, json!({"base_branch": "main"})),
        (&DelegateToOpenCode, Value::Null),
    ] {
        let out = tool.call(&rig.ctx, args.clone()).await;
        assert!(is_error(&out), "{args}: {out:?}");
        assert!(text(out).contains("invalid arguments"), "{args}");
    }
    // Nothing ran: there is still no worktree.
    assert!(!rig.worktree().exists());
}
