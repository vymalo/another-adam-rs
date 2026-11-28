//! The **broker** adapter of [`RunCertIssuer`]: a run's certificate over the internal contract v1
//! ([ADR 0086]).
//!
//! A [`RunKeyring`](adam_core::turn::RunKeyring) makes an Ed25519 key and a PKCS#10 request inside
//! the run, and asks this adapter for the chain that certifies it. The adapter sends
//! `POST {RUN_CERT_ISSUER_URL}/internal/runs/{run}/certificate` with `{"v":1,"csr":"<base64 DER>"}`
//! and the projected token from `RUN_CERT_TOKEN_FILE`, and answers the leaf-first chain of
//! `{"v":1,"chain":["<base64 DER>", …]}`. The private key never crosses this port.
//!
//! * **The token is re-read on every call.** A projected service-account token is rotated in place
//!   by the platform, so the adapter opens the file each time and sends what it holds now. The
//!   token is never logged, never in `Debug` and never in an error: the file's path is not a
//!   secret, its contents are.
//! * **The answer's `v` is checked.** A body that is not contract v1 is
//!   [`RunCertError::Internal`], never a chain. The status decides the rest: `400` is `Invalid`,
//!   `401`/`403` are `Refused`, `429` and `5xx` are `Unavailable`, and a status v1 does not name is
//!   `Internal`.
//! * **The answer's body is never echoed.** An error carries the status, not the problem document:
//!   the body is the peer's, and a token or a run id must not reach a log through it.
//! * **Trust anchors.** [`BrokerIssuer::new`] trusts the system roots;
//!   [`BrokerIssuer::trusting`] also trusts the PEM of `RUN_CERT_CA_FILE`. The anchors are read and
//!   checked at startup by [`http_client`], so a bad file fails before the first run.
//!
//! The contract is the one ANR-1306 recorded: `csr` and each `chain` element are **standard,
//! padded** base64 of DER, the chain is leaf first with the root omitted, and a refusal is an
//! RFC 9457 problem document whose `code` this adapter does not need to read. The v1 fixtures are
//! in `tests/fixtures/run-certificate-v1/`.
//!
//! adam carries at most [`CertificateChain::MAX_CERTIFICATES`](adam_core::turn::CertificateChain::MAX_CERTIFICATES)
//! (4) certificates, while contract v1 allows up to 8: a chain longer than 4 is
//! [`RunCertError::Internal`]. Real chains are 2 or 3; raising the port's bound is a later change,
//! not a silent truncation.
//!
//! [ADR 0086]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use adam_core::RunId;
use adam_core::turn::{CertificateChain, CertificateRequest, RunCertError, RunCertIssuer};
use adam_error::{BoxError, Classify, ErrorClass};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use secrecy::zeroize::Zeroizing;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use url::Url;

/// The contract version this adapter speaks, in the request's and the answer's `v`.
pub const CONTRACT_VERSION: u32 = 1;

/// The longest answer body read, in bytes: 8 certificates of 2048 DER bytes are about 22 KiB of
/// base64, so 64 KiB is generous and still bounds a hostile or broken peer.
pub const MAX_ANSWER_BYTES: usize = 64 * 1024;

/// The largest trust-anchor file [`http_client`] accepts: 1 MiB, far above any bundle of roots.
pub const MAX_CA_PEM_BYTES: usize = 1 << 20;

/// The HTTP client a [`BrokerIssuer`] uses, built once by a host with [`http_client`].
pub type HttpClient = reqwest::Client;

/// How long one certificate request may take, in total.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the connection may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The certificate authority of **one run**: the broker's internal route for that run, the
/// projected token file and the HTTP client.
///
/// One issuer is one run because the run is a path segment of the route: a broker that answered
/// another run's certificate would certify the wrong run. The keyring already belongs to one run,
/// so an adapter is built beside it.
///
/// `Debug` shows the endpoint's origin, the run and the token file's path: never the token.
pub struct BrokerIssuer {
    endpoint: Url,
    token_file: PathBuf,
    run: RunId,
    http: HttpClient,
}

