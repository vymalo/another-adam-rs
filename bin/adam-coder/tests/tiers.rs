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
use adam_llm_agent::{ToolCtx, ToolError, ToolOutput};
use adam_model::{DynModel, Message, MockModel, ModelRequest, ToolCall};
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
    for tier in [AccessTier::Edits, AccessTier::Auto, AccessTier::Bypass] {
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
}

impl Rig {
    fn new(fx: Fixture) -> Self {
        let ctx = ToolCtx::detached("tool", "call-1", Arc::new(CollectingSink::new()))
            .with_state(fx.env.clone());
        Self { fx, ctx }
    }

    async fn tier(&self, tier: AccessTier) {
        let run = self.ctx.run_id().to_string();
        let mut notes = self.fx.env.notes.load(&run).await.unwrap();
        notes.tier = Some(tier);
        self.fx.env.notes.save(&run, &notes).await.unwrap();
    }

    /// The person says `text`: the repositories it names are named.
    async fn say(&self, text: &str) {
        let run = self.ctx.run_id().to_string();
        let mut notes = self.fx.env.notes.load(&run).await.unwrap();
        notes.name_repos(adam_coder::tools::named::named_in(
            text,
            &self.fx.env.settings.default_repo_host,
        ));
        notes.say_words([text.to_owned()]);
        self.fx.env.notes.save(&run, &notes).await.unwrap();
    }

    async fn call(&self, name: &'static str, args: Value) -> Result<ToolOutput, ToolError> {
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
        out.is_error && out.content.contains("is not a repository the person named"),
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
/// tier and is said on the task, and the card that goes with it lists the extension.
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
    // The card says what the coder honours.
    let url: url::Url = "https://agents.example.com/coder/".parse().unwrap();
    let card = adam_coder::agent_card(&url);
    let entry = card
        .extensions
        .iter()
        .find(|e| e.uri == ACCESS_TIER_EXTENSION)
        .expect("the card lists access-tier/v1");
    assert!(!entry.required);
    assert_eq!(
        Value::Object(entry.params.clone()),
        json!({"tiers": ["plan", "edits", "auto", "bypass"], "default": "auto"})
    );
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
