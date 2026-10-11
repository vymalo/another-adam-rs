//! The access tiers of the coder (`access-tier/v1`, ADR 0036), end to end where it matters and tool
//! by tool where the rule is:
//!
//! * the tools the model is **shown** on a turn depend on the tier of the message (a real `Runtime`,
//!   a scripted model, the agent's own tool source);
//! * a model that calls a tool above the tier **anyway** is refused when the call would run, and the
//!   host is never asked;
//! * the tools check the tier again from the run's notes, the root run's, so a subagent keeps to it;
//! * `merge_pull_request` against the GitHub test double, and what GitHub refuses reported in its own
//!   words with the class of the error;
//! * `rename_repository` and `delete_repository` on the repositories the person named, the
//!   `confirm` guard of the deletion;
//! * `CREATE_REPO_OWNERS` empty means the accounts a GitHub App acts for, and a list still restricts;
//!   `bypass` asks no question, `auto` still does;
//! * the A2A side of the coder: a bad tier is refused before anything is written, the tier is said
//!   on the task.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use a2a::{Message as A2aMessage, Part, Role};
use adam_a2a::{ACCESS_TIER_EXTENSION, AccessTier, BackendError, Caller, TaskBackend as _};
use adam_a2a_runtime::CONTEXT_ACCESS_TIER;
use adam_coder::{AGENT_NAME, Coder, CoderAgent, RuntimeOptions, coder_tools};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    LlmAgent, SourceCtx, Tool, ToolCtx, ToolError, ToolOutput, ToolSource, user_message,
};
use adam_model::{DynModel, Message, MockModel, ModelRequest, ToolCall, ToolSpec};
use adam_runtime::{CollectingSink, Inbound, Runtime};
use adam_workspace::{CodeHost, DynCodeHost, MemoryCodeHost, NewPullRequest, RepoRef};
use common::Fixture;
use serde_json::{Value, json};

/// The tools of the coder in the order they are offered, with the two subagents, as the model is
/// shown them at the highest tier.
const ALL: [&str; 25] = [
    "prepare_workspace",
    "start_scratch",
    "publish_scratch",
    "request_repository",
    "create_repository",
    "run_command",
    "run",
    "read_file",
    "write_file",
    "apply_patch",
    "edit_file",
    "share_file",
    "delegate_to_opencode",
    "run_checks",
    "rebuild_environment",
    "commit_and_push",
    "open_pull_request",
    "merge_pull_request",
    "rename_repository",
    "delete_repository",
    "ask_user",
    "show",
    "ui_catalog",
    "explorer",
    "reviewer",
];

fn tier_message(text: &str, tier: Option<AccessTier>) -> Inbound {
    let mut payload = json!({ "text": text });
    if let Some(tier) = tier {
        payload["context"] = json!({ CONTEXT_ACCESS_TIER: { "tier": tier.as_str() } });
    }
    Inbound::new("message", payload)
}

/// Run the coder on `fx` for one message that names `tier` (or none), with the model scripted by
/// `script`, until it is done; what the model was sent, the run and its end.
async fn drive(
    fx: &Fixture,
    text: &str,
    tier: Option<AccessTier>,
    script: impl FnOnce(&MockModel),
) -> (Vec<ModelRequest>, RunId) {
    let mock = Arc::new(MockModel::new());
    script(&mock);
    let model: DynModel = mock.clone();
    let agent = CoderAgent::new(model, "test-model", fx.env.clone());
    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = Runtime::builder(store)
        .agent(agent)
        .poll_interval(Duration::from_millis(20))
        .build();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    let worker = {
        let rt = rt.clone();
        tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
        })
    };
    let run = rt
        .start(AGENT_NAME, tier_message(text, tier), None)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let view = rt.view(run).await.unwrap().expect("run exists");
        match view.status {
            RunStatus::Done => break,
            RunStatus::Parked if view.waiting => break,
            RunStatus::Failed => panic!("the run failed: {view:#?}"),
            _ => {}
        }
        assert!(Instant::now() < deadline, "timed out: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = stop.send(());
    worker.await.unwrap().unwrap();
    (mock.requests(), run)
}

