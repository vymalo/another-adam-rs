//! What every write checks before an adapter stores it: the scrub, the marker, the bounds, and the
//! addresses an agent scope may not name.

use adam_core::Timestamp;

use crate::{
    Attempt, AttemptDraft, Kind, MemoryError, MemoryResult, Observation, REDACTION_MARKER, Scope,
    Settings,
};

pub(crate) struct Prepared {
    pub root_cause: String,
    pub attempt: Attempt,
}

/// Scrub every text of `draft` and check it, then stamp the attempt at `now`.
pub(crate) fn prepare(
    scope: &Scope,
    draft: AttemptDraft,
    settings: &Settings,
    now: Timestamp,
) -> MemoryResult<Prepared> {
    scope.validate()?;
    if draft.id.as_str().trim().is_empty() {
        return Err(MemoryError::EmptyField {
            field: "attempt id",
        });
    }
    if draft.provenance.conversation.trim().is_empty() {
        return Err(MemoryError::EmptyField {
            field: "provenance conversation",
        });
    }
    if draft.anchors.len() > settings.limits.anchors_per_attempt {
        return Err(MemoryError::TooManyAnchors {
            anchors: draft.anchors.len(),
            limit: settings.limits.anchors_per_attempt,
        });
    }
    let root_cause = root_cause(scope, &draft.root_cause, settings)?;
    let remedy = text(scope, "remedy", &draft.remedy, settings)?;
    let rationale = text(scope, "rationale", &draft.rationale, settings)?;
    let predicted = text(scope, "prediction", &draft.predicted, settings)?;
    let anchors = draft
        .anchors
        .iter()
        .map(|anchor| text(scope, "anchor", anchor, settings))
        .collect::<MemoryResult<Vec<_>>>()?;
    Ok(Prepared {
        root_cause,
        attempt: Attempt {
            id: draft.id,
            remedy,
            rationale,
            predicted,
            anchors,
            provenance: draft.provenance,
            recorded: now,
            observed: None,
            observed_at: None,
        },
    })
}

/// Scrub an observation's text and check it.
pub(crate) fn prepare_observation(
    scope: &Scope,
    observation: Observation,
    settings: &Settings,
) -> MemoryResult<Observation> {
    scope.validate()?;
    Ok(Observation {
        outcome: observation.outcome,
        detail: text(scope, "observation", &observation.detail, settings)?,
    })
}

fn root_cause(scope: &Scope, raw: &str, settings: &Settings) -> MemoryResult<String> {
    if raw.trim().is_empty() {
        return Err(MemoryError::EmptyField {
            field: "root cause",
        });
    }
    let clean = scrub(scope, "root cause", raw, settings)?;
    let chars = clean.chars().count();
    if chars > settings.limits.root_cause_chars {
        return Err(MemoryError::TooManyChars {
            field: "root cause",
            chars,
            limit: settings.limits.root_cause_chars,
        });
    }
    Ok(clean)
}

fn text(
    scope: &Scope,
    field: &'static str,
    raw: &str,
    settings: &Settings,
) -> MemoryResult<String> {
    let clean = scrub(scope, field, raw, settings)?;
    if clean.len() > settings.limits.field_bytes {
        return Err(MemoryError::TooManyBytes {
            field,
            bytes: clean.len(),
            limit: settings.limits.field_bytes,
        });
    }
    Ok(clean)
}

/// The scrub, the marker check and the agent-scope check, in that order: a secret is removed first,
/// so the marker is what a refusal sees.
fn scrub(
    scope: &Scope,
    field: &'static str,
    raw: &str,
    settings: &Settings,
) -> MemoryResult<String> {
    scrub_text(
        field,
        raw,
        &settings.scrubber,
        matches!(scope.kind(), Kind::Agent),
    )
}

/// The scrub and the marker check every text passes, then the address check when `forbid_address`
/// is set. L2 lessons forbid addresses whatever their audience (ADR 0081 decision 5), so they call
/// this directly instead of going through an L1 scope.
pub(crate) fn scrub_text(
    field: &'static str,
    raw: &str,
    scrubber: &crate::Scrubber,
    forbid_address: bool,
) -> MemoryResult<String> {
    let clean = scrubber.scrub(raw);
    if clean.contains(REDACTION_MARKER) {
        return Err(MemoryError::Redacted { field });
    }
    if forbid_address && let Some(what) = names_address(&clean) {
        return Err(MemoryError::NamesAddress { field, what });
    }
    Ok(clean)
}

