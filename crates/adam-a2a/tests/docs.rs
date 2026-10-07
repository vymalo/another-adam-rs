//! The OpenAPI document and Swagger UI: public, the same for every caller, and held to the router
//! (every JSON-RPC method and every HTTP+JSON route, both ways) and to the wire (its schemas
//! validate the examples and real responses).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use std::collections::BTreeSet;

use a2a::jsonrpc::methods;
use adam_a2a::{AuthConfig, ExtendedCardConfig, ExtensionConfig, SkillConfig};
use serde_json::{Value, json};
use support::{OTHER_TOKEN, Setup, TOKEN, TestServer, Webhook, local_push};

/// `https://spec.openapis.org/oas/3.1/schema/2022-10-07`.
const OAS_3_1_SCHEMA: &str = include_str!("fixtures/openapi-3.1-schema-2022-10-07.json");

async fn document(server: &TestServer) -> Value {
    let (status, headers, body) = server.http("GET", "/openapi.json", None, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(headers["content-type"], "application/json");
    serde_json::from_str(&body).unwrap()
}

/// A validator for the component schema `name` of `doc`.
fn validator(doc: &Value, name: &str) -> jsonschema::Validator {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": format!("#/components/schemas/{name}"),
        "components": {"schemas": doc["components"]["schemas"]},
    });
    jsonschema::draft202012::new(&schema).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[track_caller]
fn assert_valid(doc: &Value, name: &str, instance: &Value) {
    let validator = validator(doc, name);
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{name}: {errors:?}\n{instance:#}");
}

/// The name of the schema a `$ref` points to.
fn ref_name(value: &Value) -> &str {
    value["$ref"]
        .as_str()
        .and_then(|r| r.strip_prefix("#/components/schemas/"))
        .unwrap_or_else(|| panic!("not a schema $ref: {value}"))
}

fn json_rpc_entries(doc: &Value) -> Vec<String> {
    doc["paths"]["/"]["post"]["requestBody"]["content"]["application/json"]["schema"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| ref_name(r).to_owned())
        .collect()
}

/// The paths of the HTTP+JSON binding in the document, with their methods.
fn rest_operations(doc: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, operation) in item.as_object().unwrap() {
            let tags = operation["tags"].as_array().unwrap();
            if tags
                .iter()
                .any(|t| t.as_str().unwrap().starts_with("HTTP+JSON"))
            {
                out.push((method.to_uppercase(), path.clone()));
            }
        }
    }
    out
}

fn concrete(path: &str) -> String {
    path.replace("{id}", "t-1").replace("{configId}", "c-1")
}

fn collect_refs<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get("$ref") {
                out.push(r);
            }
            map.values().for_each(|v| collect_refs(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| collect_refs(v, out)),
        _ => {}
    }
}

// ------------------------------------------------------------------- the page

