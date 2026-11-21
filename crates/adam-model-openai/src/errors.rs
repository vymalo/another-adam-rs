//! Mapping of HTTP failures and in-band error objects onto [`ModelError`].

use std::time::{Duration, SystemTime};

use adam_model::{ModelError, ProblemType};
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

/// The longest slug a problem type is read as: a word of the gateway's vocabulary, not a sentence.
const MAX_SLUG: usize = 64;

/// Whether a `Content-Type` is `application/problem+json` (RFC 9457), whatever its parameters
/// and its case.
fn is_problem_json(content_type: Option<&str>) -> bool {
    content_type
        .and_then(|value| value.split(';').next())
        .is_some_and(|essence| {
            essence
                .trim()
                .eq_ignore_ascii_case("application/problem+json")
        })
}

/// The problem type of a refusal (ADR 0067 decision 3), read only from a body whose `Content-Type`
/// is `application/problem+json`.
///
/// * [`ProblemType::Absent`]: no problem document, an empty body, no `type` member, `about:blank`.
/// * [`ProblemType::Slug`]: the `type`'s last path segment
///   (`https://gw.example/problems/run_not_bound` is `run_not_bound`), or the member as is when it
///   holds no `/`, when it is a word of at most [`MAX_SLUG`] letters, digits, `_`, `-` and `.`.
/// * [`ProblemType::Unreadable`]: any other `type` (a `urn:`, a query, a fragment, a trailing `/`,
///   a member that is not a string), and a body that is not a JSON object.
///
/// The slug is the only part of a refusal's body an error keeps.
pub(crate) fn problem_type(content_type: Option<&str>, body: &str) -> ProblemType {
    if !is_problem_json(content_type) || body.trim().is_empty() {
        return ProblemType::Absent;
    }
    let Ok(Value::Object(document)) = serde_json::from_str::<Value>(body) else {
        return ProblemType::Unreadable;
    };
    let kind = match document.get("type") {
        None => return ProblemType::Absent,
        Some(Value::String(kind)) => kind.trim(),
        Some(_) => return ProblemType::Unreadable,
    };
    if kind == "about:blank" {
        return ProblemType::Absent;
    }
    let slug = kind.rsplit('/').next().unwrap_or(kind);
    let word = !slug.is_empty()
        && slug.len() <= MAX_SLUG
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if word {
        ProblemType::Slug(slug.to_owned())
    } else {
        ProblemType::Unreadable
    }
}

/// Map an error object delivered inside a 200 response or mid-stream, where
/// there is no status code to go by.
///
/// Such an object is the upstream model's, never the gateway's refusal: a gateway refuses before a
/// response streams, so a refusal is always an HTTP answer ([`from_http`]), and nothing here parks.
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

/// Parse a `Retry-After` header: delay-seconds, or an HTTP-date. A number of seconds too large for
/// a `u64` is the longest wait there is, not none: the endpoint asked for a long one (a park clamps
/// it, `adam_model::MAX_PARK`).
pub(crate) fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return Some(Duration::from_secs(
            value.parse::<u64>().unwrap_or(u64::MAX),
        ));
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
    fn a_problem_type_is_read_by_its_last_segment() {
        let read = |body: &str| problem_type(Some("application/problem+json"), body);
        let slug = |s: &str| ProblemType::Slug(s.to_owned());
        assert_eq!(
            read(r#"{"type":"https://gw.example/problems/run_not_bound","title":"x"}"#),
            slug("run_not_bound")
        );
        assert_eq!(
            read(r#"{"type":"/problems/over_limit"}"#),
            slug("over_limit")
        );
        assert_eq!(read(r#"{"type":"lease_expired"}"#), slug("lease_expired"));
        let at_cap = "a".repeat(MAX_SLUG);
        assert_eq!(read(&format!(r#"{{"type":"{at_cap}"}}"#)), slug(&at_cap));
        // No problem type: the status says it all.
        for absent in [
            "",
            "  ",
            r#"{"title":"no type"}"#,
            r#"{"type":"about:blank"}"#,
        ] {
            assert_eq!(read(absent), ProblemType::Absent, "{absent}");
        }
        // A type that is there and cannot be read: never taken for no type.
        let long = format!(r#"{{"type":"{}"}}"#, "a".repeat(MAX_SLUG + 1));
        for unreadable in [
            "not json",
            "[]",
            r#""run_not_bound""#,
            r#"{"type":7}"#,
            r#"{"type":null}"#,
            r#"{"type":""}"#,
            r#"{"type":"urn:gw:model_locked"}"#,
            r#"{"type":"https://gw.example/problems/model_locked?v=1"}"#,
            r#"{"type":"https://gw.example/problems#run_not_bound"}"#,
            r#"{"type":"https://gw.example/"}"#,
            r#"{"type":"/problems/over_limit/"}"#,
            r#"{"type":"two words"}"#,
            r#"{"type":"sk-live 0123"}"#,
            long.as_str(),
        ] {
            assert_eq!(read(unreadable), ProblemType::Unreadable, "{unreadable}");
        }
    }

    #[test]
    fn a_problem_document_is_told_by_its_content_type() {
        let body = r#"{"type":"over_limit"}"#;
        for yes in [
            "application/problem+json",
            "Application/Problem+JSON",
            "application/problem+json; charset=utf-8",
        ] {
            assert_eq!(
                problem_type(Some(yes), body),
                ProblemType::Slug("over_limit".into()),
                "{yes}"
            );
        }
        for no in [
            "application/json",
            "text/plain",
            "",
            "application/problem+xml",
        ] {
            assert_eq!(problem_type(Some(no), body), ProblemType::Absent, "{no}");
            assert_eq!(
                problem_type(Some(no), "not json"),
                ProblemType::Absent,
                "{no}"
            );
        }
        assert_eq!(problem_type(None, body), ProblemType::Absent);
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
        assert_eq!(parse_retry_after("-5", now), None);
        assert_eq!(parse_retry_after("1.5", now), None);
        // Too large for a u64: the longest wait, not none.
        assert_eq!(
            parse_retry_after("99999999999999999999999999", now),
            Some(Duration::from_secs(u64::MAX))
        );
        assert_eq!(
            parse_retry_after(&u64::MAX.to_string(), now),
            Some(Duration::from_secs(u64::MAX))
        );
        assert!(
            parse_retry_after("Fri, 31 Dec 9999 23:59:59 GMT", now)
                .is_some_and(|d| d > Duration::from_secs(200 * 365 * 24 * 3600)),
            "a date far ahead is a long wait"
        );
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
