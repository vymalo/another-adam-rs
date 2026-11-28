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
//! [`FakeRunCertIssuer`] is a fake CA: it parses the request's PKCS#10 DER, mints a real X.509 leaf
//! that certifies that key, valid from its clock's now for its life, and answers it with its own
//! certificate. So it passes the suite, a test of the run-key path has a leaf to read (its window,
//! its key), and a test of renewal can move the clock. [`Certified`] is marked [`RunKeys`] here, so
//! a test of the run-key path can also build one from an issued chain.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use jiff::SignedDuration;
use sha2::{Digest, Sha256};
use x509_cert::name::Name;

use super::keyring::{RunClock, SystemClock};
use super::x509;
use super::{CertificateRequest, RunCertError, RunCertIssuer, RunKeys};
use crate::turn::CertificateChain;
use crate::turn::testkit::Certified;

/// A [`RunKeys`] for test code: the certificate adapter [`Certified`] is a run's signer.
impl RunKeys for Certified {}

/// A fake CA: a fixed Ed25519 key signs leaves for whatever key a request asks about, valid from
/// its clock's now to now plus its life, with no extension.
///
/// It parses the request, so a request that is not a PKCS#10 CSR is
/// [`RunCertError::Invalid`]; and it mints a leaf per request (a serial from the request's hash),
/// so two requests get two leaves. The CA key never leaves it: `Debug` shows its life and kind.
pub struct FakeRunCertIssuer {
    ca: SigningKey,
    ca_cert: Vec<u8>,
    ca_name: Name,
    life: SignedDuration,
    clock: Arc<dyn RunClock>,
}

impl FakeRunCertIssuer {
    /// The fake CA, reading the system clock, whose leaves live an hour.
    #[must_use]
    #[allow(
        clippy::expect_used,
        reason = "test code: the CA certificate is minted from fixed values"
    )]
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock))
    }

    /// The same, reading `clock`: a test's clock, so the leaf's window starts where the test says.
    #[must_use]
    #[allow(
        clippy::expect_used,
        reason = "test code: the CA certificate is minted from fixed values"
    )]
    pub fn with_clock(clock: Arc<dyn RunClock>) -> Self {
        let mut seed = [0_u8; 32];
        seed.copy_from_slice(&Sha256::digest(b"adam-testkit-ca/v2"));
        let ca = SigningKey::from_bytes(&seed);
        let ca_name =
            Name::from_str(x509::ISSUER_SUBJECT).expect("the fake CA's name is a fixed string");
        let ca_cert = x509::self_signed(&ca, &ca_name, 1, clock.now())
            .expect("the fake CA's certificate is minted from fixed values");
        Self {
            ca,
            ca_cert,
            ca_name,
            life: SignedDuration::from_hours(1),
            clock,
        }
    }

    /// The same, whose leaves live `life`.
    #[must_use]
    pub fn with_life(mut self, life: SignedDuration) -> Self {
        self.life = life;
        self
    }
}

impl Default for FakeRunCertIssuer {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for FakeRunCertIssuer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeRunCertIssuer")
            .field("life", &self.life)
            .field("kind", &"fake")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl RunCertIssuer for FakeRunCertIssuer {
    async fn issue(&self, request: &CertificateRequest) -> Result<CertificateChain, RunCertError> {
        let subject = x509::requested_key(request.der())
            .map_err(|why| RunCertError::Invalid(Box::new(why)))?;
        let now = self.clock.now();
        let leaf = x509::mint(
            &self.ca,
            &self.ca_name,
            subject,
            serial_of(request.der()),
            now,
            now + self.life,
        )
        .map_err(|why| RunCertError::Internal(Box::new(why)))?;
        CertificateChain::new(vec![leaf, self.ca_cert.clone()])
            .map_err(|why| RunCertError::Internal(Box::new(why)))
    }

    fn kind(&self) -> &'static str {
        "fake"
    }
}

/// A serial for the leaf of `der`: the first eight bytes of the request's SHA-256, never zero.
fn serial_of(der: &[u8]) -> u64 {
    let digest = Sha256::digest(der);
    let mut serial = [0_u8; 8];
    serial.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(serial) | 1
}

