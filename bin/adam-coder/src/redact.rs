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
use std::sync::{Arc, RwLock};

use adam_llm_agent::StepIo;
use adam_workspace::{DynGitCredentials, GitCredentials, RepoRef, WorkspaceError};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use secrecy::{ExposeSecret as _, SecretString};
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

/// How many of the secrets that were registered after startup ([`Redactor::add`]: the installation
/// tokens a GitHub App mints, one an hour) are remembered; the oldest is forgotten first. A token
/// that old has long expired, and the list is read at every scrub.
const MAX_ADDED: usize = 16;

/// What a [`Redactor`] knows: the secrets of startup, and the latest of those added since.
#[derive(Default)]
struct Registered {
    /// The secrets the process started with.
    fixed: Vec<String>,
    /// The secrets added later, oldest first (at most [`MAX_ADDED`]).
    added: std::collections::VecDeque<String>,
    /// Every needle of both, longest first, so that a secret that contains another is replaced
    /// whole.
    needles: Vec<String>,
}

impl Registered {
    /// `needles` again, from the secrets.
    fn rebuild(&mut self) {
        let mut needles: Vec<String> = Vec::new();
        for secret in self.fixed.iter().chain(&self.added) {
            needles.push(secret.clone());
            needles.push(STANDARD.encode(secret));
            needles.push(STANDARD_NO_PAD.encode(secret));
            // Git's basic authorization for a token: base64("x-access-token:<token>").
            let basic = format!("x-access-token:{secret}");
            needles.push(STANDARD.encode(&basic));
            needles.push(STANDARD_NO_PAD.encode(&basic));
        }
        needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        needles.dedup();
        self.needles = needles;
    }
}

/// Replaces registered secret values in text.
///
/// Clones share what they know: a secret added through one ([`add`](Self::add)) is scrubbed by all of
/// them, which is how a token minted while the process runs reaches the tools' copies.
#[derive(Clone, Default)]
pub struct Redactor {
    registered: Arc<RwLock<Registered>>,
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Redactor({} values)", self.read().needles.len())
    }
}

