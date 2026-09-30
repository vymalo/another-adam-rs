//! Running the `git` CLI: environment hygiene, per-invocation credentials,
//! error classification and secret scrubbing.
//!
//! # Credentials
//!
//! A token is handed to `git` only through the environment of that one child
//! process:
//!
//! ```text
//! GIT_CONFIG_COUNT=1
//! GIT_CONFIG_KEY_0=http.<scheme>://<host>/.extraHeader
//! GIT_CONFIG_VALUE_0=Authorization: Basic base64("x-access-token:<token>")
//! ```
//!
//! For http(s) remotes the header is scoped to the remote's origin, so it is
//! never sent to a different host (redirects). For local remotes it is
//! unscoped and simply ignored. The token is never in a URL, in
//! `.git/config`, in the arguments (which `ps` shows), or in logs; everything
//! `git` writes to stderr is scrubbed of the token and of its base64 form
//! before it can become an error message.
//!
//! The one place the token is visible is the child's environment
//! (`/proc/<pid>/environ`, readable by the same uid only) for the duration of
//! the command.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use secrecy::{ExposeSecret, SecretString};
use tokio::process::Command;

use crate::error::{WorkspaceError, WorkspaceResult};

/// Commands that talk to a remote may take long (first clone of a big repo).
const NETWORK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Purely local commands should be quick.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Longest stderr excerpt kept in an error.
const MAX_STDERR: usize = 4000;

/// Environment variables that could redirect git to another repository or make
/// it log request headers (and thereby the token).
const SCRUBBED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_TRACE",
    "GIT_TRACE2",
    "GIT_TRACE2_EVENT",
    "GIT_TRACE2_PERF",
    "GIT_TRACE_CURL",
    "GIT_TRACE_CURL_NO_DATA",
    "GIT_TRACE_PACKET",
    "GIT_TRACE_PACK_ACCESS",
    "GIT_TRACE_PERFORMANCE",
    "GIT_TRACE_SETUP",
    "GIT_TRACE_SHALLOW",
    "GIT_CURL_VERBOSE",
];

/// A token plus the URL scope its header is limited to.
pub(crate) struct Auth {
    token: SecretString,
    /// `scheme://host[:port]/` for http(s) remotes.
    scope: Option<String>,
}

impl Auth {
    pub(crate) fn new(token: SecretString, scope: Option<&str>) -> Self {
        Self {
            token,
            scope: scope.map(str::to_owned),
        }
    }

    fn basic(&self) -> String {
        STANDARD.encode(format!("x-access-token:{}", self.token.expose_secret()))
    }

    fn config_key(&self) -> String {
        match &self.scope {
            Some(scope) => format!("http.{scope}.extraHeader"),
            None => "http.extraHeader".to_owned(),
        }
    }

    fn config_value(&self) -> String {
        format!("Authorization: Basic {}", self.basic())
    }

    /// Remove the token, its base64 form (padded or not) and the header from
    /// `text`.
    pub(crate) fn scrub(&self, text: &str) -> String {
        let secret = self.token.expose_secret();
        if secret.is_empty() {
            return text.to_owned();
        }
        let b64 = self.basic();
        let b64_bare = b64.trim_end_matches('=');
        text.replace(&b64, "[REDACTED]")
            .replace(b64_bare, "[REDACTED]")
            .replace(secret, "[REDACTED]")
    }
}

/// Captured result of a finished command.
pub(crate) struct GitOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) success: bool,
    pub(crate) code: Option<i32>,
}

impl GitOutput {
    pub(crate) fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }
}

/// Builder for one `git` invocation.
pub(crate) struct GitCmd {
    args: Vec<OsString>,
    config: Vec<(String, String)>,
    cwd: Option<PathBuf>,
    ceiling: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
    auth: Option<Auth>,
}

