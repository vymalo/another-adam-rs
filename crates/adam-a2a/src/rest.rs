//! The HTTP+JSON (REST) binding of A2A 1.0, mounted beside JSON-RPC.
//!
//! The binding is the SDK's own (`a2a_server::rest::rest_router`), over the **same**
//! [`BackendHandler`](crate::handler::BackendHandler) as the JSON-RPC endpoint, so caller identity,
//! extension activation, push configs, `ListTasks`, the extended card and the mapping of a
//! [`BackendError`](crate::BackendError) are one code path for both bindings. What this module adds
//! is what the SDK's router does not do, the same things the JSON-RPC endpoint gets from
//! `server.rs`:
//!
//! * [`rejections`]: the extractor's plain-text 400/413/415/422 (a body that is not JSON, a query
//!   string that does not parse, a body over the limit) become the binding's own error envelope
//!   (`google.rpc.Status` with an `ErrorInfo`, *verified* 2026-10-07,
//!   <https://a2a-protocol.org/latest/specification/> §11.6), and a send whose configuration names
//!   the push config the way earlier drafts did is refused, as on JSON-RPC.
//! * [`error_response`]: that envelope. The SDK builds the same one in `rest.rs`, but privately.
//!
//! [`REST_ROUTES`] lists what `rest_router` mounts in `a2a-server-lf` 0.4.4: the paths of the
//! specification and the aliases the SDK keeps from earlier drafts. The OpenAPI document is
//! generated from it, and the tests check it against the router in both directions.

use std::collections::HashMap;

use a2a::error_code::{
    CONTENT_TYPE_NOT_SUPPORTED, EXTENDED_CARD_NOT_CONFIGURED, EXTENSION_SUPPORT_REQUIRED,
    INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR,
    PUSH_NOTIFICATION_NOT_SUPPORTED, TASK_NOT_CANCELABLE, TASK_NOT_FOUND, UNSUPPORTED_OPERATION,
    VERSION_NOT_SUPPORTED,
};
use a2a::{A2AError, FieldViolation, TypedDetail, errordetails};
use a2a_server::jsonrpc::MAX_REQUEST_BODY_BYTES;
use axum::Json;
use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// One route of the HTTP+JSON binding: what `rest_router` serves, as the OpenAPI document names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RestRoute {
    /// The HTTP method.
    pub(crate) method: RestMethod,
    /// The path, in OpenAPI form (`{id}`, `{configId}`), relative to the interface URL.
    pub(crate) path: &'static str,
    /// The A2A operation it serves (the JSON-RPC method of the same name).
    pub(crate) operation: &'static str,
    /// A path of an earlier draft the SDK still answers: documented as deprecated.
    pub(crate) alias: bool,
}

/// The HTTP methods the binding uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestMethod {
    Get,
    Post,
    Delete,
}

const fn route(
    method: RestMethod,
    path: &'static str,
    operation: &'static str,
    alias: bool,
) -> RestRoute {
    RestRoute {
        method,
        path,
        operation,
        alias,
    }
}

use RestMethod::{Delete, Get, Post};
use a2a::jsonrpc::methods::{
    CANCEL_TASK, CREATE_PUSH_CONFIG, DELETE_PUSH_CONFIG, GET_EXTENDED_AGENT_CARD, GET_PUSH_CONFIG,
    GET_TASK, LIST_PUSH_CONFIGS, LIST_TASKS, SEND_MESSAGE, SEND_STREAMING_MESSAGE,
    SUBSCRIBE_TO_TASK,
};

