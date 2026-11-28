//! The run's certificate and the key that signs its model requests ([ADR 0086]).
//!
//! * [`RunCertIssuer`] is the port: a certificate signing request goes in, a [`CertificateChain`]
//!   comes out. A private key never crosses it, so a host may ask a CA over HTTP, a KMS or a
//!   secrets service.
//! * [`RunKeys`] is a [`TurnKeys`] that belongs to **one run**: the key a CA certified for it, and
//!   the key it renews to. It signs the run's model requests only; the history keeps the process's
//!   turn keys ([ADR 0068], *Amended 2026-11-28*).
//! * [`CertificateRequest`] is the CSR as adam carries it: bounded DER bytes, never a driver type.
//!
//! The run's model requests carry the leaf as `x5c` when the host gives run keys
//! (`adam_model::SignedRequests` with `KeySource::Run`). Nothing issues a certificate here: the
//! fake issuer and the cases every adapter passes are in [`testkit`].
//!
//! [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
//! [ADR 0086]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md

use std::fmt;
use std::sync::Arc;

use adam_error::{BoxError, Classify, ErrorClass};
use async_trait::async_trait;

use super::{CertificateChain, TurnKeys};

pub mod testkit;

mod keyring;
mod x509;

pub use keyring::{
    BadRenewFraction, BadRunSubject, RENEW_RETRY_SECS, RenewFraction, RunClock, RunKeyError,
    RunKeyring, RunSubject, SystemClock,
};

/// The certificate signing request a [`RunCertIssuer`] signs: the DER of a PKCS#10 request
/// (RFC 2986), which carries the run key's public key and the name the CA writes into the leaf.
///
/// Bounded, so a request cannot be a payload: at most [`MAX_LEN`](Self::MAX_LEN) bytes, never
/// empty. The bytes are not parsed here; whether the request is one is the issuer's answer
/// ([`RunCertError::Invalid`]).
#[derive(Clone, PartialEq, Eq)]
pub struct CertificateRequest {
    der: Vec<u8>,
}

impl CertificateRequest {
    /// The longest request, in DER bytes.
    pub const MAX_LEN: usize = 8 * 1024;

    /// The request `der`.
    ///
    /// # Errors
    ///
    /// [`BadCertificateRequest`] when `der` is empty or longer than [`MAX_LEN`](Self::MAX_LEN).
    /// The error does not repeat the bytes.
    pub fn new(der: impl Into<Vec<u8>>) -> Result<Self, BadCertificateRequest> {
        let der = der.into();
        if der.is_empty() || der.len() > Self::MAX_LEN {
            return Err(BadCertificateRequest);
        }
        Ok(Self { der })
    }

    /// The DER bytes.
    pub fn der(&self) -> &[u8] {
        &self.der
    }
}

impl fmt::Debug for CertificateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertificateRequest")
            .field("der_len", &self.der.len())
            .finish()
    }
}

/// A request that is not one ([`CertificateRequest::new`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "a certificate request is 1 to {max} DER bytes",
    max = CertificateRequest::MAX_LEN
)]
pub struct BadCertificateRequest;

impl Classify for BadCertificateRequest {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// Issues the certificate of a run's key: a [`CertificateRequest`] in, the [`CertificateChain`]
/// that vouches for the key out ([ADR 0086] decision 1).
///
/// The chain is the `x5c` a run's model request carries (leaf first). It is carried, not trusted:
/// the verifier in front of the model trusts the CA. `Debug` of a `dyn RunCertIssuer` shows
/// [`kind`](Self::kind) only, never the CA's address or a token.
///
/// [ADR 0086]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md
#[async_trait]
pub trait RunCertIssuer: Send + Sync + 'static {
    /// The chain that certifies `request`'s key.
    ///
    /// # Errors
    ///
    /// [`RunCertError::Invalid`] when the CA refuses the request as written,
    /// [`RunCertError::Unavailable`] when it cannot be reached, [`RunCertError::Refused`] when it
    /// refuses to issue, and [`RunCertError::Internal`] when what it answered is not a chain adam
    /// can carry.
    async fn issue(&self, request: &CertificateRequest) -> Result<CertificateChain, RunCertError>;

