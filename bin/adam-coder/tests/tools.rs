//! The tools one by one, against real worktrees over a local bare remote.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_coder::ToolEnv;
use adam_coder::opencode::OpenCodeLaunch;
use adam_coder::tools::checks::RunChecks;
use adam_coder::tools::create::CreateRepository;
use adam_coder::tools::delegate::DelegateToOpenCode;
use adam_coder::tools::files::{ApplyPatch, ReadFile, WriteFile};
use adam_coder::tools::inspect::RunCommand;
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_coder::tools::notes::{Consent, PushedBranch};
use adam_coder::tools::prepare::PrepareWorkspace;
use adam_coder::tools::publish::{CommitAndPush, OpenPullRequest};
use adam_coder::tools::scratch::{PublishScratch, StartScratch};
use adam_coder::tools::share::ShareFile;
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
        common::slot_dir(&self.fx.root, &self.ctx.run_id().to_string())
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
        !root.join("git").exists() && !root.join("workspaces").exists(),
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
    // Beside the worktree, in the run's workspace, and above it.
    let escaped = rig.worktree().parent().unwrap().join("escaped.txt");
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
    let worktree = common::slot_dir(&rig.fx.root, &two.run_id().to_string());
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
    let worktree = common::slot_dir(&rig.fx.root, &two.run_id().to_string());
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
    let dir = common::slot_dir(&rig.fx.root, &three.run_id().to_string());
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

// ------------------------------------------------------------------ read_file, write_file, apply_patch

/// A unified diff that changes the one line of `file` (`old` becomes `new`).
fn one_line_patch(file: &str, old: &str, new: &str) -> String {
    format!("--- a/{file}\n+++ b/{file}\n@@ -1 +1 @@\n-{old}\n+{new}\n")
}

#[tokio::test]
async fn the_file_tools_need_a_workspace_like_the_others() {
    let rig = Rig::new().await;
    let cases: Vec<(&str, Result<ToolOutput, ToolError>)> = vec![
        (
            "read",
            ReadFile.call(&rig.ctx, json!({"path": "README.md"})).await,
        ),
        (
            "write",
            WriteFile
                .call(&rig.ctx, json!({"path": "a.txt", "content": "a"}))
                .await,
        ),
        (
            "patch",
            ApplyPatch
                .call(
                    &rig.ctx,
                    json!({"patch": one_line_patch("README.md", "a", "b")}),
                )
                .await,
        ),
    ];
    for (name, out) in cases {
        assert!(is_error(&out), "{name}: {out:?}");
        assert!(text(out).contains("prepare_workspace"), "{name}");
    }
}

#[tokio::test]
async fn read_file_reads_the_worktree_and_numbers_a_range() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let wt = rig.worktree();
    std::fs::write(wt.join("lines.txt"), "one\ntwo\nthree\nfour\n").unwrap();

    let whole = ReadFile
        .call(&rig.ctx, json!({"path": "README.md"}))
        .await
        .unwrap();
    assert!(!whole.is_error, "{}", whole.content);
    assert_eq!(whole.content, "widgets\n", "the file as it is");
    assert!(
        rig.progress()
            .contains(&"read README.md (remote)".to_owned()),
        "{:?}",
        rig.progress()
    );

    let range = ReadFile
        .call(
            &rig.ctx,
            json!({"path": "lines.txt", "start_line": 2, "end_line": 3}),
        )
        .await
        .unwrap();
    assert_eq!(range.content, "     2\ttwo\n     3\tthree\n");

    // Binary, a directory, a missing file, an empty range: results the model can act on.
    std::fs::write(wt.join("data.bin"), b"\x00\x01\x02 binary").unwrap();
    for (args, needle) in [
        (json!({"path": "data.bin"}), "binary file, 10 bytes"),
        (json!({"path": "."}), "name a file"),
        (json!({"path": "nope.txt"}), "does not exist"),
        (
            json!({"path": "lines.txt", "start_line": 9}),
            "past the end",
        ),
        (json!({"path": "lines.txt", "start_line": 0}), "from 1"),
        (json!({"path": "  "}), "path is required"),
    ] {
        let out = ReadFile.call(&rig.ctx, args.clone()).await;
        assert!(is_error(&out), "{args}: {out:?}");
        assert!(text(out).contains(needle), "{args}");
    }
}

#[tokio::test]
async fn read_file_cuts_a_big_file_and_says_so() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let big = "0123456789".repeat(40 * 1024); // 400 KiB
    std::fs::write(rig.worktree().join("big.txt"), &big).unwrap();
    let out = ReadFile
        .call(&rig.ctx, json!({"path": "big.txt"}))
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(
        out.content.len() < 256 * 1024 + 300,
        "{}",
        out.content.len()
    );
    assert!(
        out.content.contains("[cut: `big.txt` is 409600 bytes"),
        "{}",
        &out.content[out.content.len() - 200..]
    );
}

/// Whatever the model passes, a path that leaves the worktree reads and writes nothing, and says
/// why.
#[tokio::test]
async fn the_file_tools_never_leave_the_worktree() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let wt = rig.worktree();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "s3cr3t").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), wt.join("leak")).unwrap();
    std::os::unix::fs::symlink(outside.path(), wt.join("outdir")).unwrap();
    std::os::unix::fs::symlink("README.md", wt.join("alias")).unwrap();

    let refused = [
        ("../escape.txt", "`..`"),
        ("sub/../../escape.txt", "`..`"),
        ("/tmp/escape.txt", "absolute"),
        (".git/config", ".git"),
        (".GIT/config", ".git"),
        ("outdir/new.txt", "symlink"),
        ("leak", "symlink"),
        ("alias", "symlink"),
    ];
    for (path, needle) in refused {
        let out = WriteFile
            .call(&rig.ctx, json!({"path": path, "content": "pwned"}))
            .await;
        assert!(is_error(&out), "write {path}: {out:?}");
        assert!(text(out).contains(needle), "write {path}");
    }
    for (path, needle) in [
        ("../secret.txt", "`..`"),
        (".git", ".git"),
        ("leak", "outside the worktree"),
        ("outdir/secret.txt", "outside the worktree"),
    ] {
        let out = ReadFile.call(&rig.ctx, json!({"path": path})).await;
        assert!(is_error(&out), "read {path}: {out:?}");
        let message = text(out);
        assert!(message.contains(needle), "read {path}: {message}");
        assert!(!message.contains("s3cr3t"), "read {path}: {message}");
    }
    assert_eq!(
        std::fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
        "s3cr3t"
    );
    assert!(!outside.path().join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(wt.join("README.md")).unwrap(),
        "widgets\n"
    );
    // A link that stays inside is read, through to its target.
    let out = ReadFile
        .call(&rig.ctx, json!({"path": "alias"}))
        .await
        .unwrap();
    assert_eq!(out.content, "widgets\n");
}

/// A file written by the tool is a change of the worktree like any other: the checks see it, and
/// a commit made after a later edit is not bound to them.
#[tokio::test]
async fn write_file_changes_what_the_checks_see_and_the_gate_binds() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let wt = rig.worktree();

    let made = WriteFile
        .call(
            &rig.ctx,
            json!({"path": "docs/notes/hello.txt", "content": "hello\n"}),
        )
        .await
        .unwrap();
    assert!(!made.is_error, "{}", made.content);
    assert!(
        made.content.starts_with("Created docs/notes/hello.txt"),
        "{}",
        made.content
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("docs/notes/hello.txt")).unwrap(),
        "hello\n"
    );
    assert!(
        rig.progress()
            .contains(&"wrote docs/notes/hello.txt (remote)".to_owned())
    );

    let checked = RunChecks
        .call(
            &rig.ctx,
            json!({"command": "grep -qx hello docs/notes/hello.txt"}),
        )
        .await
        .unwrap();
    assert_eq!(checks_of(&checked)["passed"], true);
    // The same path again replaces it.
    let again = WriteFile
        .call(
            &rig.ctx,
            json!({"path": "docs/notes/hello.txt", "content": "hello, again\n"}),
        )
        .await
        .unwrap();
    assert!(
        again.content.starts_with("Replaced docs/notes/hello.txt"),
        "{}",
        again.content
    );

    let out = CommitAndPush
        .call(&rig.ctx, json!({"message": "docs: add a note"}))
        .await
        .unwrap();
    let (bound, _) = commit_artifacts(&out);
    assert_eq!(
        bound["passed"], false,
        "edited after the checks, so not bound to them: {bound}"
    );
    // Checked again, the commit is bound.
    let rechecked = RunChecks
        .call(
            &rig.ctx,
            json!({"command": "grep -qx 'hello, again' docs/notes/hello.txt"}),
        )
        .await
        .unwrap();
    assert_eq!(checks_of(&rechecked)["passed"], true);
    let out = CommitAndPush
        .call(&rig.ctx, json!({"message": "docs: add a note"}))
        .await
        .unwrap();
    let (bound, _) = commit_artifacts(&out);
    assert_eq!(bound["passed"], true, "{bound}");

    let too_big = WriteFile
        .call(
            &rig.ctx,
            json!({"path": "big.txt", "content": "x".repeat(1024 * 1024 + 1)}),
        )
        .await;
    assert!(is_error(&too_big), "{too_big:?}");
    assert!(!wt.join("big.txt").exists());
}

