//! Configuration of the `adam-run-signer` sidecar, read from the environment.
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `RUN_SIGNER_LISTEN` | the address the sidecar listens on; **must be loopback** | `127.0.0.1:8443` |
//! | `RUN_SIGNER_UPSTREAM` | the gateway's base URL: absolute `http` or `https`, a host, no query, no fragment, no credentials | required |
//! | `RUN_SIGNER_UPSTREAM_CA_FILE` | a PEM bundle the client trusts for the gateway, beside the platform's roots | the CA bundle the caller passed |
//!
//! The CA the **run certificate** comes from is not read here: it is the run certificate
//! deployment's (`RUN_CERT_CA_FILE` in the image), and [`crate::broker::from_process_env`] reads the
//! file and passes the bytes as [`SignerConfig::from_env`]'s `ca_pem`. Those bytes are also the
//! upstream client's trust anchors by default, because the gateway and the broker are one trust
//! domain today; `RUN_SIGNER_UPSTREAM_CA_FILE` names the anchors separately so that a reader does
//! not assume they always are one, and so that an operator can point the client at another bundle
//! without touching the certificate's.
//!
//! The listen address must be a loopback one: the sidecar holds the run's private key, so it must
//! never be reachable from outside the pod. A non-loopback address is a configuration problem
//! (exit 78), not a warning.
//!
//! Every problem is reported at once, and no problem quotes a value that may hold a secret: the
//! upstream's URL is never echoed, and its `Debug` shows the scheme, the host and the port only.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use adam_error::{Classify, ErrorClass};
use adam_run_cert_broker::MAX_CA_PEM_BYTES;
use url::Url;

use crate::issuer::IssuerFactory;

/// The address the sidecar listens on, when `RUN_SIGNER_LISTEN` is unset: loopback, so that only the
/// run's own pod can reach it.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8443";

/// `RUN_SIGNER_LISTEN`.
const LISTEN: &str = "RUN_SIGNER_LISTEN";
/// `RUN_SIGNER_UPSTREAM`.
const UPSTREAM: &str = "RUN_SIGNER_UPSTREAM";
/// `RUN_SIGNER_UPSTREAM_CA_FILE`.
const UPSTREAM_CA_FILE: &str = "RUN_SIGNER_UPSTREAM_CA_FILE";
/// What the `Debug` says when the caller passed no CA bundle at all.
const NO_CA: &str = "none";
/// What the `Debug` says when the anchors are the caller's (the run certificate's CA bundle).
const DEFAULT_CA: &str = "the CA bundle passed in (RUN_CERT_CA_FILE)";

/// Why the environment does not describe a sidecar: every problem at once, never a secret.
/// Classified as [`ErrorClass::Invalid`]: the same environment never works.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
#[error("invalid configuration:\n  - {}", problems.join("\n  - "))]
pub struct ConfigError {
    /// One line per problem.
    pub problems: Vec<String>,
}

impl ConfigError {
    /// A configuration error of `problems`, for a host that found some of its own beside the
    /// sidecar's (the binary's `RUN_CERT_*` half, for example).
    #[must_use]
    pub fn new(problems: Vec<String>) -> Self {
        Self { problems }
    }
}

impl Classify for ConfigError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// The limits that are not variables: fixed numbers of the sidecar, a field here so that a test can
/// make them short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most body bytes buffered before signing (16 MiB): a larger body is `413`.
    pub max_body: usize,
    /// How long connecting to the gateway may take (10 s).
    pub connect_timeout: Duration,
    /// How long the gateway may take to answer at all (300 s): a black-holed gateway frees the
    /// request instead of pinning it until the process stops. Above any healthy server-sent event
    /// gap, and above [`shutdown_grace`](Self::shutdown_grace), so a drain is never cut by it.
    pub read_timeout: Duration,
    /// How many runs' keyrings are kept; the least recently used is dropped beyond it (1024).
    pub max_runs: usize,
    /// How long a shutdown waits for the requests in flight (25 s).
    pub shutdown_grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_body: 16 * 1024 * 1024,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(300),
            max_runs: 1024,
            shutdown_grace: Duration::from_secs(25),
        }
    }
}

