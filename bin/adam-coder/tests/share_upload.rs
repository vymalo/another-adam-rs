//! `share_file` over the inline threshold (`uploads/v1`, the agent's side): a file over it goes to
//! the bucket by reference, with no bytes in the artifact, the step's output or the journal; a file
//! under it is the `raw` artifact it always was; a refusal is an error the model reads and never an
//! inline fallback.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam::mcp::{REQUEST_UPLOAD, UploadRequest};
use adam_a2a::UPLOADS_EXTENSION;
use adam_a2a_runtime::{CONTEXT_THREAD_TOOLS, CONTEXT_UPLOADS, artifact_of};
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_coder::tools::prepare::PrepareWorkspace;
use adam_coder::tools::share::ShareFile;
use adam_llm_agent::{Tool, ToolCtx, ToolOutput};
use adam_mcp_testkit::ThreadToolsServer;
use adam_runtime::{ArtifactRef, CollectingSink};
use common::{Fixture, GITHUB_TOKEN};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "thread-token-9f3a";
const SIGNATURE: &str = "X-Amz-Signature=sigsecret77";
const UPLOAD_ID: &str = "k1.cGF5bG9hZA.mac";

/// A run with the tools, a thread-tools endpoint and a bucket.
struct Rig {
    fx: Fixture,
    thread: ThreadToolsServer,
    bucket: MockServer,
}

impl Rig {
    /// A rig whose tool sends a file over `threshold` bytes by reference.
    async fn new(threshold: u64) -> Self {
        let mut fx = Fixture::new().await;
        Arc::get_mut(&mut fx.env)
            .expect("not shared yet")
            .inline_upload_threshold = threshold;
        let thread = ThreadToolsServer::start(&[TOKEN]).await;
        let bucket = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&bucket)
            .await;
        Self { fx, thread, bucket }
    }

    /// The slot the thread tools hand out for whatever is asked.
    fn slot_for(&self, bytes: &[u8], filename: &str, media_type: &str) {
        let request = UploadRequest::for_bytes(filename, media_type, bytes);
        self.thread.set_structured_answer(
            REQUEST_UPLOAD,
            json!({
                "uploadId": UPLOAD_ID,
                "url": format!("{}/artifacts/threads/t1/{}?X-Amz-Expires=300&{SIGNATURE}",
                    self.bucket.uri(), request.sha256()),
                "method": "PUT",
                "headers": {"content-type": media_type,
                            "x-amz-checksum-sha256": "Y2hlY2tzdW0=",
                            "x-amz-meta-sha256": request.sha256()},
                "expiresAt": "2099-01-01T00:00:00Z"
            }),
            false,
        );
    }

    /// The context of a message that activated `uploads/v1` (with `limits`) and brought a grant.
    fn context(&self, uploads: Option<Value>, grant: bool) -> serde_json::Map<String, Value> {
        let mut context = serde_json::Map::new();
        if grant {
            context.insert(
                CONTEXT_THREAD_TOOLS.to_owned(),
                json!({"url": self.thread.url("t1"), "token": TOKEN,
                       "expiresAt": "2099-01-01T00:00:00Z"}),
            );
        }
        if let Some(limits) = uploads {
            context.insert(CONTEXT_UPLOADS.to_owned(), limits);
        }
        context
    }

    fn ctx(&self, context: serde_json::Map<String, Value>) -> ToolCtx {
        ToolCtx::detached("tool", "call-1", Arc::new(CollectingSink::new()))
            .with_state(self.fx.env.clone())
            .with_context(context)
    }

    /// The run's workspace, so that a file can be written to its worktree.
    async fn prepare(&self, ctx: &ToolCtx) -> std::path::PathBuf {
        let run = ctx.run_id().to_string();
        let mut notes = self.fx.env.notes.load(&run).await.unwrap();
        notes.name_repos(named_in(
            &without_untrusted(&self.fx.remote_url()),
            &self.fx.env.settings.default_repo_host,
        ));
        self.fx.env.notes.save(&run, &notes).await.unwrap();
        let out = PrepareWorkspace
            .call(
                ctx,
                json!({"repo_url": self.fx.remote_url(), "base_branch": "main"}),
            )
            .await
            .expect("prepare");
        assert!(!out.is_error, "{}", out.content);
        common::slot_dir(&self.fx.root, &run)
    }

    /// What reached the bucket: the bodies of its `PUT`s.
    async fn put_bodies(&self) -> Vec<Vec<u8>> {
        self.bucket
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.method.as_str() == "PUT")
            .map(|r| r.body)
            .collect()
    }
}

fn activated() -> Option<Value> {
    Some(json!({"maxBytes": 1_073_741_824u64, "ttlSecs": 300}))
}