impl Redactor {
    /// Redact each of `secrets` (and its Base64 forms).
    pub fn new<S: AsRef<str>>(secrets: impl IntoIterator<Item = S>) -> Self {
        let mut registered = Registered {
            fixed: secrets
                .into_iter()
                .map(|s| s.as_ref().to_owned())
                .filter(|s| s.len() >= MIN_SECRET_LEN)
                .collect(),
            ..Registered::default()
        };
        registered.rebuild();
        Self {
            registered: Arc::new(RwLock::new(registered)),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Registered> {
        self.registered
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Also redact `secret` (and its Base64 forms) from now on, in this redactor and every clone of
    /// it: a value the process only learned while it ran, such as an installation token minted
    /// for a GitHub App. Adding a known value changes nothing; only the latest 16 added
    /// values are kept (those of startup are never dropped).
    pub fn add(&self, secret: &str) {
        if secret.len() < MIN_SECRET_LEN {
            return;
        }
        {
            let known = self.read();
            if known.fixed.iter().chain(&known.added).any(|s| s == secret) {
                return;
            }
        }
        let mut registered = self
            .registered
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registered
            .fixed
            .iter()
            .chain(&registered.added)
            .any(|s| s == secret)
        {
            return;
        }
        registered.added.push_back(secret.to_owned());
        while registered.added.len() > MAX_ADDED {
            registered.added.pop_front();
        }
        registered.rebuild();
    }

    /// The secrets the process holds: every accepted A2A bearer token, the password of
    /// `DATABASE_URL`, and, for a role that runs workers, the model key and the GitHub credentials
    /// (a control plane holds neither): the personal access token, or the private key of the
    /// GitHub App, as its PEM and as the base64 body that a line of a log would carry. The
    /// installation tokens an App mints are added when they are ([`add`](Self::add)).
    pub fn from_config(config: &Config) -> Self {
        let mut secrets: Vec<String> = Vec::new();
        if let Some(worker) = &config.worker {
            secrets.push(worker.model.api_key.expose_secret().to_owned());
            secrets.extend(worker.github.secrets());
        }
        secrets.extend(
            config
                .service
                .a2a_bearer_tokens
                .iter()
                .map(|t| t.expose_secret().to_owned()),
        );
        if let Ok(url) = url::Url::parse(config.service.database_url.expose_secret())
            && let Some(password) = url.password()
        {
            secrets.push(password.to_owned());
            if let Ok(decoded) = urlencoding_decode(password) {
                secrets.push(decoded);
            }
        }
        Self::new(secrets)
    }

    /// How a tool call's step reports its input and output ([`StepIo`]), with this redactor
    /// over every string of both: the arguments the model gave and what the tool answered are sent to
    /// whoever reads the steps, so the process's secrets are scrubbed first. The redactor shared
    /// with this one, so a token registered later (an installation token the GitHub App minted) is
    /// scrubbed from the steps too.
    pub fn step_io(&self) -> StepIo {
        let redactor = self.clone();
        StepIo::default().redact(move |text| redactor.scrub(text).into_owned())
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.read().needles.is_empty()
    }

    /// `text` with every registered value replaced by [`REDACTED`].
    pub fn scrub<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(text);
        for needle in &self.read().needles {
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

/// Git credentials whose every token is registered with a [`Redactor`] before it is handed out, so
/// that a token the process only learns while it runs (an installation token a GitHub App has just
/// minted) is scrubbed from the tools' results, errors and progress like the secrets of startup.
/// The token of a personal access token is registered at startup already; registering it again
/// changes nothing.
pub struct RedactingCredentials {
    inner: DynGitCredentials,
    redactor: Redactor,
}

impl RedactingCredentials {
    /// `inner`'s tokens, registered with `redactor` as they are handed out.
    pub fn new(inner: DynGitCredentials, redactor: Redactor) -> Self {
        Self { inner, redactor }
    }
}

impl fmt::Debug for RedactingCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RedactingCredentials(..)")
    }
}

#[async_trait]
impl GitCredentials for RedactingCredentials {
    async fn token_for(&self, repo: &RepoRef) -> Result<SecretString, WorkspaceError> {
        let token = self.inner.token_for(repo).await?;
        self.redactor.add(token.expose_secret());
        Ok(token)
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
        assert!(config.worker.is_some());
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

    #[test]
    fn a_control_plane_redacts_what_it_holds_and_needs_no_worker_secrets() {
        let vars = std::collections::HashMap::from([
            ("ROLE", "control-plane"),
            ("DATABASE_URL", "postgres://u:db-password-1@db/adam"),
            ("A2A_BEARER_TOKENS", "tok-one-1"),
            ("PUBLIC_URL", "http://coder:8080/"),
        ]);
        let config = Config::from_lookup(|k| vars.get(k).map(|v| (*v).to_owned())).unwrap();
        assert!(config.worker.is_none());
        let r = Redactor::from_config(&config);
        for leaked in ["db-password-1", "tok-one-1"] {
            assert_eq!(r.scrub(&format!("<{leaked}>")), "<[redacted]>", "{leaked}");
        }
    }

    #[test]
    fn a_secret_added_later_is_scrubbed_by_every_clone_and_only_the_latest_are_kept() {
        let r = Redactor::new(["startup-secret"]);
        let clone = r.clone();
        assert_eq!(
            clone.scrub("ghs_minted_1 startup-secret"),
            "ghs_minted_1 [redacted]"
        );
        r.add("ghs_minted_1");
        assert_eq!(
            clone.scrub("token ghs_minted_1 and startup-secret"),
            "token [redacted] and [redacted]",
            "a clone sees what was added to another"
        );
        // Its Base64 forms too, and adding it again changes nothing.
        let basic = STANDARD.encode("x-access-token:ghs_minted_1");
        assert_eq!(clone.scrub(&format!("Basic {basic}")), "Basic [redacted]");
        let before = format!("{r:?}");
        r.add("ghs_minted_1");
        r.add("startup-secret");
        r.add("abc");
        assert_eq!(format!("{r:?}"), before);
        // The sixteen latest are kept; the oldest is forgotten, the startup secret never is.
        for n in 2..=17 {
            r.add(&format!("ghs_minted_{n:02}"));
        }
        assert_eq!(r.scrub("ghs_minted_1"), "ghs_minted_1", "forgotten");
        assert_eq!(
            r.scrub("ghs_minted_02 ghs_minted_17"),
            "[redacted] [redacted]"
        );
        assert_eq!(r.scrub("startup-secret"), "[redacted]");
        // An empty redactor learns too.
        let empty = Redactor::default();
        assert!(empty.is_empty());
        empty.add("ghs_first_token");
        assert!(!empty.is_empty());
    }

    /// Credentials that hand out a token the redactor has never heard of.
    struct Minting(&'static str);

    #[async_trait]
    impl GitCredentials for Minting {
        async fn token_for(&self, _repo: &RepoRef) -> Result<SecretString, WorkspaceError> {
            Ok(SecretString::from(self.0))
        }
    }

    #[tokio::test]
    async fn a_token_is_registered_as_it_is_handed_out() {
        let redactor = Redactor::default();
        let tools_copy = redactor.clone();
        let creds =
            RedactingCredentials::new(Arc::new(Minting("ghs_installation_token")), redactor);
        assert_eq!(
            tools_copy.scrub("push ghs_installation_token"),
            "push ghs_installation_token"
        );
        let token = creds
            .token_for(&RepoRef::new("https://github.com/o/r", "main"))
            .await
            .unwrap();
        assert_eq!(
            token.expose_secret(),
            "ghs_installation_token",
            "the token is handed out as it is"
        );
        assert_eq!(
            tools_copy.scrub("push ghs_installation_token"),
            "push [redacted]"
        );
        assert_eq!(format!("{creds:?}"), "RedactingCredentials(..)");
    }

    #[test]
    fn a_github_apps_key_is_redacted_as_its_pem_and_as_its_body() {
        use adam_workspace::testing::TestAppKey;
        let key = TestAppKey::generate();
        let vars: std::collections::HashMap<&str, String> = std::collections::HashMap::from([
            ("DATABASE_URL", "postgres://u:p@db/adam".to_owned()),
            ("MODEL_BASE_URL", "https://gw.example/v1".to_owned()),
            ("MODEL_API_KEY", "sk-model-key".to_owned()),
            ("MODEL", "m".to_owned()),
            ("GITHUB_APP_ID", "12345".to_owned()),
            ("GITHUB_APP_INSTALLATION_ID", "67890".to_owned()),
            ("GITHUB_APP_PRIVATE_KEY", key.pkcs8_pem.replace('\n', "\\n")),
            ("A2A_BEARER_TOKENS", "tok-one-1".to_owned()),
            ("PUBLIC_URL", "http://coder:8080/".to_owned()),
        ]);
        let config = Config::from_lookup(|k| vars.get(k).cloned()).unwrap();
        let r = Redactor::from_config(&config);
        let body: String = key
            .pkcs8_pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        // The whole PEM and its body are found. A slice of the body is not: this is exact-value
        // replacement, not a detector.
        for leaked in [key.pkcs8_pem.trim(), body.as_str()] {
            assert_eq!(r.scrub(&format!("<{leaked}>")), "<[redacted]>");
        }
        let slice = &body[10..200];
        assert_eq!(r.scrub(&format!("<{slice}>")), format!("<{slice}>"));
        assert_eq!(
            r.scrub(&format!("key={body} and {}", key.pkcs8_pem.trim())),
            "key=[redacted] and [redacted]"
        );
    }
}