impl GitCmd {
    pub(crate) fn new() -> Self {
        Self {
            args: Vec::new(),
            config: Vec::new(),
            cwd: None,
            ceiling: None,
            env: Vec::new(),
            auth: None,
        }
    }

    /// Operate on this bare repository explicitly (never discover one).
    pub(crate) fn git_dir(mut self, dir: &Path) -> Self {
        let mut arg = OsString::from("--git-dir=");
        arg.push(dir);
        self.args.insert(0, arg);
        self
    }

    pub(crate) fn cwd(mut self, dir: &Path) -> Self {
        self.cwd = Some(dir.to_owned());
        self
    }

    /// Repository discovery must not climb into or above this directory.
    pub(crate) fn ceiling(mut self, dir: &Path) -> Self {
        self.ceiling = Some(dir.to_owned());
        self
    }

    /// A `-c key=value` passed before the subcommand.
    pub(crate) fn config(mut self, key: &str, value: &str) -> Self {
        self.config.push((key.to_owned(), value.to_owned()));
        self
    }

    pub(crate) fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    pub(crate) fn maybe_auth(mut self, auth: Option<Auth>) -> Self {
        self.auth = auth;
        self
    }

    pub(crate) fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub(crate) fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    /// The subcommand name, for messages.
    fn subcommand(&self) -> String {
        self.args
            .iter()
            .map(|a| a.to_string_lossy())
            .find(|a| !a.starts_with('-'))
            .map_or_else(|| "git".to_owned(), |a| a.into_owned())
    }

    fn scrub(&self, text: &str) -> String {
        match &self.auth {
            Some(auth) => auth.scrub(text),
            None => text.to_owned(),
        }
    }

    fn build(&self) -> Command {
        let mut cmd = Command::new("git");
        // Never run repository hooks, credential helpers or background gc from
        // an orchestrator-driven command; these -c flags are protected config.
        cmd.args(["-c", "core.hooksPath=/dev/null"])
            .args(["-c", "gc.auto=0"])
            .args(["-c", "maintenance.auto=false"])
            .args(["-c", "credential.helper="])
            // A configured fsmonitor is a program git runs on every status.
            .args(["-c", "core.fsmonitor=false"])
            .args(["-c", "commit.gpgsign=false"]);
        for (k, v) in &self.config {
            cmd.arg("-c").arg(format!("{k}={v}"));
        }
        cmd.args(&self.args);
        for var in SCRUBBED_ENV {
            cmd.env_remove(var);
        }
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ASKPASS", "false")
            .env("SSH_ASKPASS", "false")
            .env("LC_ALL", "C");
        if let Some(ceiling) = &self.ceiling {
            cmd.env("GIT_CEILING_DIRECTORIES", ceiling);
        }
        if let Some(auth) = &self.auth {
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", auth.config_key())
                .env("GIT_CONFIG_VALUE_0", auth.config_value());
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    /// Run to completion; a non-zero exit is an error.
    #[tracing::instrument(level = "debug", skip(self), fields(git = %self.subcommand()))]
    pub(crate) async fn run(self) -> WorkspaceResult<GitOutput> {
        let (out, stderr) = self.exec().await?;
        if out.success {
            Ok(out)
        } else {
            Err(classify(&self.subcommand(), out.code, &self.scrub(&stderr)))
        }
    }

    /// Run to completion and report the exit status instead of failing on it,
    /// for probes such as `git diff --quiet`. Spawn failures and timeouts are
    /// still errors.
    #[tracing::instrument(level = "debug", skip(self), fields(git = %self.subcommand()))]
    pub(crate) async fn run_status(self) -> WorkspaceResult<GitOutput> {
        Ok(self.exec().await?.0)
    }

    async fn exec(&self) -> WorkspaceResult<(GitOutput, String)> {
        let timeout = if self.auth.is_some() {
            NETWORK_TIMEOUT
        } else {
            LOCAL_TIMEOUT
        };
        let sub = self.subcommand();
        let child = self
            .build()
            .spawn()
            .map_err(|e| WorkspaceError::io(format!("cannot run git {sub}"), e))?;
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Err(_) => {
                return Err(WorkspaceError::transient(format!(
                    "git {sub} timed out after {}s",
                    timeout.as_secs()
                )));
            }
            Ok(res) => res.map_err(|e| WorkspaceError::io(format!("git {sub} failed"), e))?,
        };
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        Ok((
            GitOutput {
                stdout: output.stdout,
                success: output.status.success(),
                code: output.status.code(),
            },
            stderr,
        ))
    }
}

/// Turn a failed command's (already scrubbed) stderr into the closest
/// [`WorkspaceError`].
pub(crate) fn classify(command: &str, code: Option<i32>, stderr: &str) -> WorkspaceError {
    let message = truncate_tail(stderr.trim(), MAX_STDERR);
    let lower = message.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));

    if has(&[
        "authentication failed",
        "could not read username",
        "could not read password",
        "returned error: 401",
        "returned error: 403",
        "http basic: access denied",
        "permission denied",
    ]) {
        WorkspaceError::Auth(message)
    } else if has(&[
        "returned error: 404",
        "repository not found",
        "' not found",
        "does not appear to be a git repository",
        "couldn't find remote ref",
    ]) {
        WorkspaceError::NotFound(message)
    } else if has(&["[rejected]", "[remote rejected]", "non-fast-forward"]) {
        WorkspaceError::Invalid(message)
    } else if has(&[
        "could not resolve host",
        "failed to connect",
        "connection refused",
        "connection reset",
        "timed out",
        "early eof",
        "rpc failed",
        "returned error: 5",
        "returned error: 429",
        "the remote end hung up",
        "tls connection",
        "ssl_",
    ]) {
        WorkspaceError::transient(message)
    } else {
        WorkspaceError::Git {
            command: command.to_owned(),
            status: code.map_or_else(
                || "killed by signal".to_owned(),
                |c| format!("exit status {c}"),
            ),
            message,
        }
    }
}

