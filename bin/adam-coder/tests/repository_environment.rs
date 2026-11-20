//! `use_repository_environment` (ADR 0052 decisions 4–6): the tool is offered only when a run pod
//! can take the repository's image, it moves the run to it, and every refusal is in words and leaves
//! the run's pod as it was.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_coder::tools::environment::{
    RepositoryEnvironmentSource, USE_REPOSITORY_ENVIRONMENT, use_repository_environment_spec,
};
use adam_coder::tools::scratch::StartScratch;
use adam_llm_agent::{SourceCtx, Tool, ToolCtx, ToolSource};
use adam_runtime::CollectingSink;
use common::{FakeEnvironment, Fixture};
use serde_json::{Map, json};

struct Rig {
    fx: Fixture,
    fake: Arc<FakeEnvironment>,
    source: RepositoryEnvironmentSource,
    ctx: ToolCtx,
}

impl Rig {
    async fn with(fake: Arc<FakeEnvironment>) -> Self {
        let fx = Fixture::new().await.using(fake.clone());
        let ctx = ToolCtx::detached("tool", "call-1", Arc::new(CollectingSink::new()))
            .with_state(fx.env.clone());
        let source = RepositoryEnvironmentSource::new(fx.env.clone());
        Self {
            fx,
            fake,
            source,
            ctx,
        }
    }

    async fn run_pods() -> Self {
        let fake = FakeEnvironment::in_run_pods(None, &["ghcr.io/acme/"]);
        let rig = Self::with(fake).await;
        rig.start_scratch().await;
        rig
    }

    async fn start_scratch(&self) {
        let out = StartScratch
            .call(&self.ctx, json!({"name": "app"}))
            .await
            .expect("start_scratch");
        assert!(!out.is_error, "{}", out.content);
    }

    /// The path of the run's only slot.
    async fn slot(&self) -> std::path::PathBuf {
        let run = self.ctx.run_id().to_string();
        self.fx
            .env
            .workspaces
            .run(&run)
            .unwrap()
            .slots()
            .await
            .unwrap()[0]
            .path()
            .to_owned()
    }

