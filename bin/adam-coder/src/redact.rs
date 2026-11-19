//! The coder's own half of [`Redactor`]: the secrets its configuration holds, and the git
//! credentials whose tokens are registered as they are handed out. The redactor itself is
//! [`adam_code_tools::redact`], re-exported here.

use std::fmt;

use adam_workspace::{DynGitCredentials, GitCredentials, RepoRef, WorkspaceError};
use async_trait::async_trait;
use secrecy::{ExposeSecret as _, SecretString};

pub use adam_code_tools::redact::*;

use crate::Config;

/// [`Redactor::from_config`]: a redactor of what the coder's configuration holds.
pub trait RedactorFromConfig {
    /// The secrets the process holds: every accepted A2A bearer token, the password of
    /// `DATABASE_URL`, and, for a role that runs workers, the model key and the GitHub credentials
    /// (a control plane holds neither): the personal access token, or the private key of the
    /// GitHub App, as its PEM and as the base64 body that a line of a log would carry. The
    /// installation tokens an App mints are added when they are ([`Redactor::add`]).
    fn from_config(config: &Config) -> Self;
}

impl RedactorFromConfig for Redactor {
    fn from_config(config: &Config) -> Self {
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
        Redactor::new(secrets)
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

    async fn acts_for(&self) -> Result<Option<Vec<String>>, WorkspaceError> {
        self.inner.acts_for().await
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
    use std::sync::Arc;

    use super::*;

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

    /// A GitHub App that finds the installation of each owner keeps a token for each of up to
    /// `MAX_CACHED_INSTALLATIONS` installations, and one more for each while a refresh overlaps:
    /// the redactor remembers two per cached installation, so none of them is forgotten while the
    /// credentials can still hand it out, and the one after that makes the oldest go.
    #[test]
    fn the_redactor_remembers_two_tokens_per_cached_installation() {
        let installations = adam_workspace::MAX_CACHED_INSTALLATIONS;
        assert_eq!(MAX_ADDED, 2 * installations);
        let r = Redactor::new(["startup-secret"]);
        let token = |installation: usize, generation: usize| {
            format!("ghs_inst{installation:03}_gen{generation}_token")
        };
        for installation in 0..installations {
            for generation in 0..2 {
                r.add(&token(installation, generation));
            }
        }
        for installation in 0..installations {
            for generation in 0..2 {
                assert_eq!(
                    r.scrub(&token(installation, generation)),
                    REDACTED,
                    "installation {installation}, token {generation} is still remembered"
                );
            }
        }
        // One token more than the bound: the oldest of all goes, the rest stay.
        r.add("ghs_one_more_token");
        assert_eq!(
            r.scrub(&token(0, 0)),
            token(0, 0),
            "the oldest is forgotten"
        );
        assert_eq!(r.scrub(&token(0, 1)), REDACTED);
        assert_eq!(r.scrub("ghs_one_more_token"), REDACTED);
        assert_eq!(
            r.scrub("startup-secret"),
            REDACTED,
            "a startup secret never is"
        );
    }
}