impl BrokerIssuer {
    /// The issuer of `run`, at `base_url` (for example `https://broker.internal:8443`), presenting
    /// the token in `token_file`, trusting the system roots.
    ///
    /// # Errors
    ///
    /// [`BrokerError::Url`] or [`BrokerError::Scheme`] when `base_url` is not an absolute `http` or
    /// `https` URL with a host and no query or fragment, and [`BrokerError::Client`] when the HTTP
    /// client cannot be built.
    pub fn new(
        base_url: impl AsRef<str>,
        token_file: impl Into<PathBuf>,
        run: RunId,
    ) -> Result<Self, BrokerError> {
        Self::with_client(base_url, token_file, run, http_client(None)?)
    }

    /// The same, also trusting the certificates of the PEM `ca_pem` beside the system roots.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new), and [`BrokerError::CaFile`] when `ca_pem` is larger than
    /// [`MAX_CA_PEM_BYTES`], is not PEM, or holds no certificate.
    pub fn trusting(
        base_url: impl AsRef<str>,
        token_file: impl Into<PathBuf>,
        run: RunId,
        ca_pem: &[u8],
    ) -> Result<Self, BrokerError> {
        Self::with_client(base_url, token_file, run, http_client(Some(ca_pem))?)
    }

    /// The same, with an HTTP client the caller built once (a host makes one per process, not one
    /// per run, with [`http_client`]).
    ///
    /// # Errors
    ///
    /// [`BrokerError::Url`] or [`BrokerError::Scheme`] when `base_url` is not an absolute `http` or
    /// `https` URL with a host and no query or fragment.
    pub fn with_client(
        base_url: impl AsRef<str>,
        token_file: impl Into<PathBuf>,
        run: RunId,
        http: HttpClient,
    ) -> Result<Self, BrokerError> {
        Ok(Self {
            endpoint: endpoint(base_url.as_ref(), run)?,
            token_file: token_file.into(),
            run,
            http,
        })
    }

    /// The route this issuer posts to, run and all.
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// The run whose certificate this issuer asks for.
    pub fn run(&self) -> RunId {
        self.run
    }

    /// The file the projected token is read from, again on every call.
    pub fn token_file(&self) -> &Path {
        &self.token_file
    }

    /// The token now: the file's contents without the whitespace a projected secret ends with.
    async fn token(&self) -> Result<SecretString, RunCertError> {
        let raw = Zeroizing::new(tokio::fs::read_to_string(&self.token_file).await.map_err(
            |source| {
                RunCertError::Refused(Box::new(TokenError::Unreadable {
                    path: self.token_file.clone(),
                    source,
                }))
            },
        )?);
        let token = raw.trim();
        if token.is_empty() {
            return Err(RunCertError::Refused(Box::new(TokenError::Empty {
                path: self.token_file.clone(),
            })));
        }
        Ok(SecretString::from(token.to_owned()))
    }

    /// The answer's chain, or the class the status names.
    async fn answer(&self, response: reqwest::Response) -> Result<CertificateChain, RunCertError> {
        if response
            .content_length()
            .is_some_and(|len| len > MAX_ANSWER_BYTES as u64)
        {
            return Err(too_long());
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| RunCertError::Unavailable(Box::new(e.without_url())))?;
        if bytes.len() > MAX_ANSWER_BYTES {
            return Err(too_long());
        }
        let answer: Answer = serde_json::from_slice(&bytes)
            .map_err(|e| RunCertError::Internal(Box::new(AnswerError::Json(e))))?;
        if answer.v != CONTRACT_VERSION {
            return Err(RunCertError::Internal(Box::new(AnswerError::Version {
                found: answer.v,
            })));
        }
        let mut der = Vec::with_capacity(answer.chain.len());
        for certificate in &answer.chain {
            der.push(
                STANDARD
                    .decode(certificate)
                    .map_err(|e| RunCertError::Internal(Box::new(AnswerError::Base64(e))))?,
            );
        }
        CertificateChain::new(der)
            .map_err(|e| RunCertError::Internal(Box::new(AnswerError::Chain(e))))
    }
}

