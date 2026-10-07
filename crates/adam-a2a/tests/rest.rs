//! The HTTP+JSON binding beside JSON-RPC: the official client over REST, and plain HTTP where the
//! client hides what we need to see (status codes, the error envelope, headers).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use a2a::{CancelTaskRequest, ListTasksRequest, StreamResponse, SubscribeToTaskRequest, TaskState};
use adam_a2a::{AuthConfig, ExtendedCardConfig, ExtensionConfig, SkillConfig};
use futures::StreamExt;
use serde_json::{Value, json};
use support::{
    OTHER_TOKEN, Setup, TOKEN, TestServer, Webhook, get, local_push, send, stream_label, task_of,
    wait_for_state,
};

const EXT_A: &str = "https://example.org/extensions/a/v1";
const EXT_B: &str = "https://example.org/extensions/b/v1";

fn body(text: &str) -> Value {
    json!({
        "message": {"messageId": format!("m-{text}"), "role": "ROLE_USER", "parts": [{"text": text}]},
        "configuration": {"returnImmediately": true}
    })
}

/// The `ErrorInfo` reason of a REST error body, and its `status`.
fn reason(body: &str) -> (String, String) {
    let value: Value = serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"));
    let error = &value["error"];
    let info = error["details"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["@type"] == "type.googleapis.com/google.rpc.ErrorInfo")
        .unwrap_or_else(|| panic!("no ErrorInfo in {body}"));
    assert_eq!(info["domain"], "a2a-protocol.org");
    (
        info["reason"].as_str().unwrap().to_owned(),
        error["status"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn the_card_lists_json_rpc_first_and_http_json_at_the_same_url() {
    let server = TestServer::start(Setup::default()).await;
    let (_, _, card) = server
        .http("GET", "/.well-known/agent-card.json", None, None)
        .await;
    let card: Value = serde_json::from_str(&card).unwrap();
    let interfaces: Vec<(&str, &str)> = card["supportedInterfaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["protocolBinding"].as_str().unwrap(),
                i["url"].as_str().unwrap(),
            )
        })
        .collect();
    let url = format!("{}/", server.base());
    assert_eq!(
        interfaces,
        [("JSONRPC", url.as_str()), ("HTTP+JSON", url.as_str())]
    );
}

#[tokio::test]
async fn the_official_client_round_trips_over_http_json() {
    let server = TestServer::start(Setup::default()).await;
    let (client, interface) = server.rest_client(Some(TOKEN)).await;
    assert_eq!(interface.protocol_binding, "HTTP+JSON");

    // Send, get, list.
    let task = task_of(client.send_message(&send("hello", None)).await.unwrap());
    let done = wait_for_state(&client, &task.id, TaskState::Completed).await;
    let echo = done.artifacts.unwrap()[0].parts[0]
        .as_text()
        .unwrap()
        .to_owned();
    assert_eq!(echo, "echo: hello");
    let page = client
        .list_tasks(&ListTasksRequest {
            context_id: None,
            status: None,
            page_size: Some(10),
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        })
        .await
        .unwrap();
    assert_eq!(
        page.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        [task.id.as_str()]
    );

    // Cancel a task that holds.
    let held = task_of(client.send_message(&send("[hold]", None)).await.unwrap());
    wait_for_state(&client, &held.id, TaskState::Working).await;
    let canceled = client
        .cancel_task(&CancelTaskRequest {
            id: held.id.clone(),
            metadata: None,
            tenant: None,
        })
        .await
        .unwrap();
    assert_eq!(canceled.status.state, TaskState::Canceled);

    // Stream a send, and resubscribe to a running task.
    let mut events = client
        .send_streaming_message(&send("streamed", None))
        .await
        .unwrap();
    let mut labels = Vec::new();
    while let Some(item) = events.next().await {
        labels.push(stream_label(&item.unwrap()));
    }
    assert_eq!(labels.first().map(String::as_str), Some("task:Submitted"));
    assert_eq!(
        labels.last().map(String::as_str),
        Some("status:Completed"),
        "{labels:?}"
    );

    let held = task_of(client.send_message(&send("[hold]", None)).await.unwrap());
    let mut events = client
        .subscribe_to_task(&SubscribeToTaskRequest {
            id: held.id.clone(),
            tenant: None,
        })
        .await
        .unwrap();
    let first = events.next().await.unwrap().unwrap();
    assert!(
        matches!(&first, StreamResponse::Task(t) if t.id == held.id),
        "{first:?}"
    );
    assert!(server.backend.release(&held.id));
    let mut last = None;
    while let Some(item) = events.next().await {
        last = Some(stream_label(&item.unwrap()));
    }
    assert_eq!(last.as_deref(), Some("status:Completed"));
}

#[tokio::test]
async fn a_task_is_private_to_its_caller_on_both_bindings() {
    let server = TestServer::start(Setup::default()).await;
    let (status, _, sent) = server
        .http("POST", "/message:send", Some(TOKEN), Some(&body("mine")))
        .await;
    assert_eq!(status, 200, "{sent}");
    let id = serde_json::from_str::<Value>(&sent).unwrap()["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let (status, _, _) = server
        .http("GET", &format!("/tasks/{id}"), Some(TOKEN), None)
        .await;
    assert_eq!(status, 200);
    let (status, _, other) = server
        .http("GET", &format!("/tasks/{id}"), Some(OTHER_TOKEN), None)
        .await;
    assert_eq!(status, 404, "{other}");
    assert_eq!(
        reason(&other),
        ("TASK_NOT_FOUND".into(), "NOT_FOUND".into())
    );
    // The same task over JSON-RPC, for the owner: one handler, one store.
    let client = server.client(Some(TOKEN)).await;
    assert_eq!(client.get_task(&get(&id)).await.unwrap().id, id);
    let (status, _, listed) = server.http("GET", "/tasks", Some(OTHER_TOKEN), None).await;
    assert_eq!(status, 200);
    assert!(!listed.contains(&id), "{listed}");
}

#[tokio::test]
async fn errors_map_to_the_binding_s_status_and_reason() {
    let server = TestServer::start(Setup::default()).await;
    // (method, path, body, status, ErrorInfo reason, google.rpc status)
    type Case = (
        &'static str,
        &'static str,
        Option<Value>,
        u16,
        &'static str,
        &'static str,
    );
    let cases: [Case; 5] = [
        (
            "GET",
            "/tasks/nope",
            None,
            404,
            "TASK_NOT_FOUND",
            "NOT_FOUND",
        ),
        (
            "POST",
            "/tasks/nope:cancel",
            None,
            404,
            "TASK_NOT_FOUND",
            "NOT_FOUND",
        ),
        (
            "POST",
            "/tasks/nope/pushNotificationConfigs",
            Some(json!({"url": "https://hooks.example.com/x"})),
            400,
            "PUSH_NOTIFICATION_NOT_SUPPORTED",
            "FAILED_PRECONDITION",
        ),
        (
            "GET",
            "/extendedAgentCard",
            None,
            400,
            "UNSUPPORTED_OPERATION",
            "FAILED_PRECONDITION",
        ),
        (
            "POST",
            "/message:send",
            Some(json!({"message": {"messageId": "m", "role": "ROLE_USER", "parts": []}})),
            400,
            "INVALID_PARAMS",
            "INVALID_ARGUMENT",
        ),
    ];
    for (method, path, request, status, why, grpc) in cases {
        let (got, _, body) = server
            .http(method, path, Some(TOKEN), request.as_ref())
            .await;
        assert_eq!(got, status, "{method} {path}: {body}");
        assert_eq!(
            reason(&body),
            (why.to_owned(), grpc.to_owned()),
            "{method} {path}"
        );
    }

    // A finished task cannot be canceled.
    let (_, _, sent) = server
        .http("POST", "/message:send", Some(TOKEN), Some(&body("done")))
        .await;
    let id = serde_json::from_str::<Value>(&sent).unwrap()["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let client = server.client(Some(TOKEN)).await;
    wait_for_state(&client, &id, TaskState::Completed).await;
    let (status, _, body) = server
        .http("POST", &format!("/tasks/{id}:cancel"), Some(TOKEN), None)
        .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(reason(&body).0, "TASK_NOT_CANCELABLE");
}

#[tokio::test]
async fn malformed_requests_get_the_binding_s_envelope_and_never_before_authentication() {
    let server = TestServer::start(Setup::default()).await;
    let auth = format!("Bearer {TOKEN}");
    // (method, path, content type, body, ErrorInfo reason)
    type Case = (
        &'static str,
        &'static str,
        Option<&'static str>,
        String,
        &'static str,
    );
    let cases: [Case; 5] = [
        (
            "POST",
            "/message:send",
            Some("application/json"),
            "{not json".into(),
            "PARSE_ERROR",
        ),
        (
            "POST",
            "/message:send",
            Some("text/plain"),
            body("x").to_string(),
            "INVALID_REQUEST",
        ),
        (
            "POST",
            "/message:send",
            Some("application/json"),
            "x".repeat(10 * 1024 * 1024 + 1),
            "INVALID_REQUEST",
        ),
        (
            "GET",
            "/tasks?pageSize=many",
            None,
            String::new(),
            "INVALID_PARAMS",
        ),
        (
            "POST",
            "/message:send",
            Some("application/json"),
            json!({"message": {"messageId": "m", "role": "ROLE_USER", "parts": [{"text": "x"}]},
                   "configuration": {"pushNotificationConfig": {"url": "https://hooks.example.com/x"}}})
            .to_string(),
            "INVALID_PARAMS",
        ),
    ];
    for (method, path, content_type, request, why) in cases {
        let body = content_type.map(|ct| (ct, request.clone()));
        let (status, headers, text) = server
            .http_with(method, path, None, &[], body.clone())
            .await;
        assert_eq!(status, 401, "{why} without a token");
        assert!(headers.get("www-authenticate").is_some());
        let unauthorized: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(unauthorized["error"]["status"], "UNAUTHENTICATED", "{text}");

        let (status, headers, text) = server
            .http_with(method, path, None, &[("authorization", &auth)], body)
            .await;
        assert_eq!(status, 400, "{why}: {text}");
        assert!(
            headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        assert_eq!(reason(&text).0, why, "{text}");
    }
    assert!(
        server.backend.task_ids().is_empty(),
        "no malformed request created a task"
    );
    // `application/a2a+json`, the type A2A 1.0 §11.1 recommends, is accepted.
    let (status, _, text) = server
        .http_with(
            "POST",
            "/message:send",
            None,
            &[("authorization", &auth)],
            Some(("application/a2a+json", body("typed").to_string())),
        )
        .await;
    assert_eq!(status, 200, "{text}");
}

#[tokio::test]
async fn extensions_are_activated_and_echoed_on_http_json_as_on_json_rpc() {
    let server = TestServer::start(Setup {
        extensions: vec![ExtensionConfig::new(EXT_A), ExtensionConfig::new(EXT_B)],
        ..Setup::default()
    })
    .await;
    let mut request = body("ext");
    request["message"]["extensions"] = json!([EXT_B]);
    let named = format!("{EXT_A}, https://example.org/undeclared/v1");
    let (status, headers, text) = server
        .http_with(
            "POST",
            "/message:send",
            Some(TOKEN),
            &[("a2a-extensions", &named)],
            Some(("application/json", request.to_string())),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(
        headers["a2a-extensions"].to_str().unwrap(),
        format!("{EXT_A}, {EXT_B}")
    );

    // A poll carries its own header; one that names nothing declared gets none back.
    let id = serde_json::from_str::<Value>(&text).unwrap()["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_, headers, _) = server
        .http_with(
            "GET",
            &format!("/tasks/{id}"),
            Some(TOKEN),
            &[("a2a-extensions", EXT_B)],
            None,
        )
        .await;
    assert_eq!(headers["a2a-extensions"].to_str().unwrap(), EXT_B);
    let (_, headers, _) = server
        .http("GET", &format!("/tasks/{id}"), Some(TOKEN), None)
        .await;
    assert!(headers.get("a2a-extensions").is_none());
}

#[tokio::test]
async fn push_configs_round_trip_over_http_json_when_push_is_on() {
    let hook = Webhook::start().await;
    let (push, _store) = local_push(&[&hook]);
    let server = TestServer::start(Setup {
        push: Some(push),
        ..Setup::default()
    })
    .await;
    let mut request = body("[hold]");
    request["configuration"]["taskPushNotificationConfig"] =
        json!({"id": "inline", "url": hook.url(), "token": "inline-secret"});
    let (status, _, sent) = server
        .http("POST", "/message:send", Some(TOKEN), Some(&request))
        .await;
    assert_eq!(status, 200, "{sent}");
    let id = serde_json::from_str::<Value>(&sent).unwrap()["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let configs = format!("/tasks/{id}/pushNotificationConfigs");

    let created = json!({
        "id": "second", "url": hook.url(), "token": "secret-token",
        "authentication": {"scheme": "Bearer", "credentials": "secret-credentials"}
    });
    let (status, _, text) = server
        .http("POST", &configs, Some(TOKEN), Some(&created))
        .await;
    assert_eq!(status, 200, "{text}");
    let config: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        (config["taskId"].as_str(), config["id"].as_str()),
        (Some(id.as_str()), Some("second"))
    );
    assert!(!text.contains("secret"), "write-only: {text}");

    let (status, _, text) = server.http("GET", &configs, Some(TOKEN), None).await;
    assert_eq!(status, 200, "{text}");
    let mut ids: Vec<String> = serde_json::from_str::<Value>(&text).unwrap()["configs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    assert_eq!(ids, ["inline", "second"]);
    assert!(!text.contains("secret"), "{text}");

    let one = format!("{configs}/second");
    let (status, _, text) = server.http("GET", &one, Some(TOKEN), None).await;
    assert_eq!((status, text.contains("secret")), (200, false), "{text}");
    // Another caller does not see the task's configs.
    let (status, _, _) = server.http("GET", &one, Some(OTHER_TOKEN), None).await;
    assert_eq!(status, 404);
    // A config whose taskId is another task's is refused.
    let (status, _, text) = server
        .http(
            "POST",
            &configs,
            Some(TOKEN),
            Some(&json!({"taskId": "another", "url": hook.url()})),
        )
        .await;
    assert_eq!(status, 400, "{text}");

    let (status, _, _) = server.http("DELETE", &one, Some(TOKEN), None).await;
    assert_eq!(status, 200);
    let (status, _, text) = server.http("GET", &one, Some(TOKEN), None).await;
    assert_eq!(status, 404, "{text}");
    assert!(server.backend.release(&id));
}

#[tokio::test]
async fn the_extended_card_is_served_on_http_json_to_an_authenticated_caller() {
    let server = TestServer::start(Setup {
        extended: Some(ExtendedCardConfig::new().with_skill(SkillConfig::new(
            "admin",
            "Admin",
            "Only for the signed in",
        ))),
        ..Setup::default()
    })
    .await;
    let (status, _, text) = server
        .http("GET", "/extendedAgentCard", Some(TOKEN), None)
        .await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("\"admin\""), "{text}");
    let (status, _, _) = server.http("GET", "/extendedAgentCard", None, None).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn an_anonymous_server_serves_http_json_without_a_token() {
    let server = TestServer::start(Setup {
        auth: AuthConfig::AllowAnonymous,
        ..Setup::default()
    })
    .await;
    let (status, _, text) = server
        .http("POST", "/message:send", None, Some(&body("anon")))
        .await;
    assert_eq!(status, 200, "{text}");
}
