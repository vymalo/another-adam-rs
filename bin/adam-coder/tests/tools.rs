//! The tools one by one, against real worktrees over a local bare remote.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_coder::ToolEnv;
use adam_coder::opencode::OpenCodeLaunch;
use adam_coder::tools::checks::RunChecks;
use adam_coder::tools::delegate::DelegateToOpenCode;
use adam_coder::tools::inspect::RunCommand;
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_coder::tools::notes::PushedBranch;
use adam_coder::tools::prepare::PrepareWorkspace;
use adam_coder::tools::publish::{CommitAndPush, OpenPullRequest};
use adam_llm_agent::{Tool, ToolCtx, ToolError, ToolOutput};
use adam_runtime::{
    CancelToken, CollectingSink, RunEvent, StepEvent, StepIcon, StepKind, StepState,
};
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
                // What a tool says with `emit_progress`: the detail of an update of its own step.
                RunEvent::Step(step) if step.parent.is_none() => step.detail,
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

/// A repository nobody named is not probed by leaving the base branch out: the default branch is
/// asked of the remote with the credentials, so the gate comes first. Refused, the remote sees no
/// request and no mirror is made; named, the same call reaches it.
#[tokio::test]
async fn an_unnamed_repository_is_not_probed_for_its_default_branch() {
    use adam_workspace::{ScopedToken, Workspaces};
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let rig = Rig::new().await;
    let remote = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .mount(&remote)
        .await;
    let host = format!("127.0.0.1:{}", remote.address().port());
    let work = rig.fx.tmp.path().join("probe-work");
    let env = Arc::new(ToolEnv::new(
        Workspaces::new(
            work.clone(),
            Arc::new(ScopedToken::new(host.as_str(), common::GITHUB_TOKEN)),
        )
        .allow_hosts([host.clone()])
        .allow_local(true),
        rig.fx.env.code_host.clone(),
        rig.fx.env.settings.clone(),
    ));
    let ctx = ToolCtx::detached("tool", "call-x", Arc::new(CollectingSink::new()))
        .with_state(env.clone());
    let probe = json!({"repo_url": format!("http://{host}/octo/private.git")});

    let out = PrepareWorkspace.call(&ctx, probe.clone()).await;
    assert!(is_error(&out), "{out:?}");
    assert!(text(out).contains("ask_user"));
    assert!(
        remote.received_requests().await.unwrap().is_empty(),
        "no request reached the remote"
    );
    assert!(!work.join("git").exists(), "no mirror was made");
    // The branch form of the probe is refused the same way.
    let with_branch =
        json!({"repo_url": format!("http://{host}/octo/private.git"), "branch": "agent/x"});
    assert!(is_error(&PrepareWorkspace.call(&ctx, with_branch).await));
    assert!(remote.received_requests().await.unwrap().is_empty());

    // Named: the same call goes past the gate and asks the remote.
    say(
        &env,
        &ctx,
        &format!("work on http://{host}/octo/private.git"),
    )
    .await;
    let _ = PrepareWorkspace.call(&ctx, probe).await;
    assert!(!remote.received_requests().await.unwrap().is_empty());
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
        progress.contains(&"starting OpenCode".to_owned()),
        "{progress:?}"
    );
    assert!(
        progress.contains(&"write: ok".to_owned()),
        "a line of the agent's reply is an update of the OpenCode step: {progress:?}"
    );

    // The call is a sub-agent step, and OpenCode's tool call is a child under it, ended by its
    // update; the reply is a `message` child at the end.
    let style = DelegateToOpenCode.step_style();
    assert_eq!(style.kind, StepKind::Subagent);
    assert_eq!(style.label.as_deref(), Some("OpenCode"));
    assert_eq!(style.icon, Some(StepIcon::Agent));
    let call = rig.ctx.step_id().to_owned();
    let child_id = format!("acp:{}:tc-1", rig.ctx.call_id());
    let steps: Vec<StepEvent> = rig
        .sink
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            RunEvent::Step(step) => Some(step),
            _ => None,
        })
        .collect();
    let child = |state| {
        steps
            .iter()
            .filter(|s| s.id == child_id && s.state == state)
            .collect::<Vec<_>>()
    };
    let started = child(StepState::Running);
    let ended = child(StepState::Completed);
    assert_eq!((started.len(), ended.len()), (1, 1), "{steps:#?}");
    for step in [started[0], ended[0]] {
        assert_eq!(step.parent.as_deref(), Some(call.as_str()));
        assert_eq!(
            (step.kind, step.icon),
            (StepKind::Tool, Some(StepIcon::Edit))
        );
        assert!(
            step.label.starts_with("Write ") && step.label.ends_with("hello.txt"),
            "{}",
            step.label
        );
    }
    let summary = steps
        .iter()
        .find(|s| s.id.ends_with(":summary"))
        .unwrap_or_else(|| panic!("no summary step in {steps:#?}"));
    assert_eq!(
        (
            summary.parent.as_deref(),
            summary.kind,
            summary.state,
            summary.label.as_str()
        ),
        (
            Some(call.as_str()),
            StepKind::Message,
            StepState::Completed,
            "OpenCode's summary"
        )
    );
    assert!(
        summary.detail.as_deref().unwrap().contains("write: ok"),
        "{summary:?}"
    );
    // How the turn ended is in the result (`stop reason: end_turn`, above), not a line of its own.
    assert!(
        !progress.iter().any(|p| p.contains("turn ended")),
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
    // The coder's `ask_user` is the screen's (`adam-ui`), under the coder's own words about when to
    // ask: with no choices it is the question, as text.
    let ask = rig.fx.env.ui.tools().get("ask_user").cloned().unwrap();
    assert!(ask.asks_user());
    let out = ask
        .call(&rig.ctx, json!({"question": " Which repo? "}))
        .await;
    assert_eq!(out, Err(ToolError::needs_input("Which repo?")));
    assert!(is_error(&ask.call(&rig.ctx, Value::Null).await));
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

// ------------------------------------------------ run_command: looking around, and nothing else

/// `git status --porcelain` and `HEAD` of the run's worktree, with the content of every file
/// that is not ignored: what `run_command` must leave as it found it.
fn worktree_state(rig: &Rig) -> (String, String, String) {
    let dir = rig.worktree();
    (
        common::git(&dir, &["status", "--porcelain=v1", "--untracked-files=all"]),
        common::git(&dir, &["rev-parse", "HEAD"]),
        common::git(&dir, &["symbolic-ref", "HEAD"]),
    )
}

async fn failures(rig: &Rig) -> u32 {
    let notes = rig
        .fx
        .env
        .notes
        .load(&rig.ctx.run_id().to_string())
        .await
        .unwrap();
    notes.checks.failures
}

/// A command for looking is not a check: no artifact, no cycle, and a non-zero exit is an answer.
#[tokio::test]
async fn run_command_looks_around_without_reporting_checks_or_using_cycles() {
    let rig = Rig::new().await;
    rig.prepare().await;

    let listing = RunCommand
        .call(
            &rig.ctx,
            json!({"command": "ls; git branch -r; cat README.md"}),
        )
        .await
        .unwrap();
    assert!(!listing.is_error, "{}", listing.content);
    assert!(
        listing.content.contains("README.md")
            && listing.content.contains("origin/main")
            && listing.content.contains("widgets")
            && listing.content.contains("exit code 0"),
        "{}",
        listing.content
    );
    assert!(listing.artifacts.is_empty(), "no checks artifact");

    // Exits that are answers (a missing file), as many times as the model likes: no cycle is used
    // and nothing is reported, where run_checks would have failed the run at the third.
    for _ in 0..(rig.fx.env.settings.max_check_cycles + 2) {
        let out = RunCommand
            .call(&rig.ctx, json!({"command": "cat CLAUDE.md"}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("exit code 1"), "{}", out.content);
        assert!(out.content.contains("CLAUDE.md"), "{}", out.content);
        assert!(out.artifacts.is_empty());
    }
    assert_eq!(failures(&rig).await, 0, "no check cycle used");
    let notes = rig
        .fx
        .env
        .notes
        .load(&rig.ctx.run_id().to_string())
        .await
        .unwrap();
    assert!(notes.checks.last.is_none(), "nothing recorded as a check");
    // The budget is whole: a real check still runs, and reports.
    let check = RunChecks
        .call(&rig.ctx, json!({"command": "true"}))
        .await
        .unwrap();
    assert_eq!(check.artifacts.len(), 1);

    // cwd stays inside the worktree, and a missing workspace is said.
    let out = RunCommand
        .call(&rig.ctx, json!({"command": "ls", "cwd": "../.."}))
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("cwd"),
        "{}",
        out.content
    );
    let blank = RunCommand
        .call(&rig.ctx, json!({"command": "  "}))
        .await
        .unwrap();
    assert!(blank.is_error);
    let other = Rig::new().await;
    let none = RunCommand
        .call(&other.ctx, json!({"command": "ls"}))
        .await
        .unwrap();
    assert!(
        none.is_error && none.content.contains("prepare_workspace"),
        "{}",
        none.content
    );
}

/// `run_command` is not an editing path: what a command changes is undone, whatever it is, and
/// the uncommitted work that was in the worktree is exactly as it was.
#[tokio::test]
async fn run_command_undoes_a_change_and_says_where_changes_go() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let dir = rig.worktree();
    // Uncommitted work of the run: an edit to a tracked file, a new file, a deleted-to-be file.
    std::fs::write(dir.join("README.md"), "widgets\nedited by opencode\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "mine\n").unwrap();
    std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
    let before = worktree_state(&rig);
    let content_before = std::fs::read_to_string(dir.join("README.md")).unwrap();
    let diff_before = common::git(&dir, &["diff", "HEAD"]);

    for command in [
        // New file.
        "echo x > created.txt",
        // A second edit to a file that was already modified (git status looks the same).
        "echo more >> README.md",
        // Delete tracked and untracked files, and create a directory with a nested repository.
        "rm README.md notes.txt; mkdir -p sub && git init -q sub && echo y > sub/f",
        // HEAD: a commit, a new branch, a reset.
        "git add -A && git -c user.name=a -c user.email=a@b commit -qm sneaky",
        "git checkout -q -b elsewhere",
        "git reset -q --hard HEAD && git -c user.name=a -c user.email=a@b commit -q --allow-empty -m e",
        // Through a tool that edits in place.
        "sed -i 's/widgets/gadgets/' README.md",
    ] {
        let out = RunCommand
            .call(&rig.ctx, json!({ "command": command }))
            .await
            .unwrap();
        assert!(out.is_error, "{command}: {}", out.content);
        assert!(
            out.content.contains("changed the worktree")
                && out.content.contains("exactly as it was")
                && out.content.contains("delegate_to_opencode"),
            "{command}: {}",
            out.content
        );
        assert!(out.artifacts.is_empty());
        assert_eq!(
            worktree_state(&rig),
            before,
            "{command}: status, HEAD and branch"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("README.md")).unwrap(),
            content_before,
            "{command}: a tracked file"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
            "mine\n",
            "{command}: an untracked file"
        );
        assert!(!dir.join("created.txt").exists(), "{command}");
        assert!(!dir.join("sub").exists(), "{command}");
        assert_eq!(
            common::git(&dir, &["diff", "HEAD"]),
            diff_before,
            "{command}: the tracked changes, to the byte"
        );
    }
    // Branch `elsewhere` (made by one of the commands) was only ever HEAD's name: the run is
    // back on its own branch, and the cycles are untouched.
    assert!(
        common::git(&dir, &["symbolic-ref", "--short", "HEAD"]).starts_with("agent/"),
        "back on the run's own branch"
    );
    assert_eq!(failures(&rig).await, 0);

    // Writing where git does not look (ignored build output) is not a change to the worktree.
    let built = RunCommand
        .call(
            &rig.ctx,
            json!({"command": "mkdir -p target && echo built > target/out && cat target/out"}),
        )
        .await
        .unwrap();
    assert!(!built.is_error, "{}", built.content);
    assert!(built.content.contains("built"));
    assert_eq!(worktree_state(&rig), before);

    // Reading commands that look like editing ones are fine: `git status`, `git diff`, `sed -n`.
    let reading = RunCommand
        .call(
            &rig.ctx,
            json!({"command": "git status --short; git diff --stat; sed -n 1p README.md"}),
        )
        .await
        .unwrap();
    assert!(!reading.is_error, "{}", reading.content);
    assert!(reading.content.contains("notes.txt"), "{}", reading.content);
}