    async fn write_config(&self, content: &str) {
        let path = self.slot().await.join(".devcontainer/devcontainer.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn source_ctx(&self) -> SourceCtx {
        SourceCtx::detached_under(self.ctx.run_id(), Map::new())
    }

    async fn withheld(&self) -> Vec<String> {
        self.source
            .withhold(&self.source_ctx(), &[USE_REPOSITORY_ENVIRONMENT])
            .await
            .into_iter()
            .map(|held| held.reason)
            .collect()
    }

    async fn call(&self, args: serde_json::Value) -> adam_llm_agent::ToolOutput {
        self.source
            .call(&self.ctx, USE_REPOSITORY_ENVIRONMENT, args)
            .await
            .expect("the source owns the tool")
            .expect("the tool answers")
    }

    fn images(&self) -> Vec<(String, Option<String>)> {
        self.fake.images.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn the_tool_is_withheld_when_the_commands_do_not_run_in_run_pods() {
    let fake = FakeEnvironment::new(None);
    let rig = Rig::with(fake).await;
    rig.start_scratch().await;
    rig.write_config(r#"{"image":"ghcr.io/acme/dev:1"}"#).await;
    let reasons = rig.withheld().await;
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(
        reasons[0].contains("coder's own container") && reasons[0].contains("no run pod image"),
        "{}",
        reasons[0]
    );
    assert!(
        rig.source.specs(&rig.source_ctx()).await.is_empty(),
        "not in the list either"
    );
}

#[tokio::test]
async fn the_tool_is_withheld_when_the_deployment_allows_no_prefix() {
    let fake = FakeEnvironment::in_run_pods(None, &[]);
    let rig = Rig::with(fake).await;
    rig.start_scratch().await;
    rig.write_config(r#"{"image":"ghcr.io/acme/dev:1"}"#).await;
    let reasons = rig.withheld().await;
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(
        reasons[0].contains("allows no repository image")
            && reasons[0].contains("RUN_POD_REPOSITORY_IMAGES"),
        "{}",
        reasons[0]
    );
}

#[tokio::test]
async fn the_tool_is_withheld_until_a_writable_slot_has_a_configuration() {
    let rig = Rig::run_pods().await;
    let reasons = rig.withheld().await;
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(
        reasons[0].contains("no writable slot")
            && reasons[0].contains(".devcontainer/devcontainer.json")
            && reasons[0].contains(".devcontainer.json"),
        "{}",
        reasons[0]
    );
    assert!(rig.source.specs(&rig.source_ctx()).await.is_empty());

    rig.write_config(r#"{"image":"ghcr.io/acme/dev:1"}"#).await;
    assert!(rig.withheld().await.is_empty(), "a configuration offers it");
    let specs = rig.source.specs(&rig.source_ctx()).await;
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, USE_REPOSITORY_ENVIRONMENT);
    let listing = rig.source.listing(&rig.source_ctx()).await;
    assert_eq!(listing.specs, specs);
    assert_eq!(listing.notes.len(), 1);
    assert!(
        listing.notes[0].timeout().is_some(),
        "a call that pulls an image has a bound of its own"
    );
}

#[tokio::test]
async fn the_spec_is_the_tools_contract() {
    let spec = use_repository_environment_spec();
    assert_eq!(spec.name, "use_repository_environment");
    assert_eq!(
        spec.parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["config", "off", "repo"]
    );
    assert!(
        spec.parameters.get("required").is_none(),
        "every argument is optional"
    );
}

#[tokio::test]
async fn a_repository_that_names_its_image_moves_the_run_to_it() {
    let rig = Rig::run_pods().await;
    rig.write_config(
        r#"{"customizations":{"adam":{"image":"ghcr.io/acme/dev:1"}},"remoteUser":"vscode"}"#,
    )
    .await;
    let out = rig.call(json!({"repo": "app"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("image: ghcr.io/acme/dev:1")
            && out
                .content
                .contains("file: .devcontainer/devcontainer.json")
            && out.content.contains("ignored"),
        "{}",
        out.content
    );
    let run = rig.ctx.run_id().to_string();
    assert_eq!(
        rig.images(),
        [(run.clone(), Some("ghcr.io/acme/dev:1".to_owned()))],
        "the environment was told the image"
    );
    assert_eq!(
        *rig.fake.ensured.lock().unwrap(),
        [run],
        "the pod was made now, as this call's step"
    );
}

#[tokio::test]
async fn a_configuration_that_only_builds_is_refused_in_words_and_leaves_the_pod_alone() {
    let rig = Rig::run_pods().await;
    rig.write_config(r#"{"build":{"dockerfile":"Dockerfile"}}"#)
        .await;
    let out = rig.call(json!({})).await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("builds its image")
            && out.content.contains("build.dockerfile")
            && out.content.contains("customizations.adam.image")
            && out.content.contains("ghcr.io/acme/"),
        "{}",
        out.content
    );
    assert!(rig.images().is_empty(), "the image was not changed");
    assert!(
        rig.fake.ensured.lock().unwrap().is_empty(),
        "no pod was made or replaced"
    );
}

#[tokio::test]
async fn an_image_outside_the_prefixes_is_refused_and_the_prefixes_are_named() {
    let rig = Rig::run_pods().await;
    rig.write_config(r#"{"image":"docker.io/library/node:24"}"#)
        .await;
    let out = rig.call(json!({})).await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("outside the prefixes") && out.content.contains("ghcr.io/acme/"),
        "{}",
        out.content
    );
    assert!(rig.images().is_empty());
    assert!(rig.fake.ensured.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_compose_configuration_is_refused() {
    let rig = Rig::run_pods().await;
    rig.write_config(r#"{"dockerComposeFile":"docker-compose.yml","service":"app"}"#)
        .await;
    let out = rig.call(json!({})).await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content
            .contains("Compose configuration is not supported")
            && out.content.contains("name one image"),
        "{}",
        out.content
    );
    assert!(rig.images().is_empty());
}

#[tokio::test]
async fn off_goes_back_to_the_deployment_image() {
    let rig = Rig::run_pods().await;
    rig.write_config(r#"{"image":"ghcr.io/acme/dev:1"}"#).await;
    assert!(!rig.call(json!({})).await.is_error);
    let out = rig.call(json!({"off": true})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("deployment's image again"),
        "{}",
        out.content
    );
    let run = rig.ctx.run_id().to_string();
    assert_eq!(
        rig.images(),
        [
            (run.clone(), Some("ghcr.io/acme/dev:1".to_owned())),
            (run, None),
        ]
    );
}

#[tokio::test]
async fn a_call_that_names_a_file_the_slot_does_not_have_is_refused() {
    let rig = Rig::run_pods().await;
    rig.write_config(r#"{"image":"ghcr.io/acme/dev:1"}"#).await;
    let out = rig
        .call(json!({"config": ".devcontainer/rust/devcontainer.json"}))
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("is not a devcontainer configuration"),
        "{}",
        out.content
    );
    assert!(rig.images().is_empty());
}

/// A refused repository is a **result**, not a failed call: the model reads the words and can ask
/// the person, and the run's environment was not touched.
#[tokio::test]
async fn the_tool_is_not_a_tool_error_when_the_repository_is_refused() {
    let rig = Rig::run_pods().await;
    rig.write_config(r#"{"image":"./image"}"#).await;
    let out = rig.call(json!({})).await;
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("is a path"), "{}", out.content);
    assert!(rig.images().is_empty());
    assert!(rig.fake.ensured.lock().unwrap().is_empty());
}