/// The shapes an agent-scoped text, or an L2 lesson, may not name. The list is conservative on
/// purpose: a false positive refuses a write, which decision 12 makes the safe side.
fn names_address(text: &str) -> Option<&'static str> {
    for token in text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '<' | '>' | '`' | '|'
            )
    }) {
        let token = token.trim_matches(|c: char| matches!(c, '.' | '!' | '?' | ':'));
        if token.is_empty() {
            continue;
        }
        if token.contains("://") {
            return Some("URL");
        }
        if is_email(token) {
            return Some("e-mail address");
        }
        if is_repository(token) {
            return Some("repository");
        }
        if is_path(token) {
            return Some("path");
        }
        if is_host(token) {
            return Some("host");
        }
    }
    None
}

fn is_email(token: &str) -> bool {
    let mut parts = token.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && !domain.is_empty() && is_dotted_host(domain)
}

fn is_repository(token: &str) -> bool {
    if token.starts_with("git@") {
        return true;
    }
    let token = token.strip_prefix("repository:").unwrap_or(token);
    let mut parts = token.split('/');
    let Some(first) = parts.next() else {
        return false;
    };
    parts.count() >= 2 && is_dotted_host(first)
}

fn is_path(token: &str) -> bool {
    if token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.starts_with("\\\\")
        || token.contains('\\')
    {
        return true;
    }
    let bytes = token.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
    {
        return true;
    }
    if token.matches('/').count() >= 2 {
        return true;
    }
    match token.rsplit_once('/') {
        Some((_, file)) => has_code_extension(file),
        None => has_code_extension(token),
    }
}

fn has_code_extension(token: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        "rs", "toml", "json", "jsonl", "md", "py", "js", "ts", "tsx", "jsx", "go", "java", "kt",
        "rb", "php", "c", "h", "cpp", "hpp", "cs", "swift", "sh", "bash", "zsh", "fish", "ps1",
        "yaml", "yml", "lock", "txt", "cfg", "ini", "env", "sql", "html", "css", "scss", "vue",
        "svelte", "xml", "proto", "gradle",
    ];
    match token.rsplit_once('.') {
        Some((stem, extension)) => !stem.is_empty() && EXTENSIONS.contains(&extension),
        None => false,
    }
}

fn is_host(token: &str) -> bool {
    let host = match token.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            host
        }
        _ => token,
    };
    host == "localhost" || is_dotted_host(host)
}

fn is_dotted_host(token: &str) -> bool {
    let labels: Vec<&str> = token.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    let valid = labels.iter().all(|label| {
        !label.is_empty()
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    });
    let Some(last) = labels.last() else {
        return false;
    };
    valid && last.len() >= 2 && last.bytes().all(|byte| byte.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_prose_names_nothing() {
        for text in [
            "the build failed when the lockfile was stale",
            "version 0.6.0",
            "see e.g. the log",
            "the test took 2.5 seconds",
            "read/write access",
            "and/or",
            "vymalo/another-adam-rs",
        ] {
            assert_eq!(names_address(text), None, "{text:?}");
        }
    }

    #[test]
    fn every_shape_an_agent_scope_forbids_is_found() {
        let cases = [
            ("github.com/vymalo/another-adam-rs", "repository"),
            ("repository:github.com/vymalo/another-adam-rs", "repository"),
            ("git@github.com:vymalo/another-adam-rs", "repository"),
            ("/etc/passwd", "path"),
            ("./target/debug", "path"),
            ("src/lib.rs", "path"),
            (r"C:\Users\me", "path"),
            ("https://example.com/x", "URL"),
            ("someone@example.com", "e-mail address"),
            ("db.internal:5432", "host"),
            ("example.com", "host"),
            ("localhost:8080", "host"),
        ];
        for (text, what) in cases {
            assert_eq!(names_address(text), Some(what), "{text:?}");
        }
    }

    #[test]
    fn a_url_in_a_sentence_is_found() {
        assert_eq!(
            names_address("the docs are at https://example.com/x."),
            Some("URL")
        );
        assert_eq!(
            names_address("(someone@example.com) wrote it"),
            Some("e-mail address")
        );
    }
}