/// What a command could change without touching a file of the worktree, and that later commands
/// of the coder's own (`git status`, the push that carries the token) would act on: git's
/// configuration and the refs. The refs of other runs (`agent/*`) are theirs and not compared.
#[tokio::test]
async fn run_command_undoes_changes_to_the_git_configuration_and_to_refs() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let dir = rig.worktree();
    common::git(&dir, &["branch", "keep"]);
    // (Restoring a key appends it: where it sits in the file does not matter.)
    let config = |d: &std::path::Path| {
        let mut lines: Vec<String> = common::git(d, &["config", "--local", "--list"])
            .lines()
            .map(str::to_owned)
            .collect();
        lines.sort();
        lines
    };
    let refs = |d: &std::path::Path| {
        common::git(
            d,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
                "refs/tags",
            ],
        )
    };
    let (config_before, refs_before) = (config(&dir), refs(&dir));
    let ran = rig.fx.tmp.path().join("fsmonitor-ran");

    for command in [
        // A program git runs on every `status`.
        &format!("git config core.fsmonitor 'touch {}'", ran.display()),
        // Where the credentials would go.
        "git config remote.origin.pushurl https://evil.example/octo/widgets.git",
        "git config url.https://evil.example/.insteadOf https://github.com/",
        "git config alias.status '!echo pwned'",
        "git config --unset remote.origin.url",
        // Refs outside the run's own branch.
        "git branch created",
        "git update-ref refs/heads/zzz HEAD",
        "git branch -D keep",
        "git update-ref -d refs/heads/keep && git branch keep2",
    ] {
        let out = RunCommand
            .call(&rig.ctx, json!({ "command": command }))
            .await
            .unwrap();
        assert!(out.is_error, "{command}: {}", out.content);
        assert!(
            out.content.contains("changed the worktree")
                && out.content.contains("exactly as it was"),
            "{command}: {}",
            out.content
        );
        assert_eq!(config(&dir), config_before, "{command}: the configuration");
        assert_eq!(refs(&dir), refs_before, "{command}: the refs");
    }
    // Other runs' branches move while a command runs: that is not this command's change. A
    // command that only reads is not refused for it.
    let read = RunCommand
        .call(
            &rig.ctx,
            json!({"command": "git config --local --list | head -3; git for-each-ref"}),
        )
        .await
        .unwrap();
    assert!(!read.is_error, "{}", read.content);
    assert!(!ran.exists(), "the fsmonitor program never ran");
}