fn names(request: &ModelRequest) -> Vec<&str> {
    request.tools.iter().map(|t| t.name.as_str()).collect()
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

/// What the tool results of the model's next request say, by call id.
fn result_of(request: &ModelRequest, id: &str) -> String {
    request
        .messages
        .iter()
        .find_map(|m| match m {
            Message::Tool {
                call_id, content, ..
            } if call_id == id => Some(content.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}: {:?}", request.messages))
}

// ----------------------------------------------------------------------- what the model is shown

#[tokio::test]
async fn the_model_is_shown_only_the_tools_the_tier_of_the_message_allows() {
    let fx = Fixture::new("hello\n").await.offering_creation();
    let without = |drop: &[&str]| -> Vec<&'static str> {
        ALL.into_iter().filter(|n| !drop.contains(n)).collect()
    };
    let plan = [
        "prepare_workspace",
        "request_repository",
        "run_command",
        "read_file",
        "share_file",
        "rebuild_environment",
        "ask_user",
        "show",
        "ui_catalog",
        "explorer",
        "reviewer",
    ];
    let edits_drop = [
        "create_repository",
        "merge_pull_request",
        "rename_repository",
        "delete_repository",
    ];
    for (tier, expected) in [
        (Some(AccessTier::Plan), plan.to_vec()),
        (Some(AccessTier::Edits), without(&edits_drop)),
        (
            Some(AccessTier::Auto),
            without(&["rename_repository", "delete_repository"]),
        ),
        (Some(AccessTier::Bypass), without(&[])),
        // No message named a tier: the default, auto.
        (None, without(&["rename_repository", "delete_repository"])),
    ] {
        let (requests, run) = drive(&fx, "hi", tier, |m| {
            m.push_text("Hello.");
        })
        .await;
        assert_eq!(names(&requests[0]), expected, "{tier:?}");
        // The agent noted the tier from the message, for the tools to check again.
        let notes = fx.env.notes.load(&run.to_string()).await.unwrap();
        assert_eq!(notes.tier(), tier.unwrap_or(AccessTier::Auto), "{tier:?}");
        // A tier that was named is said to the model, in words; none named says nothing extra.
        let system = requests[0].system.clone().unwrap();
        match tier {
            Some(tier) => assert!(
                system.contains("## Access tier of this message")
                    && system.contains(&format!("**{tier}**")),
                "{tier}: {system}"
            ),
            None => assert!(!system.contains("## Access tier of this message")),
        }
    }
}

#[tokio::test]
async fn the_newest_message_names_the_tier_from_the_next_step_and_a_task_starts_at_the_default() {
    use adam_coder::CoderStarter;
    use adam_llm_agent::Conversation;
    use adam_runtime::AgentStarter as _;
    // A conversation whose earlier task worked under plan.
    let mut prior = Conversation::new("look");
    prior
        .context
        .insert(CONTEXT_ACCESS_TIER.to_owned(), json!({"tier": "plan"}));
    prior.context.insert("other".to_owned(), json!(1));
    let run = RunId::new();
    // The next task names nothing: it starts at the default, whatever the task before it had; the
    // rest of the context is carried as ever.
    let next = CoderStarter
        .init_continuing(tier_message("now do it", None), &prior, run)
        .unwrap();
    assert!(next.context.get(CONTEXT_ACCESS_TIER).is_none());
    assert_eq!(
        adam_coder::tier::in_context(&next.context),
        AccessTier::Auto
    );
    assert_eq!(next.context.get("other"), Some(&json!(1)));
    // One that names a tier has that one, and only that one.
    let named = CoderStarter
        .init_continuing(
            tier_message("now do it", Some(AccessTier::Edits)),
            &prior,
            run,
        )
        .unwrap();
    assert_eq!(
        adam_coder::tier::in_context(&named.context),
        AccessTier::Edits
    );
    // The prior conversation is untouched (it is the other task's state).
    assert_eq!(
        adam_coder::tier::in_context(&prior.context),
        AccessTier::Plan
    );
}

// --------------------------------------------------------------------- refused at the call

/// A fixture whose code host is `host`, which has the pull request #1 of `octo/widgets`, so that a
/// merge of it is something the host would do.
async fn with_memory_host() -> (Fixture, Arc<MemoryCodeHost>) {
    let host = Arc::new(MemoryCodeHost::new());
    host.open_pull_request(NewPullRequest {
        repo: RepoRef::new("https://github.com/octo/widgets", "main"),
        head: "agent/x".into(),
        title: "t".into(),
        body: "b".into(),
        draft: false,
    })
    .await
    .unwrap();
    let dyn_host: DynCodeHost = host.clone();
    (Fixture::new("hello\n").await.hosted_by(dyn_host), host)
}

#[tokio::test]
async fn a_model_that_calls_a_tool_above_the_tier_anyway_is_refused_and_the_host_is_not_asked() {
    for tier in AccessTier::ALL {
        let merge_refused = tier < AccessTier::Auto;
        let delete_refused = tier < AccessTier::Bypass;
        let (fx, host) = with_memory_host().await;
        let (requests, _) = drive(
            &fx,
            "merge pull request 1 of octo/widgets and delete octo/widgets",
            Some(tier),
            |m| {
                m.push_tool_calls(vec![
                    call(
                        "c-write",
                        "write_file",
                        json!({"path": "x.txt", "content": "x"}),
                    ),
                    call(
                        "c-merge",
                        "merge_pull_request",
                        json!({"repo": "octo/widgets", "number": 1}),
                    ),
                    call(
                        "c-delete",
                        "delete_repository",
                        json!({"repo": "octo/widgets", "confirm": "octo/widgets"}),
                    ),
                ])
                .push_text("Done as far as I may.");
            },
        )
        .await;
        let merge = result_of(&requests[1], "c-merge");
        let delete = result_of(&requests[1], "c-delete");
        let write = result_of(&requests[1], "c-write");
        assert_eq!(
            write.contains("needs the edits access tier"),
            tier < AccessTier::Edits,
            "{tier}: {write}"
        );
        // The host is asked for what the tier allows, and for nothing else.
        assert_eq!(
            host.merges().len(),
            usize::from(!merge_refused),
            "{tier}: {merge}"
        );
        assert_eq!(
            host.deletions().len(),
            usize::from(!delete_refused),
            "{tier}: {delete}"
        );
        assert_eq!(
            merge.contains("needs the auto access tier"),
            merge_refused,
            "{tier}: {merge}"
        );
        assert_eq!(
            delete.contains("needs the bypass access tier"),
            delete_refused,
            "{tier}: {delete}"
        );
        // The refusal says which tier the act needs and the one the message is under, in words the
        // model can pass on to the person.
        if merge_refused {
            assert!(
                merge.contains(&format!("this message is under {tier}"))
                    && merge.contains("Pick auto and send it again"),
                "{merge}"
            );
        }
    }
}

/// Where the tools call each other or a subagent calls a tool of the coder, only the notes of the
/// root run can say the tier: a tool called directly is refused from them, a child's call too.
#[tokio::test]
async fn every_tool_checks_the_tier_in_the_notes_of_the_root_run_when_it_is_called() {
    let fx = Fixture::new("hello\n").await.offering_creation();
    let sink = CollectingSink::new();
    let root =
        ToolCtx::detached("tool", "call-1", Arc::new(sink.clone())).with_state(fx.env.clone());
    let tools = coder_tools(&fx.env);
    let set_tier = |tier: AccessTier| {
        let env = fx.env.clone();
        let run = root.run_id().to_string();
        async move {
            let mut notes = env.notes.load(&run).await.unwrap();
            notes.tier = Some(tier);
            env.notes.save(&run, &notes).await.unwrap();
        }
    };
    let call_tool = |ctx: &ToolCtx, name: &'static str, args: Value| {
        let tool = tools.get(name).expect("the tool is in the set").clone();
        let ctx = ctx.clone();
        async move { tool.call(&ctx, args).await }
    };
    let text = |out: Result<ToolOutput, ToolError>| out.unwrap().content;

    set_tier(AccessTier::Plan).await;
    let refused = text(call_tool(&root, "write_file", json!({"path": "a", "content": "b"})).await);
    assert!(
        refused.contains("`write_file` needs the edits access tier"),
        "{refused}"
    );
    // A read is not refused at the lowest tier: it fails for its own reason (no workspace yet).
    let read = text(call_tool(&root, "read_file", json!({"path": "a"})).await);
    assert!(!read.contains("access tier"), "{read}");

    // A subagent's run has a context of its own and no tier in it: it is the root's notes that
    // hold it to `plan`.
    let child = ToolCtx::detached("tool", "call-9", Arc::new(sink.clone()))
        .with_state(fx.env.clone())
        .with_root_run(root.run_id());
    assert_ne!(child.run_id(), root.run_id());
    let refused = text(call_tool(&child, "run", json!({"command": "true"})).await);
    assert!(
        refused.contains("`run` needs the edits access tier"),
        "{refused}"
    );

    set_tier(AccessTier::Edits).await;
    let refused = text(
        call_tool(
            &root,
            "merge_pull_request",
            json!({"repo": "octo/widgets", "number": 1}),
        )
        .await,
    );
    assert!(
        refused.contains("`merge_pull_request` needs the auto access tier"),
        "{refused}"
    );
    let refused = text(
        call_tool(
            &root,
            "create_repository",
            json!({"owner": "acme", "name": "fib"}),
        )
        .await,
    );
    assert!(
        refused.contains("`create_repository` needs the auto access tier"),
        "{refused}"
    );

    set_tier(AccessTier::Auto).await;
    let refused = text(
        call_tool(
            &root,
            "rename_repository",
            json!({"repo": "octo/widgets", "new_name": "gadgets"}),
        )
        .await,
    );
    assert!(
        refused.contains("`rename_repository` needs the bypass access tier"),
        "{refused}"
    );
}

// -------------------------------------------------------------------------------------- merge

/// A rig for one tool call on the fixture, the tier set in the run's notes and the person's words
/// recorded as the agent does before a step.
struct Rig {
    fx: Fixture,
    ctx: ToolCtx,
    /// What the person has said so far, oldest first.
    said: std::sync::Mutex<Vec<String>>,
}

impl Rig {
    fn new(fx: Fixture) -> Self {
        let ctx = ToolCtx::detached("tool", "call-1", Arc::new(CollectingSink::new()))
            .with_state(fx.env.clone());
        Self {
            fx,
            ctx,
            said: std::sync::Mutex::new(Vec::new()),
        }
    }

    async fn tier(&self, tier: AccessTier) {
        let run = self.ctx.run_id().to_string();
        let mut notes = self.fx.env.notes.load(&run).await.unwrap();
        notes.tier = Some(tier);
        self.fx.env.notes.save(&run, &notes).await.unwrap();
    }

    /// The person says `text`, their newest words: what the agent records before a step
    /// (`RunNotes::hear`), from what they typed.
    async fn say(&self, text: &str) {
        let run = self.ctx.run_id().to_string();
        let host = self.fx.env.settings.default_repo_host.clone();
        let mut notes = self.fx.env.notes.load(&run).await.unwrap();
        // The earlier words stand as they were said: the conversation so far, then this.
        self.said.lock().unwrap().push(text.to_owned());
        let said = self.said.lock().unwrap().clone();
        notes.hear(&said, &host, &[]);
        self.fx.env.notes.save(&run, &notes).await.unwrap();
    }

    /// A run of the conversation pushed the commit `abc123`, the head the mock gives its pull
    /// requests, to `branch` of the repository `slug` (`owner/name`).
    async fn pushed(&self, slug: &str, branch: &str) {
        self.pushed_sha(slug, branch, "abc123").await;
    }

    /// A run of the conversation pushed the commit `sha` to `branch` of `slug`.
    async fn pushed_sha(&self, slug: &str, branch: &str, sha: &str) {
        let run = self.ctx.run_id().to_string();
        let mut notes = self.fx.env.notes.load(&run).await.unwrap();
        notes.name_pushed_branches([adam_coder::tools::notes::PushedBranch {
            repo: format!("github.com/{slug}"),
            branch: branch.to_owned(),
            base: Some("main".to_owned()),
            shas: vec![sha.to_owned()],
        }]);
        self.fx.env.notes.save(&run, &notes).await.unwrap();
    }

    /// Pull request `number` of `slug` was opened with red checks accepted.
    async fn opened_red(&self, slug: &str, number: u64) {
        let run = self.ctx.run_id().to_string();
        let mut notes = self.fx.env.notes.load(&run).await.unwrap();
        notes.name_red_pulls([adam_coder::tools::notes::PullRef {
            repo: Some(format!("github.com/{slug}")),
            number,
        }]);
        self.fx.env.notes.save(&run, &notes).await.unwrap();
    }

    /// The tool `merge_pull_request` on pull request 7 of `octo/widgets`.
    async fn merge(&self) -> ToolOutput {
        self.call(
            "merge_pull_request",
            json!({"repo": "octo/widgets", "number": 7}),
        )
        .await
        .unwrap()
    }

    async fn call(&self, name: &'static str, args: Value) -> Result<ToolOutput, ToolError> {
        // The agent writes the tier of the message into the notes before each step: a call with no
        // notes is under a plan (`Gated`), so a rig that never said anything says the default.
        let run = self.ctx.run_id().to_string();
        if self
            .fx
            .env
            .notes
            .load_existing(&run)
            .await
            .unwrap()
            .is_none()
        {
            self.tier(adam_coder::tier::DEFAULT).await;
        }
        let tools = coder_tools(&self.fx.env);
        let tool = tools.get(name).expect("the tool is in the set").clone();
        tool.call(&self.ctx, args).await
    }

    async fn requests(&self, method: &str) -> Vec<wiremock::Request> {
        self.fx
            .github
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.method.as_str() == method)
            .collect()
    }
}

