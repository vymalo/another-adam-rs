//! `uploads/v1` through the UI's thread-tools client: what a run's context allows, and a file sent
//! by reference over the grant the run holds.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use adam_a2a_runtime::{CONTEXT_THREAD_TOOLS, CONTEXT_UPLOADS};
use adam_mcp::{McpPolicy, REQUEST_UPLOAD, UploadRequest};
use adam_mcp_testkit::ThreadToolsServer;
use adam_ui::Ui;
use serde_json::{Map, Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "thread-token-5d1c";

fn context(server: &ThreadToolsServer, uploads: Option<Value>, grant: bool) -> Map<String, Value> {
    let mut context = Map::new();
    if grant {
        context.insert(
            CONTEXT_THREAD_TOOLS.to_owned(),
            json!({"url": server.url("t1"), "token": TOKEN, "expiresAt": "2099-01-01T00:00:00Z"}),
        );
    }
    if let Some(uploads) = uploads {
        context.insert(CONTEXT_UPLOADS.to_owned(), uploads);
    }
    context
}

#[tokio::test]
async fn uploads_need_a_message_that_offered_them_and_a_usable_grant() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    let ui = Ui::new(McpPolicy::default());

    let none = ui.upload_access(&context(&server, None, true)).unwrap_err();
    assert!(none.contains("offered uploads"), "{none}");
    // A null (a later message that did not activate it) is the same as none.
    let cleared = ui
        .upload_access(&context(&server, Some(Value::Null), true))
        .unwrap_err();
    assert!(cleared.contains("offered uploads"), "{cleared}");

    let no_grant = ui
        .upload_access(&context(&server, Some(json!({})), false))
        .unwrap_err();
    assert!(no_grant.contains("announced no endpoint"), "{no_grant}");

    let access = ui
        .upload_access(&context(&server, Some(json!({"maxBytes": 5000})), true))
        .unwrap();
    assert_eq!(access.max_bytes(), Some(5000));
    let open = ui
        .upload_access(&context(&server, Some(json!({})), true))
        .unwrap();
    assert_eq!(open.max_bytes(), None);
    // The access holds a credential: it shows none.
    let shown = format!("{access:?} {none} {no_grant}");
    assert!(!shown.contains(TOKEN), "{shown}");
}

#[tokio::test]
async fn a_file_goes_to_the_bucket_over_the_grant_the_run_holds() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    let bucket = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&bucket)
        .await;
    let bytes = b"hello, bucket".to_vec();
    let request = UploadRequest::for_bytes("a.txt", "text/plain", &bytes);
    server.set_structured_answer(
        REQUEST_UPLOAD,
        json!({"uploadId": "k.p.m",
               "url": format!("{}/o/{}?X-Amz-Signature=s", bucket.uri(), request.sha256()),
               "method": "PUT",
               "headers": {"content-type": "text/plain"},
               "expiresAt": "2099-01-01T00:00:00Z"}),
        false,
    );
    let ui = Ui::new(McpPolicy::default());
    let access = ui
        .upload_access(&context(&server, Some(json!({})), true))
        .unwrap();
    let done = ui.upload(&access, &request, bytes.clone()).await.unwrap();
    assert_eq!(done.upload_id, "k.p.m");
    let puts: Vec<_> = bucket
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.body)
        .collect();
    assert_eq!(puts, vec![bytes]);
    let authorizations = server.authorizations();
    assert!(!authorizations.is_empty());
    assert!(
        authorizations
            .iter()
            .all(|a| a == &format!("Bearer {TOKEN}"))
    );
}

#[tokio::test]
async fn an_embedded_ui_has_no_thread_tools_and_so_no_uploads() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    let ui = Ui::embedded();
    let why = ui
        .upload_access(&context(&server, Some(json!({})), true))
        .unwrap_err();
    assert!(why.contains("no thread tools"), "{why}");
}