#[tokio::test]
async fn apply_patch_changes_a_file_and_the_checks_see_it() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let wt = rig.worktree();
    let before = worktree_state(&rig);

    let patch = format!(
        "{}--- /dev/null\n+++ b/NOTES.md\n@@ -0,0 +1,2 @@\n+# Notes\n+second\n",
        one_line_patch("README.md", "widgets", "widgets, patched")
    );
    let out = ApplyPatch
        .call(&rig.ctx, json!({"patch": patch}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("2 file(s): README.md, NOTES.md"),
        "{}",
        out.content
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("README.md")).unwrap(),
        "widgets, patched\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("NOTES.md")).unwrap(),
        "# Notes\nsecond\n"
    );
    assert!(
        rig.progress()
            .contains(&"patched README.md, NOTES.md (remote)".to_owned()),
        "{:?}",
        rig.progress()
    );
    assert_ne!(worktree_state(&rig), before, "the tree changed");

    // `run_checks` sees it, and the commit is bound to those checks.
    let checked = RunChecks
        .call(&rig.ctx, json!({"command": "grep -q patched README.md"}))
        .await
        .unwrap();
    assert_eq!(checks_of(&checked)["passed"], true);
    let out = CommitAndPush
        .call(
            &rig.ctx,
            json!({"message": "fix: say what the widgets are"}),
        )
        .await
        .unwrap();
    let (bound, branch) = commit_artifacts(&out);
    assert_eq!(bound["passed"], true, "{bound}");
    assert_eq!(bound["commit"], branch["commit"]);
    // The patch changed the index of nothing: git sees one commit with both files.
    let pushed = branch["commit"].as_str().unwrap();
    assert_eq!(
        common::git(&wt, &["show", "--stat", "--format=", pushed])
            .lines()
            .count(),
        3,
        "two files and the summary line"
    );
}

#[tokio::test]
async fn a_patch_whose_hunk_does_not_match_changes_nothing() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let wt = rig.worktree();
    // The first file would apply; the second hunk does not match: all or nothing.
    let patch = format!(
        "{}{}",
        one_line_patch("README.md", "widgets", "changed"),
        "--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-not what the file says\n+x\n"
    );
    let out = ApplyPatch.call(&rig.ctx, json!({"patch": patch})).await;
    assert!(is_error(&out), "{out:?}");
    let message = text(out);
    assert!(
        message.contains("does not apply") && message.contains("nothing was changed"),
        "{message}"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("README.md")).unwrap(),
        "widgets\n"
    );
}

#[tokio::test]
async fn a_malformed_or_empty_patch_is_the_models_to_fix() {
    let rig = Rig::new().await;
    rig.prepare().await;
    for (patch, needle) in [
        ("", "patch is required"),
        ("   \n", "patch is required"),
        ("this is not a diff at all\n", "unified diff"),
        (
            "--- a/README.md\n+++ b/README.md\n@@ garbage @@\n",
            "unified diff",
        ),
    ] {
        let out = ApplyPatch.call(&rig.ctx, json!({"patch": patch})).await;
        assert!(is_error(&out), "{patch:?}: {out:?}");
        let message = text(out);
        assert!(message.contains(needle), "{patch:?}: {message}");
    }
    let huge = format!(
        "--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-widgets\n+{}\n",
        "x".repeat(1024 * 1024)
    );
    let out = ApplyPatch.call(&rig.ctx, json!({"patch": huge})).await;
    assert!(is_error(&out), "{out:?}");
    assert!(text(out).contains("over the limit"));
    let nul = "--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-widgets\n+x\0y\n";
    let out = ApplyPatch.call(&rig.ctx, json!({"patch": nul})).await;
    assert!(is_error(&out), "{out:?}");
    assert!(text(out).contains("NUL"));
}

/// A patch is checked by the paths git reads from it, whatever the model wrote: nothing in `.git`,
/// nothing above the worktree, no symlink created, none written through.
#[tokio::test]
async fn a_patch_cannot_reach_git_the_outside_or_a_symlink() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let wt = rig.worktree();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), wt.join("outdir")).unwrap();
    std::os::unix::fs::symlink("README.md", wt.join("alias")).unwrap();
    let before = worktree_state(&rig);
    let config_before = std::fs::read_to_string(
        common::git(&wt, &["rev-parse", "--git-common-dir"])
            .parse::<std::path::PathBuf>()
            .map(|p| wt.join(p).join("config"))
            .unwrap(),
    )
    .unwrap();

    let new_file = |path: &str| format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+pwned\n");
    let cases: Vec<(String, &str)> = vec![
        (new_file(".git/config"), ".git"),
        (new_file(".GIT/hooks/pre-commit"), ".git"),
        (new_file("../escape.txt"), "x"),
        (new_file("outdir/new.txt"), "symlink"),
        (one_line_patch("alias", "widgets", "pwned"), "symlink"),
        // A symlink created by the patch itself.
        (
            "diff --git a/link b/link\nnew file mode 120000\n--- /dev/null\n+++ b/link\n@@ -0,0 +1 @@\n+/etc/passwd\n\\ No newline at end of file\n".to_owned(),
            "symlink or a submodule",
        ),
        // A rename into `.git`.
        (
            "diff --git a/README.md b/.git/moved\nsimilarity index 100%\nrename from README.md\nrename to .git/moved\n".to_owned(),
            ".git",
        ),
        // A binary patch.
        (
            "diff --git a/img.png b/img.png\nnew file mode 100644\nindex 0000000..e69de29\nGIT binary patch\nliteral 0\nHcmV?d00001\n\n".to_owned(),
            "binary",
        ),
    ];
    for (patch, needle) in cases {
        let out = ApplyPatch.call(&rig.ctx, json!({"patch": patch})).await;
        let shown = patch.replace('\n', "\\n");
        assert!(is_error(&out), "{shown}: {out:?}");
        let message = text(out);
        if needle != "x" {
            assert!(message.contains(needle), "{shown}: {message}");
        }
    }
    assert!(!wt.join("link").exists(), "no symlink was created");
    assert!(!wt.parent().unwrap().join("escape.txt").exists());
    assert!(!outside.path().join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(wt.join("README.md")).unwrap(),
        "widgets\n"
    );
    let config_after = std::fs::read_to_string(
        wt.join(common::git(&wt, &["rev-parse", "--git-common-dir"]))
            .join("config"),
    )
    .unwrap();
    assert_eq!(
        config_after, config_before,
        "the repository's config is untouched"
    );
    assert_eq!(
        worktree_state(&rig),
        before,
        "nothing in the worktree changed"
    );
}

/// `run_command` is still for looking: a write it makes is undone, whatever the new tools allow.
#[tokio::test]
async fn run_command_stays_read_only_beside_the_file_tools() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let out = RunCommand
        .call(&rig.ctx, json!({"command": "echo x > made-by-command.txt"}))
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert!(!rig.worktree().join("made-by-command.txt").exists());
}

/// A model counts the lines of a hunk badly. A patch that only applies once git goes by the lines
/// themselves is applied; one that is wrong in its lines is not.
#[tokio::test]
async fn a_hunk_header_with_wrong_counts_is_applied_by_its_lines() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let wt = rig.worktree();
    let miscounted =
        "--- a/README.md\n+++ b/README.md\n@@ -1,7 +1,9 @@\n-widgets\n+widgets, counted wrong\n";
    let out = ApplyPatch
        .call(&rig.ctx, json!({"patch": miscounted}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(wt.join("README.md")).unwrap(),
        "widgets, counted wrong\n"
    );
}

// ------------------------------------------------------------------ a workspace of two repositories