#[tokio::test]
async fn a_pull_request_is_merged_through_github_with_the_method_and_the_head_it_read() {
    let rig = Rig::new(Fixture::new("hello\n").await);
    common::mount_mergeable(&rig.fx.github, "octo/widgets", 7, "agent/x", "9f8e7d").await;
    rig.say("merge octo/widgets").await;
    rig.pushed("octo/widgets", "agent/x").await;
    let out = rig
        .call(
            "merge_pull_request",
            json!({"repo": "octo/widgets", "number": 7, "method": "rebase", "delete_branch": true}),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content
            .starts_with("Merged pull request #7 of octo/widgets (rebase)")
            && out.content.contains("commit: 9f8e7d")
            && out
                .content
                .contains("pull request: https://github.com/octo/widgets/pull/7")
            && out.content.contains("branch deleted: agent/x"),
        "{}",
        out.content
    );
    let puts = rig.requests("PUT").await;
    assert_eq!(puts.len(), 1);
    let body: Value = serde_json::from_slice(&puts[0].body).unwrap();
    assert_eq!(body, json!({"merge_method": "rebase", "sha": "abc123"}));
    assert_eq!(rig.requests("DELETE").await.len(), 1);

    // The default is a squash, and the branch stays unless asked.
    let rig = Rig::new(Fixture::new("hello\n").await);
    common::mount_mergeable(&rig.fx.github, "octo/widgets", 7, "agent/x", "1").await;
    rig.say("merge octo/widgets").await;
    rig.pushed("octo/widgets", "agent/x").await;
    let out = rig
        .call(
            "merge_pull_request",
            json!({"repo": "octo/widgets", "number": 7}),
        )
        .await
        .unwrap();
    assert!(
        out.content
            .starts_with("Merged pull request #7 of octo/widgets (squash)"),
        "{}",
        out.content
    );
    let body: Value = serde_json::from_slice(&rig.requests("PUT").await[0].body).unwrap();
    assert_eq!(body["merge_method"], "squash");
    assert!(rig.requests("DELETE").await.is_empty());
}

#[tokio::test]
async fn what_github_refuses_is_the_tools_error_in_githubs_words_with_the_class() {
    for (status, said, class) in [
        (405, "Required status check \"ci\" is expected.", "rejected"),
        (
            409,
            "Head branch was modified. Review and try the merge again.",
            "rejected",
        ),
        (
            403,
            "Resource not accessible by integration",
            "unauthenticated",
        ),
    ] {
        let rig = Rig::new(Fixture::new("hello\n").await);
        common::mount_mergeable(&rig.fx.github, "octo/widgets", 7, "agent/x", "1").await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/repos/octo/widgets/pulls/7/merge",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(status).set_body_json(json!({"message": said})),
            )
            .with_priority(1)
            .mount(&rig.fx.github)
            .await;
        rig.say("merge octo/widgets").await;
        rig.pushed("octo/widgets", "agent/x").await;
        let out = rig
            .call(
                "merge_pull_request",
                json!({"repo": "octo/widgets", "number": 7}),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains(said)
                && out.content.contains(&format!("(class: {class})"))
                && out.content.contains("Do not look for another way"),
            "{}",
            out.content
        );
        if status == 403 {
            assert!(
                out.content
                    .contains("Pull requests: write and Contents: write"),
                "{}",
                out.content
            );
        }
        // The secret of the process is not in what the model reads.
        assert!(!out.content.contains(common::GITHUB_TOKEN));
    }
}

#[tokio::test]
async fn only_a_pull_request_of_a_repository_the_person_gave_the_run_is_merged() {
    let rig = Rig::new(Fixture::new("hello\n").await);
    common::mount_mergeable(&rig.fx.github, "octo/widgets", 7, "agent/x", "1").await;
    // Nobody named it: refused before GitHub is asked.
    let out = rig
        .call(
            "merge_pull_request",
            json!({"repo": "octo/widgets", "number": 7}),
        )
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("is not one to merge"),
        "{}",
        out.content
    );
    // Not a repository at all, a bad method, and a host the workspace does not allow.
    for (args, needle) in [
        (
            json!({"repo": "widgets", "number": 7}),
            "is not a repository",
        ),
        (
            json!({"repo": "octo/widgets", "number": 7, "method": "fast-forward"}),
            "is not a merge method",
        ),
        (
            json!({"repo": "https://evil.example/octo/widgets", "number": 7}),
            "cannot be used here",
        ),
    ] {
        let out = rig.call("merge_pull_request", args.clone()).await.unwrap();
        assert!(
            out.is_error && out.content.contains(needle),
            "{args}: {}",
            out.content
        );
    }
    assert!(
        rig.requests("PUT").await.is_empty(),
        "GitHub was never asked"
    );
}

/// The mock GitHub with pull request 7 of `octo/widgets` from `agent/x`, whose head is `abc123`,
/// and the run having pushed that branch: what a merge of the pull request the run opened is.
async fn rig_with_a_pull_request_of_the_run(state: &str) -> Rig {
    let rig = Rig::new(Fixture::new("hello\n").await);
    common::mount_pull(
        &rig.fx.github,
        "octo/widgets",
        7,
        "agent/x",
        "abc123",
        state,
    )
    .await;
    common::mount_merge(&rig.fx.github, "octo/widgets", 7, "agent/x", "9f8e7d").await;
    rig.say("work on octo/widgets").await;
    rig.pushed("octo/widgets", "agent/x").await;
    rig
}

fn green() -> (Value, Value) {
    (
        json!({"state": "success", "total_count": 1}),
        json!({"total_count": 1, "check_runs": [
            {"name": "build", "status": "completed", "conclusion": "success"}
        ]}),
    )
}

/// Rule 1: a pull request is merged when this conversation pushed its branch, or the person named it.
#[tokio::test]
async fn a_pull_request_nobody_here_pushed_and_the_person_did_not_name_is_not_merged() {
    // A pull request of somebody else's branch.
    let rig = Rig::new(Fixture::new("hello\n").await);
    common::mount_pull(
        &rig.fx.github,
        "octo/widgets",
        7,
        "feature/theirs",
        "abc123",
        "clean",
    )
    .await;
    common::mount_merge(&rig.fx.github, "octo/widgets", 7, "feature/theirs", "1").await;
    rig.say("work on octo/widgets").await;
    rig.pushed("octo/widgets", "agent/x").await;
    let out = rig.merge().await;
    assert!(
        out.is_error
            && out.content.contains("is not one to merge")
            && out.content.contains("feature/theirs")
            && out.content.contains("ask them to say"),
        "{}",
        out.content
    );
    assert!(rig.requests("PUT").await.is_empty());
    // The person names the pull request, by its number in a sentence that says to merge it: now it is.
    rig.say("merge pull request 7 please").await;
    let out = rig.merge().await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(rig.requests("PUT").await.len(), 1);

    // Naming another number, another repository's pull request or a repository alone is not naming it.
    for words in [
        "merge #8",
        "merge https://github.com/other/widgets/pull/7",
        "merge octo/widgets",
        "merge the one with 7 files",
    ] {
        let rig = Rig::new(Fixture::new("hello\n").await);
        common::mount_pull(
            &rig.fx.github,
            "octo/widgets",
            7,
            "feature/theirs",
            "abc123",
            "clean",
        )
        .await;
        rig.say(&format!("work on octo/widgets, {words}")).await;
        let out = rig.merge().await;
        assert!(
            out.is_error && out.content.contains("is not one to merge"),
            "{words}: {}",
            out.content
        );
        assert!(rig.requests("PUT").await.is_empty(), "{words}");
    }
    // The URL of the pull request names it, and says which repository's.
    let rig = Rig::new(Fixture::new("hello\n").await);
    common::mount_pull(
        &rig.fx.github,
        "octo/widgets",
        7,
        "feature/theirs",
        "abc123",
        "clean",
    )
    .await;
    common::mount_merge(&rig.fx.github, "octo/widgets", 7, "feature/theirs", "1").await;
    rig.say("merge https://github.com/octo/widgets/pull/7")
        .await;
    assert!(!rig.merge().await.is_error);
}