#[tokio::test]
async fn the_page_and_the_document_are_public_and_calls_are_not() {
    let server = TestServer::start(Setup::default()).await;

    let (status, headers, _) = server.http("GET", "/docs", None, None).await;
    assert_eq!(status, 303);
    assert_eq!(
        headers["location"], "docs/",
        "relative, so a nested router keeps its prefix"
    );

    let (status, headers, index) = server.http("GET", "/docs/", None, None).await;
    assert_eq!(status, 200, "{index}");
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(
        csp.contains("default-src 'none'") && csp.contains("script-src 'self'"),
        "{csp}"
    );
    assert!(
        !csp.contains("http") && !csp.contains('*') && !csp.contains("unsafe-eval"),
        "only this origin: {csp}"
    );
    assert_eq!(headers["x-content-type-options"], "nosniff");

    // Every file the page loads is served, publicly, under the same policy; nothing is external.
    let mut loaded = Vec::new();
    for attribute in ["src=\"", "href=\""] {
        for piece in index.split(attribute).skip(1) {
            loaded.push(
                piece
                    .split('"')
                    .next()
                    .unwrap()
                    .trim_start_matches("./")
                    .to_owned(),
            );
        }
    }
    assert!(loaded.len() >= 6, "{loaded:?}");
    for file in &loaded {
        assert!(!file.contains("//"), "external asset {file}");
        let (status, headers, _) = server
            .http("GET", &format!("/docs/{file}"), None, None)
            .await;
        assert_eq!(status, 200, "{file}");
        assert_eq!(headers["content-security-policy"], csp, "{file}");
    }
    let (_, _, initializer) = server
        .http("GET", "/docs/swagger-initializer.js", None, None)
        .await;
    assert!(initializer.contains("\"../openapi.json\""), "{initializer}");
    assert!(
        initializer.contains("\"validatorUrl\": \"none\""),
        "{initializer}"
    );
    assert!(!initializer.contains("petstore"), "{initializer}");

    document(&server).await;

    // Calls still need the token, on both bindings; files the page does not load are not public.
    for (method, path) in [
        ("POST", "/message:send"),
        ("GET", "/tasks"),
        ("POST", "/"),
        ("GET", "/docs/oauth2-redirect.html"),
        ("POST", "/openapi.json"),
    ] {
        let (status, _, _) = server.http(method, path, None, Some(&json!({}))).await;
        assert_eq!(status, 401, "{method} {path}");
    }
    let (status, _, _) = server
        .http("GET", "/docs/oauth2-redirect.html", Some(TOKEN), None)
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn turned_off_the_docs_are_closed_like_any_unknown_route() {
    let server = TestServer::start(Setup {
        docs: false,
        ..Setup::default()
    })
    .await;
    for path in ["/docs", "/docs/", "/docs/index.html", "/openapi.json"] {
        let (status, _, _) = server.http("GET", path, None, None).await;
        assert_eq!(status, 401, "{path} without a token");
        let (status, _, _) = server.http("GET", path, Some(TOKEN), None).await;
        assert_eq!(status, 404, "{path} with a token");
    }
    // The bindings are unchanged.
    let (status, _, _) = server.http("GET", "/tasks", Some(TOKEN), None).await;
    assert_eq!(status, 200);
}

// --------------------------------------------------------------- the document

#[tokio::test]
async fn the_document_is_openapi_3_1_and_every_reference_resolves() {
    let server = TestServer::start(Setup::default()).await;
    let doc = document(&server).await;
    assert_eq!(doc["openapi"], "3.1.0");
    // The official JSON Schema of OpenAPI 3.1 documents (OAI, 2022-10-07; vendored, Apache-2.0,
    // see `third-party-notices.md`). It checks the document, not the Schema Objects: those are
    // compiled as JSON Schema 2020-12 below.
    let oas: Value = serde_json::from_str(OAS_3_1_SCHEMA).unwrap();
    let meta = jsonschema::draft202012::new(&oas).unwrap();
    let errors: Vec<String> = meta
        .iter_errors(&doc)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "not an OpenAPI 3.1 document: {errors:#?}"
    );

    let mut refs = Vec::new();
    collect_refs(&doc, &mut refs);
    assert!(refs.len() > 50);
    for r in refs {
        let pointer = r
            .strip_prefix('#')
            .unwrap_or_else(|| panic!("external $ref {r}"));
        assert!(doc.pointer(pointer).is_some(), "{r} does not resolve");
    }

    let mut ids = BTreeSet::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, operation) in item.as_object().unwrap() {
            let id = operation["operationId"].as_str().unwrap();
            assert!(ids.insert(id.to_owned()), "operationId {id} twice");
            assert!(operation["responses"]["200"].is_object(), "{method} {path}");
            let declared: BTreeSet<&str> = operation["parameters"]
                .as_array()
                .map(|ps| {
                    ps.iter()
                        .filter(|p| p["in"] == "path")
                        .map(|p| p["name"].as_str().unwrap())
                        .collect()
                })
                .unwrap_or_default();
            let templated: BTreeSet<&str> = path
                .split('{')
                .skip(1)
                .map(|s| s.split('}').next().unwrap())
                .collect();
            assert_eq!(declared, templated, "{method} {path}");
        }
    }
    // Every schema compiles as JSON Schema 2020-12.
    for name in doc["components"]["schemas"].as_object().unwrap().keys() {
        validator(&doc, name);
    }
}