/// `len` bytes of text that has a marker in it no other test output holds.
fn text_of(len: usize) -> Vec<u8> {
    let mut text = b"UPLOADED-BYTES-MARKER ".to_vec();
    while text.len() < len {
        text.extend_from_slice(b"lorem ipsum dolor sit amet ");
    }
    text.truncate(len);
    text
}

fn everything_of(out: &ToolOutput) -> String {
    format!(
        "{} {}",
        out.content,
        serde_json::to_string(&out.artifacts).unwrap()
    )
}

#[tokio::test]
async fn a_file_under_the_threshold_is_still_the_raw_artifact_and_asks_for_no_slot() {
    let rig = Rig::new(2000).await;
    let ctx = rig.ctx(rig.context(activated(), true));
    let worktree = rig.prepare(&ctx).await;
    let body = text_of(1500);
    std::fs::write(worktree.join("small.txt"), &body).unwrap();
    // Exactly at the threshold is under it too.
    std::fs::write(worktree.join("edge.txt"), text_of(2000)).unwrap();

    for (path, len) in [("small.txt", 1500), ("edge.txt", 2000)] {
        let out = ShareFile.call(&ctx, json!({"path": path})).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        let file = out.artifacts[0].file.as_ref().expect("a raw file");
        assert_eq!(file.bytes.len(), len, "{path}");
        assert_eq!(out.artifacts[0].mime_type.as_deref(), Some("text/plain"));
    }
    assert!(rig.thread.calls().is_empty(), "no slot was asked for");
    assert!(rig.put_bodies().await.is_empty());
}