/// An embedded repository with no commit makes `git add -A` fail, so there is no tree id: the
/// snapshot falls back to `git status`, so that looking around still works (it used to refuse
/// every command), a change is still seen, and nothing is "restored" from a tree that does not
/// exist.
#[tokio::test]
async fn run_command_still_works_when_the_tree_cannot_be_computed() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let dir = rig.worktree();
    let embedded = dir.join("vendor/embedded");
    std::fs::create_dir_all(&embedded).unwrap();
    common::git(&embedded, &["init", "--quiet"]);
    std::fs::write(embedded.join("f.txt"), "x\n").unwrap();

    let ls = RunCommand
        .call(
            &rig.ctx,
            json!({"command": "ls vendor; git status --short"}),
        )
        .await
        .unwrap();
    assert!(!ls.is_error, "{}", ls.content);
    assert!(ls.content.contains("embedded"), "{}", ls.content);

    // A change is seen. The files are not written back (there is nothing exact to write), and the
    // model is told.
    let changed = RunCommand
        .call(&rig.ctx, json!({"command": "echo y > added.txt"}))
        .await
        .unwrap();
    assert!(changed.is_error, "{}", changed.content);
    assert!(
        changed.content.contains("changed the worktree")
            && changed.content.contains("could not be fully undone"),
        "{}",
        changed.content
    );
}