#[tokio::test]
async fn a_branch_of_a_fork_with_the_name_of_a_pushed_branch_is_not_the_one_pushed() {
    let rig = Rig::new(Fixture::new("hello\n").await);
    common::mount_pull_from(
        &rig.fx.github,
        "octo/widgets",
        7,
        "agent/x",
        "stranger/widgets",
        "abc123",
        "clean",
    )
    .await;
    rig.say("work on octo/widgets").await;
    rig.pushed("octo/widgets", "agent/x").await;
    let out = rig.merge().await;
    assert!(
        out.is_error && out.content.contains("a fork"),
        "{}",
        out.content
    );
    assert!(rig.requests("PUT").await.is_empty());
}

/// Rule 2: the checks of the head commit passed, and that commit is the one merged.
#[tokio::test]
async fn a_pull_request_whose_checks_did_not_pass_is_not_merged() {
    let failing = (
        json!({"state": "success", "total_count": 1}),
        json!({"total_count": 1, "check_runs": [
            {"name": "build", "status": "completed", "conclusion": "failure"}
        ]}),
    );
    let running = (
        json!({"state": "pending", "total_count": 0}),
        json!({"total_count": 1, "check_runs": [
            {"name": "build", "status": "in_progress", "conclusion": null}
        ]}),
    );
    let none = (
        json!({"state": "pending", "total_count": 0}),
        json!({"total_count": 0, "check_runs": []}),
    );
    for (state, (status, runs), said) in [
        ("blocked", failing, "did not pass: build: failure"),
        ("blocked", running, "have not finished: build (in_progress)"),
        ("unstable", none, "No check passed"),
    ] {
        let rig = rig_with_a_pull_request_of_the_run(state).await;
        common::mount_checks(&rig.fx.github, "octo/widgets", "abc123", status, runs).await;
        let out = rig.merge().await;
        assert!(
            out.is_error && out.content.contains(said),
            "{said}: {}",
            out.content
        );
        assert!(rig.requests("PUT").await.is_empty(), "{said}");
    }
    // Everything green, whatever the merge state says: merged.
    let rig = rig_with_a_pull_request_of_the_run("blocked").await;
    let (status, runs) = green();
    common::mount_checks(&rig.fx.github, "octo/widgets", "abc123", status, runs).await;
    let out = rig.merge().await;
    assert!(!out.is_error, "{}", out.content);
    let body: Value = serde_json::from_slice(&rig.requests("PUT").await[0].body).unwrap();
    assert_eq!(
        body["sha"], "abc123",
        "the commit that was checked is the one merged"
    );
}