#[tokio::test]
async fn a_file_over_the_threshold_goes_by_reference_with_no_bytes_anywhere() {
    let rig = Rig::new(2000).await;
    let ctx = rig.ctx(rig.context(activated(), true));
    let worktree = rig.prepare(&ctx).await;
    let body = text_of(2001);
    std::fs::write(worktree.join("report.txt"), &body).unwrap();
    rig.slot_for(&body, "report.txt", "text/plain");

    let out = ShareFile
        .call(&ctx, json!({"path": "report.txt", "name": "The report"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);

    // The bytes went to the bucket, exactly, and to nothing else.
    assert_eq!(rig.put_bodies().await, vec![body.clone()]);
    let request = UploadRequest::for_bytes("report.txt", "text/plain", &body);
    let calls = rig.thread.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, REQUEST_UPLOAD);
    assert_eq!(
        calls[0].1,
        json!({"filename": "report.txt", "mediaType": "text/plain",
               "size": 2001, "sha256": request.sha256()})
    );

    // The artifact is a reference: no file, and the data names the upload.
    assert_eq!(out.artifacts.len(), 1);
    let artifact = &out.artifacts[0];
    assert!(artifact.file.is_none());
    assert_eq!(artifact.name, "The report");
    assert_eq!(
        artifact.data,
        json!({"filename": "report.txt", "mediaType": "text/plain", "uploadId": UPLOAD_ID})
    );

    // No byte of the file is in the tool's output, the artifact, the journal's form of it or the
    // outputs the model and the client are given; nor is the slot's URL or signature.
    let journal = serde_json::to_string(&ArtifactRef::from(artifact)).unwrap();
    let served = serde_json::to_string(&artifact_of(&ArtifactRef::from(artifact), None)).unwrap();
    for (what, shown) in [
        ("the output", everything_of(&out)),
        ("the journal", journal),
        ("the A2A artifact", served),
    ] {
        assert!(!shown.contains("UPLOADED-BYTES-MARKER"), "{what}: {shown}");
        assert!(!shown.contains("lorem ipsum"), "{what}: {shown}");
        assert!(!shown.contains("X-Amz"), "{what}: {shown}");
        assert!(!shown.contains(SIGNATURE), "{what}: {shown}");
    }
    assert!(
        out.content.starts_with("Shared report.txt"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn the_a2a_artifact_is_the_contracts_data_part_with_the_upload_id_and_no_raw() {
    let rig = Rig::new(100).await;
    let ctx = rig.ctx(rig.context(activated(), true));
    let worktree = rig.prepare(&ctx).await;
    let body = text_of(300);
    std::fs::write(worktree.join("notes.md"), &body).unwrap();
    rig.slot_for(&body, "notes.md", "text/markdown");
    let out = ShareFile
        .call(&ctx, json!({"path": "notes.md"}))
        .await
        .unwrap();

    let served = artifact_of(&ArtifactRef::from(&out.artifacts[0]), None).expect("served");
    assert_eq!(served.parts.len(), 1);
    let wire = serde_json::to_value(&served.parts[0]).unwrap();
    assert_eq!(wire["data"], json!({}));
    assert_eq!(wire["filename"], "notes.md");
    assert_eq!(wire["mediaType"], "text/markdown");
    assert_eq!(
        wire["metadata"][UPLOADS_EXTENSION],
        json!({"uploadId": UPLOAD_ID})
    );
    for absent in ["raw", "url", "text"] {
        assert!(wire.get(absent).is_none(), "{absent}: {wire}");
    }
}

/// The folder's key, then the environment's, then the default decide the boundary: the file at the
/// boundary is raw and one byte more is a reference.
#[tokio::test]
async fn the_folders_key_and_the_environments_override_each_move_the_boundary() {
    let default = adam_a2a::DEFAULT_UPLOADS_INLINE_MAX_BYTES;
    assert!(
        default > 4 * 1024 * 1024,
        "the default is above the inline cap"
    );
    for (folder, env, boundary) in [
        (Some(2048), Some(4096), 2048),
        (Some(3000), None, 3000),
        (None, Some(4096), 4096),
        // The default (10 MiB) is above what an artifact can hold, so the boundary is its 4 MiB.
        (None, None, 4 * 1024 * 1024),
    ] {
        let threshold = adam_service::inline_upload_threshold(folder, env);
        let rig = Rig::new(threshold).await;
        let ctx = rig.ctx(rig.context(activated(), true));
        let worktree = rig.prepare(&ctx).await;
        for (len, by_reference) in [(boundary, false), (boundary + 1, true)] {
            let body = text_of(len);
            let name = format!("f{len}.txt");
            std::fs::write(worktree.join(&name), &body).unwrap();
            rig.slot_for(&body, &name, "text/plain");
            let out = ShareFile.call(&ctx, json!({"path": name})).await.unwrap();
            assert!(!out.is_error, "{folder:?}/{env:?}/{len}: {}", out.content);
            assert_eq!(
                out.artifacts[0].file.is_none(),
                by_reference,
                "{folder:?}/{env:?}: {len} bytes against {boundary}"
            );
        }
    }
}

#[tokio::test]
async fn without_the_extension_or_the_thread_tools_a_big_file_is_refused_never_inlined() {
    for (what, uploads, grant) in [
        ("not activated", None, true),
        ("no grant", activated(), false),
        ("neither", None, false),
    ] {
        let rig = Rig::new(1000).await;
        let ctx = rig.ctx(rig.context(uploads, grant));
        let worktree = rig.prepare(&ctx).await;
        std::fs::write(worktree.join("big.txt"), text_of(1500)).unwrap();
        let out = ShareFile
            .call(&ctx, json!({"path": "big.txt"}))
            .await
            .unwrap();
        assert!(out.is_error, "{what}: {}", out.content);
        assert!(
            out.content.contains("too large to share inline")
                && out.content.contains("uploads are not available"),
            "{what}: {}",
            out.content
        );
        assert!(out.artifacts.is_empty(), "{what}: nothing is shared");
        assert!(rig.thread.calls().is_empty(), "{what}");
        assert!(rig.put_bodies().await.is_empty(), "{what}");
    }
}

/// With the default threshold (above the artifact's own cap) and no uploads, a file over 4 MiB is
/// refused as it always was.
#[tokio::test]
async fn without_uploads_a_file_over_the_inline_cap_is_refused_as_before() {
    let rig = Rig::new(adam_a2a::DEFAULT_UPLOADS_INLINE_MAX_BYTES).await;
    let ctx = rig.ctx(rig.context(None, false));
    let worktree = rig.prepare(&ctx).await;
    std::fs::write(worktree.join("big.bin"), vec![0u8; 4 * 1024 * 1024 + 1]).unwrap();
    let out = ShareFile
        .call(&ctx, json!({"path": "big.bin"}))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(
        out.content.contains("over the limit of 4194304 bytes"),
        "{}",
        out.content
    );
    assert!(out.artifacts.is_empty());
}

#[tokio::test]
async fn a_refused_slot_is_an_error_the_model_reads_and_nothing_is_sent_anywhere() {
    for code in [
        "too_large",
        "over_limit",
        "not_activated",
        "task_over",
        "unavailable",
    ] {
        let rig = Rig::new(1000).await;
        let ctx = rig.ctx(rig.context(activated(), true));
        let worktree = rig.prepare(&ctx).await;
        std::fs::write(worktree.join("big.txt"), text_of(1500)).unwrap();
        rig.thread.set_structured_answer(
            REQUEST_UPLOAD,
            json!(format!("{code}: the slot was refused")),
            true,
        );
        let out = ShareFile
            .call(&ctx, json!({"path": "big.txt"}))
            .await
            .unwrap();
        assert!(out.is_error, "{code}");
        assert!(out.content.contains(code), "{code}: {}", out.content);
        assert!(
            out.content.contains("not shared"),
            "{code}: {}",
            out.content
        );
        assert!(out.artifacts.is_empty(), "{code}: no inline fallback");
        assert!(rig.put_bodies().await.is_empty(), "{code}: no PUT");
    }
}

#[tokio::test]
async fn a_bucket_that_refuses_the_put_is_an_error_without_the_url_and_never_an_inline_file() {
    let rig = Rig::new(1000).await;
    rig.bucket.reset().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&rig.bucket)
        .await;
    let ctx = rig.ctx(rig.context(activated(), true));
    let worktree = rig.prepare(&ctx).await;
    let body = text_of(1500);
    std::fs::write(worktree.join("big.txt"), &body).unwrap();
    rig.slot_for(&body, "big.txt", "text/plain");

    let out = ShareFile
        .call(&ctx, json!({"path": "big.txt"}))
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("403"), "{}", out.content);
    assert!(!out.content.contains("X-Amz") && !out.content.contains(SIGNATURE));
    assert!(!out.content.contains(&rig.bucket.uri()));
    assert!(out.artifacts.is_empty());
    assert_eq!(rig.put_bodies().await.len(), 1, "one PUT, no retry");
}

#[tokio::test]
async fn a_file_over_what_the_activation_allows_is_refused_before_a_slot_is_asked() {
    let rig = Rig::new(1000).await;
    let ctx = rig.ctx(rig.context(Some(json!({"maxBytes": 1500})), true));
    let worktree = rig.prepare(&ctx).await;
    std::fs::write(worktree.join("big.txt"), text_of(1800)).unwrap();
    let out = ShareFile
        .call(&ctx, json!({"path": "big.txt"}))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(
        out.content.contains("1800") && out.content.contains("1500"),
        "{}",
        out.content
    );
    assert!(rig.thread.calls().is_empty());
    assert!(out.artifacts.is_empty());
}

/// The coder's tools are wrapped by its redactor, which scrubs what a tool returns; an uploaded
/// file is not returned, so the tool scrubs a text file before it is hashed and sent.
#[tokio::test]
async fn a_text_file_is_scrubbed_of_the_secrets_before_it_is_hashed_and_uploaded() {
    let rig = Rig::new(100).await;
    let ctx = rig.ctx(rig.context(activated(), true));
    let worktree = rig.prepare(&ctx).await;
    let mut body = text_of(400);
    body.extend_from_slice(format!(" TOKEN={GITHUB_TOKEN} ").as_bytes());
    std::fs::write(worktree.join("env.txt"), &body).unwrap();
    // The slot answers whatever is asked: what matters is what was asked for and what was sent.
    rig.slot_for(&body, "env.txt", "text/plain");

    let tool = adam_coder::coder_tools(&rig.fx.env)
        .into_iter()
        .find(|t| t.spec().name == "share_file")
        .expect("share_file is a coder tool");
    let out = tool.call(&ctx, json!({"path": "env.txt"})).await.unwrap();
    assert!(!out.is_error, "{}", out.content);

    let sent = rig.put_bodies().await;
    // The bucket would refuse a body that is not the signed one, so the request is for the
    // scrubbed bytes: the hash asked for is the hash of what was sent.
    assert_eq!(sent.len(), 1);
    let sent = String::from_utf8(sent[0].clone()).unwrap();
    assert!(!sent.contains(GITHUB_TOKEN), "{sent}");
    assert!(sent.contains("UPLOADED-BYTES-MARKER"));
    let asked = &rig.thread.calls()[0].1;
    assert_eq!(
        asked["sha256"],
        UploadRequest::for_bytes("env.txt", "text/plain", sent.as_bytes()).sha256()
    );
    assert_eq!(asked["size"], sent.len());
}

#[tokio::test]
async fn an_image_over_the_threshold_keeps_its_checked_type() {
    let rig = Rig::new(64).await;
    let ctx = rig.ctx(rig.context(activated(), true));
    let worktree = rig.prepare(&ctx).await;
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01".to_vec();
    png.resize(300, 7);
    std::fs::write(worktree.join("shot.png"), &png).unwrap();
    rig.slot_for(&png, "shot.png", "image/png");
    let out = ShareFile
        .call(&ctx, json!({"path": "shot.png"}))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(rig.thread.calls()[0].1["mediaType"], "image/png");
    assert_eq!(out.artifacts[0].data["mediaType"], "image/png");
    assert_eq!(rig.put_bodies().await, vec![png]);
}