/// Refs and configuration are shared by every run on the mirror: what another run (or a fetch, or
/// a `git stash` in another worktree) writes while a command runs is not this command's change, so
/// it is neither refused nor undone. A rewritten `.git` file is put right.
#[tokio::test]
async fn run_command_leaves_what_other_runs_write_meanwhile_alone() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let dir = rig.worktree();
    let concurrent = async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        common::git(&dir, &["tag", "fetched-tag"]);
        common::git(&dir, &["update-ref", "refs/stash", "HEAD"]);
        common::git(&dir, &["branch", "agent/other-run"]);
        common::git(
            &dir,
            &["config", "branch.agent/other-run.adam-run", "other"],
        );
    };
    let (out, ()) = tokio::join!(
        async {
            RunCommand
                .call(&rig.ctx, json!({"command": "sleep 1; echo looked"}))
                .await
                .unwrap()
        },
        concurrent
    );
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("looked"));
    let refs = common::git(&dir, &["for-each-ref", "--format=%(refname)"]);
    for kept in [
        "refs/tags/fetched-tag",
        "refs/stash",
        "refs/heads/agent/other-run",
    ] {
        assert!(refs.contains(kept), "{kept} was deleted: {refs}");
    }
    assert!(
        common::git(&dir, &["config", "--local", "--list"])
            .contains("branch.agent/other-run.adam-run=other")
    );

    // A command that rewrites the `.git` file would point every later git call elsewhere: it is
    // refused, the file is put back, and the worktree works.
    let dot_git = std::fs::read_to_string(dir.join(".git")).unwrap();
    let out = RunCommand
        .call(
            &rig.ctx,
            json!({"command": "echo 'gitdir: /nonexistent' > .git"}),
        )
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert_eq!(std::fs::read_to_string(dir.join(".git")).unwrap(), dot_git);
    let status = RunCommand
        .call(&rig.ctx, json!({"command": "git status --short"}))
        .await
        .unwrap();
    assert!(!status.is_error, "{}", status.content);
}