/// The sidecar's configuration.
///
/// The HTTP client is built here, from [`Limits::connect_timeout`] and the trust anchors
/// (`RUN_SIGNER_UPSTREAM_CA_FILE` when it is set, the caller's `ca_pem` otherwise), so a bad CA file
/// fails at startup and never at the first call.
pub struct SignerConfig {
    /// `RUN_SIGNER_LISTEN`.
    pub listen: SocketAddr,
    /// `RUN_SIGNER_UPSTREAM`: the base every request's path and query are appended to.
    pub upstream: Url,
    /// The client that forwards a signed request, trusting the anchors below.
    pub client: reqwest::Client,
    /// Where each run's keyring gets its issuer.
    pub issuer: Arc<dyn IssuerFactory>,
    /// The fixed limits.
    pub limits: Limits,
    /// Which bundle the client trusts beside the platform's roots: `RUN_SIGNER_UPSTREAM_CA_FILE`,
    /// the caller's bundle, or [`NO_CA`]'s `"none"`. A name, never a path or a byte.
    upstream_ca: &'static str,
    /// How many certificates that bundle holds.
    upstream_ca_roots: usize,
}

impl SignerConfig {
    /// Read the process environment, with `issuer` for every run's keyring and `ca_pem` (the bytes
    /// of `RUN_CERT_CA_FILE`, read by the caller) as the default trust anchors of the client.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] listing every problem.
    pub fn from_env(
        issuer: Arc<dyn IssuerFactory>,
        ca_pem: Option<Vec<u8>>,
    ) -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok(), issuer, ca_pem)
    }

    /// Read variables through `lookup` (`None` = unset; a blank value counts as unset). The file
    /// `RUN_SIGNER_UPSTREAM_CA_FILE` names is read from the file system.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] listing every problem.
    pub fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
        issuer: Arc<dyn IssuerFactory>,
        ca_pem: Option<Vec<u8>>,
    ) -> Result<Self, ConfigError> {
        let limits = Limits::default();
        let get = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        let mut problems = Vec::new();

        let listen = listen(get(LISTEN), &mut problems);
        let upstream = upstream(get(UPSTREAM), &mut problems);
        let anchors = anchors(get(UPSTREAM_CA_FILE), ca_pem, &mut problems);
        let (upstream_ca, built) = match anchors {
            Anchors::Refused => (UPSTREAM_CA_FILE, None),
            Anchors::None => (NO_CA, client(&limits, None, &mut problems)),
            Anchors::Pem { label, pem } => (
                label,
                client(&limits, Some((label, pem.as_slice())), &mut problems),
            ),
        };

        match (problems.is_empty(), listen, upstream, built) {
            (true, Some(listen), Some(upstream), Some((client, roots))) => Ok(Self {
                listen,
                upstream,
                client,
                issuer,
                limits,
                upstream_ca,
                upstream_ca_roots: roots,
            }),
            _ => Err(ConfigError { problems }),
        }
    }
}