/// Keep the end of `s`: git puts the reason last.
fn truncate_tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_owned();
    }
    let skip = count - max;
    let start = s.char_indices().nth(skip).map_or(0, |(i, _)| i);
    format!("...{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_removes_token_and_base64_forms() {
        let auth = Auth::new("s3cr3t-token".into(), Some("https://github.com/"));
        let b64 = auth.basic();
        let text = format!(
            "fatal: url with s3cr3t-token; header Authorization: Basic {b64}; bare {}",
            b64.trim_end_matches('=')
        );
        let scrubbed = auth.scrub(&text);
        assert!(!scrubbed.contains("s3cr3t-token"), "{scrubbed}");
        assert!(!scrubbed.contains(b64.trim_end_matches('=')), "{scrubbed}");
    }

    #[test]
    fn header_is_scoped_for_http_and_plain_for_local() {
        let scoped = Auth::new("t".into(), Some("https://github.com/"));
        assert_eq!(scoped.config_key(), "http.https://github.com/.extraHeader");
        assert_eq!(Auth::new("t".into(), None).config_key(), "http.extraHeader");
        assert_eq!(
            scoped.config_value(),
            format!(
                "Authorization: Basic {}",
                STANDARD.encode("x-access-token:t")
            )
        );
    }

    #[test]
    fn classifies_common_failures() {
        let cases = [
            ("fatal: Authentication failed for 'x'", "auth"),
            (
                "fatal: unable to access 'x': The requested URL returned error: 403",
                "auth",
            ),
            (
                "fatal: unable to access 'x': The requested URL returned error: 404",
                "not_found",
            ),
            (
                "fatal: repository 'http://h/o/r.git/' not found",
                "not_found",
            ),
            (
                "fatal: '/nope' does not appear to be a git repository",
                "not_found",
            ),
            (" ! [rejected] a -> a (non-fast-forward)", "invalid"),
            (
                "fatal: unable to access 'x': Failed to connect to h port 1",
                "transient",
            ),
            (
                "fatal: unable to access 'x': The requested URL returned error: 503",
                "transient",
            ),
            (
                "fatal: unable to access 'x': The requested URL returned error: 429",
                "transient",
            ),
            ("fatal: something odd", "git"),
        ];
        for (stderr, want) in cases {
            let got = match classify("fetch", Some(128), stderr) {
                WorkspaceError::Auth(_) => "auth",
                WorkspaceError::NotFound(_) => "not_found",
                WorkspaceError::Invalid(_) => "invalid",
                WorkspaceError::Transient { .. } => "transient",
                WorkspaceError::Git { .. } => "git",
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(got, want, "{stderr}");
        }
    }

    #[test]
    fn truncate_tail_keeps_the_end() {
        assert_eq!(truncate_tail("abcdef", 3), "...def");
        assert_eq!(truncate_tail("abc", 3), "abc");
    }

    mod prop {
        use proptest::collection::vec;
        use proptest::prelude::*;

        use super::*;

        /// The forms a token takes in git's output.
        #[derive(Clone, Debug)]
        enum Leak {
            Token,
            Basic,
            BasicUnpadded,
            Header,
            ConfigValue,
            Url,
        }

        fn arb_leak() -> impl Strategy<Value = Leak> {
            prop_oneof![
                Just(Leak::Token),
                Just(Leak::Basic),
                Just(Leak::BasicUnpadded),
                Just(Leak::Header),
                Just(Leak::ConfigValue),
                Just(Leak::Url),
            ]
        }

        proptest! {
            /// Whatever surrounds them, and however often they repeat, the
            /// token and its base64 forms (padded or not, in a header, in a
            /// config value, in a URL) never survive scrubbing. Tokens are at
            /// least 12 characters, as real ones are (a token that is a
            /// substring of the `[REDACTED]` marker could not be scrubbed
            /// meaningfully).
            #[test]
            fn prop_scrub_removes_every_token_encoding(
                token in "[A-Za-z0-9_\\-]{12,60}",
                scoped in any::<bool>(),
                parts in vec(("\\PC{0,30}", arb_leak()), 1..8),
                tail in "\\PC{0,30}",
            ) {
                let auth = Auth::new(token.clone().into(), scoped.then_some("https://github.com/"));
                let b64 = auth.basic();
                let bare = b64.trim_end_matches('=').to_owned();
                let mut text = String::new();
                for (noise, leak) in &parts {
                    text.push_str(noise);
                    match leak {
                        Leak::Token => text.push_str(&token),
                        Leak::Basic => text.push_str(&b64),
                        Leak::BasicUnpadded => text.push_str(&bare),
                        Leak::Header => text.push_str(&format!("Authorization: Basic {b64}")),
                        Leak::ConfigValue => text.push_str(&auth.config_value()),
                        Leak::Url => text.push_str(&format!("https://x-access-token:{token}@github.com/o/r")),
                    }
                }
                text.push_str(&tail);
                let scrubbed = auth.scrub(&text);
                prop_assert!(!scrubbed.contains(&token), "token left in {scrubbed:?}");
                prop_assert!(!scrubbed.contains(&b64), "base64 left in {scrubbed:?}");
                prop_assert!(!scrubbed.contains(&bare), "unpadded base64 left in {scrubbed:?}");
            }

            /// Text without any secret passes through unchanged.
            #[test]
            fn prop_scrub_leaves_clean_text_alone(
                token in "[A-Za-z0-9_\\-]{12,60}",
                text in "[ -~]{0,200}",
            ) {
                let auth = Auth::new(token.clone().into(), None);
                prop_assume!(!text.contains(&token));
                prop_assume!(!text.contains(auth.basic().trim_end_matches('=')));
                prop_assert_eq!(auth.scrub(&text), text);
            }
        }
    }
}
