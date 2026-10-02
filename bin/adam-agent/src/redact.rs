//! What the steps of a tool call may not say: the secret values this process holds.
//!
//! The step of a tool call carries the arguments the model gave and what the tool answered, and
//! whoever reads the steps keeps them (ADR 0011). A process that holds secrets scrubs them first:
//! [`step_io`] makes the [`StepIo`] that does, from the values the configuration holds (the model's
//! key, the A2A bearer tokens, the password of the database URL) and from the environment: every
//! variable whose name says it is a secret (`*_KEY`, `*_TOKEN`, `*_SECRET`, `*_PASSWORD`, ...), which
//! is where the `${VAR}` values of an agent's `mcp.json` come from.
//!
//! It is exact-value replacement, not a detector: a secret that is not in the environment, or that
//! was transformed (hashed, split over two lines), is not found. The orchestration layer redacts
//! patterns (bearer tokens, JWTs, keys) as well; this is the agent's half.

use adam_llm_agent::StepIo;
use secrecy::ExposeSecret as _;

use crate::config::Config;

/// What replaces a secret.
pub const REDACTED: &str = "[redacted]";

/// Shortest value that is worth (and safe) replacing: replacing every `a` of an output would
/// destroy it and protect nothing.
pub const MIN_SECRET_LEN: usize = 4;

/// The words in a variable's name that say its value is a secret.
const SECRET_WORDS: [&str; 8] = [
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PASSPHRASE",
    "CREDENTIAL",
    "PRIVATE",
];

/// The secret values of `config` and of `vars` (the process environment as pairs): the model's key,
/// the A2A bearer tokens, the password of `DATABASE_URL` (as written and decoded), and the value of
/// each variable named like a secret (and, for a comma-separated value such as a list of tokens, each
/// of its parts). Longest first, so a value that contains another is replaced whole.
pub fn secret_values(
    config: &Config,
    vars: impl IntoIterator<Item = (String, String)>,
) -> Vec<String> {
    let mut secrets: Vec<String> = Vec::new();
    if let Some(worker) = &config.worker {
        secrets.push(worker.model.api_key.expose_secret().to_owned());
    }
    secrets.extend(
        config
            .service
            .a2a_bearer_tokens
            .iter()
            .map(|token| token.expose_secret().to_owned()),
    );
    if let Ok(url) = url::Url::parse(config.service.database_url.expose_secret())
        && let Some(password) = url.password()
    {
        secrets.push(password.to_owned());
        secrets.push(percent_decode(password));
    }
    for (name, value) in vars {
        let name = name.to_ascii_uppercase();
        if SECRET_WORDS.iter().any(|word| name.contains(word)) {
            secrets.extend(value.split(',').map(|part| part.trim().to_owned()));
            secrets.push(value);
        }
    }
    secrets.retain(|secret| secret.len() >= MIN_SECRET_LEN);
    secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    secrets.dedup();
    secrets
}

/// `text` with every value of `secrets` (longest first) replaced by [`REDACTED`].
pub fn scrub(secrets: &[String], text: &str) -> String {
    let mut out = text.to_owned();
    for secret in secrets {
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), REDACTED);
        }
    }
    out
}

/// The environment of this process as pairs, leaving out what is not text (`std::env::vars` panics on it).
pub fn process_vars() -> impl Iterator<Item = (String, String)> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
}

/// The [`StepIo`] of this process: every string of a call's input and output scrubbed of the secrets
/// of `config` and of the environment `vars`, then cut to the contract's bounds.
pub fn step_io(config: &Config, vars: impl IntoIterator<Item = (String, String)>) -> StepIo {
    let secrets = secret_values(config, vars);
    if secrets.is_empty() {
        return StepIo::default();
    }
    StepIo::default().redact(move |text| scrub(&secrets, text))
}

/// `%XX` decoding, enough for a password in a URL.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config() -> Config {
        let dir = tempfile::tempdir().unwrap();
        let vars: HashMap<&str, String> = HashMap::from([
            ("DATABASE_URL", "postgres://u:p%40ss-word-1@db/adam".into()),
            ("MODEL_BASE_URL", "https://gw.example/v1".into()),
            ("MODEL_API_KEY", "sk-model-key-9f".into()),
            ("MODEL", "large".into()),
            ("A2A_BEARER_TOKENS", "tok-one-aa,tok-two-bb".into()),
            ("PUBLIC_URL", "http://agent.svc:8080/".into()),
            (adam::AGENT_DIR_ENV, dir.path().display().to_string()),
        ]);
        let config = Config::from_lookup(|name| vars.get(name).cloned()).expect("valid");
        // The directory only has to exist while the configuration is read.
        drop(dir);
        config
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_configurations_secrets_and_the_secret_looking_variables_are_values_to_scrub() {
        let secrets = secret_values(
            &config(),
            env(&[
                ("GITHUB_TOKEN", "ghp_abcdef123456"),
                ("SEARCH_API_KEY", "key-for-search"),
                ("PATH", "/usr/bin:/bin"),
                ("HOME", "/home/agent"),
                ("TOKENS_LIST", "list-aaa, list-bbb"),
                ("SHORT_SECRET", "abc"),
            ]),
        );
        for value in [
            "sk-model-key-9f",
            "tok-one-aa",
            "tok-two-bb",
            "p%40ss-word-1",
            "p@ss-word-1",
            "ghp_abcdef123456",
            "key-for-search",
            "list-aaa",
            "list-bbb",
        ] {
            assert!(secrets.iter().any(|s| s == value), "{value} in {secrets:?}");
        }
        for value in ["/usr/bin:/bin", "/home/agent", "abc"] {
            assert!(
                !secrets.iter().any(|s| s == value),
                "{value} in {secrets:?}"
            );
        }
        // Longest first: `list-aaa, list-bbb` goes before its parts.
        let lens: Vec<usize> = secrets.iter().map(String::len).collect();
        assert!(lens.windows(2).all(|w| w[0] >= w[1]), "{lens:?}");
    }

    #[test]
    fn a_process_with_nothing_to_hide_sets_no_redactor() {
        let io = step_io(&config(), env(&[("GITHUB_TOKEN", "ghp_abcdef123456")]));
        assert_eq!(
            format!("{io:?}"),
            "StepIo { on: true, redact: true, input_max: 4096, output_max: 8192 }"
        );
        let mut bare = config();
        bare.worker = None;
        bare.service.a2a_bearer_tokens.clear();
        bare.service.database_url = "postgres://db/adam".to_owned().into();
        let io = step_io(&bare, env(&[("PATH", "/bin")]));
        assert_eq!(
            format!("{io:?}"),
            "StepIo { on: true, redact: false, input_max: 4096, output_max: 8192 }"
        );
    }

    #[test]
    fn scrub_replaces_every_occurrence_longest_value_first() {
        let secrets = vec!["abcd-longer".to_owned(), "abcd".to_owned()];
        assert_eq!(
            scrub(&secrets, "x abcd-longer y abcd z"),
            "x [redacted] y [redacted] z"
        );
        assert_eq!(scrub(&secrets, "nothing"), "nothing");
    }
}
