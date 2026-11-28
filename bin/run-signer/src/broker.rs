//! The production configuration: the run certificate's half (`RUN_CERT_*`) and the sidecar's own
//! (`RUN_SIGNER_*`) read from one environment, into the factory the sidecar signs each run with.
//!
//! The run certificate is **required** here, unlike for an agent: a signer with no issuer cannot sign
//! at all. Every `RUN_CERT_*` unset is therefore this binary's own problem, and a half-set
//! configuration is refused with the problems `adam-service` reports — beside the sidecar's own, so
//! one start reports everything that is wrong rather than the first half.
//!
//! `RUN_CERT_CA_FILE` is also the upstream client's default trust anchor: the broker and the gateway
//! are one trust domain by design today. `RUN_SIGNER_UPSTREAM_CA_FILE` overrides it for the upstream
//! alone.

use std::sync::Arc;

use adam_core::RunId;
use adam_core::turn::run_cert::{DynRunCertIssuer, RenewFraction};
use adam_error::BoxError;
use adam_service::config::RunCertSettings;

use crate::config::{ConfigError, SignerConfig, read_ca_file};
use crate::error::SignerError;
use crate::issuer::IssuerFactory;

/// The problem every `RUN_CERT_*` unset is for this binary, which cannot sign without one.
const RUN_CERTIFICATES_REQUIRED: &str = "run certificates are required: set RUN_CERT_ISSUER, \
     RUN_CERT_ISSUER_URL and RUN_CERT_TOKEN_FILE (the signer signs only with a run's certificate)";
/// `RUN_CERT_CA_FILE`: the file the broker's anchors and the upstream's default anchors come from.
const RUN_CERT_CA_FILE: &str = "RUN_CERT_CA_FILE";

/// `RUN_CERT_*` as an [`IssuerFactory`]: one broker issuer per run, from the same settings the agents
/// read ([`RunCertSettings`]).
pub struct Broker(RunCertSettings);

impl IssuerFactory for Broker {
    fn issuer(&self, run: RunId) -> Result<DynRunCertIssuer, BoxError> {
        Ok(self.0.issuer(run)?)
    }

    fn renew_fraction(&self) -> RenewFraction {
        self.0.renew_fraction
    }

    fn kind(&self) -> &'static str {
        "broker"
    }
}

/// A factory for a configuration that has no certificate half: it refuses every run. It exists only so
/// that the sidecar's own problems can be collected beside the certificate half's, never to serve.
struct NoIssuer;

impl IssuerFactory for NoIssuer {
    fn issuer(&self, _run: RunId) -> Result<DynRunCertIssuer, BoxError> {
        Err("run certificates are not configured".into())
    }

    fn kind(&self) -> &'static str {
        "none"
    }
}

/// The sidecar's configuration, from the process's `RUN_CERT_*` and `RUN_SIGNER_*`.
///
/// # Errors
///
/// [`SignerError::Config`] listing **every** problem of both halves at once.
pub fn from_process_env() -> Result<SignerConfig, SignerError> {
    from_lookup_env(|name| std::env::var(name).ok())
}