/// A continued branch's base is the base of its pull request: when that base is gone upstream, the
/// error says it is fixed and to ask, not to pick another.
#[tokio::test]
async fn a_recorded_base_that_is_gone_is_not_to_be_replaced() {
    let rig = Rig::new().await;
    let remote = rig.fx.remote_url();
    let (_two, _worktree, branch, _tip) = rework_of_an_open_pull_request(&rig).await;
    let three = next_task_with_base(&rig.fx, &remote, &[(&remote, &branch)], Some("gone")).await;
    let out = PrepareWorkspace
        .call(
            &three,
            json!({"repo_url": remote, "base_branch": "main", "branch": branch}),
        )
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("branch gone does not exist")
            && out.content.contains("fixed by that pull request")
            && out.content.contains("ask_user"),
        "{}",
        out.content
    );
}

/// A toolchain the workspace lacks is reported as that: the workspace lacks it, no cycle is used,
/// no failing `checks` artifact exists, and the model is told to report and wait (nothing is
/// installed).
#[tokio::test]
async fn a_missing_toolchain_is_reported_and_costs_nothing() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let dir = rig.worktree();

    for _ in 0..(rig.fx.env.settings.max_check_cycles + 2) {
        let out = RunChecks
            .call(
                &rig.ctx,
                json!({"command": "no-such-toolchain package -DskipTests"}),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        let said = &out.content;
        assert!(
            said.contains("The workspace has no `no-such-toolchain`")
                && said.contains("no check cycle was used")
                && said.contains("ask_user")
                && said.contains("do not try to install it"),
            "{said}"
        );
        assert!(out.artifacts.is_empty(), "no failing checks artifact");
    }
    assert_eq!(failures(&rig).await, 0);
    let notes = rig
        .fx
        .env
        .notes
        .load(&rig.ctx.run_id().to_string())
        .await
        .unwrap();
    assert!(
        notes.checks.last.is_none(),
        "not recorded: the gate sees no check"
    );
    assert!(!notes.cycles_exhausted(rig.fx.env.settings.max_check_cycles));

    // The same in a script, and from run_command (which also leaves the worktree alone).
    std::fs::write(
        dir.join("check.sh"),
        "#!/bin/sh\necho building\nnosuchbuild verify\n",
    )
    .unwrap();
    let before = worktree_state(&rig);
    let scripted = RunChecks
        .call(&rig.ctx, json!({"command": "sh ./check.sh"}))
        .await
        .unwrap();
    assert!(
        scripted.is_error
            && scripted.content.contains("no `nosuchbuild`")
            && scripted.artifacts.is_empty(),
        "{}",
        scripted.content
    );
    let looked = RunCommand
        .call(&rig.ctx, json!({"command": "nosuchbuild -v"}))
        .await
        .unwrap();
    assert!(
        looked.is_error && looked.content.contains("no `nosuchbuild`"),
        "{}",
        looked.content
    );
    assert!(looked.artifacts.is_empty());
    assert_eq!(worktree_state(&rig), before);
    assert_eq!(failures(&rig).await, 0);

    // A check that really fails still costs a cycle, and says it is a failed check.
    let red = RunChecks
        .call(&rig.ctx, json!({"command": "echo nope; exit 3"}))
        .await
        .unwrap();
    assert!(
        red.is_error && red.content.contains("failed check run 1"),
        "{}",
        red.content
    );
    assert_eq!(red.artifacts.len(), 1);
    assert_eq!(failures(&rig).await, 1);
}

