//! [`Redactor`]: what a third-party message may not say. Every value that `${VAR}` expansion put
//! into a server's command, arguments, environment, URL or headers is registered, and every
//! error message and stderr line that comes from the server, from the transport or from the
//! library goes through [`Redactor::scrub`] before it is logged, shown to the model or put in an
//! error.
//!
//! A value is remembered as it is **and as a URL or a JSON text would print it** (percent-encoded
//! in a path or a query, form-encoded, JSON-escaped): the URL parser and the HTTP client print an
//! expanded URL in its encoded form, and a structured tool result is JSON. Short values are
//! remembered too, however short: a short secret is still a secret, and the price is that its
//! characters are replaced wherever they occur in a message.

use url::Url;

/// What replaces a secret in a message.
pub(crate) const REDACTED: &str = "[REDACTED]";

/// The values one server's messages must not contain, longest first (so `Bearer abc` goes before
/// `abc`).
#[derive(Default)]
pub(crate) struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    /// Remember a value, and the forms of it that a URL or a JSON text would print. Empty
    /// values are ignored: they would match everywhere.
    pub(crate) fn add(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        self.push(value);
        for form in encoded_forms(value) {
            self.push(&form);
        }
        self.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    }

    fn push(&mut self, value: &str) {
        if !value.is_empty() && !self.secrets.iter().any(|s| s == value) {
            self.secrets.push(value.to_owned());
        }
    }

    /// `text` with every remembered value replaced by [`REDACTED`].
    pub(crate) fn scrub(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED);
            }
        }
        out
    }
}

/// How `value` reads once a URL or a JSON text has printed it, when that differs from the value:
/// as a path (`Url` percent-encodes it the way it prints a path), as a query, form-encoded (the
/// `+` for a space of `application/x-www-form-urlencoded`), and JSON-escaped.
fn encoded_forms(value: &str) -> Vec<String> {
    let mut forms = Vec::new();
    if let Ok(mut url) = Url::parse("http://h/") {
        url.set_path(value);
        forms.push(url.path().trim_start_matches('/').to_owned());
        url.set_query(Some(value));
        forms.extend(url.query().map(str::to_owned));
        url.set_query(None);
        url.set_fragment(Some(value));
        forms.extend(url.fragment().map(str::to_owned));
    }
    forms.push(url::form_urlencoded::byte_serialize(value.as_bytes()).collect());
    if let Ok(json) = serde_json::to_string(value)
        && let Some(inner) = json.strip_prefix('"').and_then(|j| j.strip_suffix('"'))
    {
        forms.push(inner.to_owned());
    }
    forms.retain(|form| form != value);
    forms
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("values", &self.secrets.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_replaced_longest_first() {
        let mut redactor = Redactor::default();
        redactor.add("abc123");
        redactor.add("Bearer abc123");
        redactor.add("");
        assert_eq!(
            redactor.scrub("sent `Bearer abc123`, then abc123 again"),
            "sent `[REDACTED]`, then [REDACTED] again"
        );
        assert_eq!(redactor.scrub("nothing here"), "nothing here");
        // `abc123`, `Bearer abc123`, and the latter as a URL prints it (`%20`) and a form has it (`+`).
        assert_eq!(format!("{redactor:?}"), "Redactor { values: 4 }");
    }

    #[test]
    fn encoded_forms_are_replaced_too() {
        let mut redactor = Redactor::default();
        redactor.add("a b/ü&\"q\"");
        // As `Url` prints it in a path, in a query, and as a form or a JSON text has it.
        for shown in [
            "http://h/a%20b/%C3%BC&%22q%22/mcp",
            "http://h/mcp?k=a%20b/%C3%BC&%22q%22",
            "a+b%2F%C3%BC%26%22q%22",
            "{\"k\":\"a b/ü&\\\"q\\\"\"}",
            "a b/ü&\"q\"",
        ] {
            let scrubbed = redactor.scrub(shown);
            assert!(scrubbed.contains("[REDACTED]"), "{shown} -> {scrubbed}");
            assert!(
                !scrubbed.contains("%C3%BC") && !scrubbed.contains("ü"),
                "{shown} -> {scrubbed}"
            );
        }
    }

    #[test]
    fn a_short_secret_is_still_redacted_and_garbles_what_it_matches() {
        let mut redactor = Redactor::default();
        redactor.add("ab");
        assert_eq!(redactor.scrub("a table"), "a t[REDACTED]le");
    }
}