#[tokio::test]
async fn a_head_that_moved_after_the_checks_were_read_is_not_merged() {
    let rig = Rig::new(Fixture::new("hello\n").await);
    rig.say("work on octo/widgets").await;
    rig.pushed_sha("octo/widgets", "agent/x", "aaa").await;
    let github = &rig.fx.github;
    let (status, runs) = green();
    common::mount_checks(github, "octo/widgets", "aaa", status, runs).await;
    // The first read of the pull request (the checks') shows `aaa`; the next (the merge's) `bbb`.
    let pull = |sha: &str| {
        wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "number": 7, "html_url": common::pull_url(7), "state": "open", "merged": false,
            "mergeable_state": "clean",
            "head": {"ref": "agent/x", "sha": sha, "repo": {"full_name": "octo/widgets"}},
            "base": {"ref": "main", "repo": {"full_name": "octo/widgets"}},
        }))
    };
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/repos/octo/widgets"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(json!({"full_name": "octo/widgets"})),
        )
        .mount(github)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/repos/octo/widgets/pulls/7"))
        .respond_with(pull("aaa"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(github)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/repos/octo/widgets/pulls/7"))
        .respond_with(pull("bbb"))
        .with_priority(2)
        .mount(github)
        .await;
    common::mount_merge(github, "octo/widgets", 7, "agent/x", "1").await;
    let out = rig.merge().await;
    assert!(
        out.is_error
            && out.content.contains("(class: rejected)")
            && out.content.contains("bbb")
            && out.content.contains("aaa"),
        "{}",
        out.content
    );
    assert!(rig.requests("PUT").await.is_empty());
}

/// Rule 3: a pull request opened with red checks accepted needs the person to say, in a later message,
/// to merge it.
#[tokio::test]
async fn a_pull_request_opened_with_red_checks_needs_a_fresh_yes_to_be_merged() {
    let red = || {
        (
            json!({"state": "success", "total_count": 1}),
            json!({"total_count": 1, "check_runs": [
                {"name": "build", "status": "completed", "conclusion": "failure"}
            ]}),
        )
    };
    // Accepted red, and the checks are red: no merge until the person says so.
    let rig = rig_with_a_pull_request_of_the_run("blocked").await;
    let (status, runs) = red();
    common::mount_checks(&rig.fx.github, "octo/widgets", "abc123", status, runs).await;
    rig.opened_red("octo/widgets", 7).await;
    let out = rig.merge().await;
    assert!(
        out.is_error && out.content.contains("not for merging it"),
        "{}",
        out.content
    );
    // A message that does not name the pull request, or does not say to merge, is not that yes.
    for words in [
        "ok, go on",
        "merge it",
        "what about #7?",
        "do not merge #7",
        "fix the merge conflict in #7",
        "merged #7 already?",
    ] {
        rig.say(words).await;
        let out = rig.merge().await;
        assert!(out.is_error, "{words}: {}", out.content);
    }
    assert!(rig.requests("PUT").await.is_empty());
    // Newest message, named and asked: the fresh yes. The red checks no longer stop it.
    rig.say("yes, merge #7").await;
    let out = rig.merge().await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(rig.requests("PUT").await.len(), 1);
    // A later message that does not repeat it takes the yes away again.
    rig.say("thanks!").await;
    let out = rig.merge().await;
    assert!(out.is_error, "{}", out.content);

    // Opened with red accepted, green now: still needs the person's word.
    let rig = rig_with_a_pull_request_of_the_run("blocked").await;
    let (status, runs) = green();
    common::mount_checks(&rig.fx.github, "octo/widgets", "abc123", status, runs).await;
    rig.opened_red("octo/widgets", 7).await;
    assert!(rig.merge().await.is_error);
    rig.say("merge pull request 7").await;
    assert!(!rig.merge().await.is_error);

    // Checks that have not finished are waited for, whatever the person said.
    let rig = rig_with_a_pull_request_of_the_run("blocked").await;
    common::mount_checks(
        &rig.fx.github,
        "octo/widgets",
        "abc123",
        json!({"state": "pending", "total_count": 0}),
        json!({"total_count": 1, "check_runs": [
            {"name": "build", "status": "queued", "conclusion": null}
        ]}),
    )
    .await;
    rig.opened_red("octo/widgets", 7).await;
    rig.say("merge #7").await;
    let out = rig.merge().await;
    assert!(
        out.is_error && out.content.contains("have not finished"),
        "{}",
        out.content
    );

    // A pull request that was not opened red is not merged on red checks because the person asks.
    let rig = rig_with_a_pull_request_of_the_run("blocked").await;
    let (status, runs) = red();
    common::mount_checks(&rig.fx.github, "octo/widgets", "abc123", status, runs).await;
    rig.say("merge #7").await;
    let out = rig.merge().await;
    assert!(
        out.is_error && out.content.contains("did not pass"),
        "{}",
        out.content
    );
}

/// The pull request must be one whose head is a commit **this conversation pushed**, not just a
/// branch of a name it used.
#[tokio::test]
async fn a_branch_somebody_pushed_to_since_is_not_the_conversations() {
    let rig = Rig::new(Fixture::new("hello\n").await);
    common::mount_pull(
        &rig.fx.github,
        "octo/widgets",
        7,
        "agent/x",
        "f00d",
        "clean",
    )
    .await;
    common::mount_merge(&rig.fx.github, "octo/widgets", 7, "agent/x", "1").await;
    rig.say("work on octo/widgets").await;
    // The branch is the run's, but the commit the pull request has now (`f00d`) is not one it pushed.
    rig.pushed_sha("octo/widgets", "agent/x", "abc123").await;
    let out = rig.merge().await;
    assert!(
        out.is_error && out.content.contains("somebody pushed to it since"),
        "{}",
        out.content
    );
    assert!(rig.requests("PUT").await.is_empty());
    rig.pushed_sha("octo/widgets", "agent/x", "f00d").await;
    assert!(!rig.merge().await.is_error);
}

/// A pull request nothing ever checked (no status, no check run) is the host's `clean` and no more:
/// the run's own checks must have passed on that commit, unless the person named the pull request.
#[tokio::test]
async fn a_pull_request_no_ci_ever_checked_needs_the_checks_the_run_ran() {
    let no_ci = (
        json!({"state": "pending", "total_count": 0}),
        json!({"total_count": 0, "check_runs": []}),
    );
    // The conversation's own pull request: the run's checks passed on the commit, or it is not merged.
    let rig = Rig::new(Fixture::new("hello\n").await);
    common::mount_pull(
        &rig.fx.github,
        "octo/widgets",
        7,
        "agent/x",
        "abc123",
        "clean",
    )
    .await;
    common::mount_merge(&rig.fx.github, "octo/widgets", 7, "agent/x", "1").await;
    common::mount_checks(
        &rig.fx.github,
        "octo/widgets",
        "abc123",
        no_ci.0.clone(),
        no_ci.1.clone(),
    )
    .await;
    rig.say("work on octo/widgets").await;
    rig.pushed("octo/widgets", "agent/x").await;
    let out = rig.merge().await;
    assert!(
        out.is_error && out.content.contains("Nothing has checked the head"),
        "{}",
        out.content
    );
    assert!(rig.requests("PUT").await.is_empty());
    // `open_pull_request` let this commit in because the run's checks passed on it.
    let run = rig.ctx.run_id().to_string();
    let mut notes = rig.fx.env.notes.load(&run).await.unwrap();
    notes.verify_sha("abc123");
    rig.fx.env.notes.save(&run, &notes).await.unwrap();
    assert!(!rig.merge().await.is_error);

    // One the person named, with its repository in full: the host's word is theirs to rely on.
    let rig = Rig::new(Fixture::new("hello\n").await);
    common::mount_pull(
        &rig.fx.github,
        "octo/widgets",
        7,
        "feature/theirs",
        "abc123",
        "clean",
    )
    .await;
    common::mount_merge(&rig.fx.github, "octo/widgets", 7, "feature/theirs", "1").await;
    common::mount_checks(&rig.fx.github, "octo/widgets", "abc123", no_ci.0, no_ci.1).await;
    rig.say("merge https://github.com/octo/widgets/pull/7")
        .await;
    assert!(!rig.merge().await.is_error);
}

/// Checks of a pull request that was not opened red must pass: nobody is told to ask for a yes that
/// the code would not honour.
#[tokio::test]
async fn failing_checks_of_a_pull_request_not_opened_red_are_not_a_question_for_the_person() {
    let rig = rig_with_a_pull_request_of_the_run("blocked").await;
    common::mount_checks(
        &rig.fx.github,
        "octo/widgets",
        "abc123",
        json!({"state": "success", "total_count": 1}),
        json!({"total_count": 1, "check_runs": [
            {"name": "build", "status": "completed", "conclusion": "failure"}
        ]}),
    )
    .await;
    rig.say("merge #7 now, whatever the checks say").await;
    let out = rig.merge().await;
    assert!(
        out.is_error
            && out.content.contains("did not pass: build: failure")
            && out.content.contains("merged only when they pass")
            && !out.content.contains("new message"),
        "{}",
        out.content
    );
}

/// A slot that came from a bare name, and a bare number, are no grant for a merge.
#[tokio::test]
async fn a_bare_name_and_a_bare_number_do_not_make_a_pull_request_one_to_merge() {
    let rig = Rig::new(Fixture::new("hello\n").await.acting_for(&["octo"]));
    common::mount_pull(
        &rig.fx.github,
        "octo/widgets",
        7,
        "feature/theirs",
        "abc123",
        "clean",
    )
    .await;
    common::mount_merge(&rig.fx.github, "octo/widgets", 7, "feature/theirs", "1").await;
    rig.tier(AccessTier::Auto).await;
    // `widgets` alone is a bare name (a workspace may hold it), `#7` alone is bound to no repository.
    rig.say("fix widgets and merge #7").await;
    let out = rig.merge().await;
    assert!(
        out.is_error && out.content.contains("is not one to merge"),
        "{}",
        out.content
    );
    assert!(rig.requests("PUT").await.is_empty());
}

/// With no notes of the run nothing was recorded, so it is a plan: the tools check it themselves.
#[tokio::test]
async fn the_admin_tools_take_missing_notes_for_a_plan() {
    let rig = Rig::new(Fixture::new("hello\n").await);
    assert!(
        rig.fx
            .env
            .notes
            .load_existing(&rig.ctx.run_id().to_string())
            .await
            .unwrap()
            .is_none()
    );
    for (tool, args, needs) in [
        (
            &adam_coder::tools::admin::MergePullRequest as &dyn adam_llm_agent::Tool,
            json!({"repo": "octo/widgets", "number": 7}),
            "auto",
        ),
        (
            &adam_coder::tools::admin::RenameRepository,
            json!({"repo": "octo/widgets", "new_name": "gadgets"}),
            "bypass",
        ),
        (
            &adam_coder::tools::admin::DeleteRepository,
            json!({"repo": "octo/widgets", "confirm": "octo/widgets"}),
            "bypass",
        ),
    ] {
        let out = direct(&rig, tool, args).await;
        assert!(
            out.is_error
                && out
                    .content
                    .contains(&format!("needs the {needs} access tier"))
                && out.content.contains("this message is under plan"),
            "{}",
            out.content
        );
    }
    assert!(rig.fx.github.received_requests().await.unwrap().is_empty());
}

/// A call of `tool` itself, without the set's own check of the tier (`Gated`).
async fn direct(rig: &Rig, tool: &dyn adam_llm_agent::Tool, args: Value) -> ToolOutput {
    tool.call(&rig.ctx, args).await.unwrap()
}

/// The tool checks the tier itself, from the notes, whoever called it: below `auto` nothing is read
/// from GitHub, and the code host is never asked.
#[tokio::test]
async fn the_admin_tools_check_the_tier_in_their_own_body() {
    let rig = rig_with_a_pull_request_of_the_run("clean").await;
    rig.tier(AccessTier::Edits).await;
    let merge = direct(
        &rig,
        &adam_coder::tools::admin::MergePullRequest,
        json!({"repo": "octo/widgets", "number": 7}),
    )
    .await;
    assert!(
        merge.is_error && merge.content.contains("needs the auto access tier"),
        "{}",
        merge.content
    );
    rig.tier(AccessTier::Auto).await;
    for (tool, args) in [
        (
            &adam_coder::tools::admin::RenameRepository as &dyn adam_llm_agent::Tool,
            json!({"repo": "octo/widgets", "new_name": "gadgets"}),
        ),
        (
            &adam_coder::tools::admin::DeleteRepository,
            json!({"repo": "octo/widgets", "confirm": "octo/widgets"}),
        ),
    ] {
        let out = direct(&rig, tool, args).await;
        assert!(
            out.is_error && out.content.contains("needs the bypass access tier"),
            "{}",
            out.content
        );
    }
    assert!(rig.requests("PUT").await.is_empty());
    assert!(rig.requests("PATCH").await.is_empty() && rig.requests("DELETE").await.is_empty());
    // And the deployment's cap is above the tier the notes hold.
    let rig = Rig::new(Fixture::new("hello\n").await.capped_at(AccessTier::Auto));
    rig.tier(AccessTier::Bypass).await;
    rig.say("delete octo/widgets").await;
    let out = rig
        .call(
            "delete_repository",
            json!({"repo": "octo/widgets", "confirm": "octo/widgets"}),
        )
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("needs the bypass access tier"),
        "{}",
        out.content
    );
}

/// A repository on another host than the code host's is not one these tools act on.
#[tokio::test]
async fn nothing_is_merged_renamed_or_deleted_on_another_host_than_the_code_hosts() {
    let rig = Rig::new(
        Fixture::new("hello\n")
            .await
            .also_allowing(&["ghe.example.com"]),
    );
    rig.tier(AccessTier::Bypass).await;
    rig.say("merge https://ghe.example.com/octo/widgets/pull/7 and drop https://ghe.example.com/octo/widgets").await;
    for (tool, args) in [
        (
            "merge_pull_request",
            json!({"repo": "https://ghe.example.com/octo/widgets", "number": 7}),
        ),
        (
            "rename_repository",
            json!({"repo": "https://ghe.example.com/octo/widgets", "new_name": "x"}),
        ),
        (
            "delete_repository",
            json!({"repo": "https://ghe.example.com/octo/widgets", "confirm": "octo/widgets"}),
        ),
    ] {
        let out = rig.call(tool, args).await.unwrap();
        assert!(
            out.is_error && out.content.contains("is not on github.com"),
            "{tool}: {}",
            out.content
        );
    }
    assert!(rig.fx.github.received_requests().await.unwrap().is_empty());
}

