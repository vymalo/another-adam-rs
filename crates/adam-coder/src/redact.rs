//! Keeping the process's own secrets out of everything that leaves it.
//!
//! Text produced by things this process does not control ends up in places
//! clients can read: OpenCode's stderr tail in an "ACP agent exited" error, a
//! check's output in the findings of a failed run, a model provider's error
//! body (which may echo the key it rejected). A [`Redactor`] replaces the
//! *values* of the secrets the process holds with `[redacted]`, and is applied
//! at the boundaries: tool results and errors, OpenCode's and the checks'
//! progress lines, and the agent's final failure message.
//!
//! It is exact-value replacement, not a detector: a secret that was never
//! registered (or was transformed, say hashed or split across lines) is not
//! found. Alongside each value it also removes its standard Base64 form
//! (padded and unpadded), the way HTTP basic authorization would carry it.
//! Values shorter than [`MIN_SECRET_LEN`] are not registered: replacing every
//! `a` in the output would destroy it and protect nothing.

use std::borrow::Cow;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use secrecy::ExposeSecret as _;
use serde_json::Value;

use crate::Config;

/// What replaces a secret.
pub const REDACTED: &str = "[redacted]";

/// The most a run's failure text (`RunView::error`, the A2A `Failed` status message) may carry, in
/// bytes. Upstream bodies and OpenCode's stderr tail can be long, and the text is shown to clients.
pub const MAX_FAILURE_TEXT: usize = 2048;

/// Appended to a failure text that was cut.
const TRUNCATED: &str = " [truncated]";

/// Shortest value that is worth (and safe) replacing.
pub const MIN_SECRET_LEN: usize = 4;

/// Replaces registered secret values in text.
#[derive(Clone, Default)]
pub struct Redactor {
    /// Longest first, so a secret that contains another is replaced whole.
    needles: Vec<String>,
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Redactor({} values)", self.needles.len())
    }
}

impl Redactor {
    /// Redact each of `secrets` (and its Base64 forms).
    pub fn new<S: AsRef<str>>(secrets: impl IntoIterator<Item = S>) -> Self {
        let mut needles: Vec<String> = Vec::new();
        for secret in secrets {
            let secret = secret.as_ref();
            if secret.len() < MIN_SECRET_LEN {
                continue;
            }
            needles.push(secret.to_owned());
            needles.push(STANDARD.encode(secret));
            needles.push(STANDARD_NO_PAD.encode(secret));
            // Git's basic authorization for a token: base64("x-access-token:<token>").
            let basic = format!("x-access-token:{secret}");
            needles.push(STANDARD.encode(&basic));
            needles.push(STANDARD_NO_PAD.encode(&basic));
        }
        needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        needles.dedup();
        Self { needles }
    }

    /// The secrets the process holds: the model key, the GitHub token, every
    /// accepted A2A bearer token, and the password of `DATABASE_URL`.
    pub fn from_config(config: &Config) -> Self {
        let mut secrets: Vec<String> = vec![
            config.model_api_key.expose_secret().to_owned(),
            config.github_token.expose_secret().to_owned(),
        ];
        secrets.extend(
            config
                .a2a_bearer_tokens
                .iter()
                .map(|t| t.expose_secret().to_owned()),
        );
        if let Ok(url) = url::Url::parse(config.database_url.expose_secret())
            && let Some(password) = url.password()
        {
            secrets.push(password.to_owned());
            if let Ok(decoded) = urlencoding_decode(password) {
                secrets.push(decoded);
            }
        }
        Self::new(secrets)
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.needles.is_empty()
    }