/// A real request for a key derived from `label`: the suite's input, so a fake CA can certify it.
#[allow(
    clippy::expect_used,
    reason = "test code: a failed request is a failed case"
)]
/// A real PKCS#10 request for a fresh key, seeded by `label`: what [`FakeRunCertIssuer`] parses
/// since A2. A test outside this crate builds its issuer input with it, never with hand-written
/// bytes (the fake refuses those).
pub fn request_for(label: &str) -> CertificateRequest {
    let mut seed = [0_u8; 32];
    seed.copy_from_slice(&Sha256::digest(label.as_bytes()));
    let key = SigningKey::from_bytes(&seed);
    let san = format!("urn:another:person:test:agent:{}", crate::RunId::new());
    x509::request(&key, &san).expect("a request for a fresh key is built")
}

/// A clock a test sets by hand: the two thirds and the expiry are reached without waiting.
///
/// Share one between a [`FakeRunCertIssuer`] and a [`RunKeyring`](super::RunKeyring) so a minted
/// leaf's window starts where the keyring's clock says.
#[derive(Debug)]
pub struct TestClock(AtomicI64);

impl TestClock {
    /// A clock reading `at`.
    pub fn at(at: crate::time::Timestamp) -> Self {
        Self(AtomicI64::new(at.as_millisecond()))
    }

    /// Make the clock read `at`.
    pub fn set(&self, at: crate::time::Timestamp) {
        self.0.store(at.as_millisecond(), Ordering::SeqCst);
    }

    /// Move the clock forward by `by`.
    pub fn advance(&self, by: SignedDuration) {
        let now = self.0.load(Ordering::SeqCst) + by.as_millis() as i64;
        self.0.store(now, Ordering::SeqCst);
    }
}

impl RunClock for TestClock {
    fn now(&self) -> crate::time::Timestamp {
        crate::time::Timestamp::from_millisecond(self.0.load(Ordering::SeqCst))
            .unwrap_or(crate::time::Timestamp::UNIX_EPOCH)
    }
}

/// The names of the cases, in the order [`run_cert_issuer_conformance!`](crate::run_cert_issuer_conformance)
/// generates them.
pub const CASES: [&str; 3] = [
    "an_issued_chain_is_a_chain_adam_can_carry",
    "the_chain_is_named_by_its_leaf",
    "two_requests_get_two_chains",
];

/// `label`'s request, as a request, or a failed case.
#[allow(
    clippy::expect_used,
    reason = "test code: a failed request is a failed case"
)]
async fn issue(issuer: &dyn RunCertIssuer, label: &str) -> CertificateChain {
    issuer
        .issue(&request_for(label))
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
    let chain = issue(issuer, "one").await;
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
    let chain = issue(issuer, "one").await;
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
    let one = issue(issuer, "one").await;
    let two = issue(issuer, "two").await;
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

    use super::*;
    use crate::turn::DynRunKeys;

    #[tokio::test]
    async fn the_fake_passes_every_case() {
        let issuer = FakeRunCertIssuer::new();
        an_issued_chain_is_a_chain_adam_can_carry(&issuer).await;
        the_chain_is_named_by_its_leaf(&issuer).await;
        two_requests_get_two_chains(&issuer).await;
    }

    #[tokio::test]
    async fn the_fake_refuses_bytes_that_are_not_a_csr() {
        let issuer = FakeRunCertIssuer::new();
        let request = CertificateRequest::new(b"adam-testkit-csr/one".to_vec()).unwrap();
        let error = issuer.issue(&request).await.unwrap_err();
        assert!(
            matches!(error, RunCertError::Invalid(_)),
            "a request that is not a CSR is Invalid, got {error:?}"
        );
    }

    #[tokio::test]
    async fn the_fake_leaf_certifies_the_requested_key_and_lives_its_life() {
        use ed25519_dalek::SigningKey;

        let clock = Arc::new(TestClock::at(crate::time::Timestamp::UNIX_EPOCH));
        let issuer = FakeRunCertIssuer::with_clock(clock).with_life(SignedDuration::from_hours(3));
        let key = SigningKey::from_bytes(&[7_u8; 32]);
        let request = x509::request(&key, "urn:another:person:test:agent:one").unwrap();
        let chain = issuer.issue(&request).await.unwrap();
        let leaf = &chain.certificates()[0];
        assert!(x509::certifies(leaf, &key.verifying_key()).unwrap());
        let (not_before, not_after) = x509::window(leaf).unwrap();
        assert_eq!(not_before, crate::time::Timestamp::UNIX_EPOCH);
        assert_eq!(
            not_after,
            crate::time::Timestamp::UNIX_EPOCH + SignedDuration::from_hours(3)
        );
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