impl fmt::Debug for BrokerIssuer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrokerIssuer")
            .field(
                "endpoint",
                &format_args!(
                    "{}://{}",
                    self.endpoint.scheme(),
                    self.endpoint.host_str().unwrap_or_default()
                ),
            )
            .field("run", &self.run)
            .field("token_file", &self.token_file)
            .field("kind", &self.kind())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl RunCertIssuer for BrokerIssuer {
    async fn issue(&self, request: &CertificateRequest) -> Result<CertificateChain, RunCertError> {
        // Re-read on every call: the platform rotates a projected token in place.
        let token = self.token().await?;
        let body = RequestBody {
            v: CONTRACT_VERSION,
            csr: STANDARD.encode(request.der()),
        };
        let response = self
            .http
            .post(self.endpoint.clone())
            .header(AUTHORIZATION, bearer(&token)?)
            .header(ACCEPT, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| RunCertError::Unavailable(Box::new(e.without_url())))?;
        let status = response.status();
        if !status.is_success() {
            // The status, never the problem document's body.
            tracing::debug!(
                run = %self.run,
                status = status.as_u16(),
                "the broker refused the run's certificate"
            );
            return Err(classify_status(status.as_u16()));
        }
        let chain = self.answer(response).await?;
        tracing::debug!(
            run = %self.run,
            kid = %chain.kid(),
            "the broker issued the run's certificate"
        );
        Ok(chain)
    }

    fn kind(&self) -> &'static str {
        "broker"
    }
}

/// The HTTP client a [`BrokerIssuer`] uses: no redirect (a POST carrying a token is never replayed
/// elsewhere), a 30 s bound, and `ca_pem` trusted beside the system roots when given.
///
/// A host builds one and gives it to [`BrokerIssuer::with_client`] for every run, instead of one
/// client per run.
///
/// # Errors
///
/// [`BrokerError::CaFile`] when `ca_pem` is larger than [`MAX_CA_PEM_BYTES`], is not PEM, or holds
/// no certificate; [`BrokerError::Client`] when the TLS backend refuses the configuration.
pub fn http_client(ca_pem: Option<&[u8]>) -> Result<HttpClient, BrokerError> {
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("adam-run-cert-broker/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    if let Some(pem) = ca_pem {
        if pem.len() > MAX_CA_PEM_BYTES {
            return Err(BrokerError::CaFile(
                format!("it is longer than {MAX_CA_PEM_BYTES} bytes").into(),
            ));
        }
        let certificates = reqwest::Certificate::from_pem_bundle(pem)
            .map_err(|e| BrokerError::CaFile(Box::new(e.without_url())))?;
        if certificates.is_empty() {
            return Err(BrokerError::CaFile("it holds no certificate".into()));
        }
        builder = builder.tls_certs_merge(certificates);
    }
    builder.build().map_err(BrokerError::Client)
}

/// The route of `run` under `base_url`, keeping any path prefix the base carries.
fn endpoint(base_url: &str, run: RunId) -> Result<Url, BrokerError> {
    let mut url = Url::parse(base_url).map_err(BrokerError::Url)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(BrokerError::Scheme);
    }
    let path = format!(
        "{}/internal/runs/{run}/certificate",
        url.path().trim_end_matches('/')
    );
    url.set_path(&path);
    Ok(url)
}

/// `Authorization: Bearer <token>`, marked sensitive so a `Debug` of the header shows no value.
fn bearer(token: &SecretString) -> Result<HeaderValue, RunCertError> {
    let header = Zeroizing::new(format!("Bearer {}", token.expose_secret()));
    let mut value = HeaderValue::from_str(&header)
        .map_err(|_| RunCertError::Refused(Box::new(TokenError::NotAHeader)))?;
    value.set_sensitive(true);
    Ok(value)
}

/// The class a refusal's status names.
fn classify_status(status: u16) -> RunCertError {
    let error = || Box::new(AnswerError::Status { status }) as BoxError;
    match status {
        400 => RunCertError::Invalid(error()),
        401 | 403 => RunCertError::Refused(error()),
        429 | 500..=599 => RunCertError::Unavailable(error()),
        _ => RunCertError::Internal(error()),
    }
}

fn too_long() -> RunCertError {
    RunCertError::Internal(Box::new(AnswerError::TooLong {
        max: MAX_ANSWER_BYTES,
    }))
}

#[derive(Serialize)]
struct RequestBody {
    v: u32,
    csr: String,
}

#[derive(Deserialize)]
struct Answer {
    v: u32,
    chain: Vec<String>,
}

