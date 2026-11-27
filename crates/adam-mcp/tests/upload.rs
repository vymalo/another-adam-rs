//! `uploads/v1`, the outgoing direction: a slot from the fake thread-tools endpoint, the bytes to
//! a fake bucket.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use adam_mcp::{
    Endpoint, EndpointError, McpPolicy, REQUEST_UPLOAD, UploadError, UploadRequest, Uploader,
};
use adam_mcp_testkit::{LogCapture, ThreadToolsServer};
use secrecy::SecretString;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "thread-token-9f3a";
const SIGNATURE: &str = "X-Amz-Signature=sigsecret77";

fn endpoint(server: &ThreadToolsServer) -> Endpoint {
    Endpoint::new(
        &server.url("thread-1"),
        &SecretString::from(TOKEN.to_owned()),
        &McpPolicy::default(),
    )
    .unwrap()
}

fn slot_body(bucket: &MockServer, request: &UploadRequest) -> Value {
    json!({
        "uploadId": "k1.payload.mac",
        "url": format!("{}/artifacts/threads/thread-1/{}?X-Amz-Expires=300&{SIGNATURE}",
            bucket.uri(), request.sha256()),
        "method": "PUT",
        "headers": {
            "content-type": request.media_type(),
            "x-amz-checksum-sha256": "Y2hlY2tzdW0=",
            "x-amz-meta-sha256": request.sha256(),
            "x-amz-meta-filename": request.filename(),
        },
        "expiresAt": "2026-10-11T12:05:00Z"
    })
}

async fn fixture(bytes: &[u8]) -> (ThreadToolsServer, MockServer, UploadRequest) {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    let bucket = MockServer::start().await;
    let request = UploadRequest::for_bytes("report.pdf", "application/pdf", bytes);
    server.set_structured_answer(REQUEST_UPLOAD, slot_body(&bucket, &request), false);
    (server, bucket, request)
}

#[tokio::test]
async fn the_request_carries_name_type_size_and_hash_and_the_id_is_kept() {
    let bytes = b"%PDF-1.7 hello".to_vec();
    let (server, bucket, request) = fixture(&bytes).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&bucket)
        .await;

    let uploaded = Uploader::new(&McpPolicy::default())
        .upload(&endpoint(&server), &request, bytes.clone())
        .await
        .unwrap();

    assert_eq!(uploaded.upload_id, "k1.payload.mac");
    assert_eq!(uploaded.expires_at, "2026-10-11T12:05:00Z");
    let calls = server.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, REQUEST_UPLOAD);
    assert_eq!(
        calls[0].1,
        json!({"filename": "report.pdf", "mediaType": "application/pdf",
               "size": bytes.len(), "sha256": request.sha256()})
    );
    // The hash is the content's own (64 lower-case hex characters).
    assert_eq!(request.sha256().len(), 64);
    assert!(
        request
            .sha256()
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
}