/// [`from_process_env`] over `lookup` (`None` = unset; a blank value counts as unset), for a test.
///
/// # Errors
///
/// [`SignerError::Config`] listing **every** problem of both halves at once: the certificate
/// settings' own, "run certificates are required" when none of them is set, an unusable
/// `RUN_CERT_CA_FILE`, and every `RUN_SIGNER_*` problem.
pub fn from_lookup_env(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<SignerConfig, SignerError> {
    let mut problems = Vec::new();
    let settings = RunCertSettings::parse(&lookup, &mut problems);
    if settings.is_none() && problems.is_empty() {
        problems.push(RUN_CERTIFICATES_REQUIRED.to_owned());
    }

    // The default anchors of the upstream client: the broker's file. `RunCertSettings::parse` has
    // already read and checked it, so a failure here is a race, and it is still a problem.
    let mut ca_pem = None;
    if let Some(settings) = &settings
        && let Some(path) = settings.ca_file.as_deref()
    {
        match read_ca_file(RUN_CERT_CA_FILE, path) {
            Ok(pem) => ca_pem = Some(pem),
            Err(why) => problems.push(why),
        }
    }

    let issuer: Arc<dyn IssuerFactory> = match settings {
        Some(settings) => Arc::new(Broker(settings)),
        None => Arc::new(NoIssuer),
    };
    // The sidecar's half is read whether or not the certificate half parsed, so both halves' problems
    // arrive together.
    match SignerConfig::from_lookup(&lookup, issuer, ca_pem) {
        Ok(config) if problems.is_empty() => Ok(config),
        Ok(_) => Err(SignerError::Config(ConfigError::new(problems))),
        Err(sidecar) => {
            problems.extend(sidecar.problems);
            Err(SignerError::Config(ConfigError::new(problems)))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use adam_error::Classify;

    use super::*;

    fn read(vars: &[(&str, &str)]) -> Result<SignerConfig, SignerError> {
        let vars: HashMap<_, _> = vars.iter().copied().collect();
        from_lookup_env(|name| vars.get(name).map(|value| (*value).to_owned()))
    }

    fn problems(vars: &[(&str, &str)]) -> Vec<String> {
        match read(vars) {
            Ok(_) => Vec::new(),
            Err(SignerError::Config(e)) => e.problems,
            Err(other) => panic!("not a configuration problem: {other}"),
        }
    }

    fn token_file() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "a token").unwrap();
        (dir, path.to_str().unwrap().to_owned())
    }

    /// The empty environment is refused with **both** halves' problems, in one error: the sidecar's
    /// upstream, and the certificate half's three names.
    #[test]
    fn an_empty_environment_names_both_halves() {
        let e = read(&[]).unwrap_err();
        assert_eq!(e.class(), adam_error::ErrorClass::Invalid);
        let SignerError::Config(config) = &e else {
            panic!("not a configuration problem: {e}");
        };
        for name in [
            "RUN_CERT_ISSUER",
            "RUN_CERT_ISSUER_URL",
            "RUN_CERT_TOKEN_FILE",
            "RUN_SIGNER_UPSTREAM",
        ] {
            assert!(
                config.problems.iter().any(|problem| problem.contains(name)),
                "{name} is named by none of {:?}",
                config.problems
            );
        }
    }

    /// A broken certificate half does not hide the sidecar's own problem: both are reported.
    #[test]
    fn a_broken_certificate_half_still_reports_the_sidecars_own() {
        let (_dir, token) = token_file();
        let problems = problems(&[
            ("RUN_CERT_ISSUER", "broker"),
            ("RUN_CERT_TOKEN_FILE", &token),
            ("RUN_SIGNER_LISTEN", "0.0.0.0:8443"),
        ]);
        assert!(
            problems.iter().any(|p| p.contains("RUN_CERT_ISSUER_URL")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("RUN_SIGNER_UPSTREAM")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("RUN_SIGNER_LISTEN")),
            "{problems:?}"
        );
    }

    /// The certificate half's renewal fraction reaches the factory, and so the keyring.
    #[test]
    fn the_renew_fraction_is_the_settings_own() {
        let (_dir, token) = token_file();
        let config = read(&[
            ("RUN_CERT_ISSUER", "broker"),
            ("RUN_CERT_ISSUER_URL", "https://broker.internal:8443"),
            ("RUN_CERT_TOKEN_FILE", &token),
            ("RUN_CERT_RENEW_FRACTION", "1/2"),
            ("RUN_SIGNER_UPSTREAM", "http://gateway:8080"),
        ])
        .unwrap();
        assert_eq!(
            config.issuer.renew_fraction(),
            RenewFraction::new(1, 2).unwrap()
        );
    }
}
