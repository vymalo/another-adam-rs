//! The cases every [`RunCertIssuer`] passes, and a fake issuer for the tests of what holds one.
//!
//! Use [`run_cert_issuer_conformance!`](crate::run_cert_issuer_conformance) in an integration test
//! (it needs `tokio` with `macros` and `rt` among the test's dependencies):
//!
//! ```ignore
//! use std::sync::Arc;
//!
//! use adam_core::turn::run_cert::DynRunCertIssuer;
//!
//! fn make() -> DynRunCertIssuer {
//!     Arc::new(MyCaIssuer::new())
//! }
//!
//! adam_core::run_cert_issuer_conformance!(make);
//! ```
//!
//! `$make` is a path to a `fn() -> DynRunCertIssuer`. A case panics on a failure: it is test code.
//!
//! [`FakeRunCertIssuer`] answers a chain derived from the request's bytes, so it passes the suite
//! and makes a different leaf for every key. [`Certified`] is marked [`RunKeys`] here, so a test of
//! the run-key path can build one from an issued chain.

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use super::{CertificateRequest, RunCertError, RunCertIssuer, RunKeys};
use crate::turn::CertificateChain;
use crate::turn::testkit::Certified;

/// A [`RunKeys`] for test code: the certificate adapter [`Certified`] is a run's signer.
impl RunKeys for Certified {}

/// An issuer that answers a chain built from the request's bytes: the leaf is a prefix and the
/// SHA-256 of the CSR, so two requests get two leaves, and the CA is a fixed certificate.
///
/// It is not a CA: nothing in the chain verifies the run key. It passes the conformance suite so
/// that the suite is proven to pass on something, and so a test of `KeySource::Run` has a chain
/// to carry.
#[derive(Clone, Debug)]
pub struct FakeRunCertIssuer {
    ca: Vec<u8>,
}

impl FakeRunCertIssuer {
    /// The fake CA, named `adam-testkit-ca/v1`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ca: b"adam-testkit-ca/v1".to_vec(),
        }
    }
}

impl Default for FakeRunCertIssuer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RunCertIssuer for FakeRunCertIssuer {
    async fn issue(&self, request: &CertificateRequest) -> Result<CertificateChain, RunCertError> {
        CertificateChain::new(vec![leaf_of(request.der()), self.ca.clone()])
            .map_err(|why| RunCertError::Internal(Box::new(why)))
    }

    fn kind(&self) -> &'static str {
        "fake"
    }
}

/// The leaf the fake issuer makes for `der`: a prefix and the SHA-256 of the request, so a chain
/// that ignores the request is a chain for another key.
fn leaf_of(der: &[u8]) -> Vec<u8> {
    let mut leaf = b"adam-testkit-leaf/v1:".to_vec();
    leaf.extend_from_slice(&Sha256::digest(der));
    leaf
}

/// The names of the cases, in the order [`run_cert_issuer_conformance!`](crate::run_cert_issuer_conformance)
/// generates them.
pub const CASES: [&str; 3] = [
    "an_issued_chain_is_a_chain_adam_can_carry",
    "the_chain_is_named_by_its_leaf",
    "two_requests_get_two_chains",
];

/// `request` as a request, or a failed case.
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a failed unwrap is a failed case"
)]
async fn issue(issuer: &dyn RunCertIssuer, der: &[u8]) -> CertificateChain {
    let request = CertificateRequest::new(der.to_vec()).expect("the case's request is bounded");
    issuer
        .issue(&request)
        .await
        .expect("the case's request is issued")
}

/// The chain is one adam can carry: a non-empty leaf-first chain within the bounds a signature
/// header allows ([`CertificateChain`]).
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a failed unwrap is a failed case"
)]
pub async fn an_issued_chain_is_a_chain_adam_can_carry(issuer: &dyn RunCertIssuer) {
    let chain = issue(issuer, b"adam-testkit-csr/one").await;
    let certificates = chain.certificates();
    assert!(
        (1..=CertificateChain::MAX_CERTIFICATES).contains(&certificates.len()),
        "the issuer answered {} certificates, and a chain carries 1 to {}",
        certificates.len(),
        CertificateChain::MAX_CERTIFICATES
    );
    assert!(
        certificates
            .iter()
            .all(|certificate| (1..=CertificateChain::MAX_CERTIFICATE_LEN)
                .contains(&certificate.len())),
        "the issuer answered a certificate that is empty or longer than {} bytes",
        CertificateChain::MAX_CERTIFICATE_LEN
    );
}

/// The chain is named by its leaf's `x5t#S256`, so a signature's `kid` finds the leaf a verifier
/// trusts ([ADR 0068] slice T1): an adapter never invents a `kid`.
///
/// [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a failed unwrap is a failed case"
)]
pub async fn the_chain_is_named_by_its_leaf(issuer: &dyn RunCertIssuer) {
    let chain = issue(issuer, b"adam-testkit-csr/one").await;
    let leaf = &chain.certificates()[0];
    assert_eq!(
        chain.kid().as_str(),
        URL_SAFE_NO_PAD.encode(Sha256::digest(leaf)),
        "the chain's kid is not its leaf's x5t#S256"
    );
}

/// Two different requests get two different leaves: the chain is bound to the request's key, not
/// reused. An issuer that ignores the request fails here, and a run would carry a certificate for
/// another key.
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a failed unwrap is a failed case"
)]
pub async fn two_requests_get_two_chains(issuer: &dyn RunCertIssuer) {
    let one = issue(issuer, b"adam-testkit-csr/one").await;
    let two = issue(issuer, b"adam-testkit-csr/two").await;
    assert_ne!(
        one.certificates()[0],
        two.certificates()[0],
        "two different certificate requests got the same leaf: the chain is not bound to the request's key"
    );
}

/// Generate one `#[tokio::test]` per case of [`run_cert::testkit`](crate::turn::run_cert::testkit).
/// `$make` is a path to a `fn() -> DynRunCertIssuer`.
#[macro_export]
macro_rules! run_cert_issuer_conformance {
    ($make:path) => {
        $crate::run_cert_issuer_conformance!(@cases $make;
            an_issued_chain_is_a_chain_adam_can_carry,
            the_chain_is_named_by_its_leaf,
            two_requests_get_two_chains,
        );
    };
    (@cases $make:path; $($case:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $case() {
                let issuer: $crate::turn::run_cert::DynRunCertIssuer = $make();
                $crate::turn::run_cert::testkit::$case(&*issuer).await;
            }
        )*
    };
}

/// The fake passes the suite, and the suite can be run on it directly.
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use super::*;
    use crate::turn::DynRunKeys;

    #[tokio::test]
    async fn the_fake_passes_every_case() {
        let issuer = FakeRunCertIssuer::new();
        an_issued_chain_is_a_chain_adam_can_carry(&issuer).await;
        the_chain_is_named_by_its_leaf(&issuer).await;
        two_requests_get_two_chains(&issuer).await;
    }

    #[test]
    fn a_certified_key_is_a_run_key() {
        let certified = Certified::new(
            crate::turn::Ed25519TurnKeys::ephemeral().unwrap(),
            CertificateChain::new(vec![b"leaf".to_vec()]).unwrap(),
        );
        let keys: DynRunKeys = Arc::new(certified);
        assert_eq!(format!("{keys:?}"), "RunKeys(certificate)");
    }
}