    /// `text` with every registered value replaced by [`REDACTED`].
    pub fn scrub<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(text);
        for needle in &self.needles {
            if out.contains(needle.as_str()) {
                out = Cow::Owned(out.replace(needle.as_str(), REDACTED));
            }
        }
        out
    }

    /// [`scrub`](Self::scrub) for an owned string.
    pub fn scrub_string(&self, text: String) -> String {
        match self.scrub(&text) {
            Cow::Borrowed(_) => text,
            Cow::Owned(clean) => clean,
        }
    }

    /// `text` scrubbed, then cut to [`MAX_FAILURE_TEXT`] bytes (at a character boundary, with a
    /// marker): what a run's failure text may carry out of the process. It is in this order on
    /// purpose: cutting first could leave the front half of a secret that scrubbing would then no
    /// longer recognise.
    pub fn failure_text(&self, text: String) -> String {
        let mut clean = self.scrub_string(text);
        if clean.len() > MAX_FAILURE_TEXT {
            let mut end = MAX_FAILURE_TEXT;
            while !clean.is_char_boundary(end) {
                end -= 1;
            }
            clean.truncate(end);
            clean.push_str(TRUNCATED);
        }
        clean
    }

    /// Scrub every string (and object key) inside `value`.
    pub fn scrub_value(&self, value: &mut Value) {
        match value {
            Value::String(s) => {
                if let Cow::Owned(clean) = self.scrub(s) {
                    *s = clean;
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.scrub_value(v)),
            Value::Object(map) => {
                let entries = std::mem::take(map);
                for (key, mut v) in entries {
                    self.scrub_value(&mut v);
                    map.insert(self.scrub_string(key), v);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
}

/// `%XX` decoding, enough for a password in a URL (no extra dependency).
fn urlencoding_decode(s: &str) -> Result<String, std::string::FromUtf8Error> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn replaces_every_occurrence_and_the_base64_forms() {
        let r = Redactor::new(["sk-secret-value"]);
        assert_eq!(
            r.scrub("key sk-secret-value and again sk-secret-value."),
            "key [redacted] and again [redacted]."
        );
        let b64 = STANDARD.encode("sk-secret-value");
        let bare = STANDARD_NO_PAD.encode("sk-secret-value");
        assert_eq!(r.scrub(&format!("Basic {b64}")), "Basic [redacted]");
        assert_eq!(r.scrub(&format!("Basic {bare}")), "Basic [redacted]");
        let basic = STANDARD.encode("x-access-token:sk-secret-value");
        assert_eq!(
            r.scrub(&format!("Authorization: Basic {basic}")),
            "Authorization: Basic [redacted]"
        );
        assert!(matches!(r.scrub("nothing here"), Cow::Borrowed(_)));
    }

    #[test]
    fn short_and_empty_values_are_ignored_and_longer_ones_win() {
        let r = Redactor::new(["", "a", "abc", "abcd-longer", "abcd"]);
        assert_eq!(r.scrub("abc a"), "abc a", "too short to be safe to replace");
        assert_eq!(r.scrub("x abcd-longer y"), "x [redacted] y");
        assert!(Redactor::new(Vec::<String>::new()).is_empty());
    }

    #[test]
    fn json_values_and_keys_are_scrubbed() {
        let r = Redactor::new(["hunter2-hunter2"]);
        let mut v = json!({"a": ["x hunter2-hunter2", 1, null], "hunter2-hunter2": {"b": "ok"}});
        r.scrub_value(&mut v);
        assert_eq!(
            v,
            json!({"a": ["x [redacted]", 1, null], "[redacted]": {"b": "ok"}})
        );
    }

    #[test]
    fn debug_shows_no_secret() {
        let r = Redactor::new(["hunter2-hunter2"]);
        assert!(!format!("{r:?}").contains("hunter2"));
    }

    #[test]
    fn a_database_password_is_found_raw_and_percent_decoded() {
        let vars = std::collections::HashMap::from([
            ("DATABASE_URL", "postgres://u:p%40ss%2Fw0rd@db/adam"),
            ("MODEL_BASE_URL", "https://gw.example/v1"),
            ("MODEL_API_KEY", "sk-model-key"),
            ("MODEL", "m"),
            ("GITHUB_TOKEN", "ghp_token_value"),
            ("A2A_BEARER_TOKENS", "tok-one-1, tok-two-2"),
            ("PUBLIC_URL", "http://coder:8080/"),
        ]);
        let config = Config::from_lookup(|k| vars.get(k).map(|v| (*v).to_owned())).unwrap();
        let r = Redactor::from_config(&config);
        for leaked in [
            "p%40ss%2Fw0rd",
            "p@ss/w0rd",
            "sk-model-key",
            "ghp_token_value",
            "tok-one-1",
            "tok-two-2",
        ] {
            assert_eq!(r.scrub(&format!("<{leaked}>")), "<[redacted]>", "{leaked}");
        }
        assert_eq!(
            r.scrub("postgres://u:[redacted]@db/adam"),
            "postgres://u:[redacted]@db/adam"
        );
    }
}