/// A tool the project brings itself is a dependency to install with the project's own command, not
/// a toolchain to wait for; and a "not found" printed by a nested shell in a run that failed on
/// its own terms is a failed check, costing its cycle.
#[tokio::test]
async fn a_project_dependency_is_to_be_installed_and_a_nested_not_found_is_a_failed_check() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let dir = rig.worktree();
    std::fs::write(
        dir.join("package.json"),
        r#"{"devDependencies": {"zzz-local-tool": "1"}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
    // The first time: install the project's dependencies, through OpenCode and not run_checks.
    let first = RunChecks
        .call(&rig.ctx, json!({"command": "zzz-local-tool --run"}))
        .await
        .unwrap();
    assert!(first.is_error, "{}", first.content);
    let said = &first.content;
    assert!(
        said.contains("`zzz-local-tool` is not on the PATH")
            && said.contains("this project's own dependencies")
            && said.contains("`pnpm install`")
            && said.contains("with delegate_to_opencode (not run_checks")
            && said.contains("no check cycle was used")
            && !said.contains("missing toolchain, not a failing check"),
        "{said}"
    );
    assert!(first.artifacts.is_empty());
    // The same call again (a replay of it) gets the same answer and counts once.
    let replay = RunChecks
        .call(&rig.ctx, json!({"command": "zzz-local-tool --run"}))
        .await
        .unwrap();
    assert_eq!(replay.content, first.content);
    let run = rig.ctx.run_id().to_string();
    assert_eq!(
        rig.fx
            .env
            .notes
            .load(&run)
            .await
            .unwrap()
            .missing_tools
            .len(),
        1
    );

    // When an earlier call of the run already found the same tool missing, installing did not
    // help: the answer is to ask the person (the call that comes after it is another call id).
    let mut notes = rig.fx.env.notes.load(&run).await.unwrap();
    notes.missing_tools.clear();
    notes.record_missing_tool("an-earlier-call", "zzz-local-tool");
    rig.fx.env.notes.save(&run, &notes).await.unwrap();
    let second = RunCommand
        .call(&rig.ctx, json!({"command": "zzz-local-tool --again"}))
        .await
        .unwrap();
    assert!(second.is_error, "{}", second.content);
    assert!(
        second
            .content
            .contains("The workspace has no `zzz-local-tool`")
            && second.content.contains("still missing")
            && second.content.contains("ask_user")
            && !second.content.contains("pnpm install"),
        "{}",
        second.content
    );
    assert!(second.artifacts.is_empty());
    assert_eq!(failures(&rig).await, 0);

    // A nested shell says "not found", and the run fails on its own terms: a failed check.
    let red = RunChecks
        .call(
            &rig.ctx,
            json!({"command": "sh -c 'zzz-inner-tool'; echo tests failed; exit 101"}),
        )
        .await
        .unwrap();
    assert!(
        red.is_error && red.content.contains("failed check run 1"),
        "{}",
        red.content
    );
    assert_eq!(red.artifacts.len(), 1, "reported as a failed check");
    assert_eq!(failures(&rig).await, 1);
}