/// A second repository in the run's workspace: its slot, named after it.
async fn add_second(rig: &Rig, files: &[(&str, &str)]) -> (std::path::PathBuf, std::path::PathBuf) {
    let remote = rig.fx.extra_remote("lib", files);
    let url = remote.to_string_lossy().into_owned();
    rig.say(&url).await;
    let out = PrepareWorkspace
        .call(&rig.ctx, json!({"repo_url": url, "base_branch": "main"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("slot: lib\n"),
        "the result says the slot: {}",
        out.content
    );
    let wt = common::slot_dir(&rig.fx.root, &rig.ctx.run_id().to_string())
        .parent()
        .unwrap()
        .join("lib");
    (remote, wt)
}

#[tokio::test]
async fn a_second_repository_joins_the_workspace_and_repo_says_which_one() {
    let rig = Rig::new().await;
    let first = rig.prepare().await;
    assert!(
        first.content.contains("slot: remote\n"),
        "{}",
        first.content
    );
    let (remote_b, wt_b) = add_second(&rig, &[("lib.txt", "lib\n")]).await;
    let wt_a = rig.worktree();
    assert!(wt_a.join("README.md").is_file() && wt_b.join("lib.txt").is_file());
    assert_ne!(wt_a, wt_b);

    // Without `repo` the model must say which: the error lists the slots.
    for out in [
        RunCommand.call(&rig.ctx, json!({"command": "ls"})).await,
        ReadFile.call(&rig.ctx, json!({"path": "README.md"})).await,
        RunChecks.call(&rig.ctx, json!({"command": "true"})).await,
        CommitAndPush.call(&rig.ctx, json!({"message": "m"})).await,
    ] {
        assert!(is_error(&out), "{out:?}");
        let message = text(out);
        assert!(
            message.contains("2 slots")
                && message.contains("`lib`")
                && message.contains("`remote`"),
            "{message}"
        );
    }
    // By the slot's name, and by the address of the repository.
    let by_name = RunCommand
        .call(&rig.ctx, json!({"command": "cat lib.txt", "repo": "lib"}))
        .await
        .unwrap();
    assert!(by_name.content.contains("lib\n"), "{}", by_name.content);
    let by_address = ReadFile
        .call(
            &rig.ctx,
            json!({"path": "lib.txt", "repo": remote_b.to_string_lossy()}),
        )
        .await
        .unwrap();
    assert_eq!(by_address.content, "lib\n");
    let in_first = ReadFile
        .call(&rig.ctx, json!({"path": "README.md", "repo": "remote"}))
        .await
        .unwrap();
    assert_eq!(in_first.content, "widgets\n");
    // A file of one slot is not in the other.
    let wrong = ReadFile
        .call(&rig.ctx, json!({"path": "lib.txt", "repo": "remote"}))
        .await;
    assert!(is_error(&wrong), "{wrong:?}");
    // A slot that is not there: the error says which are.
    let unknown = RunCommand
        .call(&rig.ctx, json!({"command": "ls", "repo": "nope"}))
        .await;
    assert!(is_error(&unknown), "{unknown:?}");
    assert!(text(unknown).contains("no slot of this workspace is `nope`"));

    // Writes and patches go to the slot that was named.
    WriteFile
        .call(
            &rig.ctx,
            json!({"path": "new.txt", "content": "n\n", "repo": "lib"}),
        )
        .await
        .unwrap();
    assert!(wt_b.join("new.txt").is_file() && !wt_a.join("new.txt").exists());
    let patched = ApplyPatch
        .call(
            &rig.ctx,
            json!({"patch": one_line_patch("README.md", "widgets", "widgets!"), "repo": "remote"}),
        )
        .await
        .unwrap();
    assert!(!patched.is_error, "{}", patched.content);
    assert_eq!(
        std::fs::read_to_string(wt_a.join("README.md")).unwrap(),
        "widgets!\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt_b.join("lib.txt")).unwrap(),
        "lib\n"
    );
    // The progress line names the slot.
    assert!(
        rig.progress().contains(&"wrote new.txt (lib)".to_owned()),
        "{:?}",
        rig.progress()
    );
    // Asking for the repository again is the same slot, and keeps what is in it.
    let again = PrepareWorkspace
        .call(
            &rig.ctx,
            json!({"repo_url": remote_b.to_string_lossy(), "base_branch": "main"}),
        )
        .await
        .unwrap();
    assert!(again.content.contains("slot: lib\n"), "{}", again.content);
    assert!(wt_b.join("new.txt").is_file(), "work in progress survives");
}

/// `run_checks` and `commit_and_push` per slot: each pushes to its own remote, the `checks`
/// artifact names the repository it ran in, and a check in one slot does not bind a commit in the
/// other unless the code is the same.
#[tokio::test]
async fn checks_and_pushes_are_per_slot_and_the_gate_binds_by_the_tree() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let (remote_b, _) = add_second(&rig, &[("README.md", "widgets\n")]).await;
    let (url_a, url_b) = (rig.fx.remote_url(), remote_b.to_string_lossy().into_owned());

    // The same new file in both: the trees are the same code.
    for repo in ["remote", "lib"] {
        WriteFile
            .call(
                &rig.ctx,
                json!({"path": "same.txt", "content": "same\n", "repo": repo}),
            )
            .await
            .unwrap();
    }
    let checked = RunChecks
        .call(
            &rig.ctx,
            json!({"command": "test -f same.txt", "repo": "remote"}),
        )
        .await
        .unwrap();
    let report = checks_of(&checked);
    assert_eq!(report["passed"], true);
    assert_eq!(
        report["repository"],
        url_a.as_str(),
        "the check names the repository it ran in"
    );

    // The commit in the *other* slot has the same tree, so the check in the first binds it.
    let out = CommitAndPush
        .call(&rig.ctx, json!({"message": "feat: same", "repo": "lib"}))
        .await
        .unwrap();
    let (bound, branch) = commit_artifacts(&out);
    assert_eq!(bound["passed"], true, "the same code was checked: {bound}");
    assert_eq!(
        bound["repository"],
        url_b.as_str(),
        "the verdict names the repository pushed to"
    );
    assert_eq!(branch["repository"], url_b.as_str());
    assert_eq!(
        bound["tree"], report["tree"],
        "a tree id is a content address"
    );
    // It went to its own remote only.
    let branches_b = common::git(
        &remote_b,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/agent",
        ],
    );
    assert_eq!(branches_b.lines().count(), 1, "{branches_b}");
    assert!(
        rig.fx.agent_branches().is_empty(),
        "nothing went to the first remote yet"
    );

    // A different file in the first slot: the check that ran on the other code does not bind it.
    WriteFile
        .call(
            &rig.ctx,
            json!({"path": "other.txt", "content": "other\n", "repo": "remote"}),
        )
        .await
        .unwrap();
    let out = CommitAndPush
        .call(
            &rig.ctx,
            json!({"message": "feat: other", "repo": "remote"}),
        )
        .await
        .unwrap();
    let (unbound, _) = commit_artifacts(&out);
    assert_eq!(unbound["passed"], false, "{unbound}");
    assert_eq!(unbound["repository"], url_a.as_str());
    assert_eq!(rig.fx.agent_branches().len(), 1);
    assert!(
        text_of_findings(&unbound).contains("was not checked"),
        "{unbound}"
    );
    // Checked there, it binds, and the pull request of that slot is allowed.
    RunChecks
        .call(
            &rig.ctx,
            json!({"command": "test -f other.txt", "repo": "remote"}),
        )
        .await
        .unwrap();
    let out = CommitAndPush
        .call(
            &rig.ctx,
            json!({"message": "feat: other", "repo": "remote"}),
        )
        .await
        .unwrap();
    let (rebound, _) = commit_artifacts(&out);
    assert_eq!(rebound["passed"], true, "{rebound}");
    let pr = OpenPullRequest
        .call(
            &rig.ctx,
            json!({"title": "feat: other", "body": "x\n\n## Verification\n- test -f other.txt", "repo": "remote"}),
        )
        .await
        .unwrap();
    assert!(!pr.is_error, "{}", pr.content);
    assert_eq!(pr.artifacts[0].data["repository"], url_a.as_str());
}

