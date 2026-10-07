//! The OpenAPI 3.1 document of an A2A server: both bindings (JSON-RPC at `POST /`, HTTP+JSON at the
//! paths of [`REST_ROUTES`]) and the public discovery routes.
//!
//! Written by hand from the SDK's types (`a2a-lf` 0.3, ProtoJSON on the wire): they derive no
//! schema, and the tests hold the document to the router (every method and route, both ways) and the
//! schemas to the wire (examples and real responses validate against them).
//!
//! The document is public and the same for every caller: it is built once, from the public card and
//! the switches the card already shows ([`Flags`]). It never names a token, the extended card's
//! content or anything the public card does not say.

use a2a::jsonrpc::methods::{
    CANCEL_TASK, CREATE_PUSH_CONFIG, DELETE_PUSH_CONFIG, GET_EXTENDED_AGENT_CARD, GET_PUSH_CONFIG,
    GET_TASK, LIST_PUSH_CONFIGS, LIST_TASKS, SEND_MESSAGE, SEND_STREAMING_MESSAGE,
    SUBSCRIBE_TO_TASK,
};
use serde_json::{Map, Value, json};

use crate::auth::JWKS_PATH;
use crate::card::{AgentCardConfig, Flags};
use crate::rest::{REST_ROUTES, RestRoute};

/// The bearer scheme's name in the document (the card's is `bearer` too).
const BEARER: &str = "bearer";

/// A JSON-RPC method the endpoint serves: `a2a-server-lf` 0.4.4 `src/jsonrpc.rs` dispatches these
/// eleven names and answers any other with `MethodNotFound`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RpcMethod {
    /// The method name.
    pub(crate) name: &'static str,
    /// The schema of `params`.
    params: &'static str,
    /// The schema of `result`; `None` for a method whose result is empty.
    result: Option<&'static str>,
    /// Answered with `text/event-stream`.
    streaming: bool,
}

const fn rpc(
    name: &'static str,
    params: &'static str,
    result: Option<&'static str>,
    streaming: bool,
) -> RpcMethod {
    RpcMethod {
        name,
        params,
        result,
        streaming,
    }
}

/// Every method of the JSON-RPC endpoint, in the order the document lists them.
pub(crate) const RPC_METHODS: &[RpcMethod] = &[
    rpc(
        SEND_MESSAGE,
        "SendMessageRequest",
        Some("SendMessageResponse"),
        false,
    ),
    rpc(
        SEND_STREAMING_MESSAGE,
        "SendMessageRequest",
        Some("StreamResponse"),
        true,
    ),
    rpc(GET_TASK, "GetTaskRequest", Some("Task"), false),
    rpc(
        LIST_TASKS,
        "ListTasksRequest",
        Some("ListTasksResponse"),
        false,
    ),
    rpc(CANCEL_TASK, "CancelTaskRequest", Some("Task"), false),
    rpc(
        SUBSCRIBE_TO_TASK,
        "SubscribeToTaskRequest",
        Some("StreamResponse"),
        true,
    ),
    rpc(
        CREATE_PUSH_CONFIG,
        "TaskPushNotificationConfig",
        Some("TaskPushNotificationConfig"),
        false,
    ),
    rpc(
        GET_PUSH_CONFIG,
        "GetTaskPushNotificationConfigRequest",
        Some("TaskPushNotificationConfig"),
        false,
    ),
    rpc(
        LIST_PUSH_CONFIGS,
        "ListTaskPushNotificationConfigsRequest",
        Some("ListTaskPushNotificationConfigsResponse"),
        false,
    ),
    rpc(
        DELETE_PUSH_CONFIG,
        "DeleteTaskPushNotificationConfigRequest",
        None,
        false,
    ),
    rpc(
        GET_EXTENDED_AGENT_CARD,
        "GetExtendedAgentCardRequest",
        Some("AgentCard"),
        false,
    ),
];

/// A placeholder task id in the examples: an unknown task is `TaskNotFound`, not a parse error.
const TASK_ID: &str = "replace-with-a-task-id";
const CONFIG_ID: &str = "replace-with-a-config-id";

/// What an example message looks like: the shortest message an agent accepts.
fn example_send() -> Value {
    json!({
        "message": {
            "messageId": "swagger-1",
            "role": "ROLE_USER",
            "parts": [{"text": "Hello"}]
        },
        "configuration": {"returnImmediately": true}
    })
}