/// An address that GitHub redirects (the repository was renamed or transferred) is not acted on.
#[tokio::test]
async fn a_repository_that_moved_is_not_merged_or_deleted_through_its_old_address() {
    let rig = Rig::new(Fixture::new("hello\n").await);
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/repos/octo/widgets"))
        .respond_with(
            wiremock::ResponseTemplate::new(301).insert_header("location", "/repositories/42"),
        )
        .mount(&rig.fx.github)
        .await;
    wiremock::Mock::given(wiremock::matchers::path("/repositories/42"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "stranger/widgets"
        })))
        .expect(0)
        .mount(&rig.fx.github)
        .await;
    rig.say("merge pull request 7 of octo/widgets").await;
    let out = rig.merge().await;
    assert!(
        out.is_error
            && out.content.contains("has moved")
            && out.content.contains("not the repository that was named"),
        "{}",
        out.content
    );
    assert!(rig.requests("PUT").await.is_empty());
}

// -------------------------------------------------------------------------- rename and delete

#[tokio::test]
async fn a_repository_is_deleted_only_when_named_and_confirmed_and_once() {
    let (fx, host) = with_memory_host().await;
    let rig = Rig::new(fx);
    rig.tier(AccessTier::Bypass).await;
    let delete = |confirm: &'static str| {
        rig.call(
            "delete_repository",
            json!({"repo": "octo/widgets", "confirm": confirm}),
        )
    };
    // Nobody named it.
    let out = delete("octo/widgets").await.unwrap();
    assert!(
        out.is_error && out.content.contains("is not a repository the person named"),
        "{}",
        out.content
    );
    rig.say("delete octo/widgets please").await;
    // The guard: `confirm` is another repository, or half of this one, or nothing.
    for wrong in ["octo/other", "widgets", "", "octo/widgets-2"] {
        let out = delete(wrong).await.unwrap();
        assert!(
            out.is_error
                && out
                    .content
                    .contains("Nothing was deleted: `confirm` must be `octo/widgets`"),
            "{wrong:?}: {}",
            out.content
        );
    }
    assert!(host.deletions().is_empty(), "the guard held");
    // Confirmed (without regard to case): deleted, and a repeat is the same answer.
    let out = delete("Octo/Widgets").await.unwrap();
    assert!(
        !out.is_error && out.content.starts_with("Deleted octo/widgets"),
        "{}",
        out.content
    );
    let again = delete("octo/widgets").await.unwrap();
    assert_eq!(again.content, out.content);
    assert_eq!(host.deletions(), ["https://github.com/octo/widgets"]);
}

#[tokio::test]
async fn githubs_refusal_of_a_rename_or_a_delete_is_reported_in_its_words() {
    let host =
        Arc::new(MemoryCodeHost::new().refusing_changes("Must have admin rights to Repository."));
    let dyn_host: DynCodeHost = host.clone();
    let rig = Rig::new(Fixture::new("hello\n").await.hosted_by(dyn_host));
    rig.tier(AccessTier::Bypass).await;
    rig.say("octo/widgets").await;
    for (name, args, permission) in [
        (
            "delete_repository",
            json!({"repo": "octo/widgets", "confirm": "octo/widgets"}),
            "Administration: write",
        ),
        (
            "rename_repository",
            json!({"repo": "octo/widgets", "new_name": "gadgets"}),
            "Administration: write",
        ),
    ] {
        let out = rig.call(name, args).await.unwrap();
        assert!(out.is_error, "{name}: {}", out.content);
        assert!(
            out.content
                .contains("Must have admin rights to Repository.")
                && out.content.contains("(class: unauthenticated)")
                && out.content.contains(permission),
            "{name}: {}",
            out.content
        );
    }
    assert!(host.deletions().is_empty() && host.renames().is_empty());
}

#[tokio::test]
async fn a_repository_is_renamed_to_a_valid_new_name_and_the_new_address_is_granted() {
    let (fx, host) = with_memory_host().await;
    let rig = Rig::new(fx);
    rig.tier(AccessTier::Bypass).await;
    rig.say("rename octo/widgets").await;
    for (new_name, needle) in [
        ("", "new_name is required"),
        ("a/b", "is not a repository name"),
        ("x.git", "is not a repository name"),
        ("Widgets", "is called Widgets already"),
    ] {
        let out = rig
            .call(
                "rename_repository",
                json!({"repo": "octo/widgets", "new_name": new_name}),
            )
            .await
            .unwrap();
        assert!(
            out.is_error && out.content.contains(needle),
            "{new_name:?}: {}",
            out.content
        );
    }
    assert!(host.renames().is_empty());
    let out = rig
        .call(
            "rename_repository",
            json!({"repo": "octo/widgets", "new_name": "gadgets"}),
        )
        .await
        .unwrap();
    assert!(
        !out.is_error && out.content.starts_with("Renamed to octo/gadgets"),
        "{}",
        out.content
    );
    let again = rig
        .call(
            "rename_repository",
            json!({"repo": "octo/widgets", "new_name": "gadgets"}),
        )
        .await
        .unwrap();
    assert_eq!(again.content, out.content, "a repeat is the same answer");
    assert_eq!(host.renames().len(), 1);
    // The run may work on it under its new name now.
    let notes = rig
        .fx
        .env
        .notes
        .load(&rig.ctx.run_id().to_string())
        .await
        .unwrap();
    assert_eq!(notes.renamed_repos.len(), 1);
}

// ---------------------------------------------------------------------------- creating

/// A rig for a GitHub App installed on `accounts`, with no `CREATE_REPO_OWNERS`, whose host creates
/// repositories for the organisation `acme`.
async fn app_rig(accounts: &[&str]) -> (Rig, Arc<common::CreatingHost>) {
    let (fx, host) =
        Fixture::new("hello\n")
            .await
            .acting_for(accounts)
            .creating(&[], &["acme"], None);
    let fx = fx.tweaked(|s| s.create_for_installed_owners = true);
    (Rig::new(fx), host)
}

#[tokio::test]
async fn with_no_owner_listed_a_github_app_creates_for_the_accounts_it_is_installed_on() {
    let (rig, host) = app_rig(&["acme"]).await;
    // On by default: the tool is in the set with no CREATE_REPO_OWNERS at all.
    assert!(coder_tools(&rig.fx.env).get("create_repository").is_some());
    assert!(rig.fx.env.settings.create_repo_owners.is_empty());
    // Under auto the person is still asked (the form stays), for an account the App is on.
    let asked = rig
        .call("create_repository", json!({"owner": "acme", "name": "fib"}))
        .await;
    assert!(
        matches!(asked, Err(ToolError::NeedsInput { .. })),
        "{asked:?}"
    );
    // And never for another account.
    let out = rig
        .call("create_repository", json!({"owner": "evil", "name": "fib"}))
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("only for: acme"),
        "{}",
        out.content
    );
    assert!(host.created().is_empty());
}