fn text_of_findings(report: &Value) -> String {
    report["findings"]
        .as_array()
        .map(|f| {
            f.iter()
                .map(|f| f["message"].as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// The gate for a pull request: green only if the most recent check on **this code**, in any slot,
/// passed. A green and then a red on the same code is red.
#[tokio::test]
async fn a_pull_request_needs_the_most_recent_check_of_its_code_whichever_slot_ran_it() {
    let rig = Rig::new().await;
    rig.prepare().await;
    add_second(&rig, &[("README.md", "widgets\n")]).await;
    for repo in ["remote", "lib"] {
        WriteFile
            .call(
                &rig.ctx,
                json!({"path": "same.txt", "content": "same\n", "repo": repo}),
            )
            .await
            .unwrap();
    }
    // Green in the second slot, then the commit and the pull request in the first: the same code.
    RunChecks
        .call(
            &rig.ctx,
            json!({"command": "test -f same.txt", "repo": "lib"}),
        )
        .await
        .unwrap();
    CommitAndPush
        .call(&rig.ctx, json!({"message": "feat: same", "repo": "remote"}))
        .await
        .unwrap();
    let pr = OpenPullRequest
        .call(
            &rig.ctx,
            json!({"title": "feat: same", "body": "x\n\n## Verification\n- test -f same.txt", "repo": "remote"}),
        )
        .await
        .unwrap();
    assert!(
        !pr.is_error,
        "green in the other slot, on the same tree: {}",
        pr.content
    );
    // Red on the same code afterwards: the most recent check on it wins.
    RunChecks
        .call(&rig.ctx, json!({"command": "false", "repo": "remote"}))
        .await
        .unwrap();
    let again = OpenPullRequest
        .call(
            &rig.ctx,
            json!({"title": "feat: same", "body": "x\n\n## Verification\n- test -f same.txt", "repo": "remote"}),
        )
        .await;
    assert!(
        is_error(&again),
        "red after green on the same tree: {again:?}"
    );
    assert!(text(again).contains("the last check run (`false`) failed"));
}

/// A run that began before workspaces had slots has one worktree in the old layout. It is a slot
/// like any other: the tools find it, asking for its repository again returns it, and a second
/// repository joins it.
#[tokio::test]
async fn a_run_that_began_in_the_old_layout_keeps_working() {
    let rig = Rig::new().await;
    let run = rig.ctx.run_id().to_string();
    let repo = adam_workspace::RepoRef::new(rig.fx.remote_url(), "main");
    let legacy = rig.fx.env.workspaces.prepare(&repo, &run).await.unwrap();
    assert_eq!(legacy.path(), rig.fx.root.join("worktrees").join(&run));
    std::fs::write(legacy.path().join("wip.txt"), "wip\n").unwrap();

    // The tools find it without `repo`, and `prepare_workspace` of the same repository returns it.
    let listed = RunCommand
        .call(&rig.ctx, json!({"command": "ls"}))
        .await
        .unwrap();
    assert!(listed.content.contains("wip.txt"), "{}", listed.content);
    let again = rig.prepare().await;
    assert!(
        again.content.contains("slot: remote\n"),
        "{}",
        again.content
    );
    assert!(
        again
            .content
            .contains(&format!("path: {}", legacy.path().display())),
        "{}",
        again.content
    );
    assert!(
        legacy.path().join("wip.txt").is_file(),
        "work in progress survives"
    );
    // A second repository joins it, in the new layout, and `repo` is needed from then on.
    let (_, wt_b) = add_second(&rig, &[("lib.txt", "lib\n")]).await;
    assert_eq!(
        wt_b.parent().unwrap(),
        rig.fx.root.join("workspaces").join(&run)
    );
    let out = RunCommand.call(&rig.ctx, json!({"command": "ls"})).await;
    assert!(is_error(&out), "{out:?}");
    let in_old = ReadFile
        .call(&rig.ctx, json!({"path": "wip.txt", "repo": "remote"}))
        .await
        .unwrap();
    assert_eq!(in_old.content, "wip\n");
}

// ------------------------------------------------------------------ scratch projects

/// The empty tree: what the first commit of a repository that was just created holds.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The directory of the slot `dir` of the rig's run.
fn slot_of(rig: &Rig, dir: &str) -> std::path::PathBuf {
    common::slot_dir(&rig.fx.root, &rig.ctx.run_id().to_string())
        .parent()
        .unwrap()
        .join(dir)
}

async fn start_scratch(rig: &Rig, name: &str) -> ToolOutput {
    let out = StartScratch
        .call(&rig.ctx, json!({"name": name}))
        .await
        .expect("start_scratch");
    assert!(!out.is_error, "{}", out.content);
    out
}

/// A scratch project that holds a script and the check that proves it: what the model builds
/// before any repository is named.
async fn build_fib(rig: &Rig, repo: Option<&str>) {
    let at = |extra: Value| {
        let mut args = extra;
        if let Some(repo) = repo {
            args["repo"] = json!(repo);
        }
        args
    };
    for (path, content) in [
        ("fib.sh", "echo 0 1 1 2 3 5 8\n"),
        ("check.sh", "test \"$(sh fib.sh)\" = \"0 1 1 2 3 5 8\"\n"),
    ] {
        let out = WriteFile
            .call(&rig.ctx, at(json!({"path": path, "content": content})))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
    }
}

#[tokio::test]
async fn a_scratch_project_is_built_checked_and_committed_locally() {
    let rig = Rig::new().await;
    let started = start_scratch(&rig, "fib").await;
    assert!(
        started.content.contains("slot: fib\n"),
        "{}",
        started.content
    );
    assert!(
        started.content.contains("temporary") && started.content.contains("publish_scratch"),
        "the model is told it is temporary: {}",
        started.content
    );
    let dir = slot_of(&rig, "fib");
    assert!(dir.join(".git").is_dir(), "a repository of its own");
    // Asking again is the same project.
    assert_eq!(start_scratch(&rig, "fib").await.content, started.content);

    // The file tools, the checks and the looking around work in it (one slot: `repo` may be left out).
    build_fib(&rig, None).await;
    let read = ReadFile
        .call(&rig.ctx, json!({"path": "fib.sh"}))
        .await
        .unwrap();
    assert_eq!(read.content, "echo 0 1 1 2 3 5 8\n");
    WriteFile
        .call(&rig.ctx, json!({"path": "notes.txt", "content": "draft\n"}))
        .await
        .unwrap();
    let patched = ApplyPatch
        .call(
            &rig.ctx,
            json!({"patch": one_line_patch("notes.txt", "draft", "final")}),
        )
        .await
        .unwrap();
    assert!(!patched.is_error, "{}", patched.content);
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "final\n"
    );
    let listing = RunCommand
        .call(&rig.ctx, json!({"command": "ls"}))
        .await
        .unwrap();
    assert!(
        listing.content.contains("fib.sh") && listing.content.contains("check.sh"),
        "{}",
        listing.content
    );
    // What looks around may not change the project: the change is undone, as in a worktree.
    let changed = RunCommand
        .call(&rig.ctx, json!({"command": "touch stray.txt"}))
        .await
        .unwrap();
    assert!(changed.is_error, "{}", changed.content);
    assert!(!dir.join("stray.txt").exists(), "the change was undone");
    let moved = RunCommand
        .call(
            &rig.ctx,
            json!({"command": "git -c user.name=x -c user.email=x@y commit --allow-empty -m sneaky"}),
        )
        .await
        .unwrap();
    assert!(moved.is_error, "HEAD may not move: {}", moved.content);
    assert_eq!(
        common::git(&dir, &["rev-list", "--count", "HEAD"]),
        "1",
        "the sneaky commit was undone"
    );

    let checked = RunChecks
        .call(&rig.ctx, json!({"command": "sh ./check.sh"}))
        .await
        .unwrap();
    assert!(!checked.is_error, "{}", checked.content);
    let report = checks_of(&checked);
    assert_eq!(report["passed"], true, "{report}");
    assert_eq!(report["commit"], head_of(&dir).as_str());
    assert!(
        report["tree"].as_str().is_some_and(|t| t.len() == 40),
        "the project's tree is what the gate binds: {report}"
    );
    assert!(
        report.get("repository").is_none(),
        "a scratch project has no repository: {report}"
    );

    // OpenCode works in it too, and the changed files are the project's.
    let delegated = DelegateToOpenCode
        .call(&rig.ctx, json!({"instructions": "add hello.txt"}))
        .await
        .unwrap();
    assert!(!delegated.is_error, "{}", delegated.content);
    assert!(
        delegated.content.contains("hello.txt"),
        "the files OpenCode changed in the project: {}",
        delegated.content
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("hello.txt")).unwrap(),
        "hello\n"
    );

    // `commit_and_push` there is a commit and nothing else: no push, no artifact, no `branch`.
    let committed = CommitAndPush
        .call(&rig.ctx, json!({"message": "feat: fib"}))
        .await
        .unwrap();
    assert!(!committed.is_error, "{}", committed.content);
    assert!(
        committed.content.contains("locally") && committed.content.contains("publish_scratch"),
        "{}",
        committed.content
    );
    assert!(committed.artifacts.is_empty(), "{:?}", committed.artifacts);
    assert_eq!(common::git(&dir, &["rev-list", "--count", "HEAD"]), "2");
    assert!(rig.fx.agent_branches().is_empty(), "nothing was pushed");
    let again = CommitAndPush
        .call(&rig.ctx, json!({"message": "feat: fib"}))
        .await
        .unwrap();
    assert!(
        again.content.contains("Nothing new to commit") && again.artifacts.is_empty(),
        "{}",
        again.content
    );
    assert_eq!(common::git(&dir, &["rev-list", "--count", "HEAD"]), "2");

    // No pull request comes from a project that has no remote, and the way out is named.
    let pr = OpenPullRequest
        .call(&rig.ctx, json!({"title": "feat: fib", "body": "b"}))
        .await;
    assert!(is_error(&pr), "{pr:?}");
    assert!(text(pr).contains("publish_scratch"));
    assert!(rig.fx.created_pulls().await.is_empty());
}

#[tokio::test]
async fn a_scratch_project_needs_a_plain_name_that_no_repository_has() {
    let rig = Rig::new().await;
    for bad in ["Fib", "a b", "x.git", "../x", ".hidden", "a/b"] {
        let out = StartScratch.call(&rig.ctx, json!({"name": bad})).await;
        assert!(is_error(&out), "{bad:?}: {out:?}");
        assert!(
            text(out).contains("Give the project another name"),
            "{bad:?}"
        );
    }
    assert!(
        !rig.fx.root.join("workspaces").exists(),
        "a refused name made nothing"
    );
    // A blank name is no name: the default.
    let blank = StartScratch
        .call(&rig.ctx, json!({"name": "  "}))
        .await
        .unwrap();
    assert!(
        blank.content.contains("slot: scratch\n"),
        "{}",
        blank.content
    );
    // A repository of the workspace that has the name.
    rig.prepare().await;
    let taken = StartScratch.call(&rig.ctx, json!({"name": "remote"})).await;
    assert!(is_error(&taken), "{taken:?}");
    assert!(text(taken).contains("already called remote"));
    // Two slots now: the tools ask which, and the list says what each is.
    let out = ReadFile.call(&rig.ctx, json!({"path": "x"})).await;
    let message = text(out);
    assert!(
        message.contains("a scratch project") && message.contains("`remote`"),
        "{message}"
    );
}

/// The person names the repository a scratch project goes to, or the tool refuses before it asks
/// a remote anything.
#[tokio::test]
async fn publish_scratch_works_only_on_a_repository_the_person_named() {
    let rig = Rig::new().await;
    start_scratch(&rig, "fib").await;
    build_fib(&rig, None).await;
    let empty = rig.fx.empty_remote("fibonacci");
    let url = empty.to_string_lossy().into_owned();

    let out = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await;
    assert!(is_error(&out), "{out:?}");
    let message = text(out);
    assert!(
        message.contains("not a repository the person named")
            && message.contains("ask_user")
            && message.contains("publish_scratch"),
        "{message}"
    );
    assert!(!message.contains("prepare_workspace"), "{message}");
    assert!(
        common::git(&empty, &["for-each-ref"]).is_empty(),
        "the repository was not touched"
    );
    assert!(
        !rig.fx.root.join("git").exists(),
        "no mirror was made for a repository nobody named"
    );
    // Not a repository at all: the workspace says so, with nothing made either.
    let nonsense = PublishScratch
        .call(&rig.ctx, json!({"repo_url": "not a url"}))
        .await;
    assert!(is_error(&nonsense), "{nonsense:?}");
    // Once the person has named it (what the agent records before each step), it goes through.
    rig.say(&url).await;
    let out = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
}

/// The whole path of the issue: a project is built and checked before any repository is named,
/// then published to a repository that was just created (empty), and what was checked is what is
/// pushed: the verdict bound to the pushed commit is the one the project earned.
#[tokio::test]
async fn a_scratch_project_published_to_an_empty_repository_keeps_the_checks_it_passed() {
    let rig = Rig::new().await;
    start_scratch(&rig, "fib").await;
    build_fib(&rig, None).await;
    let checked = RunChecks
        .call(&rig.ctx, json!({"command": "sh ./check.sh"}))
        .await
        .unwrap();
    let scratch_report = checks_of(&checked);
    assert_eq!(scratch_report["passed"], true);

    let empty = rig.fx.empty_remote("fibonacci");
    let url = empty.to_string_lossy().into_owned();
    rig.say(&format!("Publish it to {url}")).await;
    assert!(common::git(&empty, &["for-each-ref"]).is_empty());

    let out = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    let said = &out.content;
    for needle in [
        "Published the scratch project `fib`",
        "slot: fibonacci\n",
        "base branch: main\n",
        "The repository was empty: it now has an empty first commit on main",
        "copied (2): check.sh, fib.sh",
        "The checks passed on exactly this code (`sh ./check.sh`, run in `fib`): commit_and_push now.",
        "`repo: fibonacci`",
    ] {
        assert!(said.contains(needle), "lost {needle:?}: {said}");
    }
    assert!(
        rig.progress()
            .iter()
            .any(|p| p.starts_with("giving ") && p.contains("its first commit on main")),
        "{:?}",
        rig.progress()
    );
    // The only push outside `agent/*`: an empty root commit, never anything of the project.
    assert_eq!(common::git(&empty, &["rev-list", "--count", "main"]), "1");
    assert_eq!(
        common::git(&empty, &["rev-parse", "main^{tree}"]),
        EMPTY_TREE
    );
    let repo_dir = slot_of(&rig, "fibonacci");
    assert_eq!(
        std::fs::read_to_string(repo_dir.join("fib.sh")).unwrap(),
        "echo 0 1 1 2 3 5 8\n"
    );
    // Two slots now: the tools must be told which.
    let lost = CommitAndPush.call(&rig.ctx, json!({"message": "m"})).await;
    assert!(is_error(&lost), "{lost:?}");
    assert!(text(lost).contains("2 slots"));

    // The same code: the check that passed on the project binds the commit in the repository.
    let pushed = CommitAndPush
        .call(
            &rig.ctx,
            json!({"message": "feat: fib", "repo": "fibonacci"}),
        )
        .await
        .unwrap();
    assert!(!pushed.is_error, "{}", pushed.content);
    let (bound, branch) = commit_artifacts(&pushed);
    assert_eq!(bound["passed"], true, "{bound}");
    assert_eq!(bound["tree"], scratch_report["tree"], "{bound}");
    assert_eq!(bound["repository"], url.as_str());
    assert_eq!(branch["repository"], url.as_str());
    assert_eq!(branch["base_branch"], "main");
    let branches: Vec<String> = common::git(
        &empty,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/agent",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect();
    assert_eq!(branches.len(), 1, "{branches:?}");
    assert_eq!(
        bound["commit"],
        common::git(&empty, &["rev-parse", &branches[0]]).as_str()
    );
    // One commit on top of the empty first commit, which holds the project.
    assert_eq!(
        common::git(
            &empty,
            &["rev-list", "--count", &format!("main..{}", branches[0])]
        ),
        "1"
    );
    assert_eq!(
        common::git(&empty, &["show", &format!("{}:fib.sh", branches[0])]),
        "echo 0 1 1 2 3 5 8"
    );

    let pr = OpenPullRequest
        .call(
            &rig.ctx,
            json!({"title": "feat: fib", "body": "Adds fib.sh.\n\n## Verification\n- `sh ./check.sh`: passed", "repo": "fibonacci"}),
        )
        .await
        .unwrap();
    assert!(!pr.is_error, "{}", pr.content);
    assert_eq!(pr.artifacts[0].data["repository"], url.as_str());
    let pulls = rig.fx.created_pulls().await;
    assert_eq!(pulls.len(), 1);
    assert_eq!(pulls[0]["base"], "main", "against the empty first commit");

    // The project was left as it was, and says where it went: what changes in it is not the pull
    // request's, and the tools that change it say so.
    let wrote = WriteFile
        .call(
            &rig.ctx,
            json!({"path": "late.txt", "content": "x\n", "repo": "fib"}),
        )
        .await
        .unwrap();
    assert!(
        wrote.content.contains("was published to") && wrote.content.contains(&url),
        "{}",
        wrote.content
    );
    let committed = CommitAndPush
        .call(&rig.ctx, json!({"message": "wip", "repo": "fib"}))
        .await
        .unwrap();
    assert!(
        committed.content.contains("was published to") && committed.artifacts.is_empty(),
        "{}",
        committed.content
    );
    let started = StartScratch
        .call(&rig.ctx, json!({"name": "fib"}))
        .await
        .unwrap();
    assert!(
        started.content.contains("was published to"),
        "{}",
        started.content
    );
}

/// A repeated call (a crash before the result was journaled) finds the repository's slot, copies
/// nothing new and pushes nothing.
#[tokio::test]
async fn publishing_twice_is_the_same_publication() {
    let rig = Rig::new().await;
    start_scratch(&rig, "fib").await;
    build_fib(&rig, None).await;
    let empty = rig.fx.empty_remote("fibonacci");
    let url = empty.to_string_lossy().into_owned();
    rig.say(&url).await;
    let first = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await
        .unwrap();
    assert!(!first.is_error, "{}", first.content);
    let tip = common::git(&empty, &["rev-parse", "main"]);

    let second = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await
        .unwrap();
    assert!(!second.is_error, "{}", second.content);
    assert!(
        second.content.contains("unchanged (2): check.sh, fib.sh")
            && second.content.contains("copied (0)")
            && !second.content.contains("The repository was empty"),
        "{}",
        second.content
    );
    assert_eq!(
        common::git(&empty, &["rev-parse", "main"]),
        tip,
        "nothing was pushed"
    );
    assert_eq!(common::git(&empty, &["rev-list", "--count", "main"]), "1");
}

/// A crash between the first commit and the slot: the remote is not empty any more, but its only
/// commit holds nothing, so it is not "a repository that already has files".
#[tokio::test]
async fn a_publication_that_died_after_the_first_commit_goes_on() {
    let rig = Rig::new().await;
    start_scratch(&rig, "fib").await;
    build_fib(&rig, None).await;
    let empty = rig.fx.empty_remote("fibonacci");
    let url = empty.to_string_lossy().into_owned();
    rig.say(&url).await;
    // What the first try did before it died.
    rig.fx
        .env
        .workspaces
        .initialize_empty(
            &adam_workspace::RepoRef::new(&url, "main"),
            &rig.fx.env.settings.identity,
        )
        .await
        .unwrap();

    let out = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("copied (2)"), "{}", out.content);
    assert_eq!(common::git(&empty, &["rev-list", "--count", "main"]), "1");
}

#[tokio::test]
async fn a_repository_that_has_files_needs_a_directory_or_permission_to_overwrite() {
    let rig = Rig::new().await;
    start_scratch(&rig, "fib").await;
    build_fib(&rig, None).await;
    let lib = rig
        .fx
        .extra_remote("lib", &[("README.md", "lib\n"), ("fib.sh", "echo old\n")]);
    let url = lib.to_string_lossy().into_owned();
    rig.say(&url).await;
    let checked = RunChecks
        .call(&rig.ctx, json!({"command": "sh ./check.sh"}))
        .await
        .unwrap();
    assert!(!checked.is_error);

    // Not into the root of somebody's project on the model's say-so.
    let out = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await;
    assert!(is_error(&out), "{out:?}");
    let message = text(out);
    assert!(
        message.contains("already has files")
            && message.contains("`path`")
            && message.contains("overwrite")
            && message.contains("ask_user"),
        "{message}"
    );
    let repo_dir = slot_of(&rig, "lib");
    assert_eq!(
        std::fs::read_to_string(repo_dir.join("fib.sh")).unwrap(),
        "echo old\n",
        "nothing was copied"
    );
    // (The repository is in the workspace, as prepare_workspace would have it.)
    assert!(repo_dir.join("README.md").is_file());

    // Into a directory of it: fine, and the code is not what was checked, so the model is told.
    let out = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url, "path": "apps/fib"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("not the code the checks ran on")
            && out.content.contains("run the checks again in `lib`"),
        "{}",
        out.content
    );
    assert!(repo_dir.join("apps/fib/fib.sh").is_file());
    assert_eq!(
        std::fs::read_to_string(repo_dir.join("fib.sh")).unwrap(),
        "echo old\n"
    );
    // A hostile directory is refused by the workspace, and nothing moves.
    for bad in ["../x", "/etc", ".git/hooks", "a/.GIT/b"] {
        let out = PublishScratch
            .call(&rig.ctx, json!({"repo_url": url, "path": bad}))
            .await;
        assert!(is_error(&out), "{bad}: {out:?}");
    }

    // The same files, a changed one: a collision lists it, changes nothing, and says what to ask.
    std::fs::write(slot_of(&rig, "fib").join("fib.sh"), "echo new\n").unwrap();
    let out = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url, "path": "apps/fib"}))
        .await;
    assert!(is_error(&out), "{out:?}");
    let message = text(out);
    assert!(
        message.contains("Nothing was copied")
            && message.contains("fib.sh: the repository has a file here with other content")
            && message.contains("overwrite: true"),
        "{message}"
    );
    assert_eq!(
        std::fs::read_to_string(repo_dir.join("apps/fib/fib.sh")).unwrap(),
        "echo 0 1 1 2 3 5 8\n"
    );
    // With permission it replaces what differs, and only that.
    let out = PublishScratch
        .call(
            &rig.ctx,
            json!({"repo_url": url, "path": "apps/fib", "overwrite": true}),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("copied (1): fib.sh"),
        "{}",
        out.content
    );
    assert_eq!(
        std::fs::read_to_string(repo_dir.join("apps/fib/fib.sh")).unwrap(),
        "echo new\n"
    );
    // `overwrite` is also the person's word for the root of a repository with files, and it
    // replaces `fib.sh` there.
    let out = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url, "overwrite": true}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(repo_dir.join("fib.sh")).unwrap(),
        "echo new\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo_dir.join("README.md")).unwrap(),
        "lib\n",
        "what the project does not have is left alone"
    );
}