#[tokio::test]
async fn the_put_carries_exactly_the_returned_headers_and_the_bytes() {
    let bytes = b"%PDF-1.7 hello".to_vec();
    let (server, bucket, request) = fixture(&bytes).await;
    Mock::given(method("PUT"))
        .and(path(format!(
            "/artifacts/threads/thread-1/{}",
            request.sha256()
        )))
        .and(header("content-type", "application/pdf"))
        .and(header("x-amz-checksum-sha256", "Y2hlY2tzdW0="))
        .and(header("x-amz-meta-sha256", request.sha256()))
        .and(header("x-amz-meta-filename", "report.pdf"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&bucket)
        .await;

    Uploader::new(&McpPolicy::default())
        .upload(&endpoint(&server), &request, bytes.clone())
        .await
        .unwrap();

    let received = bucket.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let put = &received[0];
    assert_eq!(put.body, bytes);
    assert_eq!(
        put.url.query(),
        Some(&format!("X-Amz-Expires=300&{SIGNATURE}")[..])
    );
    // No credential of ours goes to the bucket.
    assert!(put.headers.get("authorization").is_none());
    let names: Vec<_> = put.headers.keys().map(|k| k.as_str().to_owned()).collect();
    for name in &names {
        assert!(
            [
                "content-type",
                "x-amz-checksum-sha256",
                "x-amz-meta-sha256",
                "x-amz-meta-filename",
                "host",
                "content-length",
                "accept",
                "user-agent",
                "accept-encoding"
            ]
            .contains(&name.as_str())
                || name.starts_with("x-amz-"),
            "unexpected header {name}"
        );
    }
}

#[tokio::test]
async fn a_refused_slot_is_a_named_error_and_nothing_is_sent_to_the_bucket() {
    let bytes = vec![7_u8; 32];
    let (server, bucket, request) = fixture(&bytes).await;
    for code in [
        "too_large",
        "over_limit",
        "not_activated",
        "task_over",
        "unavailable",
    ] {
        server.set_structured_answer(
            REQUEST_UPLOAD,
            json!(format!("{code}: the slot was refused")),
            true,
        );
        let error = Uploader::new(&McpPolicy::default())
            .upload(&endpoint(&server), &request, bytes.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, UploadError::Refused { code: c, .. } if c == code),
            "{code}: {error:?}"
        );
    }
    server.set_structured_answer(REQUEST_UPLOAD, json!("something odd"), true);
    let error = Uploader::new(&McpPolicy::default())
        .upload(&endpoint(&server), &request, bytes.clone())
        .await
        .unwrap_err();
    assert!(matches!(&error, UploadError::Refused { code, .. } if code == "unknown"));
    assert!(bucket.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_endpoint_that_does_not_accept_the_token_is_an_endpoint_error() {
    let bytes = vec![1_u8; 4];
    let (server, _bucket, request) = fixture(&bytes).await;
    server.set_tokens(&["another"]);
    let error = Uploader::new(&McpPolicy::default())
        .upload(&endpoint(&server), &request, bytes)
        .await
        .unwrap_err();
    assert!(
        matches!(error, UploadError::Endpoint(EndpointError::Unauthorized)),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_bucket_that_refuses_the_put_is_a_named_error_never_a_retry() {
    let bytes = vec![3_u8; 16];
    let (server, bucket, request) = fixture(&bytes).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&bucket)
        .await;
    let error = Uploader::new(&McpPolicy::default())
        .upload(&endpoint(&server), &request, bytes)
        .await
        .unwrap_err();
    assert_eq!(error, UploadError::PutRejected(403));
    assert_eq!(server.calls().len(), 1, "no second slot is asked for");
}

#[tokio::test]
async fn bytes_that_are_not_the_requested_ones_are_never_sent() {
    let bytes = vec![5_u8; 8];
    let (server, bucket, request) = fixture(&bytes).await;
    let uploader = Uploader::new(&McpPolicy::default());
    let slot = uploader
        .request(&endpoint(&server), &request)
        .await
        .unwrap();
    assert!(matches!(
        uploader.put(&slot, &request, vec![5_u8; 9]).await,
        Err(UploadError::BytesMismatch(_))
    ));
    assert!(matches!(
        uploader.put(&slot, &request, vec![6_u8; 8]).await,
        Err(UploadError::BytesMismatch(_))
    ));
    assert!(bucket.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_slot_that_is_not_a_put_to_an_acceptable_url_is_refused() {
    let bytes = vec![2_u8; 8];
    let (server, bucket, request) = fixture(&bytes).await;
    let good = slot_body(&bucket, &request);
    let mut cases = Vec::new();
    for (key, value) in [
        ("method", json!("POST")),
        (
            "url",
            json!("http://bucket.example.com/x?X-Amz-Signature=s"),
        ),
        ("url", json!("https://user:pw@bucket.example.com/x")),
        ("uploadId", json!("")),
        ("headers", json!({"content-type": 7})),
        ("headers", json!({"bad header": "v"})),
    ] {
        let mut body = good.clone();
        body[key] = value;
        cases.push(body);
    }
    let mut without = good.clone();
    without.as_object_mut().unwrap().remove("expiresAt");
    cases.push(without);
    for body in cases {
        server.set_structured_answer(REQUEST_UPLOAD, body.clone(), false);
        let error = Uploader::new(&McpPolicy::default())
            .upload(&endpoint(&server), &request, bytes.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, UploadError::BadSlot(_)),
            "{body}: {error:?}"
        );
        assert!(
            !error.to_string().contains("bucket.example.com/x?"),
            "{error}"
        );
    }
    assert!(bucket.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn neither_the_url_nor_the_signed_headers_reach_a_log_line_or_an_error() {
    let logs = LogCapture::start();
    let bytes = b"%PDF-1.7 secret report".to_vec();
    let (server, bucket, request) = fixture(&bytes).await;
    let uploader = Uploader::new(&McpPolicy::default());

    // A success, then a refused PUT, then a PUT that cannot connect: every path.
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .up_to_n_times(1)
        .mount(&bucket)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(403).set_body_string("denied sigsecret77"))
        .mount(&bucket)
        .await;
    let slot = uploader
        .request(&endpoint(&server), &request)
        .await
        .unwrap();
    let debug = format!("{slot:?}");
    uploader.put(&slot, &request, bytes.clone()).await.unwrap();
    let rejected = uploader
        .put(&slot, &request, bytes.clone())
        .await
        .unwrap_err();
    drop(bucket);
    let unreachable = uploader
        .put(&slot, &request, bytes.clone())
        .await
        .unwrap_err();

    let mut shown = format!("{debug} {rejected} {rejected:?} {unreachable} {unreachable:?}");
    // The MCP library logs each result it receives at `debug` and `trace` (the slot is one), and
    // this crate cannot scrub that: the README says to filter the `rmcp` target out of any log
    // that leaves the process, as for a URL secret. Everything else, `reqwest` and `hyper`
    // included, is held to the rule here, and so is the token.
    let outside_rmcp: String = logs
        .text()
        .lines()
        .filter(|line| !line.contains("rmcp::"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        outside_rmcp.contains("hyper_util"),
        "the capture saw the PUT's own client"
    );
    shown.push_str(&outside_rmcp);
    for secret in [
        "sigsecret77",
        "X-Amz-Signature",
        "X-Amz-Expires",
        "Y2hlY2tzdW0=",
        "/artifacts/threads",
    ] {
        assert!(!shown.contains(secret), "{secret} leaked:\n{shown}");
    }
    assert!(!outside_rmcp.contains(TOKEN), "the thread token leaked");
}