/// Bash constructs work in both tools where the image has bash (the owner's `${PIPESTATUS[0]}`
/// was "Bad substitution" under dash).
#[tokio::test]
async fn both_tools_run_bash_when_there_is_bash() {
    if adam_coder::tools::shell::login_shell() != "bash" {
        eprintln!("skipping: no bash on PATH");
        return;
    }
    let rig = Rig::new().await;
    rig.prepare().await;
    let command =
        "false | true; echo first=${PIPESTATUS[0]}; [[ -f README.md ]] && echo has-readme";
    let looked = RunCommand
        .call(&rig.ctx, json!({ "command": command }))
        .await
        .unwrap();
    assert!(!looked.is_error, "{}", looked.content);
    assert!(
        looked.content.contains("first=1") && looked.content.contains("has-readme"),
        "{}",
        looked.content
    );
    let checked = RunChecks
        .call(&rig.ctx, json!({ "command": command }))
        .await
        .unwrap();
    assert!(!checked.is_error, "{}", checked.content);
    assert!(checked.content.contains("first=1"), "{}", checked.content);
    assert!(!checked.content.contains("Bad substitution"));
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
    next_task_with_base(fx, repo, pushed, Some("main")).await
}

/// [`next_task`], where what the first task's notes recorded as the base of the branches is `base`
/// (`None`: the notes did not have one, as when the branches come from the result text).
async fn next_task_with_base(
    fx: &Fixture,
    repo: &str,
    pushed: &[(&str, &str)],
    base: Option<&str>,
) -> ToolCtx {
    let ctx = ToolCtx::detached("tool", "call-1", Arc::new(CollectingSink::new()))
        .with_state(fx.env.clone());
    say(&fx.env, &ctx, repo).await;
    let run = ctx.run_id().to_string();
    let mut notes = fx.env.notes.load(&run).await.unwrap();
    notes.name_pushed_branches(pushed.iter().map(|(repo, branch)| PushedBranch {
        repo: adam_coder::tools::named::key_of_argument(repo).unwrap(),
        branch: (*branch).to_owned(),
        // What `commit_and_push` of the first task recorded, and the next task inherits.
        base: base.map(str::to_owned),
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

/// A rework works against the base of the pull request it continues: the model's base is not
/// used when the notes recorded one, and a branch with no recorded base (it came from the result
/// text) still finds its pull request by head, so no second pull request is opened against
/// another base.
#[tokio::test]
async fn a_continued_branch_keeps_the_base_of_its_pull_request() {
    let rig = Rig::new().await;
    let remote = rig.fx.remote_url();
    common::git(&rig.fx.remote, &["branch", "develop", "refs/heads/main"]);
    let (first_two, _worktree, branch, _tip) = rework_of_an_open_pull_request(&rig).await;
    // The notes the first task wrote recorded the base.
    let first = rig
        .fx
        .env
        .notes
        .load(&rig.ctx.run_id().to_string())
        .await
        .unwrap();
    assert_eq!(
        first.pushed_base(
            &adam_coder::tools::named::key_of_argument(&remote).unwrap(),
            &branch
        ),
        Some("main")
    );
    drop(first_two);

    // Recorded: the model says `develop`, the run works against `main`.
    let two = next_task(&rig.fx, &remote, &[(&remote, &branch)]).await;
    let ready = PrepareWorkspace
        .call(
            &two,
            json!({"repo_url": remote, "base_branch": "develop", "branch": branch}),
        )
        .await
        .unwrap();
    assert!(!ready.is_error, "{}", ready.content);
    assert!(
        ready.content.contains("base branch: main"),
        "{}",
        ready.content
    );

    // Not recorded (the branch came from the text of a result): the model's `develop` is what
    // the run has, and the pull request of the branch is still found, once, against `main`.
    let three = next_task_with_base(&rig.fx, &remote, &[(&remote, &branch)], None).await;
    let ready = PrepareWorkspace
        .call(
            &three,
            json!({"repo_url": remote, "base_branch": "develop", "branch": branch}),
        )
        .await
        .unwrap();
    assert!(
        ready.content.contains("base branch: develop"),
        "{}",
        ready.content
    );
    let dir = rig
        .fx
        .root
        .join("worktrees")
        .join(three.run_id().to_string());
    std::fs::write(dir.join("three.txt"), "three\n").unwrap();
    RunChecks
        .call(&three, json!({"command": "true"}))
        .await
        .unwrap();
    CommitAndPush
        .call(&three, json!({"message": "fix: three"}))
        .await
        .unwrap();
    let args = json!({"title": "fix: three", "body": "Three.\n\n## Verification\n- `true`"});
    let reported = OpenPullRequest.call(&three, args).await.unwrap();
    assert!(
        reported.content.contains("was already open") && reported.content.contains(PR_URL),
        "{}",
        reported.content
    );
    assert_eq!(
        rig.fx.created_pulls().await.len(),
        1,
        "no second pull request"
    );
    // The branch was recorded with the base the run worked against.
    let notes = rig
        .fx
        .env
        .notes
        .load(&three.run_id().to_string())
        .await
        .unwrap();
    assert_eq!(
        notes.pushed_base(
            &adam_coder::tools::named::key_of_argument(&remote).unwrap(),
            &branch
        ),
        Some("develop")
    );
}

/// A comment that cannot be posted does not make the update "not updated": the branch has the
/// commits, the pull request is reported with a warning, the verdict says so if the run ends; and
/// the note is posted once per pushed commit, not again by a repeated call.
#[tokio::test]
async fn a_comment_that_fails_leaves_the_update_delivered_and_the_note_is_posted_once() {
    let rig = Rig::new().await;
    let (two, worktree, branch, tip) = rework_of_an_open_pull_request(&rig).await;
    std::fs::write(worktree.join("two.txt"), "unverified\n").unwrap();
    RunChecks
        .call(&two, json!({"command": "exit 1"}))
        .await
        .ok();
    CommitAndPush
        .call(&two, json!({"message": "fix: two"}))
        .await
        .unwrap();
    let args = json!({
        "title": "fix: two",
        "body": "Two.\n\n## Verification\n- none",
        "accept_red_checks": true
    });

    common::comments_fail_with(&rig.fx.github, 403).await;
    let reported = OpenPullRequest.call(&two, args.clone()).await.unwrap();
    assert!(!reported.is_error, "{}", reported.content);
    assert!(
        reported.content.contains("could not be posted")
            && reported.content.contains("tell the person"),
        "{}",
        reported.content
    );
    assert_ne!(
        common::git(
            &rig.fx.remote,
            &["rev-parse", &format!("refs/heads/{branch}")]
        ),
        tip,
        "the branch has the commits"
    );
    let notes = rig
        .fx
        .env
        .notes
        .load(&two.run_id().to_string())
        .await
        .unwrap();
    assert!(notes.published && notes.pull_request.is_some());
    assert_eq!(
        notes.pull_request.as_ref().unwrap().commented_sha,
        None,
        "not posted, so a repeat tries again"
    );
}

/// The note is posted once for a pushed commit: the same call again (a crash before it was
/// journaled) finds it in the notes.
#[tokio::test]
async fn the_red_checks_note_is_not_posted_twice_for_the_same_commit() {
    let rig = Rig::new().await;
    let (two, worktree, _branch, _tip) = rework_of_an_open_pull_request(&rig).await;
    std::fs::write(worktree.join("two.txt"), "unverified\n").unwrap();
    CommitAndPush
        .call(&two, json!({"message": "fix: two"}))
        .await
        .unwrap();
    let args = json!({
        "title": "fix: two",
        "body": "Two.\n\n## Verification\n- none",
        "accept_red_checks": true
    });
    OpenPullRequest.call(&two, args.clone()).await.unwrap();
    OpenPullRequest.call(&two, args).await.unwrap();
    assert_eq!(
        rig.fx.comments().await.len(),
        1,
        "{:?}",
        rig.fx.comments().await
    );
}