#[tokio::test]
async fn an_empty_project_is_not_published_and_leaves_the_repository_empty() {
    let rig = Rig::new().await;
    // No project at all.
    let empty = rig.fx.empty_remote("fibonacci");
    let url = empty.to_string_lossy().into_owned();
    rig.say(&url).await;
    let none = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await;
    assert!(is_error(&none), "{none:?}");
    assert!(text(none).contains("start_scratch"));
    // A project with nothing in it.
    start_scratch(&rig, "fib").await;
    let nothing = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await;
    assert!(is_error(&nothing), "{nothing:?}");
    assert!(text(nothing).contains("has no files yet"));
    assert!(
        common::git(&empty, &["for-each-ref"]).is_empty(),
        "an empty project does not even give the repository its first commit"
    );
}

#[tokio::test]
async fn which_scratch_project_to_publish_is_said_when_there_are_several() {
    let rig = Rig::new().await;
    start_scratch(&rig, "fib").await;
    start_scratch(&rig, "sort").await;
    for (repo, content) in [("fib", "fib\n"), ("sort", "sort\n")] {
        WriteFile
            .call(
                &rig.ctx,
                json!({"path": format!("{repo}.txt"), "content": content, "repo": repo}),
            )
            .await
            .unwrap();
    }
    let empty = rig.fx.empty_remote("algorithms");
    let url = empty.to_string_lossy().into_owned();
    rig.say(&url).await;
    let which = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await;
    assert!(is_error(&which), "{which:?}");
    let message = text(which);
    assert!(
        message.contains("2 scratch projects")
            && message.contains("`fib`")
            && message.contains("`sort`"),
        "{message}"
    );
    let unknown = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url, "scratch": "nope"}))
        .await;
    assert!(is_error(&unknown), "{unknown:?}");
    assert!(text(unknown).contains("is not a scratch project"));
    assert!(common::git(&empty, &["for-each-ref"]).is_empty());

    let out = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url, "scratch": "sort"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    let repo_dir = slot_of(&rig, "algorithms");
    assert!(repo_dir.join("sort.txt").is_file() && !repo_dir.join("fib.txt").exists());
    // A repository slot is not a project.
    let not_a_project = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url, "scratch": "algorithms"}))
        .await;
    assert!(is_error(&not_a_project), "{not_a_project:?}");
}

