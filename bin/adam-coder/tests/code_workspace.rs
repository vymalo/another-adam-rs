//! The coder's `ToolEnv` as an `adam_code_tools::CodeWorkspace` (ADR 0047, decision 7): the
//! conformance cases of `adam-code-tools-testkit` over a run whose workspace holds one repository.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use adam::agent_fs::ReferenceDef;
use adam_code_tools::{CodeWorkspace as _, Intent, Place, Target};
use adam_code_tools_testkit::{Harness, detached_ctx};
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_coder::tools::prepare::PrepareWorkspace;
use adam_llm_agent::Tool as _;
use serde_json::json;

use common::Fixture;

/// A run that named the fixture's remote and prepared it (one slot, writable), in a deployment that
/// declares the reference `design` (read, never written).
async fn harness() -> Harness {
    let fx = Fixture::new().await;
    let design = fx.extra_remote("design", &[("tokens.css", "--blue: #00f;\n")]);
    let fx = fx.referencing(vec![ReferenceDef {
        name: "design".into(),
        url: design.to_string_lossy().into_owned(),
        branch: None,
        description: "Tokens the UI follows".into(),
    }]);
    let mut h = prepared(fx).await;
    h.read_only = Some("design".into());
    h
}

async fn prepared(fx: Fixture) -> Harness {
    let ctx = detached_ctx().with_state(fx.env.clone());
    let run = ctx.run_id().to_string();
    let mut notes = fx.env.notes.load(&run).await.unwrap();
    notes.name_repos(named_in(
        &without_untrusted(&fx.remote_url()),
        &fx.env.settings.default_repo_host,
    ));
    fx.env.notes.save(&run, &notes).await.unwrap();
    let prepared = PrepareWorkspace
        .call(
            &ctx,
            json!({"repo_url": fx.remote_url(), "base_branch": "main"}),
        )
        .await
        .unwrap();
    assert!(!prepared.is_error, "{prepared:?}");
    let place = fx.env.place(&ctx, None, Intent::Read).await.unwrap();
    Harness {
        workspace: fx.env.clone(),
        ctx,
        writable: place.name().to_owned(),
        read_only: None,
        secret: common::SECRETS[0].to_owned(),
        alive: Box::new(fx),
    }
}

adam_code_tools_testkit::code_workspace_conformance!(harness().await);

/// The coder's nested project instructions reach a tool through the port: a directory's
/// `AGENTS.md`, for a path below it. (Whether it was shown already is read from the run's
/// conversation, which a detached context has none of; `project_instructions.rs` tests that.)
#[tokio::test]
async fn a_nested_agents_md_is_shown_through_the_port() {
    let fx = Fixture::new().await;
    fx.commit_to_main("sub/AGENTS.md", "sub rules\n");
    let h = prepared(fx).await;
    let place: Place = h.workspace.place(&h.ctx, None, Intent::Read).await.unwrap();
    let ask = || {
        h.workspace
            .nested_instructions(&h.ctx, &place, Some("sub/lib.rs"), Target::File)
    };
    let block = ask().await.unwrap().expect("the block of sub/AGENTS.md");
    assert!(block.contains("sub rules"), "{block}");
    // A path no `AGENTS.md` is above: nothing.
    assert_eq!(
        h.workspace
            .nested_instructions(&h.ctx, &place, Some("README.md"), Target::File)
            .await
            .unwrap(),
        None
    );
    // A place the coder did not make carries no slot: nothing to show.
    let foreign = Place::new(
        "x",
        std::sync::Arc::clone(place.root()),
        place.git().clone(),
    );
    assert_eq!(
        h.workspace
            .nested_instructions(&h.ctx, &foreign, Some("sub/lib.rs"), Target::File)
            .await
            .unwrap(),
        None
    );
}