#[tokio::test]
async fn a_list_still_restricts_a_github_app_and_a_token_creates_nothing_without_one() {
    // The App is on two accounts, the deployment lists one: a list restricts.
    let (fx, host) = Fixture::new("hello\n")
        .await
        .acting_for(&["acme", "other"])
        .creating(&["acme"], &["acme", "other"], None);
    let rig = Rig::new(fx.tweaked(|s| s.create_for_installed_owners = true));
    let out = rig
        .call(
            "create_repository",
            json!({"owner": "other", "name": "fib"}),
        )
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("only for: acme"),
        "{}",
        out.content
    );
    assert!(host.created().is_empty());

    // A token (no list of accounts, no owners named): the tool is not offered, and refuses.
    let (fx, _) = Fixture::new("hello\n").await.creating(&[], &["acme"], None);
    assert!(!fx.env.settings.can_create_repositories());
    assert!(coder_tools(&fx.env).get("create_repository").is_none());
    let rig = Rig::new(fx);
    let out = adam_coder::tools::create::CreateRepository;
    use adam_llm_agent::Tool as _;
    let out = out
        .call(&rig.ctx, json!({"owner": "acme", "name": "fib"}))
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("switched off"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn bypass_creates_with_no_question_and_auto_asks() {
    let (rig, host) = app_rig(&["acme"]).await;
    rig.tier(AccessTier::Bypass).await;
    let out = rig
        .call(
            "create_repository",
            json!({"owner": "acme", "name": "fib", "private": false}),
        )
        .await
        .unwrap();
    assert!(
        !out.is_error && out.content.starts_with("Created acme/fib (public, empty"),
        "{}",
        out.content
    );
    assert_eq!(host.created().len(), 1, "no form: it was made at once");

    let (rig, host) = app_rig(&["acme"]).await;
    rig.tier(AccessTier::Auto).await;
    let asked = rig
        .call("create_repository", json!({"owner": "acme", "name": "fib"}))
        .await;
    assert!(
        matches!(asked, Err(ToolError::NeedsInput { .. })),
        "{asked:?}"
    );
    assert!(host.created().is_empty());
}

// ----------------------------------------------------------------------------------- A2A

fn a2a_message(text: &str, id: &str, tier: Option<&str>) -> A2aMessage {
    let mut m = A2aMessage::new(Role::User, vec![Part::text(text)]);
    m.message_id = id.into();
    if let Some(tier) = tier {
        m.metadata = Some(std::collections::HashMap::from([(
            ACCESS_TIER_EXTENSION.to_owned(),
            json!({ "tier": tier }),
        )]));
    }
    m
}

/// The coder's own backend: a bad tier is refused before a task is made, a good one is the run's
/// tier and is said on the task, and the card that goes with it lists the extension: the tiers up to
/// the cap of the deployment (`MAX_ACCESS_TIER`), `auto` unless it says more.
#[tokio::test]
async fn the_coders_backend_refuses_a_bad_tier_and_says_the_tier_a_task_works_under() {
    let fx = Fixture::new("hello\n").await;
    let model: DynModel = Arc::new(MockModel::new());
    let agent = CoderAgent::new(model, "test-model", fx.env.clone());
    let coder = Coder::new(
        Arc::new(MemoryStore::new()),
        agent,
        &RuntimeOptions::default(),
    );
    let activated = Caller::new("token-0").with_extensions([ACCESS_TIER_EXTENSION]);
    for (tier, id) in [
        (None, "m-none"),
        (Some("root"), "m-root"),
        (Some("Plan"), "m-case"),
    ] {
        let err = coder
            .backend
            .submit(activated.clone(), a2a_message("hi", id, tier), None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, BackendError::InvalidParams(_)),
            "{tier:?}: {err:?}"
        );
    }
    let task = coder
        .backend
        .submit(
            activated.clone(),
            a2a_message("hi", "m-ok", Some("plan")),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        task.metadata
            .as_ref()
            .and_then(|m| m.get(ACCESS_TIER_EXTENSION)),
        Some(&json!({"tier": "plan"}))
    );
    // This deployment lets a person pick any tier; the card says so.
    let url: url::Url = "https://agents.example.com/coder/".parse().unwrap();
    let tiers_of = |card: &adam_a2a::AgentCardConfig| {
        let entry = card
            .extensions
            .iter()
            .find(|e| e.uri == ACCESS_TIER_EXTENSION)
            .expect("the card lists access-tier/v1");
        assert!(!entry.required);
        Value::Object(entry.params.clone())
    };
    let card =
        adam_coder::agent_card_for(&adam_coder::AgentFiles::Embedded, &url, AccessTier::Bypass)
            .unwrap();
    assert_eq!(
        tiers_of(&card),
        json!({"tiers": ["plan", "edits", "auto", "bypass"], "default": "auto"})
    );
    coder
        .backend
        .submit(
            activated.clone(),
            a2a_message("hi", "m-bypass", Some("bypass")),
            None,
            None,
        )
        .await
        .unwrap();
}

/// With no `MAX_ACCESS_TIER` the coder honours up to `auto`: the card lists three tiers and a message
/// that names `bypass` is refused before anything is written.
#[tokio::test]
async fn by_default_bypass_is_the_deployments_opt_in_and_not_a_tier_a_message_may_name() {
    let fx = Fixture::new("hello\n")
        .await
        .capped_at(adam_coder::tier::DEFAULT_MAX);
    let model: DynModel = Arc::new(MockModel::new());
    let agent = CoderAgent::new(model, "test-model", fx.env.clone());
    let coder = Coder::new(
        Arc::new(MemoryStore::new()),
        agent,
        &RuntimeOptions::default(),
    );
    let activated = Caller::new("token-0").with_extensions([ACCESS_TIER_EXTENSION]);
    let err = coder
        .backend
        .submit(
            activated.clone(),
            a2a_message("hi", "m-bypass", Some("bypass")),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, BackendError::InvalidParams(m) if m.contains("does not take the access tier bypass")),
        "{err:?}"
    );
    coder
        .backend
        .submit(
            activated,
            a2a_message("hi", "m-auto", Some("auto")),
            None,
            None,
        )
        .await
        .unwrap();
    let url: url::Url = "https://agents.example.com/coder/".parse().unwrap();
    let card = adam_coder::agent_card(&url);
    let entry = card
        .extensions
        .iter()
        .find(|e| e.uri == ACCESS_TIER_EXTENSION)
        .expect("the card lists access-tier/v1");
    assert_eq!(
        Value::Object(entry.params.clone()),
        json!({"tiers": ["plan", "edits", "auto"], "default": "auto"})
    );
    // The control plane of such a deployment says the same.
    let control = Coder::control_plane_capped(
        Arc::new(MemoryStore::new()),
        &RuntimeOptions::default(),
        adam_coder::LiveSignals::local(),
        AccessTier::Edits,
    );
    let err = control
        .backend
        .submit(
            Caller::new("token-0").with_extensions([ACCESS_TIER_EXTENSION]),
            a2a_message("hi", "m-auto2", Some("auto")),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, BackendError::InvalidParams(_)), "{err:?}");
}

// ------------------------------------------------------------------------------ the consent form

/// The model asks to add `acme/widgets` to the workspace.
async fn ask(rig: &Rig) -> Result<ToolOutput, ToolError> {
    rig.call(
        "request_repository",
        json!({"repo_url": "https://github.com/acme/widgets", "reason": "to read how it does it"}),
    )
    .await
}

#[tokio::test]
async fn the_form_for_a_repository_the_person_did_not_name_stays_below_auto_and_goes_at_bypass() {
    // A token and no other repository: nobody is the owner of a bare name, so every tier below bypass
    // asks, and bypass asks nothing (the tier is the person's consent).
    for (tier, asks) in [
        (AccessTier::Plan, true),
        (AccessTier::Edits, true),
        (AccessTier::Auto, true),
        (AccessTier::Bypass, false),
    ] {
        let rig = Rig::new(Fixture::new("hello\n").await);
        rig.tier(tier).await;
        rig.say("look at widgets").await;
        let out = ask(&rig).await;
        match (asks, out) {
            (true, Err(ToolError::NeedsInput { .. })) => {}
            (false, Ok(out)) => assert!(out.content.contains("already granted"), "{}", out.content),
            (_, other) => panic!("{tier}: {other:?}"),
        }
    }
    // An App installed on one account, and the person wrote the bare name: from auto up it needs no
    // form, below it still does.
    for (tier, asks) in [
        (AccessTier::Plan, true),
        (AccessTier::Edits, true),
        (AccessTier::Auto, false),
    ] {
        let rig = Rig::new(Fixture::new("hello\n").await.acting_for(&["acme"]));
        rig.tier(tier).await;
        rig.say("look at widgets").await;
        let out = ask(&rig).await;
        assert_eq!(
            matches!(out, Err(ToolError::NeedsInput { .. })),
            asks,
            "{tier}: {out:?}"
        );
    }
    // The bare name has to be the one the person wrote.
    let rig = Rig::new(Fixture::new("hello\n").await.acting_for(&["acme"]));
    rig.tier(AccessTier::Auto).await;
    rig.say("look at gadgets").await;
    assert!(matches!(ask(&rig).await, Err(ToolError::NeedsInput { .. })));
}

/// A piece of a URL, a path or a query is not a bare name, and neither is a word of an earlier
/// message: only a word that stands alone in the person's newest message is.
#[tokio::test]
async fn a_bare_name_is_a_standalone_word_of_the_newest_message_and_never_a_fragment() {
    for words in [
        "look at https://github.com/stranger/widgets/issues/1",
        "look at src/widgets/lib.rs",
        "look at github.com/stranger/widgets",
        "look at git@github.com:stranger/widgets.git",
        "look at https://example.com/?repo=widgets",
    ] {
        let rig = Rig::new(Fixture::new("hello\n").await.acting_for(&["acme"]));
        rig.tier(AccessTier::Auto).await;
        rig.say(words).await;
        assert!(
            matches!(ask(&rig).await, Err(ToolError::NeedsInput { .. })),
            "{words}"
        );
    }
    let rig = Rig::new(Fixture::new("hello\n").await.acting_for(&["acme"]));
    rig.tier(AccessTier::Auto).await;
    rig.say("look at widgets").await;
    assert!(
        !matches!(ask(&rig).await, Err(ToolError::NeedsInput { .. })),
        "the person wrote the name"
    );
    // The next message does not say it again: it is no longer the word of the person's message.
    rig.say("thanks, go on").await;
    assert!(matches!(ask(&rig).await, Err(ToolError::NeedsInput { .. })));
}