#[tokio::test]
async fn a_cancelled_run_publishes_nothing() {
    let mut rig = Rig::new().await;
    let token = CancelToken::new();
    rig.ctx = rig.ctx.with_cancel_token(token.clone());
    start_scratch(&rig, "fib").await;
    build_fib(&rig, None).await;
    let empty = rig.fx.empty_remote("fibonacci");
    let url = empty.to_string_lossy().into_owned();
    rig.say(&url).await;
    token.cancel();

    let out = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await;
    assert!(
        matches!(&out, Err(ToolError::Permanent(m)) if m.contains("cancelled")),
        "{out:?}"
    );
    assert!(
        common::git(&empty, &["for-each-ref"]).is_empty(),
        "not even the first commit was pushed"
    );
}

/// Scratch history is not carried (ADR 0008): the pull request holds the commits made in the
/// repository, and the project's own commits stay in the project.
#[tokio::test]
async fn the_history_of_the_project_is_not_carried_into_the_repository() {
    let rig = Rig::new().await;
    start_scratch(&rig, "fib").await;
    build_fib(&rig, None).await;
    CommitAndPush
        .call(&rig.ctx, json!({"message": "wip: fib"}))
        .await
        .unwrap();
    let empty = rig.fx.empty_remote("fibonacci");
    let url = empty.to_string_lossy().into_owned();
    rig.say(&url).await;
    PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await
        .unwrap();
    let repo_dir = slot_of(&rig, "fibonacci");
    assert_eq!(
        common::git(&repo_dir, &["rev-list", "--count", "HEAD"]),
        "1",
        "the worktree starts from the empty first commit alone"
    );
    assert_eq!(
        common::git(&slot_of(&rig, "fib"), &["rev-list", "--count", "HEAD"]),
        "2",
        "the project keeps its own"
    );
}

/// Credentials that mint a token the redactor has never heard of, as a GitHub App's do.
struct Minting;

#[async_trait::async_trait]
impl adam_workspace::GitCredentials for Minting {
    async fn token_for(
        &self,
        _repo: &adam_workspace::RepoRef,
    ) -> Result<secrecy::SecretString, adam_workspace::WorkspaceError> {
        Ok(secrecy::SecretString::from("ghs_runtimeMinted0123456789"))
    }
}

/// An installation token only exists once it is minted, so it cannot be registered at startup: the
/// credentials register it as they hand it out, and the tools' results, which share the redactor,
/// scrub it from then on.
#[tokio::test]
async fn a_token_minted_while_the_process_runs_is_scrubbed_from_what_the_tools_return() {
    use adam_coder::{RedactingCredentials, Redactor, coder_tools};
    use adam_workspace::{DynGitCredentials, RepoRef, Workspaces};

    let fx = Fixture::new("hello\n").await;
    let redactor = Redactor::default();
    let creds: DynGitCredentials = Arc::new(RedactingCredentials::new(
        Arc::new(Minting),
        redactor.clone(),
    ));
    let env = Arc::new(
        ToolEnv::new(
            Workspaces::new(fx.tmp.path().join("minting-work"), creds.clone()),
            fx.env.code_host.clone(),
            fx.env.settings.clone(),
        )
        .with_redactor(redactor),
    );
    let sink = CollectingSink::new();
    let ctx = ToolCtx::detached("tool", "call-1", Arc::new(sink)).with_state(env.clone());
    let tools: Vec<_> = coder_tools(&env).into_iter().collect();
    let tool = |name: &str| {
        tools
            .iter()
            .find(|t| t.spec().name == name)
            .unwrap_or_else(|| panic!("no tool {name}"))
            .clone()
    };
    tool("start_scratch")
        .call(&ctx, json!({"name": "notes"}))
        .await
        .unwrap();
    tool("write_file")
        .call(
            &ctx,
            json!({"path": "t.txt", "content": "push with ghs_runtimeMinted0123456789 please\n"}),
        )
        .await
        .unwrap();
    let read = || async { text(tool("read_file").call(&ctx, json!({"path": "t.txt"})).await) };
    assert_eq!(
        read().await,
        "push with ghs_runtimeMinted0123456789 please\n",
        "not known yet: nothing to scrub"
    );

    let token = creds
        .token_for(&RepoRef::new("https://github.com/o/r", "main"))
        .await
        .unwrap();
    assert_eq!(
        secrecy::ExposeSecret::expose_secret(&token),
        "ghs_runtimeMinted0123456789"
    );
    assert_eq!(read().await, "push with [redacted] please\n");
}

// -------------------------------------------------------------- create_repository

/// A rig whose host creates repositories for the owner `acme` (an organisation), for `me` (a user
/// the credentials are) and, when `login` is `None`, for organisations only.
async fn creating(login: Option<&str>) -> (Rig, Arc<common::CreatingHost>) {
    let (fx, host) =
        Fixture::new("hello\n")
            .await
            .creating(&["acme", "me", "somebody"], &["acme"], login);
    (Rig::from(fx), host)
}

/// The person answers the question `create_repository` asked: what the agent records from the
/// conversation before each step.
async fn answer(rig: &Rig, subject: &str, yes: bool) {
    let run = rig.ctx.run_id().to_string();
    let mut notes = rig.fx.env.notes.load(&run).await.unwrap();
    notes.record_consents([Consent {
        call_id: "call-1".into(),
        tool: "create_repository".into(),
        subject: subject.into(),
        agreed: yes,
    }]);
    rig.fx.env.notes.save(&run, &notes).await.unwrap();
}

async fn create(rig: &Rig, args: Value) -> Result<ToolOutput, ToolError> {
    CreateRepository.call(&rig.ctx, args).await
}

fn question_of(out: Result<ToolOutput, ToolError>) -> (String, bool) {
    match out {
        Err(ToolError::NeedsInput { question, ui }) => (question, ui.is_some()),
        other => panic!("the person should have been asked: {other:?}"),
    }
}

#[tokio::test]
async fn creating_is_off_unless_the_deployment_names_owners_and_never_for_another_owner() {
    // Off: the default settings name nobody.
    let rig = Rig::new().await;
    let out = create(&rig, json!({"owner": "acme", "name": "fib"})).await;
    assert!(is_error(&out), "{out:?}");
    assert!(text(out).contains("switched off"));

    let (rig, host) = creating(None).await;
    let out = create(&rig, json!({"owner": "evil", "name": "fib"})).await;
    let message = text(out);
    assert!(
        message.contains("only for: acme, me, somebody"),
        "{message}"
    );
    // The allowed owner, in another case, is the same owner.
    let asked = create(&rig, json!({"owner": "ACME", "name": "fib"})).await;
    assert!(
        matches!(asked, Err(ToolError::NeedsInput { .. })),
        "{asked:?}"
    );
    for (args, needle) in [
        (
            json!({"owner": "acme", "name": "a/b"}),
            "not a repository name",
        ),
        (
            json!({"owner": "acme", "name": ".."}),
            "not a repository name",
        ),
        (
            json!({"owner": "acme", "name": "x.git"}),
            "not a repository name",
        ),
        (json!({"owner": "acme", "name": " "}), "name is required"),
        (json!({"owner": "", "name": "x"}), "owner is required"),
        (
            json!({"owner": "acme", "name": "x", "description": "d".repeat(351)}),
            "too long",
        ),
    ] {
        let out = create(&rig, args.clone()).await;
        assert!(is_error(&out) && text(out).contains(needle), "{args}");
    }
    assert!(host.created().is_empty(), "nothing was created");
}