#[tokio::test]
async fn every_json_rpc_method_is_in_the_one_of_and_the_reverse() {
    let server = TestServer::start(Setup::default()).await;
    let doc = document(&server).await;
    let schemas = &doc["components"]["schemas"];
    let documented: BTreeSet<String> = json_rpc_entries(&doc)
        .iter()
        .map(|name| {
            schemas[name]["properties"]["method"]["const"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    // What `a2a-server-lf` 0.4.4's `jsonrpc.rs` dispatches (`method_tripwire` pins the version).
    let served: BTreeSet<String> = [
        methods::SEND_MESSAGE,
        methods::SEND_STREAMING_MESSAGE,
        methods::GET_TASK,
        methods::LIST_TASKS,
        methods::CANCEL_TASK,
        methods::SUBSCRIBE_TO_TASK,
        methods::CREATE_PUSH_CONFIG,
        methods::GET_PUSH_CONFIG,
        methods::LIST_PUSH_CONFIGS,
        methods::DELETE_PUSH_CONFIG,
        methods::GET_EXTENDED_AGENT_CARD,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(documented, served);

    // Each named example is valid against its entry and reaches its method: no parse error and
    // no `MethodNotFound`. Any other method is `MethodNotFound`.
    let examples =
        doc["paths"]["/"]["post"]["requestBody"]["content"]["application/json"]["examples"]
            .as_object()
            .unwrap();
    assert_eq!(
        examples.keys().cloned().collect::<BTreeSet<_>>(),
        served,
        "one example per method"
    );
    for (method, example) in examples {
        let body = &example["value"];
        assert_valid(&doc, &format!("JsonRpc{method}Request"), body);
        let (status, headers, text) = server.http("POST", "/", Some(TOKEN), Some(body)).await;
        assert_eq!(status, 200, "{method}: {text}");
        if headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
        {
            assert!(text.contains("data:"), "{method}: {text}");
            continue;
        }
        let response: Value = serde_json::from_str(&text).unwrap();
        assert_valid(&doc, "JsonRpcResponse", &response);
        let code = response["error"]["code"].as_i64().unwrap_or(0);
        assert!(
            ![-32601, -32700, -32600, -32602].contains(&code),
            "{method}: {response}"
        );
    }
    let (_, _, text) = server
        .http(
            "POST",
            "/",
            Some(TOKEN),
            Some(&json!({"jsonrpc": "2.0", "id": 1, "method": "NoSuchMethod", "params": {}})),
        )
        .await;
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["error"]["code"],
        -32601
    );
}

/// The document's tables mirror `a2a-server-lf` 0.4.4 (`src/jsonrpc.rs`, `src/rest.rs`); axum cannot
/// list a router's routes, so a new SDK version must be re-read before this is moved.
#[test]
fn method_tripwire() {
    let lock = include_str!("../../../Cargo.lock");
    let version = lock
        .split("[[package]]")
        .find(|p| p.contains("\nname = \"a2a-server-lf\"\n"))
        .and_then(|p| p.lines().find(|l| l.starts_with("version = ")))
        .unwrap();
    assert_eq!(
        version, "version = \"0.4.4\"",
        "a2a-server-lf moved: re-read its jsonrpc.rs and rest.rs, update RPC_METHODS and \
         REST_ROUTES (crates/adam-a2a/src/openapi.rs, src/rest.rs), then this test"
    );
}

#[tokio::test]
async fn every_http_json_route_is_documented_and_the_reverse() {
    let server = TestServer::start(Setup::default()).await;
    let doc = document(&server).await;
    let operations = rest_operations(&doc);
    assert_eq!(operations.len(), 22, "{operations:?}");

    // Every documented operation is routed: the router answers it (a handler's 404 carries an A2A
    // error body; the router's has none) and never with 405.
    for (method, path) in &operations {
        let operation = &doc["paths"][path][method.to_lowercase()];
        let body = operation["requestBody"]["content"]["application/json"]["example"].clone();
        let body = (!body.is_null()).then_some(body);
        let (status, _, text) = server
            .http(method, &concrete(path), Some(TOKEN), body.as_ref())
            .await;
        assert_ne!(status, 405, "{method} {path}");
        assert!(
            status != 404 || text.contains("TASK_NOT_FOUND"),
            "{method} {path} is not routed: {status} {text}"
        );
    }

    // And the router serves nothing else on these paths, with two exceptions that are the colon
    // syntax of A2A 1.0 §11.3, not routes: `GET /tasks/x:cancel` is `GetTask` of a task named
    // `x:cancel`, and `POST /tasks/{id}` is the SDK's dispatcher of `:cancel` and `:subscribe`,
    // which answers `INVALID_REQUEST` ("unsupported task action") to anything else.
    let paths: BTreeSet<&String> = operations.iter().map(|(_, p)| p).collect();
    for path in paths {
        for method in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
            if operations.iter().any(|(m, p)| m == method && p == path) {
                continue;
            }
            let (status, _, text) = server
                .http(method, &concrete(path), Some(TOKEN), Some(&json!({})))
                .await;
            let colon_get = method == "GET" && path.starts_with("/tasks/{id}:");
            let dispatcher = method == "POST" && path == "/tasks/{id}";
            if colon_get {
                assert!(text.contains("TASK_NOT_FOUND"), "{method} {path}: {text}");
            } else if dispatcher {
                assert!(
                    text.contains("unsupported task action"),
                    "{method} {path}: {text}"
                );
            } else {
                assert_eq!(
                    status, 405,
                    "{method} {path} is served but not documented: {text}"
                );
            }
        }
    }
}

#[tokio::test]
async fn the_schemas_hold_the_examples_and_real_responses() {
    let hook = Webhook::start().await;
    let (push, _store) = local_push(&[&hook]);
    let server = TestServer::start(Setup {
        push: Some(push),
        extended: Some(ExtendedCardConfig::new().with_description("Longer")),
        ..Setup::default()
    })
    .await;
    let doc = document(&server).await;

    // Every request example matches its schema.
    for (method, path) in rest_operations(&doc) {
        let body = &doc["paths"][&path][method.to_lowercase()]["requestBody"];
        for media in ["application/json", "application/a2a+json"] {
            let content = &body["content"][media];
            if content.is_object() {
                assert_valid(&doc, ref_name(&content["schema"]), &content["example"]);
            }
        }
    }

    // Real answers match the schemas the document gives for them.
    let send = doc["paths"]["/message:send"]["post"]["requestBody"]["content"]["application/json"]
        ["example"]
        .clone();
    let mut send = send;
    send["message"]["parts"] = json!([{"text": "schemas"}, {"data": {"n": 1}}]);
    let (status, _, sent) = server
        .http("POST", "/message:send", Some(TOKEN), Some(&send))
        .await;
    assert_eq!(status, 200, "{sent}");
    let sent: Value = serde_json::from_str(&sent).unwrap();
    assert_valid(&doc, "SendMessageResponse", &sent);
    let id = sent["task"]["id"].as_str().unwrap().to_owned();
    let client = server.client(Some(TOKEN)).await;
    support::wait_for_state(&client, &id, a2a::TaskState::Completed).await;

    let checks = [
        ("GET", format!("/tasks/{id}?historyLength=5"), "Task"),
        (
            "GET",
            "/tasks?includeArtifacts=true".to_owned(),
            "ListTasksResponse",
        ),
        (
            "GET",
            "/tasks?status=TASK_STATE_REJECTED".to_owned(),
            "ListTasksResponse",
        ),
        ("GET", "/extendedAgentCard".to_owned(), "AgentCard"),
        (
            "GET",
            "/.well-known/agent-card.json".to_owned(),
            "AgentCard",
        ),
        ("GET", "/tasks/nope".to_owned(), "RestError"),
        ("POST", format!("/tasks/{id}:cancel"), "RestError"),
        (
            "GET",
            format!("/tasks/{id}/pushNotificationConfigs"),
            "ListTaskPushNotificationConfigsResponse",
        ),
    ];
    for (method, path, schema) in checks {
        let (_, _, text) = server.http(method, &path, Some(TOKEN), None).await;
        assert_valid(&doc, schema, &serde_json::from_str(&text).unwrap());
    }
    let (status, _, text) = server
        .http(
            "POST",
            &format!("/tasks/{id}/pushNotificationConfigs"),
            Some(TOKEN),
            Some(&json!({"id": "c", "url": hook.url(), "token": "t"})),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    assert_valid(
        &doc,
        "TaskPushNotificationConfig",
        &serde_json::from_str(&text).unwrap(),
    );
    let (status, _, text) = server.http("GET", "/tasks", None, None).await;
    assert_eq!(status, 401);
    assert_valid(&doc, "RestError", &serde_json::from_str(&text).unwrap());
    let (status, _, text) = server.http("POST", "/", None, Some(&json!({}))).await;
    assert_eq!(status, 401);
    assert_valid(
        &doc,
        "JsonRpcResponse",
        &serde_json::from_str(&text).unwrap(),
    );

    // Each event of a stream, on both bindings.
    let mut streamed = send.clone();
    streamed["message"]["messageId"] = json!("streamed");
    if let Some(object) = streamed.as_object_mut() {
        object.remove("configuration");
    }
    let (_, headers, text) = server
        .http("POST", "/message:stream", Some(TOKEN), Some(&streamed))
        .await;
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let events: Vec<Value> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| serde_json::from_str(d.trim()).unwrap())
        .collect();
    assert!(events.len() >= 3, "{text}");
    for event in &events {
        assert_valid(&doc, "StreamResponse", event);
    }
    streamed["message"]["messageId"] = json!("streamed-rpc");
    let rpc =
        json!({"jsonrpc": "2.0", "id": 7, "method": "SendStreamingMessage", "params": streamed});
    let (_, _, text) = server.http("POST", "/", Some(TOKEN), Some(&rpc)).await;
    let frames: Vec<Value> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| serde_json::from_str(d.trim()).unwrap())
        .collect();
    assert!(frames.len() >= 3, "{text}");
    for frame in &frames {
        assert_valid(&doc, "JsonRpcResponse", frame);
        assert_valid(&doc, "StreamResponse", &frame["result"]);
    }
}

#[tokio::test]
async fn the_document_is_the_same_for_every_caller_and_says_nothing_secret() {
    let server = TestServer::start(Setup {
        extensions: vec![ExtensionConfig::new("https://example.org/public/v1")],
        extended: Some(
            ExtendedCardConfig::new()
                .with_description("EXTENDED-ONLY description")
                .with_skill(SkillConfig::new(
                    "hidden-skill",
                    "Hidden",
                    "EXTENDED-ONLY skill",
                ))
                .with_extension(ExtensionConfig::new("https://example.org/extended-only/v1")),
        ),
        ..Setup::default()
    })
    .await;
    let anonymous = server.http("GET", "/openapi.json", None, None).await.2;
    for token in [TOKEN, OTHER_TOKEN] {
        assert_eq!(
            server
                .http("GET", "/openapi.json", Some(token), None)
                .await
                .2,
            anonymous
        );
    }
    for secret in [
        TOKEN,
        OTHER_TOKEN,
        "EXTENDED-ONLY",
        "hidden-skill",
        "extended-only/v1",
    ] {
        assert!(!anonymous.contains(secret), "the document names {secret}");
    }
    assert!(
        anonymous.contains("https://example.org/public/v1"),
        "public extensions are listed"
    );

    // The bearer scheme is declared, and every operation but discovery needs it.
    let doc: Value = serde_json::from_str(&anonymous).unwrap();
    assert_eq!(
        doc["components"]["securitySchemes"]["bearer"],
        json!({"type": "http", "scheme": "bearer", "bearerFormat": "opaque",
               "description": "One of the tokens the deployment configured (`A2A_BEARER_TOKENS`)."})
    );
    assert_eq!(doc["security"], json!([{"bearer": []}]));
    for path in ["/.well-known/agent-card.json", "/healthz"] {
        assert_eq!(doc["paths"][path]["get"]["security"], json!([]), "{path}");
    }
    // The streaming description gives a curl to the card's URL.
    let stream = doc["paths"]["/message:stream"]["post"]["description"]
        .as_str()
        .unwrap();
    assert!(
        stream.contains(&format!("{}/message:stream", server.base()))
            && stream.contains("Authorization: Bearer $A2A_TOKEN")
            && stream.contains("cannot show a stream"),
        "{stream}"
    );
}

#[tokio::test]
async fn an_anonymous_server_declares_no_security() {
    let server = TestServer::start(Setup {
        auth: AuthConfig::AllowAnonymous,
        ..Setup::default()
    })
    .await;
    let doc = document(&server).await;
    assert!(doc["components"].get("securitySchemes").is_none());
    assert!(doc.get("security").is_none());
    assert!(!doc.to_string().contains("\"401\""), "no 401 is documented");
}