impl fmt::Debug for SignerConfig {
    /// The listen address, the upstream's scheme, host and port, which CA bundle the client trusts
    /// (a name and how many certificates, never the bytes), the issuer's kind and the limits: never
    /// the upstream's path or query, never a credential, never a key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignerConfig")
            .field("listen", &self.listen)
            .field("upstream_scheme", &self.upstream.scheme())
            .field("upstream_host", &self.upstream.host_str())
            .field("upstream_port", &self.upstream.port_or_known_default())
            .field("upstream_ca", &self.upstream_ca)
            .field("upstream_ca_roots", &self.upstream_ca_roots)
            .field("issuer", &self.issuer.kind())
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// `RUN_SIGNER_LISTEN` as an address, when it is a loopback one.
fn listen(raw: Option<String>, problems: &mut Vec<String>) -> Option<SocketAddr> {
    let Some(raw) = raw else {
        return DEFAULT_LISTEN.parse().ok();
    };
    match raw.trim().parse::<SocketAddr>() {
        Ok(addr) if addr.ip().is_loopback() => Some(addr),
        Ok(addr) => {
            problems.push(format!(
                "{LISTEN} {addr} is not a loopback address: the sidecar holds the run's private \
                 key, so it listens on loopback only, such as {DEFAULT_LISTEN}"
            ));
            None
        }
        Err(_) => {
            problems.push(format!(
                "{LISTEN} is not an address and port such as {DEFAULT_LISTEN}"
            ));
            None
        }
    }
}

/// `RUN_SIGNER_UPSTREAM` as a base URL. Nothing echoes the value: it may hold a credential.
fn upstream(raw: Option<String>, problems: &mut Vec<String>) -> Option<Url> {
    let Some(raw) = raw else {
        problems.push(format!(
            "{UPSTREAM} is required: the absolute http or https base URL of the gateway, such as \
             http://gateway:8080"
        ));
        return None;
    };
    let Ok(url) = Url::parse(raw.trim()) else {
        problems.push(format!(
            "{UPSTREAM} is not an absolute http or https URL such as http://gateway:8080"
        ));
        return None;
    };
    let mut ok = true;
    if !matches!(url.scheme(), "http" | "https") {
        problems.push(format!(
            "{UPSTREAM} is not an http or https URL: only those reach a gateway"
        ));
        ok = false;
    }
    if url.host_str().is_none() {
        problems.push(format!("{UPSTREAM} names no host"));
        ok = false;
    }
    if url.query().is_some() || url.fragment().is_some() {
        problems.push(format!(
            "{UPSTREAM} has a query or a fragment: it is a base URL, and the request's own path and \
             query are appended to it"
        ));
        ok = false;
    }
    if !url.username().is_empty() || url.password().is_some() {
        problems.push(format!(
            "{UPSTREAM} carries credentials: the gateway is reached on the pod's network, and every \
             request would send them"
        ));
        ok = false;
    }
    ok.then_some(url)
}

/// Which PEM the client trusts: `RUN_SIGNER_UPSTREAM_CA_FILE`'s when it is set, the caller's
/// otherwise.
enum Anchors {
    /// Neither was given: the platform's roots alone.
    None,
    /// A bundle, and the name the messages give it.
    Pem {
        /// `RUN_SIGNER_UPSTREAM_CA_FILE`, or the caller's default.
        label: &'static str,
        /// The bytes of the bundle.
        pem: Vec<u8>,
    },
    /// The override is set and could not be read: a problem was recorded, and the caller's default
    /// is not used (it is not what was asked for).
    Refused,
}

/// The anchors to trust, reading the override file when it is set. A problem is recorded and the
/// override refused for a file that cannot be read or is over [`MAX_CA_PEM_BYTES`] (the broker's
/// bound, imported so the two cannot drift); a file that is not PEM is refused by [`client`], which
/// has the bytes in hand.
fn anchors(
    override_file: Option<String>,
    default: Option<Vec<u8>>,
    problems: &mut Vec<String>,
) -> Anchors {
    let Some(path) = override_file else {
        return match default {
            Some(pem) => Anchors::Pem {
                label: DEFAULT_CA,
                pem,
            },
            None => Anchors::None,
        };
    };
    let path = PathBuf::from(path.trim());
    match read_ca_file(UPSTREAM_CA_FILE, &path) {
        Ok(pem) => Anchors::Pem {
            label: UPSTREAM_CA_FILE,
            pem,
        },
        Err(problem) => {
            problems.push(problem);
            Anchors::Refused
        }
    }
}

/// The bytes of the CA file at `path`, at most [`MAX_CA_PEM_BYTES`]. `variable` is the setting the
/// file came from (`RUN_SIGNER_UPSTREAM_CA_FILE`, `RUN_CERT_CA_FILE`): the problem names it and the
/// file, never a byte of it.
///
/// # Errors
///
/// A problem line naming `variable`: the file cannot be read, or it is over [`MAX_CA_PEM_BYTES`].
pub fn read_ca_file(variable: &str, path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read as _;

    let file = std::fs::File::open(path)
        .map_err(|error| format!("{variable} {} cannot be read: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_CA_PEM_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{variable} {} cannot be read: {error}", path.display()))?;
    if bytes.len() > MAX_CA_PEM_BYTES {
        return Err(format!(
            "{variable} {} is larger than {MAX_CA_PEM_BYTES} bytes",
            path.display()
        ));
    }
    Ok(bytes)
}

/// The client, trusting `ca` beside the platform's roots when it is given: the client and how many
/// roots it added.
fn client(
    limits: &Limits,
    ca: Option<(&'static str, &[u8])>,
    problems: &mut Vec<String>,
) -> Option<(reqwest::Client, usize)> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(limits.connect_timeout)
        // A black-holed gateway frees the request rather than pinning it until the process stops.
        .read_timeout(limits.read_timeout)
        // Never follow a redirect with a signed request: the signature covers this path, not another.
        .redirect(reqwest::redirect::Policy::none())
        // A signed request carries the prompt, the person's envelope and a valid `Turn-Signature`:
        // a proxy from the environment would hand all three to another host.
        .no_proxy()
        .user_agent(concat!("adam-run-signer/", env!("CARGO_PKG_VERSION")));
    let mut roots = 0;
    if let Some((label, pem)) = ca {
        match reqwest::Certificate::from_pem_bundle(pem) {
            Ok(certs) if certs.is_empty() => {
                problems.push(format!("{label} holds no certificate"));
                return None;
            }
            Ok(certs) => {
                roots = certs.len();
                builder = builder.tls_certs_merge(certs);
            }
            Err(error) => {
                problems.push(format!(
                    "{label} is not a PEM certificate bundle: {}",
                    error.without_url()
                ));
                return None;
            }
        }
    }
    match builder.build() {
        Ok(client) => Some((client, roots)),
        Err(error) => {
            problems.push(format!(
                "the HTTP client could not be built: {}",
                error.without_url()
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use adam_core::RunId;
    use adam_core::turn::run_cert::DynRunCertIssuer;
    use adam_error::BoxError;

    use super::*;
    use crate::error::{SignerError, exit_code};

    struct NoIssuer;

    impl IssuerFactory for NoIssuer {
        fn issuer(&self, _run: RunId) -> Result<DynRunCertIssuer, BoxError> {
            Err("no issuer here".into())
        }
    }

    fn read(vars: &[(&str, &str)]) -> Result<SignerConfig, ConfigError> {
        read_with(vars, None)
    }

    fn read_with(
        vars: &[(&str, &str)],
        ca_pem: Option<Vec<u8>>,
    ) -> Result<SignerConfig, ConfigError> {
        let vars: std::collections::HashMap<_, _> = vars.iter().copied().collect();
        SignerConfig::from_lookup(
            |name| vars.get(name).map(|value| (*value).to_owned()),
            Arc::new(NoIssuer),
            ca_pem,
        )
    }

    fn problems(vars: &[(&str, &str)]) -> Vec<String> {
        read(vars)
            .map(|_| Vec::new())
            .unwrap_or_else(|e| e.problems)
    }

    #[test]
    fn the_defaults_are_loopback_and_the_documented_limits() {
        let c = read(&[("RUN_SIGNER_UPSTREAM", "http://127.0.0.1:8080")]).unwrap();
        assert_eq!(c.listen, "127.0.0.1:8443".parse().unwrap());
        assert!(c.listen.ip().is_loopback());
        assert_eq!(c.upstream.as_str(), "http://127.0.0.1:8080/");
        assert_eq!(c.limits, Limits::default());
        assert_eq!(c.limits.max_body, 16 * 1024 * 1024);
        assert_eq!(c.limits.connect_timeout, Duration::from_secs(10));
        assert_eq!(c.limits.read_timeout, Duration::from_secs(300));
        assert_eq!(c.limits.max_runs, 1024);
        assert_eq!(c.limits.shutdown_grace, Duration::from_secs(25));
        // A drain is never cut short by the read timeout, and a healthy stream is never cut by it.
        assert!(c.limits.read_timeout > c.limits.shutdown_grace);
        assert!(c.limits.read_timeout > c.limits.connect_timeout);
        assert_eq!(c.upstream_ca, NO_CA);
        assert_eq!(c.upstream_ca_roots, 0);
    }

    #[test]
    fn a_loopback_listen_address_is_read() {
        for (text, ip) in [
            ("127.0.0.1:9000", "127.0.0.1"),
            ("[::1]:9000", "::1"),
            ("127.5.6.7:1", "127.5.6.7"),
        ] {
            let c = read(&[
                ("RUN_SIGNER_UPSTREAM", "http://gateway:8080"),
                ("RUN_SIGNER_LISTEN", text),
            ])
            .unwrap();
            assert_eq!(c.listen.ip().to_string(), ip, "{text}");
        }
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let e = read(&[
            ("RUN_SIGNER_LISTEN", "not-an-address"),
            ("RUN_SIGNER_UPSTREAM", "not a url"),
        ])
        .unwrap_err();
        assert_eq!(e.problems.len(), 2, "{e}");
        assert!(e.problems.iter().any(|p| p.contains(LISTEN)), "{e}");
        assert!(e.problems.iter().any(|p| p.contains(UPSTREAM)), "{e}");
        assert_eq!(e.class(), ErrorClass::Invalid);
        assert_eq!(exit_code(&SignerError::Config(e)), 78);
    }

    #[test]
    fn a_bad_upstream_is_a_problem_that_never_quotes_it() {
        for bad in [
            "ftp://gateway:8080",
            "http://gateway:8080/v1?token=SENTINEL",
            "http://gateway:8080/v1#SENTINEL",
            "http://user:SENTINEL@gateway:8080",
            "http://",
            "gateway:8080",
        ] {
            let p = problems(&[("RUN_SIGNER_UPSTREAM", bad)]);
            assert!(
                p.iter().any(|p| p.contains(UPSTREAM)),
                "{bad}: {p:?} names no variable"
            );
            for problem in &p {
                assert!(
                    !problem.contains("SENTINEL"),
                    "{bad} was echoed by {problem:?}"
                );
            }
        }
    }

    #[test]
    fn a_base_path_is_allowed() {
        let c = read(&[("RUN_SIGNER_UPSTREAM", "https://gateway.example/model")]).unwrap();
        assert_eq!(c.upstream.path(), "/model");
    }

    #[test]
    fn blank_values_are_unset() {
        let c = read(&[
            ("RUN_SIGNER_UPSTREAM", "http://gateway:8080"),
            ("RUN_SIGNER_LISTEN", "  "),
            ("RUN_SIGNER_UPSTREAM_CA_FILE", "  "),
        ])
        .unwrap();
        assert_eq!(c.listen, DEFAULT_LISTEN.parse().unwrap());
        assert_eq!(c.upstream_ca, NO_CA);
    }

    #[test]
    fn a_ca_bundle_that_is_no_pem_is_a_problem() {
        let e = read_with(
            &[("RUN_SIGNER_UPSTREAM", "http://gateway:8080")],
            Some(b"not a pem".to_vec()),
        )
        .unwrap_err();
        assert_eq!(e.problems.len(), 1, "{e}");
        assert!(
            e.problems[0].contains("CA bundle") || e.problems[0].contains("holds no certificate"),
            "{e}"
        );
        assert_eq!(e.class(), ErrorClass::Invalid);
        let e = read_with(
            &[("RUN_SIGNER_UPSTREAM", "http://gateway:8080")],
            Some(Vec::new()),
        )
        .unwrap_err();
        assert!(
            e.problems
                .iter()
                .any(|p| p.contains("CA bundle") || p.contains("holds no certificate")),
            "{e}"
        );
    }
}
