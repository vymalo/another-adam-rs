//! The tools one by one, against real worktrees over a local bare remote.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_coder::ToolEnv;
use adam_coder::opencode::OpenCodeLaunch;
use adam_coder::tools::ask::AskUser;
use adam_coder::tools::checks::RunChecks;
use adam_coder::tools::delegate::DelegateToOpenCode;
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_coder::tools::notes::PushedBranch;
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

    /// The person says `text` (what the agent records in the run notes before each step).
    async fn say(&self, text: &str) {
        say(&self.fx.env, &self.ctx, text).await;
    }

    async fn prepare(&self) -> ToolOutput {
        self.say(&self.fx.remote_url()).await;
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

/// The person says `text` in the run of `ctx`: what the agent records in `env`'s run notes before
/// each step.
async fn say(env: &ToolEnv, ctx: &ToolCtx, text: &str) {
    let run = ctx.run_id().to_string();
    let mut notes = env.notes.load(&run).await.unwrap();
    notes.name_repos(named_in(
        &without_untrusted(text),
        &env.settings.default_repo_host,
    ));
    env.notes.save(&run, &notes).await.unwrap();
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

    rig.say(&rig.fx.remote_url()).await;
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
    let production = rig.fx.production_env();
    let ctx = rig.ctx.clone().with_state(production.clone());
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
    // The person named every one of them: what is refused here is the policy's to refuse.
    say(&production, &ctx, &hostile.join(" ")).await;
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
    say(&production, &ctx, "https://evil.example/o/r.git").await;
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

/// The owner's report: given only "Hi" the model invented `rust-lang/rust-clippy`. A repository the
/// person did not name is refused as a tool error (not a failure of the run) that sends the model
/// to `ask_user`, before anything is fetched or created; one the person named is accepted, and
/// naming another later does not unlock the first.
#[tokio::test]
async fn prepare_workspace_refuses_a_repository_the_person_did_not_name() {
    let rig = Rig::new().await;
    let invented =
        json!({"repo_url": "https://github.com/rust-lang/rust-clippy", "base_branch": "master"});

    // Nobody named anything yet.
    let out = PrepareWorkspace.call(&rig.ctx, invented.clone()).await;
    assert!(
        is_error(&out),
        "a tool error for the model, not a failed run: {out:?}"
    );
    let message = text(out);
    assert!(message.contains("ask_user"), "{message}");
    assert!(message.contains("rust-clippy"), "{message}");
    assert!(
        message.contains("has not named any repository"),
        "{message}"
    );
    assert!(
        !rig.fx.root.join("git").exists() && !rig.worktree().exists(),
        "nothing was fetched or created"
    );

    // The local remote, unnamed, is refused too (the fixture policy allows local paths).
    let local = json!({"repo_url": rig.fx.remote_url(), "base_branch": "main"});
    let out = PrepareWorkspace.call(&rig.ctx, local.clone()).await;
    assert!(is_error(&out) && text(out).contains("ask_user"));

    // The person names a different repository: the invented one is still refused, and the
    // refusal says what was named.
    rig.say("please work on acme/widgets").await;
    let message = text(PrepareWorkspace.call(&rig.ctx, invented.clone()).await);
    assert!(
        message.contains("ask_user") && message.contains("github.com/acme/widgets"),
        "{message}"
    );

    // Quoted findings and file names do not name anything, and are never listed back.
    rig.say(
        "see src/main.rs\n````untrusted\n- https://github.com/evil/payload\n```\ncode\n```\n````",
    )
    .await;
    let payload = json!({"repo_url": "https://github.com/evil/payload", "base_branch": "main"});
    let message = text(PrepareWorkspace.call(&rig.ctx, payload).await);
    assert!(message.contains("ask_user"), "{message}");
    let listed = message.split("named:").nth(1).expect("the named list");
    assert!(
        listed.contains("github.com/acme/widgets")
            && !listed.contains("evil/payload")
            && !listed.contains("main.rs"),
        "{message}"
    );

    // A repository that merely shares a name with a named one is another repository.
    let lookalike = json!({"repo_url": "https://github.com/evil/widgets", "base_branch": "main"});
    assert!(text(PrepareWorkspace.call(&rig.ctx, lookalike).await).contains("ask_user"));

    // The person names the local remote: accepted, and the worktree exists.
    rig.say(&format!(
        "In {} (base branch main) add a file",
        rig.fx.remote_url()
    ))
    .await;
    let out = PrepareWorkspace.call(&rig.ctx, local).await;
    assert!(!is_error(&Ok(out.clone().expect("prepare"))), "{out:?}");
    assert!(rig.worktree().join("README.md").is_file());
    // And still not the invented one.
    let out = PrepareWorkspace.call(&rig.ctx, invented).await;
    assert!(is_error(&out) && text(out).contains("ask_user"));
}

/// Every way of writing a repository the person named opens the gate for it (and only it): the
/// URL with or without `.git`, a trailing slash and another case, and `host/owner/name`. The
/// host is a local mock, so a request reaching it proves the tool went past the gate; the
/// `owner/name` form (default host `github.com`) and the vendored e2e mocks' sandbox address have
/// the same key, which `tools::named`'s unit tests pin.
#[tokio::test]
async fn every_written_form_of_a_named_repository_passes_the_gate() {
    use adam_workspace::{ScopedToken, Workspaces};
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let rig = Rig::new().await;
    let mirror = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .mount(&mirror)
        .await;
    let host = format!("127.0.0.1:{}", mirror.address().port());
    let env = Arc::new(ToolEnv::new(
        Workspaces::new(
            rig.fx.tmp.path().join("forms-work"),
            Arc::new(ScopedToken::new(host.as_str(), common::GITHUB_TOKEN)),
        )
        .allow_hosts([host.clone()])
        .allow_local(true),
        rig.fx.env.code_host.clone(),
        rig.fx.env.settings.clone(),
    ));
    let argument =
        json!({"repo_url": format!("http://{host}/octo/widgets.git"), "base_branch": "main"});

    // Unnamed: refused, the mirror is not contacted.
    let ctx = ToolCtx::detached("tool", "call-x", Arc::new(CollectingSink::new()))
        .with_state(env.clone());
    let out = PrepareWorkspace.call(&ctx, argument.clone()).await;
    assert!(text(out).contains("ask_user"));
    assert!(mirror.received_requests().await.unwrap().is_empty());

    for written in [
        format!("http://{host}/octo/widgets.git"),
        format!("http://{host}/octo/widgets"),
        format!("http://{host}/OCTO/Widgets/"),
        format!("{host}/octo/widgets"),
        format!("{host}/octo/widgets.git"),
        format!("In {host}/octo/widgets, add hello.txt."),
    ]
    .iter()
    {
        let ctx = ToolCtx::detached("tool", "call-x", Arc::new(CollectingSink::new()))
            .with_state(env.clone());
        say(&env, &ctx, written).await;
        let before = mirror.received_requests().await.unwrap().len();
        let out = PrepareWorkspace.call(&ctx, argument.clone()).await;
        let message = text(out);
        assert!(
            !message.contains("ask_user"),
            "{written:?} names the repository: {message}"
        );
        let after = mirror.received_requests().await.unwrap().len();
        assert!(
            after > before,
            "{written:?}: this call did not reach the mirror ({before} requests before, {after} after); the tool said: {message}"
        );
    }
}

/// The owner's case: the repository's default branch is `master`, the model guessed `main`. Without a
/// base branch the workspace starts from the remote's default; a base branch that is not there is
/// an error that lists the branches that are, so the model can pick one or ask.
#[tokio::test]
async fn prepare_workspace_without_a_base_branch_starts_from_the_remotes_default() {
    let rig = Rig::new().await;
    let remote = rig.fx.remote_url();
    common::git(&rig.fx.remote, &["branch", "master", "refs/heads/main"]);
    common::git(&rig.fx.remote, &["branch", "topic/one", "refs/heads/main"]);
    common::git(
        &rig.fx.remote,
        &["symbolic-ref", "HEAD", "refs/heads/master"],
    );
    rig.say(&remote).await;

    // A guess that is not there: the error lists what is.
    let guessed = PrepareWorkspace
        .call(
            &rig.ctx,
            json!({"repo_url": remote, "base_branch": "develop"}),
        )
        .await;
    assert!(is_error(&guessed), "{guessed:?}");
    let guessed = text(guessed);
    assert!(
        guessed.contains("develop") && guessed.contains("Its branches: main, master, topic/one."),
        "{guessed}"
    );
    assert!(
        !rig.worktree().exists(),
        "nothing was created for the refused branch"
    );

    // No base branch (absent, or blank): the remote's default.
    for args in [
        json!({"repo_url": remote}),
        json!({"repo_url": remote, "base_branch": "  "}),
    ] {
        let rig = Rig::from(Fixture::new("hello\n").await);
        common::git(&rig.fx.remote, &["branch", "master", "refs/heads/main"]);
        common::git(
            &rig.fx.remote,
            &["symbolic-ref", "HEAD", "refs/heads/master"],
        );
        rig.say(&rig.fx.remote_url()).await;
        let mut args = args.clone();
        args["repo_url"] = json!(rig.fx.remote_url());
        let ready = PrepareWorkspace.call(&rig.ctx, args.clone()).await.unwrap();
        assert!(!ready.is_error, "{}", ready.content);
        assert!(
            ready.content.contains("base branch: master"),
            "{}",
            ready.content
        );
        // Again in the same run: the workspace's own base, without asking the remote.
        let again = PrepareWorkspace.call(&rig.ctx, args).await.unwrap();
        assert!(
            again.content.contains("base branch: master"),
            "{}",
            again.content
        );
    }
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

// ---------------------------------------------- a later task continues an earlier task's branch

/// The `repository:` and `branch:` lines a `commit_and_push` result ends with, which is what a
/// later task of the conversation learns its branches from.
fn pushed_lines(result: &str) -> (String, String) {
    let line = |key: &str| {
        result
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_else(|| panic!("no {key} line in {result}"))
            .to_owned()
    };
    (line("repository: "), line("branch: "))
}

/// A second run of the same conversation, in the same fixture: its own run id and notes. As the
/// agent does before each step, the person's words (naming `repo`) and the branches the carried
/// `commit_and_push` results reported are put in the notes.
async fn next_task(fx: &Fixture, repo: &str, pushed: &[(&str, &str)]) -> ToolCtx {
    let ctx = ToolCtx::detached("tool", "call-1", Arc::new(CollectingSink::new()))
        .with_state(fx.env.clone());
    say(&fx.env, &ctx, repo).await;
    let run = ctx.run_id().to_string();
    let mut notes = fx.env.notes.load(&run).await.unwrap();
    notes.name_pushed_branches(pushed.iter().map(|(repo, branch)| PushedBranch {
        repo: adam_coder::tools::named::key_of_argument(repo).unwrap(),
        branch: (*branch).to_owned(),
    }));
    fx.env.notes.save(&run, &notes).await.unwrap();
    ctx
}

/// A rework: the second task works on the branch the first pushed, its push updates that branch,
/// and the pull request it reports is the one that is already open, not a second one.
#[tokio::test]
async fn a_later_task_continues_the_pushed_branch_and_reports_the_same_pull_request() {
    let rig = Rig::new().await;
    let remote = rig.fx.remote_url();
    rig.prepare().await;
    std::fs::write(rig.worktree().join("one.txt"), "one\n").unwrap();
    RunChecks
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap();
    let pushed = CommitAndPush
        .call(&rig.ctx, json!({"message": "feat: one"}))
        .await
        .unwrap();
    let (repository, branch) = pushed_lines(&pushed.content);
    assert_eq!(repository, remote);
    assert!(branch.starts_with("agent/"), "{branch}");
    let args = json!({"title": "feat: one", "body": "One.\n\n## Verification\n- `true`"});
    let opened = OpenPullRequest.call(&rig.ctx, args.clone()).await.unwrap();
    assert!(opened.content.contains("is open"), "{}", opened.content);

    // Task 2: a new run in the same conversation.
    let two = next_task(&rig.fx, &remote, &[(&remote, &branch)]).await;
    let prepare = |branch: Option<&str>| {
        let mut args = json!({"repo_url": remote, "base_branch": "main"});
        if let Some(branch) = branch {
            args["branch"] = json!(branch);
        }
        args
    };

    // Only a branch that a commit_and_push of this conversation reported for this repository.
    for wrong in ["agent/never-pushed", "main", "agent/"] {
        let refused = PrepareWorkspace.call(&two, prepare(Some(wrong))).await;
        assert!(is_error(&refused), "{wrong}");
        let refused = text(refused);
        assert!(
            refused.contains("Refused") && refused.contains(&branch),
            "{refused}"
        );
    }
    let other_repo = next_task(
        &rig.fx,
        &remote,
        &[("https://github.com/other/repo", &branch)],
    )
    .await;
    let refused = PrepareWorkspace
        .call(&other_repo, prepare(Some(&branch)))
        .await;
    assert!(
        is_error(&refused),
        "a branch of another repository is not this repository's"
    );
    // A branch that was never pushed to the remote is the workspace's refusal, not a crash.
    let ghost = next_task(&rig.fx, &remote, &[(&remote, "agent/ghost")]).await;
    let ghost_out = PrepareWorkspace
        .call(&ghost, prepare(Some("agent/ghost")))
        .await;
    assert!(is_error(&ghost_out), "{ghost_out:?}");
    assert!(text(ghost_out).contains("agent/ghost"));
    // Notes that somehow name a branch outside the namespace cannot make the tool push onto it.
    let outside = next_task(&rig.fx, &remote, &[(&remote, "main")]).await;
    let outside_out = PrepareWorkspace.call(&outside, prepare(Some("main"))).await;
    assert!(is_error(&outside_out), "{outside_out:?}");
    assert!(text(outside_out).contains("is not a branch that can be continued"));

    let ready = PrepareWorkspace
        .call(&two, prepare(Some(&branch)))
        .await
        .unwrap();
    assert!(!ready.is_error, "{}", ready.content);
    assert!(
        ready.content.contains(&format!("branch: {branch}"))
            && ready.content.contains("earlier task pushed"),
        "{}",
        ready.content
    );
    let worktree = rig.fx.root.join("worktrees").join(two.run_id().to_string());
    assert_eq!(
        std::fs::read_to_string(worktree.join("one.txt")).unwrap(),
        "one\n",
        "the work of the first task is in the worktree"
    );

    std::fs::write(worktree.join("two.txt"), "two\n").unwrap();
    RunChecks
        .call(&two, json!({"command": "true"}))
        .await
        .unwrap();
    let pushed_again = CommitAndPush
        .call(&two, json!({"message": "fix: two"}))
        .await
        .unwrap();
    // The commit went to a branch of this run's own; the branch it continues is unchanged, and
    // the text says so. The last two lines still name the line of work.
    let own = pushed_again
        .content
        .split("pushed branch ")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .unwrap()
        .to_owned();
    assert!(own.starts_with("agent/") && own != branch, "{own}");
    assert!(
        pushed_again.content.contains("has not been changed"),
        "{}",
        pushed_again.content
    );
    assert_eq!(pushed_lines(&pushed_again.content).1, branch);
    assert_eq!(pushed_again.artifacts[1].name, "branch");
    assert_eq!(pushed_again.artifacts[1].data["branch"], own.as_str());
    assert_eq!(pushed_again.artifacts[1].data["continues"], branch.as_str());
    assert_eq!(rig.fx.agent_branches().len(), 2);
    assert_eq!(
        rig.fx.commits_ahead(&branch),
        1,
        "the pull request's branch is unchanged"
    );
    assert_eq!(rig.fx.commits_ahead(&own), 2);
    // The tool recorded the line of work itself, in the notes of the run that pushed.
    let notes = rig
        .fx
        .env
        .notes
        .load(&two.run_id().to_string())
        .await
        .unwrap();
    assert_eq!(notes.continues.as_deref(), Some(branch.as_str()));
    assert!(
        notes.has_pushed(
            &adam_coder::tools::named::key_of_argument(&remote).unwrap(),
            &branch
        ),
        "{notes:?}"
    );

    let reported = OpenPullRequest.call(&two, args).await.unwrap();
    assert!(!reported.is_error, "{}", reported.content);
    assert!(
        reported.content.contains("was already open") && reported.content.contains(PR_URL),
        "{}",
        reported.content
    );
    assert_eq!(reported.artifacts[0].name, "pull_request");
    assert_eq!(reported.artifacts[0].data["url"], PR_URL);
    assert_eq!(reported.artifacts[0].data["branch"], branch.as_str());
    assert_eq!(
        rig.fx.created_pulls().await.len(),
        1,
        "no second pull request"
    );
    // The gate passed, so the continued branch now has the commit, as a fast-forward.
    assert_eq!(rig.fx.commits_ahead(&branch), 2);
    assert_eq!(rig.fx.file_on(&branch, "two.txt"), "two");
    assert!(rig.fx.comments().await.is_empty(), "verified: no note");
}

/// A second task on a continued branch, in a fixture where the first task's pull request is open:
/// returns the context of task 2 with its worktree prepared on the branch, the branch, and the
/// tip the branch has (the code that was verified).
async fn rework_of_an_open_pull_request(
    rig: &Rig,
) -> (ToolCtx, std::path::PathBuf, String, String) {
    let remote = rig.fx.remote_url();
    rig.prepare().await;
    std::fs::write(rig.worktree().join("one.txt"), "one\n").unwrap();
    RunChecks
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap();
    let pushed = CommitAndPush
        .call(&rig.ctx, json!({"message": "feat: one"}))
        .await
        .unwrap();
    let (_, branch) = pushed_lines(&pushed.content);
    let args = json!({"title": "feat: one", "body": "One.\n\n## Verification\n- `true`"});
    let opened = OpenPullRequest.call(&rig.ctx, args).await.unwrap();
    assert!(opened.content.contains("is open"), "{}", opened.content);
    let tip = common::git(
        &rig.fx.remote,
        &["rev-parse", &format!("refs/heads/{branch}")],
    );

    let two = next_task(&rig.fx, &remote, &[(&remote, &branch)]).await;
    let ready = PrepareWorkspace
        .call(
            &two,
            json!({"repo_url": remote, "base_branch": "main", "branch": branch}),
        )
        .await
        .unwrap();
    assert!(!ready.is_error, "{}", ready.content);
    let worktree = rig.fx.root.join("worktrees").join(two.run_id().to_string());
    (two, worktree, branch, tip)
}

/// The gate holds for a branch that already has a pull request: a rework whose checks are red
/// pushes to its own branch and is refused by `open_pull_request`, and neither the branch nor its
/// pull request is touched. Accepting the red checks explicitly moves the branch and leaves a
/// comment that says the update was not verified.
#[tokio::test]
async fn red_checks_on_a_continued_branch_never_touch_the_existing_pull_request() {
    let rig = Rig::new().await;
    let (two, worktree, branch, tip) = rework_of_an_open_pull_request(&rig).await;
    let remote_tip =
        |b: &str| common::git(&rig.fx.remote, &["rev-parse", &format!("refs/heads/{b}")]);

    std::fs::write(worktree.join("two.txt"), "unverified\n").unwrap();
    let red = RunChecks
        .call(&two, json!({"command": "echo broken; exit 1"}))
        .await;
    assert!(is_error(&red));
    let pushed = CommitAndPush
        .call(&two, json!({"message": "fix: two"}))
        .await
        .unwrap();
    assert!(
        pushed.artifacts[0].data["passed"] == json!(false),
        "the verdict on the pushed commit is red: {:?}",
        pushed.artifacts[0].data
    );

    let args = json!({"title": "fix: two", "body": "Two.\n\n## Verification\n- `exit 1`"});
    let refused = OpenPullRequest.call(&two, args.clone()).await;
    assert!(is_error(&refused), "{refused:?}");
    assert!(text(refused).contains("Refusing to open a pull request"));
    assert_eq!(
        remote_tip(&branch),
        tip,
        "the pull request's branch is untouched"
    );
    assert_eq!(rig.fx.commits_ahead(&branch), 1);
    assert_eq!(rig.fx.created_pulls().await.len(), 1);
    assert!(rig.fx.comments().await.is_empty());
    let notes = rig
        .fx
        .env
        .notes
        .load(&two.run_id().to_string())
        .await
        .unwrap();
    assert!(
        notes.pull_request.is_none(),
        "nothing was delivered: {notes:?}"
    );

    // The person accepted the red checks: the branch moves, its pull request is reported, and a
    // comment says what the update was.
    let mut accepted = args;
    accepted["accept_red_checks"] = json!(true);
    let reported = OpenPullRequest.call(&two, accepted).await.unwrap();
    assert!(!reported.is_error, "{}", reported.content);
    assert!(
        reported.content.contains("was already open")
            && reported.content.contains("comment")
            && reported.content.contains(PR_URL),
        "{}",
        reported.content
    );
    assert_ne!(remote_tip(&branch), tip);
    assert_eq!(rig.fx.file_on(&branch, "two.txt"), "unverified");
    assert_eq!(
        rig.fx.created_pulls().await.len(),
        1,
        "still the one pull request"
    );
    let comments = rig.fx.comments().await;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert_eq!(comments[0].0, 7);
    assert!(
        comments[0].1.contains("not green") && comments[0].1.contains("explicitly accepted"),
        "{}",
        comments[0].1
    );
    let notes = rig
        .fx
        .env
        .notes
        .load(&two.run_id().to_string())
        .await
        .unwrap();
    assert!(
        notes
            .pull_request
            .as_ref()
            .is_some_and(|p| p.red_checks_accepted)
    );
}

/// If someone pushed to the continued branch since the task started, its commits are not added
/// (that would overwrite theirs): the error names the branch and says to ask the person, the
/// branch keeps what is on the remote, and no pull request is reported.
#[tokio::test]
async fn a_continued_branch_that_moved_on_the_remote_is_not_overwritten_by_open_pull_request() {
    let rig = Rig::new().await;
    let (two, worktree, branch, _tip) = rework_of_an_open_pull_request(&rig).await;

    std::fs::write(worktree.join("two.txt"), "two\n").unwrap();
    RunChecks
        .call(&two, json!({"command": "true"}))
        .await
        .unwrap();
    let pushed = CommitAndPush
        .call(&two, json!({"message": "fix: two"}))
        .await
        .unwrap();
    let own = pushed.artifacts[1].data["branch"]
        .as_str()
        .unwrap()
        .to_owned();

    // Somebody else moves the branch on the remote: a commit on top of it from another clone.
    let other = rig.fx.tmp.path().join("someone-else");
    common::git(
        rig.fx.tmp.path(),
        &[
            "clone",
            "--quiet",
            &rig.fx.remote_url(),
            other.to_str().unwrap(),
        ],
    );
    common::git(
        &other,
        &[
            "checkout",
            "--quiet",
            "-B",
            "theirs",
            &format!("origin/{branch}"),
        ],
    );
    std::fs::write(other.join("theirs.txt"), "theirs\n").unwrap();
    common::git(&other, &["add", "-A"]);
    common::git(&other, &["commit", "--quiet", "-m", "theirs"]);
    common::git(
        &other,
        &[
            "push",
            "--quiet",
            "origin",
            &format!("theirs:refs/heads/{branch}"),
        ],
    );
    let theirs = common::git(
        &rig.fx.remote,
        &["rev-parse", &format!("refs/heads/{branch}")],
    );

    let args = json!({"title": "fix: two", "body": "Two.\n\n## Verification\n- `true`"});
    let refused = OpenPullRequest.call(&two, args).await;
    assert!(is_error(&refused), "{refused:?}");
    let refused = text(refused);
    assert!(
        refused.contains("the branch moved on the remote since this task started")
            && refused.contains("ask_user")
            && refused.contains(&branch)
            && refused.contains(&own),
        "{refused}"
    );
    assert_eq!(
        common::git(
            &rig.fx.remote,
            &["rev-parse", &format!("refs/heads/{branch}")]
        ),
        theirs,
        "never forced"
    );
    assert_eq!(
        rig.fx.file_on(&own, "two.txt"),
        "two",
        "the work is on the run's own branch"
    );
    let notes = rig
        .fx
        .env
        .notes
        .load(&two.run_id().to_string())
        .await
        .unwrap();
    assert!(notes.pull_request.is_none());
}