/// The example `params` (JSON-RPC) or body (HTTP+JSON) of an operation.
pub(crate) fn example_params(operation: &str) -> Value {
    match operation {
        SEND_MESSAGE | SEND_STREAMING_MESSAGE => {
            let mut send = example_send();
            if operation == SEND_STREAMING_MESSAGE
                && let Some(object) = send.as_object_mut()
            {
                object.remove("configuration");
            }
            send
        }
        GET_TASK => json!({"id": TASK_ID, "historyLength": 10}),
        LIST_TASKS => json!({"pageSize": 10}),
        CANCEL_TASK | SUBSCRIBE_TO_TASK => json!({"id": TASK_ID}),
        CREATE_PUSH_CONFIG => json!({
            "taskId": TASK_ID,
            "url": "https://hooks.example.com/a2a",
            "token": "a-value-the-webhook-checks"
        }),
        GET_PUSH_CONFIG | DELETE_PUSH_CONFIG => json!({"taskId": TASK_ID, "id": CONFIG_ID}),
        LIST_PUSH_CONFIGS => json!({"taskId": TASK_ID, "pageSize": 10}),
        _ => json!({}),
    }
}

/// The document. `signed`: the card is signed, so `GET /.well-known/jwks.json` is served.
pub(crate) fn document(card: &AgentCardConfig, flags: Flags, signed: bool) -> Value {
    let base = card.url.as_str().trim_end_matches('/').to_owned();
    let mut paths = Map::new();
    paths.insert(
        "/".to_owned(),
        json!({"post": jsonrpc_operation(card, flags, &base)}),
    );
    for route in REST_ROUTES {
        let item = paths
            .entry(route.path.to_owned())
            .or_insert_with(|| json!({}));
        item[route.method.openapi_key()] = rest_operation(route, card, flags, &base);
    }
    for (path, item) in discovery(signed) {
        paths.insert(path.to_owned(), item);
    }

    let mut components = json!({"schemas": schemas()});
    let mut doc = json!({
        "openapi": "3.1.0",
        "info": {
            "title": format!("{} (A2A 1.0)", card.name),
            "version": card.version,
            "description": info_description(card, flags),
        },
        "servers": [{
            "url": ".",
            "description": "This server, at the address this document was read from"
        }],
        "tags": [
            {"name": "JSON-RPC", "description": "A2A 1.0 over JSON-RPC 2.0: one endpoint, the method in the body. The card lists it first."},
            {"name": "HTTP+JSON", "description": "A2A 1.0 over HTTP+JSON (REST), A2A 1.0 §11. The same handler as JSON-RPC."},
            {"name": "HTTP+JSON aliases", "description": "Paths of earlier drafts that the SDK still answers. Use the paths above."},
            {"name": "Discovery", "description": "Public: no credential."}
        ],
        "paths": paths,
    });
    if flags.bearer {
        components["securitySchemes"] = json!({
            BEARER: {
                "type": "http",
                "scheme": "bearer",
                "bearerFormat": "opaque",
                "description": "One of the tokens the deployment configured (`A2A_BEARER_TOKENS`)."
            }
        });
        doc["security"] = json!([{ BEARER: [] }]);
    }
    doc["components"] = components;
    doc
}

fn info_description(card: &AgentCardConfig, flags: Flags) -> String {
    let auth = if flags.bearer {
        "Every call needs a bearer token: press **Authorize** and paste one. This page and the \
         document are public; the calls are not."
    } else {
        "This server accepts every call without a credential (`AllowAnonymous`): for local \
         development only."
    };
    format!(
        "{}\n\nAn A2A 1.0 agent. Two bindings answer at the same base URL and go through the same \
         handler: **JSON-RPC** (`POST /`, the method in the body) and **HTTP+JSON** \
         (`POST /message:send`, `GET /tasks/{{id}}`, ...). {auth}\n\nPush notifications: **{}**. \
         Extended agent card: **{}**. Streaming calls answer `text/event-stream`, which Swagger UI \
         cannot show as it arrives: their descriptions give the `curl` to use.",
        card.description,
        if flags.push { "on" } else { "off" },
        if flags.extended { "on" } else { "off" },
    )
}

