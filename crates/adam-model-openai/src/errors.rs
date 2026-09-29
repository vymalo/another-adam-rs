//! Mapping of HTTP failures and in-band error objects onto [`ModelError`].

use std::time::{Duration, SystemTime};

use adam_model::ModelError;
use reqwest::StatusCode;
use serde_json::Value;

/// How much of an unparseable error body is kept in the message.
const BODY_SNIPPET: usize = 500;

/// Map a non-2xx response.
pub(crate) fn from_http(
    status: StatusCode,
    retry_after: Option<Duration>,
    body: &str,
) -> ModelError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let detail = parsed
        .as_ref()
        .and_then(error_message)
        .unwrap_or_else(|| snippet(body));
    let message = format!("HTTP {}: {detail}", status.as_u16());
    let context_length = parsed.as_ref().is_some_and(is_context_length) || {
        // Servers that do not send a structured code (vLLM, some gateways).
        let lower = detail.to_ascii_lowercase();
        lower.contains("maximum context length") || lower.contains("context_length_exceeded")
    };

    match status.as_u16() {
        401 | 403 => ModelError::Auth(message),
        429 => ModelError::RateLimited { retry_after },
        408 => ModelError::transient(message),
        400 | 413 | 422 if context_length => ModelError::ContextLength(message),
        500..=599 => ModelError::transient(message),
        400..=499 => ModelError::invalid_request(message),
        // 1xx/3xx: redirects are not followed, so this is not an API answer.
        _ => ModelError::protocol(message),
    }
}

/// Map an error object delivered inside a 200 response or mid-stream, where
/// there is no status code to go by.
pub(crate) fn from_error_object(value: &Value) -> ModelError {
    let detail = error_message(value).unwrap_or_else(|| snippet(&value.to_string()));
    let message = format!("error in response body: {detail}");
    if is_context_length(value) {
        return ModelError::ContextLength(message);
    }
    match error_field(value, "type").or_else(|| error_field(value, "code")) {
        Some(t) if t.contains("rate_limit") => ModelError::RateLimited { retry_after: None },
        Some(t) if t.contains("invalid_request") || t.contains("authentication") => {
            ModelError::invalid_request(message)
        }
        // Anything else that fails after the request was accepted is treated
        // like a server-side failure.
        _ => ModelError::transient(message),
    }
}

/// Parse a `Retry-After` header: delay-seconds, or an HTTP-date.
pub(crate) fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

fn error_object(value: &Value) -> Option<&Value> {
    value
        .get("error")
        .filter(|e| e.is_object() || e.is_string())
}

fn error_message(value: &Value) -> Option<String> {
    match error_object(value)? {
        Value::String(s) => Some(s.clone()),
        obj => obj.get("message")?.as_str().map(str::to_owned),
    }
}

/// A string-ish field (`code` may be a string or a number) of the error object.
fn error_field(value: &Value, name: &str) -> Option<String> {
    match error_object(value)?.get(name)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn is_context_length(value: &Value) -> bool {
    error_field(value, "code").is_some_and(|c| c == "context_length_exceeded")
}

fn snippet(body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        return "(empty body)".into();
    }
    match body.char_indices().nth(BODY_SNIPPET) {
        Some((cut, _)) => format!("{}...", &body[..cut]),
        None => body.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn map(status: u16, body: &str) -> ModelError {
        from_http(StatusCode::from_u16(status).unwrap(), None, body)
    }

    #[test]
    fn status_mapping() {
        assert!(matches!(map(401, ""), ModelError::Auth(_)));
        assert!(matches!(map(403, ""), ModelError::Auth(_)));
        assert!(matches!(map(408, ""), ModelError::Transient { .. }));
        assert!(matches!(map(500, ""), ModelError::Transient { .. }));
        assert!(matches!(map(503, ""), ModelError::Transient { .. }));
        assert!(matches!(map(404, ""), ModelError::InvalidRequest { .. }));
        assert!(matches!(map(409, ""), ModelError::InvalidRequest { .. }));
        assert!(matches!(
            map(400, "nope"),
            ModelError::InvalidRequest { .. }
        ));
        assert!(matches!(
            map(413, "too big"),
            ModelError::InvalidRequest { .. }
        ));
        assert!(matches!(map(302, ""), ModelError::Protocol { .. }));
        assert!(matches!(
            from_http(
                StatusCode::TOO_MANY_REQUESTS,
                Some(Duration::from_secs(3)),
                ""
            ),
            ModelError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(3)
        ));
    }

    #[test]
    fn context_length_by_code_or_message() {
        let by_code = json!({"error": {"code": "context_length_exceeded", "message": "too long"}});
        assert!(matches!(
            map(400, &by_code.to_string()),
            ModelError::ContextLength(_)
        ));
        assert!(matches!(
            map(413, &by_code.to_string()),
            ModelError::ContextLength(_)
        ));
        let by_message = json!({"error": {"message": "This model's maximum context length is 8192 tokens", "code": null}});
        assert!(matches!(
            map(400, &by_message.to_string()),
            ModelError::ContextLength(_)
        ));
        // A 500 is still transient even if the words match.
        assert!(matches!(
            map(500, &by_code.to_string()),
            ModelError::Transient { .. }
        ));
    }

    #[test]
    fn message_comes_from_error_object_or_body() {
        let e = map(
            400,
            r#"{"error":{"message":"bad tool","type":"invalid_request_error"}}"#,
        );
        assert!(
            matches!(&e, ModelError::InvalidRequest { message, source: None } if message == "HTTP 400: bad tool"),
            "{e:?}"
        );
        let e = map(400, "plain text");
        assert!(
            matches!(&e, ModelError::InvalidRequest { message, source: None } if message == "HTTP 400: plain text"),
            "{e:?}"
        );
    }

    #[test]
    fn long_bodies_are_truncated() {
        let e = map(400, &"x".repeat(5000));
        let ModelError::InvalidRequest { message: m, .. } = e else {
            panic!()
        };
        assert!(m.len() < 600, "{}", m.len());
    }

    #[test]
    fn in_band_errors() {
        let v = json!({"error": {"message": "slow down", "type": "rate_limit_error"}});
        assert!(matches!(
            from_error_object(&v),
            ModelError::RateLimited { .. }
        ));
        let v = json!({"error": {"message": "x", "code": "context_length_exceeded"}});
        assert!(matches!(
            from_error_object(&v),
            ModelError::ContextLength(_)
        ));
        let v = json!({"error": {"message": "overloaded"}});
        assert!(matches!(
            from_error_object(&v),
            ModelError::Transient { .. }
        ));
        let v = json!({"error": {"message": "bad", "type": "invalid_request_error"}});
        assert!(matches!(
            from_error_object(&v),
            ModelError::InvalidRequest { .. }
        ));
    }

    #[test]
    fn retry_after_seconds_and_date() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        assert_eq!(parse_retry_after("3", now), Some(Duration::from_secs(3)));
        assert_eq!(
            parse_retry_after(" 120 ", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_retry_after("", now), None);
        // 2001-09-09T01:46:40Z is exactly 1_000_000_000; ten seconds later:
        assert_eq!(
            parse_retry_after("Sun, 09 Sep 2001 01:46:50 GMT", now),
            Some(Duration::from_secs(10))
        );
        // A date in the past means "now".
        assert_eq!(
            parse_retry_after("Sun, 09 Sep 2001 01:46:30 GMT", now),
            Some(Duration::ZERO)
        );
    }
}