/// Every route `a2a_server::rest::rest_router` mounts (`a2a-server-lf` 0.4.4, `src/rest.rs`): the
/// paths of A2A 1.0 §11.3 (*verified* 2026-10-07, <https://a2a-protocol.org/latest/specification/>)
/// first, then the aliases. `GET /tasks/{id}:subscribe` is the SDK's: the specification has `POST`.
pub(crate) const REST_ROUTES: &[RestRoute] = &[
    route(Post, "/message:send", SEND_MESSAGE, false),
    route(Post, "/message:stream", SEND_STREAMING_MESSAGE, false),
    route(Get, "/tasks/{id}", GET_TASK, false),
    route(Get, "/tasks", LIST_TASKS, false),
    route(Post, "/tasks/{id}:cancel", CANCEL_TASK, false),
    route(Post, "/tasks/{id}:subscribe", SUBSCRIBE_TO_TASK, false),
    route(
        Post,
        "/tasks/{id}/pushNotificationConfigs",
        CREATE_PUSH_CONFIG,
        false,
    ),
    route(
        Get,
        "/tasks/{id}/pushNotificationConfigs",
        LIST_PUSH_CONFIGS,
        false,
    ),
    route(
        Get,
        "/tasks/{id}/pushNotificationConfigs/{configId}",
        GET_PUSH_CONFIG,
        false,
    ),
    route(
        Delete,
        "/tasks/{id}/pushNotificationConfigs/{configId}",
        DELETE_PUSH_CONFIG,
        false,
    ),
    route(Get, "/extendedAgentCard", GET_EXTENDED_AGENT_CARD, false),
    // Aliases of earlier drafts, kept by the SDK.
    route(Post, "/message/send", SEND_MESSAGE, true),
    route(Post, "/message/stream", SEND_STREAMING_MESSAGE, true),
    route(Get, "/tasks/{id}:subscribe", SUBSCRIBE_TO_TASK, true),
    route(Post, "/tasks/{id}/cancel", CANCEL_TASK, true),
    route(Get, "/tasks/{id}/subscribe", SUBSCRIBE_TO_TASK, true),
    route(Post, "/tasks/{id}/subscribe", SUBSCRIBE_TO_TASK, true),
    route(Post, "/tasks/{id}/push-configs", CREATE_PUSH_CONFIG, true),
    route(Get, "/tasks/{id}/push-configs", LIST_PUSH_CONFIGS, true),
    route(
        Get,
        "/tasks/{id}/push-configs/{configId}",
        GET_PUSH_CONFIG,
        true,
    ),
    route(
        Delete,
        "/tasks/{id}/push-configs/{configId}",
        DELETE_PUSH_CONFIG,
        true,
    ),
    route(Get, "/agent-card/extended", GET_EXTENDED_AGENT_CARD, true),
];

/// Whether `path` is one the binding sends a message on (`SendMessage`, `SendStreamingMessage`):
/// its body is a `SendMessageRequest`, with the message's own `extensions`.
pub(crate) fn is_send_path(path: &str) -> bool {
    REST_ROUTES
        .iter()
        .any(|r| r.path == path && matches!(r.operation, SEND_MESSAGE | SEND_STREAMING_MESSAGE))
}

/// `message.extensions` of a REST send body; nothing for any other request.
pub(crate) fn message_extensions(path: &str, body: &[u8]) -> Vec<String> {
    if !is_send_path(path) {
        return Vec::new();
    }
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Vec::new();
    };
    request["message"]["extensions"]
        .as_array()
        .map(|uris| {
            uris.iter()
                .filter_map(|u| u.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The binding's error response: the HTTP status of the A2A error, and a `google.rpc.Status` body
/// whose `details` end with an `ErrorInfo` naming the A2A error (A2A 1.0 §11.6). The same envelope
/// the SDK's `rest.rs` builds for errors from the handler (its builder is private).
pub(crate) fn error_response(error: A2AError) -> Response {
    let status =
        StatusCode::from_u16(error.http_status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let metadata = HashMap::from([("timestamp".to_owned(), chrono::Utc::now().to_rfc3339())]);
    let mut details = error.details.unwrap_or_default();
    details.push(TypedDetail::error_info(
        a2a::error_reason(error.code),
        errordetails::PROTOCOL_DOMAIN,
        Some(metadata),
    ));
    let body = serde_json::json!({
        "error": {
            "code": status.as_u16(),
            "status": grpc_status(error.code),
            "message": error.message,
            "details": details,
        }
    });
    (status, Json(body)).into_response()
}

/// The `google.rpc.Code` name of an A2A error, as the SDK's `rest.rs` maps it.
fn grpc_status(code: i32) -> &'static str {
    match code {
        TASK_NOT_FOUND => "NOT_FOUND",
        METHOD_NOT_FOUND => "UNIMPLEMENTED",
        TASK_NOT_CANCELABLE
        | EXTENDED_CARD_NOT_CONFIGURED
        | EXTENSION_SUPPORT_REQUIRED
        | PUSH_NOTIFICATION_NOT_SUPPORTED
        | UNSUPPORTED_OPERATION
        | VERSION_NOT_SUPPORTED => "FAILED_PRECONDITION",
        CONTENT_TYPE_NOT_SUPPORTED | PARSE_ERROR | INVALID_REQUEST | INVALID_PARAMS => {
            "INVALID_ARGUMENT"
        }
        _ => "INTERNAL",
    }
}

/// The REST form of a 401: `google.rpc.Status` with `UNAUTHENTICATED`. A2A has no error type for it,
/// so there is no `ErrorInfo`.
pub(crate) fn unauthorized_body(message: &str) -> serde_json::Value {
    serde_json::json!({
        "error": {
            "code": 401,
            "status": "UNAUTHENTICATED",
            "message": message,
            "details": [],
        }
    })
}

/// The longest extractor text kept in an error detail.
const MAX_DETAIL_LEN: usize = 300;

/// Answer what the SDK's extractors refuse in the binding's own envelope, and refuse a send whose
/// configuration names the push config the way earlier drafts did.
///
/// The SDK reads bodies with axum's `Json` and query strings with `Query`, which refuse with plain
/// text before its handler runs: 400 (not JSON, or a query string that does not parse), 413 (over
/// the limit), 415 (not declared as JSON), 422. A REST client reads `google.rpc.Status`. This layer,
/// inside authentication (an anonymous caller still gets a 401), answers them as the SDK answers its
/// own errors: a body that is not JSON is `PARSE_ERROR`, a query string or path that does not parse
/// is `INVALID_PARAMS` with the extractor's text as a `BadRequest` violation (as the SDK does for a
/// body that does not convert), and the rest is `INVALID_REQUEST`.
pub(crate) async fn rejections(request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_REQUEST_BODY_BYTES).await else {
        return error_response(A2AError::invalid_request(
            "invalid request: the body is too large",
        ));
    };
    if is_send_path(parts.uri.path()) && names_legacy_push_config(&bytes) {
        // The SDK reads a request as proto3 JSON, which has no such member: it would be dropped, the
        // task would start, and the client would wait for notifications that were never registered.
        return error_response(A2AError::invalid_params(
            "configuration.pushNotificationConfig is the name of an earlier draft: send \
             configuration.taskPushNotificationConfig",
        ));
    }
    let has_body = !bytes.is_empty();
    let response = next
        .run(Request::from_parts(parts, Body::from(bytes.clone())))
        .await;
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"application/json"));
    let status = response.status();
    if is_json || !matches!(status.as_u16(), 400 | 413 | 415 | 422) {
        return response;
    }
    let not_json = || has_body && serde_json::from_slice::<serde::de::IgnoredAny>(&bytes).is_err();
    let error = match status {
        StatusCode::PAYLOAD_TOO_LARGE => {
            A2AError::invalid_request("invalid request: the body is too large")
        }
        StatusCode::UNSUPPORTED_MEDIA_TYPE => A2AError::invalid_request(
            "invalid request: the content type must be application/json or application/a2a+json",
        ),
        _ if not_json() => A2AError::new(PARSE_ERROR, "parse error: the body is not valid JSON"),
        StatusCode::UNPROCESSABLE_ENTITY => {
            A2AError::invalid_request("invalid request: the body is not a JSON object")
        }
        _ => {
            let text = axum::body::to_bytes(response.into_body(), MAX_DETAIL_LEN * 4)
                .await
                .map(|b| {
                    String::from_utf8_lossy(&b)
                        .chars()
                        .take(MAX_DETAIL_LEN)
                        .collect()
                })
                .unwrap_or_default();
            A2AError::invalid_params("invalid request parameters").with_details(vec![
                TypedDetail::bad_request(vec![FieldViolation {
                    field: String::new(),
                    description: text,
                }]),
            ])
        }
    };
    tracing::debug!(%status, code = error.code, "malformed HTTP+JSON request");
    error_response(error)
}