fn schema_ref(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

/// What an operation does, shared by both bindings.
fn operation_text(operation: &str, flags: Flags) -> (&'static str, String) {
    let push = format!(
        "Push notifications are **{}** on this server (the card's `capabilities.pushNotifications`); \
         when off, this answers `PushNotificationNotSupported`.",
        if flags.push { "on" } else { "off" }
    );
    match operation {
        SEND_MESSAGE => (
            "Send a message",
            "Starts a task, or continues one (`message.taskId`). The example sets \
             `configuration.returnImmediately`, so the task comes back at once: poll it with \
             `GetTask`. Without it the call waits until the task finishes or needs input. The \
             `messageId` identifies the message: sending the same one again is a retry and gives \
             the same task, so change it for a new message."
                .to_owned(),
        ),
        SEND_STREAMING_MESSAGE => (
            "Send a message and stream the task",
            "Like `SendMessage`, answered with Server-Sent Events: the task, then its status and \
             artifact updates, until it finishes or needs input."
                .to_owned(),
        ),
        GET_TASK => (
            "Get a task",
            "The task as it is now. `historyLength` keeps the last messages only. Another caller's \
             task is `TaskNotFound`."
                .to_owned(),
        ),
        LIST_TASKS => (
            "List the caller's tasks",
            "The caller's own tasks, most recently updated first, a page at a time (`pageSize` 1 \
             to 100, 50 by default; `nextPageToken` is empty on the last page)."
                .to_owned(),
        ),
        CANCEL_TASK => (
            "Cancel a task",
            "Cancels a task that has not finished; a finished one is `TaskNotCancelable`."
                .to_owned(),
        ),
        SUBSCRIBE_TO_TASK => (
            "Stream a task's updates",
            "Server-Sent Events from the task's current state on, on any replica; a finished task \
             is `UnsupportedOperation`."
                .to_owned(),
        ),
        CREATE_PUSH_CONFIG => (
            "Register a webhook for a task",
            format!(
                "The deployment's policy decides which webhooks are allowed. `token` and \
                 `authentication.credentials` are write-only: no answer returns them. {push}"
            ),
        ),
        GET_PUSH_CONFIG => (
            "Get a webhook of a task",
            format!("Without `token` and `credentials`. {push}"),
        ),
        LIST_PUSH_CONFIGS => (
            "List the webhooks of a task",
            format!("Without `token` and `credentials`. {push}"),
        ),
        DELETE_PUSH_CONFIG => ("Remove a webhook of a task", push),
        GET_EXTENDED_AGENT_CARD => (
            "Get the extended agent card",
            format!(
                "What an authenticated caller sees on top of the public card. The extended card is \
                 **{}** on this server; when off, this answers `UnsupportedOperation`.",
                if flags.extended { "on" } else { "off" }
            ),
        ),
        _ => ("", String::new()),
    }
}

/// The `curl` that a streaming operation's description gives.
fn stream_curl(base: &str, path: &str, method: &str, body: Option<&Value>, bearer: bool) -> String {
    let mut curl = format!("curl -N -X {method}");
    if bearer {
        curl.push_str(" -H \"Authorization: Bearer $A2A_TOKEN\"");
    }
    if let Some(body) = body {
        curl.push_str(&format!(
            " -H 'Content-Type: application/json' --data '{body}'"
        ));
    }
    curl.push_str(&format!(" {base}{path}"));
    curl
}

fn stream_note(curl: &str) -> String {
    format!(
        "\n\n**Swagger UI cannot show a stream as it arrives**: it waits for the whole response, which \
         ends only when the task finishes or needs input. Use `curl` instead (`$A2A_TOKEN` holds a \
         bearer token; the URL is the card's):\n\n```sh\n{curl}\n```"
    )
}

/// The `A2A-Extensions` request header: which declared extensions a call activates.
fn extensions_header(card: &AgentCardConfig) -> Value {
    let declared: Vec<&str> = card.extensions.iter().map(|e| e.uri.as_str()).collect();
    let listed = if declared.is_empty() {
        "The public card declares none.".to_owned()
    } else {
        format!(
            "The public card declares: {}.",
            declared
                .iter()
                .map(|u| format!("`{u}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    json!({
        "name": "A2A-Extensions",
        "in": "header",
        "required": false,
        "schema": {"type": "string"},
        "description": format!(
            "Comma-separated URIs of the extensions this call activates; only those the card \
             declares count, and the response's `A2A-Extensions` header lists them. {listed}"
        )
    })
}

fn unauthorized_response(rest: bool) -> Value {
    let schema = if rest {
        schema_ref("RestError")
    } else {
        schema_ref("JsonRpcResponse")
    };
    json!({
        "description": "Missing or wrong bearer token (`WWW-Authenticate: Bearer`).",
        "content": {"application/json": {"schema": schema}}
    })
}

fn jsonrpc_operation(card: &AgentCardConfig, flags: Flags, base: &str) -> Value {
    let mut examples = Map::new();
    let mut methods_doc = String::new();
    for method in RPC_METHODS {
        let (summary, mut description) = operation_text(method.name, flags);
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method.name,
            "params": example_params(method.name),
        });
        if method.streaming {
            description.push_str(&stream_note(&stream_curl(
                base,
                "/",
                "POST",
                Some(&body),
                flags.bearer,
            )));
        }
        let result = match (method.result, method.streaming) {
            (Some(r), true) => format!("each event's `result`: `{r}`"),
            (Some(r), false) => format!("`result`: `{r}`"),
            (None, _) => "`result`: empty".to_owned(),
        };
        methods_doc.push_str(&format!(
            "* `{}` (`params`: `{}`): {summary}; {result}.\n",
            method.name, method.params
        ));
        examples.insert(
            method.name.to_owned(),
            json!({"summary": summary, "description": description, "value": body}),
        );
    }
    let mut operation = json!({
        "tags": ["JSON-RPC"],
        "operationId": "jsonrpc",
        "summary": "Call a JSON-RPC method",
        "description": format!(
            "One endpoint for every A2A 1.0 method: pick one in **Examples**.\n\n{methods_doc}\n\
             Errors come back as HTTP 200 with a JSON-RPC error object, A2A's codes \
             (`-32001` task not found, ...). `SendStreamingMessage` and `SubscribeToTask` answer \
             `text/event-stream`, which Swagger UI cannot show as it arrives: the examples give \
             the `curl`."
        ),
        "parameters": [extensions_header(card)],
        "requestBody": {
            "required": true,
            "content": {
                "application/json": {
                    "schema": {"oneOf": RPC_METHODS.iter()
                        .map(|m| schema_ref(&format!("JsonRpc{}Request", m.name)))
                        .collect::<Vec<_>>()},
                    "examples": examples,
                }
            }
        },
        "responses": {
            "200": {
                "description": "A JSON-RPC response (result or error), or, for a streaming method, \
                    Server-Sent Events whose `data` lines are JSON-RPC responses with a \
                    `StreamResponse` result.",
                "content": {
                    "application/json": {"schema": schema_ref("JsonRpcResponse")},
                    "text/event-stream": {"schema": {"type": "string"}}
                }
            }
        }
    });
    if flags.bearer {
        operation["responses"]["401"] = unauthorized_response(false);
    }
    operation
}

/// An operation id that is unique across the document: the A2A name, and the path for an alias.
fn rest_operation_id(route: &RestRoute) -> String {
    if !route.alias {
        return route.operation.to_owned();
    }
    let path = route
        .path
        .replace(['{', '}'], "")
        .replace(':', "__")
        .replace(['/', '-'], "_");
    format!("{}_{}{}", route.operation, route.method.openapi_key(), path)
}

fn path_parameter(name: &str, description: &str) -> Value {
    json!({
        "name": name, "in": "path", "required": true,
        "schema": {"type": "string"}, "description": description
    })
}

fn query_parameter(name: &str, schema: Value, description: &str) -> Value {
    json!({
        "name": name, "in": "query", "required": false,
        "schema": schema, "description": description
    })
}

fn page_size_schema() -> Value {
    json!({"type": "integer", "format": "int32", "minimum": 1, "maximum": 100})
}

fn json_body(schema: &str, example: Value) -> Value {
    let media = json!({"schema": schema_ref(schema), "example": example});
    json!({
        "required": true,
        "content": {"application/json": media.clone(), "application/a2a+json": media}
    })
}

fn rest_operation(route: &RestRoute, card: &AgentCardConfig, flags: Flags, base: &str) -> Value {
    let (summary, mut description) = operation_text(route.operation, flags);
    let mut parameters = Vec::new();
    if route.path.contains("{id}") {
        parameters.push(path_parameter("id", "The task's id."));
    }
    if route.path.contains("{configId}") {
        parameters.push(path_parameter("configId", "The webhook config's id."));
    }
    let mut request_body = None;
    let mut success = json!({"description": "The result.", "content": {}});
    let concrete = route
        .path
        .replace("{id}", TASK_ID)
        .replace("{configId}", CONFIG_ID);
    match route.operation {
        SEND_MESSAGE => {
            request_body = Some(json_body(
                "SendMessageRequest",
                example_params(SEND_MESSAGE),
            ));
            success["content"] =
                json!({"application/json": {"schema": schema_ref("SendMessageResponse")}});
        }
        SEND_STREAMING_MESSAGE | SUBSCRIBE_TO_TASK => {
            let body = (route.operation == SEND_STREAMING_MESSAGE)
                .then(|| example_params(SEND_STREAMING_MESSAGE));
            if let Some(body) = &body {
                request_body = Some(json_body("SendMessageRequest", body.clone()));
            }
            let method = route.method.http();
            description.push_str(&stream_note(&stream_curl(
                base,
                &concrete,
                method.as_str(),
                body.as_ref(),
                flags.bearer,
            )));
            success = json!({
                "description": "Server-Sent Events: each `data` line is a `StreamResponse` (A2A 1.0 §11.7).",
                "content": {"text/event-stream": {"schema": {"type": "string"}}}
            });
        }
        GET_TASK => {
            parameters.push(query_parameter(
                "historyLength",
                json!({"type": "integer", "format": "int32", "minimum": 0}),
                "Keep only the last messages of the history.",
            ));
            success["content"] = json!({"application/json": {"schema": schema_ref("Task")}});
        }
        LIST_TASKS => {
            parameters.extend([
                query_parameter(
                    "contextId",
                    json!({"type": "string"}),
                    "Only the tasks of this context.",
                ),
                query_parameter(
                    "status",
                    schema_ref("TaskState"),
                    "Only the tasks in this state.",
                ),
                query_parameter(
                    "pageSize",
                    page_size_schema(),
                    "Tasks per page (50 by default).",
                ),
                query_parameter(
                    "pageToken",
                    json!({"type": "string"}),
                    "The `nextPageToken` of the page before.",
                ),
                query_parameter(
                    "historyLength",
                    json!({"type": "integer", "format": "int32", "minimum": 0}),
                    "Keep only the last messages of each task's history.",
                ),
                query_parameter(
                    "statusTimestampAfter",
                    json!({"type": "string", "format": "date-time"}),
                    "Only the tasks whose status changed after this time.",
                ),
                query_parameter(
                    "includeArtifacts",
                    json!({"type": "boolean"}),
                    "Include the artifacts (left out by default).",
                ),
            ]);
            success["content"] =
                json!({"application/json": {"schema": schema_ref("ListTasksResponse")}});
        }
        CANCEL_TASK => {
            success["content"] = json!({"application/json": {"schema": schema_ref("Task")}});
        }
        CREATE_PUSH_CONFIG => {
            let mut example = example_params(CREATE_PUSH_CONFIG);
            if let Some(object) = example.as_object_mut() {
                object.remove("taskId");
            }
            request_body = Some(json_body("TaskPushNotificationConfig", example));
            success["content"] =
                json!({"application/json": {"schema": schema_ref("TaskPushNotificationConfig")}});
        }
        GET_PUSH_CONFIG => {
            success["content"] =
                json!({"application/json": {"schema": schema_ref("TaskPushNotificationConfig")}});
        }
        LIST_PUSH_CONFIGS => {
            parameters.extend([
                query_parameter("pageSize", page_size_schema(), "Configs per page."),
                query_parameter(
                    "pageToken",
                    json!({"type": "string"}),
                    "The `nextPageToken` of the page before.",
                ),
            ]);
            success["content"] = json!({"application/json": {"schema": schema_ref("ListTaskPushNotificationConfigsResponse")}});
        }
        DELETE_PUSH_CONFIG => {
            success = json!({"description": "Removed (an empty body)."});
        }
        GET_EXTENDED_AGENT_CARD => {
            success["content"] = json!({"application/json": {"schema": schema_ref("AgentCard")}});
        }
        _ => {}
    }
    parameters.push(extensions_header(card));
    if route.alias {
        description = format!(
            "An alias of an earlier draft that the SDK still answers: use `{}` instead.\n\n{description}",
            REST_ROUTES
                .iter()
                .find(|r| !r.alias && r.operation == route.operation)
                .map(|r| format!("{} {}", r.method.http(), r.path))
                .unwrap_or_default()
        );
    }
    let mut operation = json!({
        "tags": [if route.alias { "HTTP+JSON aliases" } else { "HTTP+JSON" }],
        "operationId": rest_operation_id(route),
        "summary": summary,
        "description": description,
        "parameters": parameters,
        "responses": {
            "200": success,
            "default": {
                "description": "An A2A error as `google.rpc.Status` (A2A 1.0 §11.6): the HTTP status \
                    of the error and an `ErrorInfo` detail whose `reason` names it (`TASK_NOT_FOUND`, ...).",
                "content": {"application/json": {"schema": schema_ref("RestError")}}
            }
        }
    });
    if let Some(body) = request_body {
        operation["requestBody"] = body;
    }
    if route.alias {
        operation["deprecated"] = json!(true);
    }
    if flags.bearer {
        operation["responses"]["401"] = unauthorized_response(true);
    }
    operation
}

/// The public routes, documented with no security requirement.
fn discovery(signed: bool) -> Vec<(&'static str, Value)> {
    let mut paths = vec![
        (
            a2a_server::WELL_KNOWN_AGENT_CARD_PATH,
            json!({"get": {
                "tags": ["Discovery"],
                "operationId": "getAgentCard",
                "summary": "The public agent card",
                "security": [],
                "responses": {"200": {
                    "description": "The card: the interfaces (JSON-RPC first, then HTTP+JSON, at the same URL), skills, extensions and security schemes.",
                    "content": {"application/json": {"schema": schema_ref("AgentCard")}}
                }}
            }}),
        ),
        (
            "/healthz",
            json!({"get": {
                "tags": ["Discovery"],
                "operationId": "healthz",
                "summary": "Liveness",
                "security": [],
                "responses": {"200": {
                    "description": "`ok`",
                    "content": {"text/plain": {"schema": {"type": "string", "const": "ok"}}}
                }}
            }}),
        ),
    ];
    if signed {
        paths.push((
            JWKS_PATH,
            json!({"get": {
                "tags": ["Discovery"],
                "operationId": "getJwks",
                "summary": "The key set that verifies the card's signature",
                "security": [],
                "responses": {"200": {
                    "description": "A JWK Set (RFC 7517).",
                    "content": {"application/json": {"schema": {"type": "object", "required": ["keys"], "properties": {"keys": {"type": "array", "items": {"type": "object"}}}}}}
                }}
            }}),
        ));
    }
    paths
}

fn metadata() -> Value {
    json!({"type": "object", "additionalProperties": true, "description": "Free-form; numbers come back as floats (ProtoJSON)."})
}

/// The components: the A2A 1.0 objects as ProtoJSON (camelCase members, enums as their names,
/// `int32` as numbers, timestamps as RFC 3339 strings, bytes as base64), and the envelopes.
fn schemas() -> Value {
    let entries = [
        (
            "Role",
            json!({"type": "string", "enum": ["ROLE_USER", "ROLE_AGENT", "ROLE_UNSPECIFIED"]}),
        ),
        (
            "TaskState",
            json!({"type": "string", "enum": [
                "TASK_STATE_SUBMITTED", "TASK_STATE_WORKING", "TASK_STATE_COMPLETED",
                "TASK_STATE_FAILED", "TASK_STATE_CANCELED", "TASK_STATE_INPUT_REQUIRED",
                "TASK_STATE_REJECTED", "TASK_STATE_AUTH_REQUIRED", "TASK_STATE_UNSPECIFIED"
            ]}),
        ),
        (
            "Part",
            json!({
                "type": "object",
                "description": "Exactly one of `text`, `raw` (base64), `url` or `data`.",
                "properties": {
                    "text": {"type": "string"},
                    "raw": {"type": "string", "contentEncoding": "base64"},
                    "url": {"type": "string"},
                    "data": {},
                    "filename": {"type": "string"},
                    "mediaType": {"type": "string"},
                    "metadata": metadata()
                },
                "oneOf": [
                    {"required": ["text"]}, {"required": ["raw"]},
                    {"required": ["url"]}, {"required": ["data"]}
                ]
            }),
        ),
        (
            "Message",
            json!({
                "type": "object",
                "required": ["messageId", "role", "parts"],
                "properties": {
                    "messageId": {"type": "string"},
                    "contextId": {"type": "string"},
                    "taskId": {"type": "string"},
                    "role": schema_ref("Role"),
                    "parts": {"type": "array", "items": schema_ref("Part")},
                    "metadata": metadata(),
                    "extensions": {"type": "array", "items": {"type": "string"}},
                    "referenceTaskIds": {"type": "array", "items": {"type": "string"}}
                }
            }),
        ),
        (
            "TaskStatus",
            json!({
                "type": "object",
                "required": ["state"],
                "properties": {
                    "state": schema_ref("TaskState"),
                    "message": schema_ref("Message"),
                    "timestamp": {"type": "string", "format": "date-time"}
                }
            }),
        ),
        (
            "Artifact",
            json!({
                "type": "object",
                "required": ["artifactId", "parts"],
                "properties": {
                    "artifactId": {"type": "string"},
                    "name": {"type": "string"},
                    "description": {"type": "string"},
                    "parts": {"type": "array", "items": schema_ref("Part")},
                    "metadata": metadata(),
                    "extensions": {"type": "array", "items": {"type": "string"}}
                }
            }),
        ),
        (
            "Task",
            json!({
                "type": "object",
                "required": ["id", "contextId", "status"],
                "properties": {
                    "id": {"type": "string"},
                    "contextId": {"type": "string"},
                    "status": schema_ref("TaskStatus"),
                    "artifacts": {"type": "array", "items": schema_ref("Artifact")},
                    "history": {"type": "array", "items": schema_ref("Message")},
                    "metadata": metadata()
                }
            }),
        ),
        (
            "AuthenticationInfo",
            json!({
                "type": "object",
                "required": ["scheme"],
                "properties": {
                    "scheme": {"type": "string", "description": "Sent as `Authorization: <scheme> <credentials>`."},
                    "credentials": {"type": "string", "description": "Write-only."}
                }
            }),
        ),
        (
            "TaskPushNotificationConfig",
            json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "id": {"type": "string"},
                    "taskId": {"type": "string", "description": "In HTTP+JSON, the task of the path (a different one is refused)."},
                    "url": {"type": "string"},
                    "token": {"type": "string", "description": "Sent as `A2A-Notification-Token`; write-only."},
                    "authentication": schema_ref("AuthenticationInfo"),
                    "tenant": {"type": "string"}
                }
            }),
        ),
        (
            "SendMessageConfiguration",
            json!({
                "type": "object",
                "properties": {
                    "acceptedOutputModes": {"type": "array", "items": {"type": "string"}},
                    "taskPushNotificationConfig": schema_ref("TaskPushNotificationConfig"),
                    "historyLength": {"type": "integer", "format": "int32"},
                    "returnImmediately": {"type": "boolean"}
                }
            }),
        ),
        (
            "SendMessageRequest",
            json!({
                "type": "object",
                "required": ["message"],
                "properties": {
                    "message": schema_ref("Message"),
                    "configuration": schema_ref("SendMessageConfiguration"),
                    "metadata": metadata(),
                    "tenant": {"type": "string"}
                }
            }),
        ),
        (
            "SendMessageResponse",
            json!({
                "type": "object",
                "oneOf": [
                    {"required": ["task"], "properties": {"task": schema_ref("Task")}},
                    {"required": ["message"], "properties": {"message": schema_ref("Message")}}
                ]
            }),
        ),
        (
            "TaskStatusUpdateEvent",
            json!({
                "type": "object",
                "required": ["taskId", "contextId", "status"],
                "properties": {
                    "taskId": {"type": "string"},
                    "contextId": {"type": "string"},
                    "status": schema_ref("TaskStatus"),
                    "metadata": metadata()
                }
            }),
        ),
        (
            "TaskArtifactUpdateEvent",
            json!({
                "type": "object",
                "required": ["taskId", "contextId", "artifact"],
                "properties": {
                    "taskId": {"type": "string"},
                    "contextId": {"type": "string"},
                    "artifact": schema_ref("Artifact"),
                    "append": {"type": "boolean"},
                    "lastChunk": {"type": "boolean"},
                    "metadata": metadata()
                }
            }),
        ),
        (
            "StreamResponse",
            json!({
                "type": "object",
                "description": "Exactly one of `task`, `message`, `statusUpdate` or `artifactUpdate`.",
                "oneOf": [
                    {"required": ["task"], "properties": {"task": schema_ref("Task")}},
                    {"required": ["message"], "properties": {"message": schema_ref("Message")}},
                    {"required": ["statusUpdate"], "properties": {"statusUpdate": schema_ref("TaskStatusUpdateEvent")}},
                    {"required": ["artifactUpdate"], "properties": {"artifactUpdate": schema_ref("TaskArtifactUpdateEvent")}}
                ]
            }),
        ),
        (
            "GetTaskRequest",
            json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": {"type": "string"},
                    "historyLength": {"type": "integer", "format": "int32"},
                    "tenant": {"type": "string"}
                }
            }),
        ),
        (
            "ListTasksRequest",
            json!({
                "type": "object",
                "properties": {
                    "contextId": {"type": "string"},
                    "status": schema_ref("TaskState"),
                    "pageSize": page_size_schema(),
                    "pageToken": {"type": "string"},
                    "historyLength": {"type": "integer", "format": "int32"},
                    "statusTimestampAfter": {"type": "string", "format": "date-time"},
                    "includeArtifacts": {"type": "boolean"},
                    "tenant": {"type": "string"}
                }
            }),
        ),
        (
            "ListTasksResponse",
            json!({
                "type": "object",
                "description": "ProtoJSON leaves out members that hold their default (an empty list, `0`, `\"\"`).",
                "properties": {
                    "tasks": {"type": "array", "items": schema_ref("Task")},
                    "nextPageToken": {"type": "string"},
                    "pageSize": {"type": "integer", "format": "int32"},
                    "totalSize": {"type": "integer", "format": "int32"}
                }
            }),
        ),
        (
            "CancelTaskRequest",
            json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": {"type": "string"},
                    "metadata": metadata(),
                    "tenant": {"type": "string"}
                }
            }),
        ),
        (
            "SubscribeToTaskRequest",
            json!({
                "type": "object",
                "required": ["id"],
                "properties": {"id": {"type": "string"}, "tenant": {"type": "string"}}
            }),
        ),
        (
            "GetTaskPushNotificationConfigRequest",
            json!({
                "type": "object",
                "required": ["taskId", "id"],
                "properties": {"taskId": {"type": "string"}, "id": {"type": "string"}, "tenant": {"type": "string"}}
            }),
        ),
        (
            "DeleteTaskPushNotificationConfigRequest",
            json!({
                "type": "object",
                "required": ["taskId", "id"],
                "properties": {"taskId": {"type": "string"}, "id": {"type": "string"}, "tenant": {"type": "string"}}
            }),
        ),
        (
            "ListTaskPushNotificationConfigsRequest",
            json!({
                "type": "object",
                "required": ["taskId"],
                "properties": {
                    "taskId": {"type": "string"},
                    "pageSize": page_size_schema(),
                    "pageToken": {"type": "string"},
                    "tenant": {"type": "string"}
                }
            }),
        ),
        (
            "ListTaskPushNotificationConfigsResponse",
            json!({
                "type": "object",
                "properties": {
                    "configs": {"type": "array", "items": schema_ref("TaskPushNotificationConfig")},
                    "nextPageToken": {"type": "string"}
                }
            }),
        ),
        (
            "GetExtendedAgentCardRequest",
            json!({
                "type": "object",
                "properties": {"tenant": {"type": "string"}}
            }),
        ),
        (
            "AgentInterface",
            json!({
                "type": "object",
                "required": ["url", "protocolBinding", "protocolVersion"],
                "properties": {
                    "url": {"type": "string"},
                    "protocolBinding": {"type": "string", "examples": ["JSONRPC", "HTTP+JSON"]},
                    "protocolVersion": {"type": "string"},
                    "tenant": {"type": "string"}
                }
            }),
        ),
        (
            "AgentCard",
            json!({
                "type": "object",
                "description": "The agent card (A2A 1.0 §4.4.1); the members other than these are described there.",
                "required": ["name", "description", "version", "supportedInterfaces", "capabilities"],
                "properties": {
                    "name": {"type": "string"},
                    "description": {"type": "string"},
                    "version": {"type": "string"},
                    "supportedInterfaces": {"type": "array", "items": schema_ref("AgentInterface")},
                    "capabilities": {"type": "object"},
                    "defaultInputModes": {"type": "array", "items": {"type": "string"}},
                    "defaultOutputModes": {"type": "array", "items": {"type": "string"}},
                    "skills": {"type": "array", "items": {"type": "object"}},
                    "securitySchemes": {"type": "object"},
                    "securityRequirements": {"type": "array"},
                    "signatures": {"type": "array"}
                }
            }),
        ),
        ("JsonRpcId", json!({"type": ["string", "integer", "null"]})),
        (
            "JsonRpcError",
            json!({
                "type": "object",
                "required": ["code", "message"],
                "properties": {
                    "code": {"type": "integer"},
                    "message": {"type": "string"},
                    "data": {}
                }
            }),
        ),
        (
            "JsonRpcResponse",
            json!({
                "type": "object",
                "required": ["jsonrpc", "id"],
                "properties": {
                    "jsonrpc": {"type": "string", "const": "2.0"},
                    "id": schema_ref("JsonRpcId"),
                    "result": {},
                    "error": schema_ref("JsonRpcError")
                },
                "oneOf": [{"required": ["result"]}, {"required": ["error"]}]
            }),
        ),
        (
            "RestError",
            json!({
                "type": "object",
                "required": ["error"],
                "properties": {
                    "error": {
                        "type": "object",
                        "required": ["code", "status", "message"],
                        "properties": {
                            "code": {"type": "integer"},
                            "status": {"type": "string"},
                            "message": {"type": "string"},
                            "details": {
                                "type": "array",
                                "items": {"type": "object", "required": ["@type"], "properties": {"@type": {"type": "string"}}}
                            }
                        }
                    }
                }
            }),
        ),
    ];
    let mut schemas: Map<String, Value> = entries
        .into_iter()
        .map(|(name, schema)| (name.to_owned(), schema))
        .collect();
    for method in RPC_METHODS {
        schemas.insert(
            format!("JsonRpc{}Request", method.name),
            json!({
                "type": "object",
                "title": method.name,
                "required": ["jsonrpc", "id", "method", "params"],
                "properties": {
                    "jsonrpc": {"type": "string", "const": "2.0"},
                    "id": schema_ref("JsonRpcId"),
                    "method": {"type": "string", "const": method.name},
                    "params": schema_ref(method.params)
                }
            }),
        );
    }
    Value::Object(schemas)
}