// ------------------------------------------------------------------------------ which layer refuses

/// A tool that counts the times its body ran.
struct Counting {
    name: &'static str,
    ran: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for Counting {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "counts".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolOutput::text("ran"))
    }
}

/// A source that offers the tools of an MCP server and counts the calls it answers.
struct Server {
    ran: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolSource for Server {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        ["github__get_issue", "github__create_issue"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                description: "an MCP server's tool".into(),
                parameters: json!({"type": "object", "properties": {}}),
            })
            .collect()
    }

    async fn call(
        &self,
        _ctx: &ToolCtx,
        name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        name.starts_with("github__").then(|| {
            self.ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolOutput::text("answered"))
        })
    }
}

async fn wait_done(rt: &Runtime, run: RunId) -> adam_runtime::RunView {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let view = rt.view(run).await.unwrap().expect("run exists");
        if view.status == RunStatus::Done {
            return view;
        }
        assert!(Instant::now() < deadline, "timed out: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn worker_of(
    rt: &Runtime,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    let rt = rt.clone();
    let handle = tokio::spawn(async move {
        rt.run_worker(async {
            let _ = rx.await;
        })
        .await
        .unwrap();
    });
    (stop, handle)
}

/// The first layer: the tier source refuses the call of a tool above the tier **before the tool's
/// body runs** (the tool here is the agent's own and is not wrapped in the coder's `Gated` check, so
/// only the source can have refused it).
#[tokio::test]
async fn the_source_refuses_a_call_above_the_tier_before_the_tool_runs() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    for (tier, merges) in [
        (AccessTier::Plan, 0),
        (AccessTier::Edits, 0),
        (AccessTier::Auto, 1),
        (AccessTier::Bypass, 1),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let ran = Arc::new(AtomicUsize::new(0));
        let mock = Arc::new(MockModel::new());
        mock.push_tool_calls(vec![call("c1", "merge_pull_request", json!({}))])
            .push_text("done");
        let model: DynModel = mock.clone();
        let agent = LlmAgent::builder("llm", model, "m")
            .tool(Counting {
                name: "merge_pull_request",
                ran: ran.clone(),
            })
            .tool_source(adam_coder::TierSource::new(
                adam_coder::tools::notes::NotesStore::new(dir.path()),
                AccessTier::Bypass,
            ))
            .build();
        let rt = Runtime::builder(Arc::new(MemoryStore::new()))
            .agent(agent)
            .poll_interval(Duration::from_millis(20))
            .build();
        let (stop, worker) = worker_of(&rt);
        let mut first = user_message("merge it");
        first.payload["context"] = json!({CONTEXT_ACCESS_TIER: {"tier": tier.as_str()}});
        let run = rt.start("llm", first, None).await.unwrap();
        wait_done(&rt, run).await;
        let _ = stop.send(());
        worker.await.unwrap();
        assert_eq!(ran.load(SeqCst), merges, "{tier}");
        let requests = mock.requests();
        assert_eq!(
            names(&requests[0]).contains(&"merge_pull_request"),
            merges == 1,
            "{tier}: shown only when the tier allows it"
        );
        let result = result_of(&requests[1], "c1");
        assert_eq!(
            result.contains("`merge_pull_request` needs the auto access tier"),
            merges == 0,
            "{tier}: {result}"
        );
    }
}

/// A message sent while the run works can lower the tier: from the next step on, the tool is not
/// shown and a call to it is refused, whatever the tier was a step before.
#[tokio::test]
async fn a_message_that_lowers_the_tier_refuses_the_next_call() {
    let (fx, host) = with_memory_host().await;
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call(
        "q1",
        "ask_user",
        json!({"question": "Merge it now?"}),
    )])
    .push_tool_calls(vec![call(
        "c-merge",
        "merge_pull_request",
        json!({"repo": "octo/widgets", "number": 1}),
    )])
    .push_text("I may not.");
    let model: DynModel = mock.clone();
    let agent = CoderAgent::new(model, "test-model", fx.env.clone());
    let rt = Runtime::builder(Arc::new(MemoryStore::new()))
        .agent(agent)
        .poll_interval(Duration::from_millis(20))
        .build();
    let (stop, worker) = worker_of(&rt);
    let run = rt
        .start(
            AGENT_NAME,
            tier_message(
                "merge pull request 1 of octo/widgets",
                Some(AccessTier::Auto),
            ),
            None,
        )
        .await
        .unwrap();
    // The run asks and waits.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !rt.view(run).await.unwrap().is_some_and(|v| v.waiting) {
        assert!(Instant::now() < deadline, "the run never asked");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The answer is a message that lowers the tier to plan, and says to merge.
    rt.deliver(
        run,
        tier_message(
            "yes, merge pull request 1 of octo/widgets",
            Some(AccessTier::Plan),
        ),
    )
    .await
    .unwrap();
    wait_done(&rt, run).await;
    let _ = stop.send(());
    worker.await.unwrap();
    let requests = mock.requests();
    // The model was shown merge_pull_request in the first turn (auto) and not in the next (plan).
    assert!(names(&requests[0]).contains(&"merge_pull_request"));
    assert!(!names(&requests[1]).contains(&"merge_pull_request"));
    let result = result_of(&requests[2], "c-merge");
    assert!(
        result.contains("needs the auto access tier")
            && result.contains("this message is under plan"),
        "{result}"
    );
    assert!(host.merges().is_empty(), "the host was never asked");
    let notes = fx.env.notes.load(&run.to_string()).await.unwrap();
    assert_eq!(notes.tier(), AccessTier::Plan);
}

/// A subagent is held to the tier of the root run for the tools of an MCP server too: the root
/// works under plan, and the child may read with the server but not write.
#[tokio::test]
async fn a_subagent_keeps_a_servers_tools_to_the_tier_of_the_root_run() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    struct Spawn;
    #[async_trait::async_trait]
    impl Tool for Spawn {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "explorer".into(),
                description: "starts the kid".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }
        }
        async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
            let run = ctx.start_child("kid", "go").await?;
            Err(ToolError::AwaitRun { run })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let notes = adam_coder::tools::notes::NotesStore::new(dir.path());
    let ran = Arc::new(AtomicUsize::new(0));
    let (top_model, kid_model) = (Arc::new(MockModel::new()), Arc::new(MockModel::new()));
    top_model
        .push_tool_calls(vec![call("t1", "explorer", json!({}))])
        .push_text("top done");
    kid_model
        .push_tool_calls(vec![
            call("k1", "github__get_issue", json!({})),
            call("k2", "github__create_issue", json!({})),
        ])
        .push_text("kid done");
    let agent = |name: &str, model: Arc<MockModel>, spawns: bool| {
        let model: DynModel = model;
        let mut builder = LlmAgent::builder(name, model, "m")
            .tool_source(Server { ran: ran.clone() })
            .tool_source(adam_coder::TierSource::new(
                notes.clone(),
                AccessTier::Bypass,
            ));
        if spawns {
            builder = builder.tool(Spawn);
        }
        builder.build()
    };
    let rt = Runtime::builder(Arc::new(MemoryStore::new()))
        .agent(agent("top", top_model, true))
        .agent(agent("kid", kid_model.clone(), false))
        .poll_interval(Duration::from_millis(20))
        .build();
    let mut first = user_message("go");
    first.payload["context"] = json!({CONTEXT_ACCESS_TIER: {"tier": "plan"}});
    let top = rt.start("top", first, None).await.unwrap();
    // The coder records the tier of the message in the notes of the root run before each step.
    let recorded = adam_coder::tools::notes::RunNotes {
        tier: Some(AccessTier::Plan),
        ..Default::default()
    };
    notes.save(&top.to_string(), &recorded).await.unwrap();
    let (stop, worker) = worker_of(&rt);
    wait_done(&rt, top).await;
    let _ = stop.send(());
    worker.await.unwrap();

    let requests = kid_model.requests();
    // The child is shown the server's read and not its write, and the write is refused.
    assert_eq!(
        names(&requests[0]),
        ["github__get_issue"],
        "the child's context holds the tier of the root"
    );
    assert_eq!(result_of(&requests[1], "k1"), "answered");
    let refused = result_of(&requests[1], "k2");
    assert!(
        refused.contains("`github__create_issue` needs the auto access tier")
            && refused.contains("this message is under plan"),
        "{refused}"
    );
    assert_eq!(ran.load(SeqCst), 1, "only the read reached the server");
}