#[tokio::test]
async fn the_person_is_asked_first_with_a_question_the_tool_wrote_and_nothing_is_created() {
    let (rig, host) = creating(None).await;
    let (question, has_form) = question_of(
        create(
            &rig,
            json!({"owner": "acme", "name": "fib", "description": "the \"first\"\n seven numbers"}),
        )
        .await,
    );
    assert!(
        question.contains(
            "May I create the repository acme/fib on github.com? It will be private and empty."
        ) && question.contains("Description: \"the 'first' seven numbers\"")
            && question.contains("a) Create acme/fib")
            && question.contains("b) Don't create it"),
        "{question}"
    );
    assert!(
        !has_form,
        "no screen was announced: the options are in the text"
    );
    assert!(host.created().is_empty());
    // A public one is another question, in other words.
    answer(&rig, "acme/fib:private", true).await;
    let (question, _) = question_of(
        create(
            &rig,
            json!({"owner": "acme", "name": "fib", "private": false}),
        )
        .await,
    );
    assert!(
        question.contains("public (anyone can see it) and empty"),
        "{question}"
    );
    assert!(
        host.created().is_empty(),
        "a yes to the private one is no yes to the public one"
    );
}

#[tokio::test]
async fn after_a_yes_the_repository_is_created_empty_private_granted_and_a_repeat_is_the_same() {
    let (rig, host) = creating(None).await;
    answer(&rig, "acme/fib:private", true).await;
    let first = create(
        &rig,
        json!({"owner": "acme", "name": "fib", "description": "Fibonacci"}),
    )
    .await
    .unwrap();
    assert!(!first.is_error, "{}", first.content);
    let path = host.path_of("acme", "fib");
    assert!(
        first
            .content
            .starts_with("Created acme/fib (private, empty")
            && first
                .content
                .contains(&format!("repository: {}", path.display())),
        "{}",
        first.content
    );
    let made = host.created();
    assert_eq!(made.len(), 1);
    assert!(made[0].private);
    assert_eq!(made[0].description.as_deref(), Some("Fibonacci"));
    assert_eq!(made[0].kind, adam_workspace::OwnerKind::Organization);
    assert!(
        common::git(&path, &["for-each-ref"]).is_empty(),
        "it was created empty: no ref at all"
    );
    // The repository is granted (by the key of its clone URL) and recorded.
    let notes = rig
        .fx
        .env
        .notes
        .load(&rig.ctx.run_id().to_string())
        .await
        .unwrap();
    assert_eq!(notes.created_repos.len(), 1);
    assert_eq!(
        notes.named_repos,
        [notes.created_repos[0].key.clone()],
        "the grant is the key of the clone URL"
    );
    // A repeated call creates nothing and says the same.
    let again = create(
        &rig,
        json!({"owner": "ACME", "name": "Fib", "description": "Fibonacci"}),
    )
    .await
    .unwrap();
    assert_eq!(again.content, first.content);
    assert_eq!(host.created().len(), 1);
    // And it is a repository the tools that need a grant now take: a scratch project goes in.
    start_scratch(&rig, "fib").await;
    build_fib(&rig, None).await;
    let published = PublishScratch
        .call(&rig.ctx, json!({"repo_url": path.to_string_lossy()}))
        .await
        .unwrap();
    assert!(!published.is_error, "{}", published.content);
}

#[tokio::test]
async fn a_no_is_remembered_and_nothing_is_created_or_asked_again() {
    let (rig, host) = creating(None).await;
    answer(&rig, "acme/fib:private", false).await;
    let out = create(&rig, json!({"owner": "acme", "name": "fib"})).await;
    assert!(is_error(&out), "{out:?}");
    assert!(text(out).contains("declined"));
    assert!(host.created().is_empty());
}

#[tokio::test]
async fn a_name_that_exists_and_was_not_made_by_this_run_is_left_alone() {
    let (rig, host) = creating(None).await;
    host.take("acme", "fib");
    answer(&rig, "acme/fib:private", true).await;
    let out = create(&rig, json!({"owner": "acme", "name": "fib"})).await;
    assert!(is_error(&out), "{out:?}");
    let message = text(out);
    assert!(
        message.contains("already exists") && message.contains("did not create it"),
        "{message}"
    );
    let notes = rig
        .fx
        .env
        .notes
        .load(&rig.ctx.run_id().to_string())
        .await
        .unwrap();
    assert!(
        notes.created_repos.is_empty() && notes.named_repos.is_empty(),
        "no grant"
    );
}

/// A process that dies after the host made the repository and before the run noted it is run again:
/// the host says the name exists, and the intent written before the host was asked says whose it is.
#[tokio::test]
async fn a_creation_that_died_before_its_note_is_adopted_when_it_is_repeated() {
    let (rig, host) = creating(None).await;
    let run = rig.ctx.run_id().to_string();
    answer(&rig, "acme/fib:private", true).await;
    let args = json!({"owner": "acme", "name": "fib"});
    let first = create(&rig, args.clone()).await.unwrap();
    assert!(!first.is_error, "{}", first.content);
    // The crash: the host has the repository, the notes have only the intent.
    let mut notes = rig.fx.env.notes.load(&run).await.unwrap();
    assert!(
        notes.creating.is_empty(),
        "a creation that was noted leaves no intent"
    );
    notes.created_repos.clear();
    notes.named_repos.clear();
    assert!(notes.begin_creating("acme/fib", true));
    rig.fx.env.notes.save(&run, &notes).await.unwrap();

    let again = create(&rig, args.clone()).await.unwrap();
    assert!(!again.is_error, "{}", again.content);
    assert_eq!(
        again.content, first.content,
        "the same answer as the first time"
    );
    assert_eq!(host.created().len(), 1, "it was not created a second time");
    let notes = rig.fx.env.notes.load(&run).await.unwrap();
    assert_eq!(notes.created_repos.len(), 1);
    assert_eq!(
        notes.named_repos,
        [notes.created_repos[0].key.clone()],
        "granted by the key of its clone URL, as when it is created"
    );
    assert!(notes.creating.is_empty(), "the intent is settled");
}

/// The intent is written before the host is asked, so a host whose answer is lost (it made the
/// repository, the call failed) is recovered by the retry.
#[tokio::test]
async fn the_intent_is_written_before_the_host_is_asked_so_a_lost_answer_is_recovered() {
    let (rig, host) = creating(None).await;
    let run = rig.ctx.run_id().to_string();
    answer(&rig, "acme/fib:private", true).await;
    host.answer_is_lost(1);
    let args = json!({"owner": "acme", "name": "fib"});
    let lost = create(&rig, args.clone()).await;
    assert!(matches!(lost, Err(ToolError::Transient(_))), "{lost:?}");
    let notes = rig.fx.env.notes.load(&run).await.unwrap();
    assert!(
        notes.is_creating("acme/fib", true),
        "the intent is kept: the host may have made it"
    );
    assert!(notes.created_repos.is_empty());

    let retried = create(&rig, args).await.unwrap();
    assert!(!retried.is_error, "{}", retried.content);
    assert!(
        retried
            .content
            .starts_with("Created acme/fib (private, empty")
    );
    assert_eq!(host.created().len(), 1, "the retry did not create it again");
    let notes = rig.fx.env.notes.load(&run).await.unwrap();
    assert_eq!(notes.created_repos.len(), 1);
    assert!(notes.creating.is_empty());
}

/// Without an intent a taken name is somebody else's, and a refusal leaves no intent behind to turn
/// the next call into an adoption.
#[tokio::test]
async fn a_refused_creation_leaves_no_intent_and_a_taken_name_is_never_adopted() {
    let (rig, host) = creating(None).await;
    let run = rig.ctx.run_id().to_string();
    host.take("acme", "fib");
    answer(&rig, "acme/fib:private", true).await;
    for _ in 0..2 {
        let out = create(&rig, json!({"owner": "acme", "name": "fib"})).await;
        assert!(is_error(&out), "{out:?}");
        assert!(text(out).contains("did not create it"));
        let notes = rig.fx.env.notes.load(&run).await.unwrap();
        assert!(notes.creating.is_empty(), "{:?}", notes.creating);
        assert!(notes.created_repos.is_empty() && notes.named_repos.is_empty());
    }
}

/// A user's repository is made for the person the credentials are, and for nobody else; an
/// installation has no such person, so it makes organisations' only.
#[tokio::test]
async fn a_user_owner_needs_credentials_that_are_that_user() {
    let (rig, host) = creating(Some("me")).await;
    answer(&rig, "me/mine:private", true).await;
    let ok = create(&rig, json!({"owner": "me", "name": "mine"}))
        .await
        .unwrap();
    assert!(!ok.is_error, "{}", ok.content);
    assert_eq!(host.created()[0].kind, adam_workspace::OwnerKind::User);
    answer(&rig, "somebody/theirs:private", true).await;
    let other = create(&rig, json!({"owner": "somebody", "name": "theirs"})).await;
    assert!(is_error(&other), "{other:?}");
    assert!(text(other).contains("these credentials are me's"));
    assert_eq!(host.created().len(), 1);

    // An installation (no login): a user owner is refused **before** the person is asked.
    let (rig, host) = creating(None).await;
    let out = create(&rig, json!({"owner": "me", "name": "mine"})).await;
    assert!(is_error(&out), "{out:?}");
    assert!(text(out).contains("GitHub App installation"));
    assert!(host.created().is_empty());
    // An organisation is fine either way.
    assert!(matches!(
        create(&rig, json!({"owner": "acme", "name": "mine"})).await,
        Err(ToolError::NeedsInput { .. })
    ));
}