/// Why the broker's URL or trust anchors cannot make an issuer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BrokerError {
    /// The base URL does not parse.
    #[error("the broker's base URL is not a URL")]
    Url(#[source] url::ParseError),
    /// The base URL parses but is not an absolute `http(s)` URL with a host, or carries a query or
    /// a fragment.
    #[error("the broker's base URL is not an absolute http(s) URL with a host")]
    Scheme,
    /// The trust anchors are not a PEM bundle of certificates.
    #[error("the trust anchors are not a PEM bundle of certificates")]
    CaFile(#[source] BoxError),
    /// The HTTP client cannot be built (a TLS configuration the backend refuses).
    #[error("the HTTP client for the broker cannot be built")]
    Client(#[source] reqwest::Error),
}

impl Classify for BrokerError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// Why the projected token could not be presented. Carries the file's path, never its contents.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TokenError {
    /// The file cannot be read (not mounted yet, rotated away, a permission).
    #[error("the projected token file {path:?} cannot be read")]
    Unreadable {
        /// The file that was read.
        path: PathBuf,
        /// Why it could not be read.
        #[source]
        source: std::io::Error,
    },
    /// The file holds nothing but whitespace.
    #[error("the projected token file {path:?} is empty")]
    Empty {
        /// The file that was read.
        path: PathBuf,
    },
    /// The token holds a byte an HTTP header cannot carry.
    #[error("the projected token is not a header value")]
    NotAHeader,
}

impl Classify for TokenError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Unauthenticated
    }
}

/// Why the broker's answer is not a chain adam can carry. Carries the status or the shape, never
/// the body.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AnswerError {
    /// The answer's `v` is not [`CONTRACT_VERSION`].
    #[error("the broker answered contract v{found}, and adam speaks v1")]
    Version {
        /// The version the broker sent.
        found: u32,
    },
    /// The body is longer than [`MAX_ANSWER_BYTES`].
    #[error("the answer is longer than {max} bytes")]
    TooLong {
        /// The bound.
        max: usize,
    },
    /// The body is not the contract's JSON.
    #[error("the answer is not the contract's JSON")]
    Json(#[source] serde_json::Error),
    /// A `chain` element is not standard, padded base64.
    #[error("a certificate of the answer is not base64")]
    Base64(#[source] base64::DecodeError),
    /// The decoded chain is not one [`CertificateChain`] can carry.
    #[error("the chain is not one adam can carry")]
    Chain(#[source] adam_core::turn::BadCertificateChain),
    /// A status contract v1 does not name, or the status of a refusal.
    #[error("the broker answered {status}")]
    Status {
        /// The HTTP status.
        status: u16,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_endpoint_keeps_a_path_prefix_and_adds_the_route() {
        let run = RunId::new();
        for (base, prefix) in [
            ("https://broker.internal:8443", ""),
            ("https://broker.internal:8443/", ""),
            ("https://broker.internal:8443/prefix/", "/prefix"),
        ] {
            let url = endpoint(base, run).unwrap();
            assert_eq!(
                url.as_str(),
                format!("https://broker.internal:8443{prefix}/internal/runs/{run}/certificate"),
                "{base}"
            );
        }
    }

    #[test]
    fn a_base_url_that_is_not_http_is_refused() {
        for bad in [
            "ftp://broker.internal/",
            "broker.internal",
            "https://broker.internal/?tenant=1",
            "https://broker.internal/#fragment",
        ] {
            let error = endpoint(bad, RunId::new()).unwrap_err();
            assert_eq!(error.class(), ErrorClass::Invalid, "{bad}");
        }
    }

    #[test]
    fn a_trust_anchor_that_is_not_pem_is_refused() {
        let error = http_client(Some(b"not a pem bundle")).unwrap_err();
        assert_eq!(error.class(), ErrorClass::Invalid);
    }

    #[test]
    fn a_refusal_status_has_the_class_the_keyring_reads() {
        assert!(matches!(classify_status(400), RunCertError::Invalid(_)));
        assert!(matches!(classify_status(401), RunCertError::Refused(_)));
        assert!(matches!(classify_status(403), RunCertError::Refused(_)));
        assert!(matches!(classify_status(429), RunCertError::Unavailable(_)));
        assert!(matches!(classify_status(503), RunCertError::Unavailable(_)));
        assert!(matches!(classify_status(404), RunCertError::Internal(_)));
    }
}