/// Whether a send body's configuration names `pushNotificationConfig` and not
/// `taskPushNotificationConfig`.
fn names_legacy_push_config(body: &[u8]) -> bool {
    const LEGACY: &[u8] = b"\"pushNotificationConfig\"";
    if !body.windows(LEGACY.len()).any(|w| w == LEGACY) {
        return false;
    }
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let configuration = &request["configuration"];
    configuration.get("pushNotificationConfig").is_some()
        && configuration.get("taskPushNotificationConfig").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_send_paths_are_the_two_sends_and_their_aliases() {
        for path in [
            "/message:send",
            "/message:stream",
            "/message/send",
            "/message/stream",
        ] {
            assert!(is_send_path(path), "{path}");
        }
        for path in ["/tasks", "/tasks/{id}", "/", "/message", "/message:sendx"] {
            assert!(!is_send_path(path), "{path}");
        }
    }

    #[test]
    fn message_extensions_are_read_only_from_a_send() {
        let body = br#"{"message":{"extensions":["urn:a","urn:b"]}}"#;
        assert_eq!(
            message_extensions("/message:send", body),
            ["urn:a", "urn:b"]
        );
        assert!(message_extensions("/tasks", body).is_empty());
        assert!(message_extensions("/message:send", b"not json").is_empty());
    }

    #[test]
    fn every_route_is_listed_once() {
        for (i, a) in REST_ROUTES.iter().enumerate() {
            for b in &REST_ROUTES[i + 1..] {
                assert!(!(a.path == b.path && a.method == b.method), "{a:?} twice");
            }
        }
    }

    #[test]
    fn the_legacy_push_config_is_spotted_only_without_the_new_name() {
        assert!(names_legacy_push_config(
            br#"{"configuration":{"pushNotificationConfig":{"url":"x"}}}"#
        ));
        assert!(!names_legacy_push_config(
            br#"{"configuration":{"pushNotificationConfig":{},"taskPushNotificationConfig":{}}}"#
        ));
        assert!(!names_legacy_push_config(
            br#"{"metadata":{"pushNotificationConfig":1}}"#
        ));
    }
}