/// What the host says the clone URL is must be somewhere the workspace may go.
#[tokio::test]
async fn a_clone_url_the_workspace_may_not_use_is_an_error_and_grants_nothing() {
    let (rig, host) = creating(None).await;
    host.clone_url_is("https://evil.example/acme/fib.git");
    answer(&rig, "acme/fib:private", true).await;
    let out = create(&rig, json!({"owner": "acme", "name": "fib"})).await;
    assert!(is_error(&out), "{out:?}");
    let message = text(out);
    assert!(
        message.contains("was created") && message.contains("not one this workspace may use"),
        "{message}"
    );
    let notes = rig
        .fx
        .env
        .notes
        .load(&rig.ctx.run_id().to_string())
        .await
        .unwrap();
    assert!(notes.named_repos.is_empty() && notes.created_repos.is_empty());
}

/// A repository that is made a moment before git can see it is waited for.
#[tokio::test]
async fn the_tool_waits_for_the_new_repository_to_be_reachable() {
    let (rig, host) = creating(None).await;
    let late = host.path_of("acme", "late");
    host.clone_url_is(&late.to_string_lossy());
    answer(&rig, "acme/late:private", true).await;
    let made = late.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        std::fs::create_dir_all(&made).unwrap();
        common::git(
            &made,
            &["init", "--bare", "--quiet", "--initial-branch=main"],
        );
    });
    let started = Instant::now();
    let out = create(&rig, json!({"owner": "acme", "name": "late"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(
        started.elapsed() >= Duration::from_millis(600),
        "it waited for it"
    );
}

// ------------------------------------------------------------------ share_file

const SVG: &[u8] = b"<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8'><rect width='8' height='8'/></svg>";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01";

/// The one file artifact of a tool result: `(name, media type, filename, bytes)`.
fn shared(out: &ToolOutput) -> (String, String, String, Vec<u8>) {
    assert_eq!(out.artifacts.len(), 1, "{out:?}");
    let artifact = &out.artifacts[0];
    let file = artifact.file.as_ref().expect("a file artifact");
    (
        artifact.name.clone(),
        artifact.mime_type.clone().expect("a media type"),
        file.filename.clone(),
        file.bytes.clone(),
    )
}

#[tokio::test]
async fn share_file_needs_a_workspace_like_the_others() {
    let rig = Rig::new().await;
    let out = ShareFile.call(&rig.ctx, json!({"path": "chart.svg"})).await;
    assert!(is_error(&out), "{out:?}");
    assert!(text(out).contains("prepare_workspace"));
}

/// The coder makes an SVG and shares it: the result is one line for the model and the whole file,
/// with its type and name, as an artifact.
#[tokio::test]
async fn share_file_returns_the_file_as_an_artifact_and_tells_the_model_one_line() {
    let rig = Rig::new().await;
    rig.prepare().await;
    WriteFile
        .call(
            &rig.ctx,
            json!({"path": "out/chart.svg", "content": String::from_utf8_lossy(SVG)}),
        )
        .await
        .unwrap();

    let out = ShareFile
        .call(&rig.ctx, json!({"path": "out/chart.svg"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        format!("Shared chart.svg ({} bytes, image/svg+xml).", SVG.len())
    );
    assert_eq!(
        shared(&out),
        (
            "chart.svg".to_owned(),
            "image/svg+xml".to_owned(),
            "chart.svg".to_owned(),
            SVG.to_vec()
        )
    );
    assert!(
        rig.progress()
            .contains(&"sharing out/chart.svg (remote)".to_owned()),
        "{:?}",
        rig.progress()
    );

    // A name of the model's own, and a PNG.
    std::fs::write(rig.worktree().join("shot.png"), PNG).unwrap();
    let named = ShareFile
        .call(&rig.ctx, json!({"path": "shot.png", "name": "Screenshot"}))
        .await
        .unwrap();
    assert_eq!(
        shared(&named),
        (
            "Screenshot".to_owned(),
            "image/png".to_owned(),
            "shot.png".to_owned(),
            PNG.to_vec()
        )
    );
    // Sharing it again is the same artifact (its id follows its bytes).
    let again = ShareFile
        .call(&rig.ctx, json!({"path": "shot.png", "name": "Screenshot"}))
        .await
        .unwrap();
    assert_eq!(again.artifacts, named.artifacts);
}

/// What a command made is shared like what the coder wrote: the workspace is one directory, whatever
/// runs in it (the repository's devcontainer mounts the same one).
#[tokio::test]
async fn share_file_shares_what_a_command_produced() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let made = RunChecks
        .call(
            &rig.ctx,
            json!({"command": "printf '<svg xmlns=\"http://www.w3.org/2000/svg\"/>' > made.svg"}),
        )
        .await
        .unwrap();
    assert_eq!(checks_of(&made)["passed"], true);
    let out = ShareFile
        .call(&rig.ctx, json!({"path": "made.svg"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    let (_, media_type, filename, bytes) = shared(&out);
    assert_eq!(
        (media_type.as_str(), filename.as_str()),
        ("image/svg+xml", "made.svg")
    );
    assert_eq!(bytes, b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>");
}

#[tokio::test]
async fn share_file_types_by_extension_and_bytes() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let wt = rig.worktree();
    for (name, bytes, expected) in [
        ("good.png", PNG, "image/png"),
        ("logo.svg", SVG, "image/svg+xml"),
        ("notes.md", b"# notes\n".as_slice(), "text/markdown"),
        ("table.csv", b"a,b\n".as_slice(), "text/csv"),
        // The bytes are a PNG, the name says text; the name says PNG, the bytes are HTML.
        ("lying.txt", PNG, "application/octet-stream"),
        (
            "lying.png",
            b"<html></html>".as_slice(),
            "application/octet-stream",
        ),
        ("blob.bin", b"\0\x01".as_slice(), "application/octet-stream"),
    ] {
        std::fs::write(wt.join(name), bytes).unwrap();
        let out = ShareFile
            .call(&rig.ctx, json!({"path": name}))
            .await
            .unwrap();
        assert!(!out.is_error, "{name}: {}", out.content);
        assert_eq!(shared(&out).1, expected, "{name}");
        assert!(out.content.contains(expected), "{name}: {}", out.content);
    }
}

/// Whatever the model passes, a path that leaves the worktree, enters `.git`, is not a file, or is
/// too big shares nothing and says why (as `read_file` refuses the same paths).
#[tokio::test]
async fn share_file_refuses_what_it_must() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let wt = rig.worktree();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "s3cr3t").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), wt.join("leak.txt")).unwrap();
    std::os::unix::fs::symlink(outside.path(), wt.join("outdir")).unwrap();
    std::os::unix::fs::symlink(".git", wt.join("gitlink")).unwrap();
    std::fs::create_dir_all(wt.join("empty_dir")).unwrap();
    std::fs::write(wt.join("big.bin"), vec![0u8; 4 * 1024 * 1024 + 1]).unwrap();

    for (path, needle) in [
        ("../secret.txt", "`..`"),
        ("sub/../../secret.txt", "`..`"),
        ("/etc/hostname", "absolute"),
        (".git/config", ".git"),
        (".GIT/HEAD", ".git"),
        ("leak.txt", "outside the worktree"),
        ("outdir/secret.txt", "outside the worktree"),
        ("gitlink", ".git"),
        ("missing.png", "does not exist"),
        ("empty_dir", "is a directory"),
        ("big.bin", "over the limit"),
        ("  ", "path is required"),
    ] {
        let out = ShareFile.call(&rig.ctx, json!({"path": path})).await;
        assert!(is_error(&out), "{path}: {out:?}");
        let (message, artifacts) = {
            let o = out.unwrap();
            (o.content, o.artifacts)
        };
        assert!(message.contains(needle), "{path}: {message}");
        assert!(!message.contains("s3cr3t"), "{path}: {message}");
        assert!(artifacts.is_empty(), "{path}: nothing is shared");
    }
}

/// The coder's tools are wrapped by its redactor: a text file that holds a value the process hides
/// is shared with the value taken out, and a file that is not text is left as it is.
#[tokio::test]
async fn a_shared_text_file_is_scrubbed_of_the_secrets_the_process_knows() {
    let rig = Rig::new().await;
    rig.prepare().await;
    let wt = rig.worktree();
    std::fs::write(
        wt.join("env.txt"),
        format!("TOKEN={}\nrest\n", common::GITHUB_TOKEN),
    )
    .unwrap();
    let mut binary = PNG.to_vec();
    binary.extend_from_slice(common::GITHUB_TOKEN.as_bytes());
    binary.push(0xFF);
    std::fs::write(wt.join("shot.png"), &binary).unwrap();

    let tool = adam_coder::coder_tools(&rig.fx.env)
        .into_iter()
        .find(|t| t.spec().name == "share_file")
        .expect("share_file is a coder tool");
    let out = tool
        .call(&rig.ctx, json!({"path": "env.txt"}))
        .await
        .unwrap();
    let (_, _, _, bytes) = shared(&out);
    let shown = String::from_utf8(bytes).unwrap();
    assert!(!shown.contains(common::GITHUB_TOKEN), "{shown}");
    assert!(shown.contains("rest"), "{shown}");

    let out = tool
        .call(&rig.ctx, json!({"path": "shot.png"}))
        .await
        .unwrap();
    assert_eq!(
        shared(&out).3,
        binary,
        "bytes that are not text are not rewritten"
    );
}