    /// What this issuer is, for `Debug` and startup logs. Never an address, a token or a key.
    fn kind(&self) -> &'static str {
        "host"
    }
}

impl fmt::Debug for dyn RunCertIssuer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RunCertIssuer({})", self.kind())
    }
}

/// A shared [`RunCertIssuer`].
pub type DynRunCertIssuer = Arc<dyn RunCertIssuer>;

/// The signing key of **one run** ([ADR 0086] decision 2): a key made for the run, certified by a
/// [`RunCertIssuer`], and renewed at two thirds of its leaf's life with a new key.
///
/// It is a [`TurnKeys`] and nothing more: the same `current`, `sign` and `key_set`, so the request
/// path does not know which it holds. What makes it the run's is the adapter that builds it and
/// the run it belongs to; there is deliberately no blanket implementation, so
/// `adam_model::KeySource::Run` accepts only a signer an adapter declared per-run.
///
/// It signs the run's **model requests only**. A run's history is signed by the process's
/// [`TurnKeys`], which outlives the run ([ADR 0068]).
///
/// [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
/// [ADR 0086]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md
pub trait RunKeys: TurnKeys {}

impl fmt::Debug for dyn RunKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RunKeys({})", self.kind())
    }
}

/// A shared [`RunKeys`].
pub type DynRunKeys = Arc<dyn RunKeys>;

/// Why a [`RunCertIssuer`] did not answer a chain. Carries no request and no key.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunCertError {
    /// The CA refuses the request as written (not a CSR, a name it does not issue).
    #[error("the certificate authority refuses the certificate request")]
    Invalid(#[source] BoxError),
    /// The CA cannot be reached now.
    #[error("the certificate authority is unavailable")]
    Unavailable(#[source] BoxError),
    /// The CA refuses to issue (a policy, a credential that does not allow it).
    #[error("the certificate authority refuses to issue")]
    Refused(#[source] BoxError),
    /// The CA answered something that is not a chain adam can carry (empty, too long, too many).
    #[error("the certificate authority answered a chain that is not one")]
    Internal(#[source] BoxError),
}

impl Classify for RunCertError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Invalid(_) => ErrorClass::Invalid,
            Self::Unavailable(_) => ErrorClass::Transient,
            Self::Refused(_) => ErrorClass::Unauthenticated,
            Self::Internal(_) => ErrorClass::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_bounded_and_holds_no_bytes_in_its_debug() {
        assert!(CertificateRequest::new(vec![0_u8; CertificateRequest::MAX_LEN]).is_ok());
        assert_eq!(
            CertificateRequest::new(Vec::new()),
            Err(BadCertificateRequest)
        );
        assert_eq!(
            CertificateRequest::new(vec![0_u8; CertificateRequest::MAX_LEN + 1]),
            Err(BadCertificateRequest)
        );
        let request = CertificateRequest::new(b"secret-csr".to_vec()).unwrap();
        assert_eq!(request.der(), b"secret-csr");
        assert!(!format!("{request:?}").contains("secret"));
        assert_eq!(BadCertificateRequest.class(), ErrorClass::Invalid);
    }

    #[test]
    fn each_error_has_its_class() {
        assert_eq!(
            RunCertError::Invalid("bad".into()).class(),
            ErrorClass::Invalid
        );
        assert_eq!(
            RunCertError::Unavailable("down".into()).class(),
            ErrorClass::Transient
        );
        assert_eq!(
            RunCertError::Refused("policy".into()).class(),
            ErrorClass::Unauthenticated
        );
        assert_eq!(
            RunCertError::Internal("not a chain".into()).class(),
            ErrorClass::Internal
        );
    }
}
