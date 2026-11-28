//! [`RunKeyring`]: one Ed25519 key per run, certified by a [`RunCertIssuer`], renewed at two
//! thirds of its leaf's life ([ADR 0086] decisions 3 and 5).
//!
//! The key is made in memory and never written anywhere: no method gives it out, `Debug` shows the
//! run and the issuer's kind, and the seed is wiped once the key exists. The certificate chain is
//! cached for the run, so a model call costs a signature and no CA round trip, until the leaf is
//! two thirds through its life.
//!
//! **The renewal is a new key, not a new certificate for the old one** ([ADR 0086] decision 5): at
//! two thirds of the interval from the leaf's `notBefore` to its `notAfter`, a new key pair, a new
//! CSR and a new leaf replace the old ones together, so the run never signs with two keys at once
//! and a key that leaked stops being usable at the renewal. The old leaf signs until the new one is
//! in hand: a renewal the issuer fails leaves the old key signing, and the next attempt is no
//! sooner than [`RENEW_RETRY_SECS`].
//!
//! **It fails closed.** A leaf whose `notAfter` has passed with no renewal in hand refuses to sign:
//! the call is not sent, and the error carries a class. Nothing is ever signed with an expired leaf.
//!
//! [ADR 0086]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md

use std::fmt;
use std::sync::Arc;

use adam_error::{BoxError, Classify, ErrorClass};
use async_trait::async_trait;
use ed25519_dalek::{Signer as _, SigningKey};
use secrecy::zeroize::Zeroizing;
use tokio::sync::Mutex;

use super::x509;
use super::{DynRunCertIssuer, RunCertError, RunKeys};
use crate::RunId;
use crate::time::Timestamp;
use crate::turn::{
    CertificateChain, CurrentKey, KeyId, KeyIdentity, KeySet, KeyStatus, PublicKey, Signature,
    TurnKeyError, TurnKeys,
};

/// How long after a failed renewal the next call tries the issuer again, so an outage costs one CA
/// round trip per window rather than one per model call.
pub const RENEW_RETRY_SECS: i64 = 15;

/// The instant a [`RunKeyring`] reads for its renewal and its expiry. A server uses
/// [`SystemClock`]; a test uses its own, so the two thirds and the expiry are seen without waiting.
pub trait RunClock: Send + Sync + 'static {
    /// Now, in UTC.
    fn now(&self) -> Timestamp;
}

/// The system's clock: [`Timestamp::now`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl RunClock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// The person and the run a run's certificate names: the leaf's URI SAN is
/// `urn:another:person:<sub>:agent:<run>` (the PKI decision of 2026-11-26), marked critical, so the
/// verifier in front of the model binds the key to that person and that run and to nothing else.
///
/// The person is the subject of the person's own token, as the host read it. It is restricted to
/// the URI's unreserved characters (RFC 3986 §2.3: letters, digits, `-`, `.`, `_`, `~`), so the
/// name cannot be bent by a `/`, a `:` or a `%`: a subject that is not one is refused, never
/// escaped into something else.
#[derive(Clone, PartialEq, Eq)]
pub struct RunSubject {
    person: String,
    run: RunId,
}

impl RunSubject {
    /// The longest person, in bytes.
    pub const MAX_PERSON_LEN: usize = 128;

    /// The subject of `person`'s key for `run`.
    ///
    /// # Errors
    ///
    /// [`BadRunSubject`] when `person` is empty, longer than
    /// [`MAX_PERSON_LEN`](Self::MAX_PERSON_LEN), or holds a byte that is not a URI unreserved
    /// character. The error does not repeat the person.
    pub fn new(person: impl Into<String>, run: RunId) -> Result<Self, BadRunSubject> {
        let person = person.into();
        if person.is_empty()
            || person.len() > Self::MAX_PERSON_LEN
            || !person
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
        {
            return Err(BadRunSubject);
        }
        Ok(Self { person, run })
    }

    /// The person the certificate names.
    pub fn person(&self) -> &str {
        &self.person
    }

    /// The run the certificate names.
    pub fn run(&self) -> RunId {
        self.run
    }

    /// The URI the leaf's subject alternative name carries.
    pub fn san(&self) -> String {
        format!("urn:another:person:{}:agent:{}", self.person, self.run)
    }
}

impl fmt::Debug for RunSubject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunSubject")
            .field("person", &self.person)
            .field("run", &self.run)
            .finish()
    }
}

/// A person or a run that cannot name a certificate ([`RunSubject::new`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "a run certificate's person is 1 to {max} bytes of URI unreserved characters (RFC 3986 §2.3)",
    max = RunSubject::MAX_PERSON_LEN
)]
pub struct BadRunSubject;

impl Classify for BadRunSubject {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// The run's signing key and the leaf that certifies it ([ADR 0086]): a [`RunKeys`], so the request
/// path signs the run's model requests with it, and nothing else.
///
/// One keyring is one run: the run is part of the certificate's name, so a keyring that stepped
/// another run would certify the wrong one. `current` issues the key and the leaf on its first
/// call, and renews at two thirds of the leaf's life; `sign` signs with the current key only.
///
/// A concurrent renewal is one renewal: the state is behind a mutex held across the issuer's
/// answer, so two model calls do not make two keys.
///
/// `Debug` shows the run and the issuer's kind: never a key, a CSR or a chain.
///
/// [ADR 0086]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md
pub struct RunKeyring {
    issuer: DynRunCertIssuer,
    subject: RunSubject,
    clock: Arc<dyn RunClock>,
    state: Mutex<State>,
}

/// What the keyring holds: the current key and leaf, and when to try the issuer again.
#[derive(Default)]
struct State {
    live: Option<Live>,
    retry_at: Option<Timestamp>,
}

/// The current key, the chain that certifies it and the instants the renewal reads.
struct Live {
    key: SigningKey,
    chain: CertificateChain,
    set: Arc<KeySet>,
    not_after: Timestamp,
    renew_at: Timestamp,
}

impl fmt::Debug for Live {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Live")
            .field("kid", self.chain.kid())
            .field("not_after", &self.not_after)
            .field("renew_at", &self.renew_at)
            .finish_non_exhaustive()
    }
}

impl RunKeyring {
    /// The keyring of `subject`'s run, certified by `issuer`, reading the system clock.
    #[must_use]
    pub fn new(issuer: DynRunCertIssuer, subject: RunSubject) -> Self {
        Self::with_clock(issuer, subject, Arc::new(SystemClock))
    }

    /// The same, reading `clock`: a test's clock, so the renewal and the expiry are reached
    /// without waiting.
    #[must_use]
    pub fn with_clock(
        issuer: DynRunCertIssuer,
        subject: RunSubject,
        clock: Arc<dyn RunClock>,
    ) -> Self {
        Self {
            issuer,
            subject,
            clock,
            state: Mutex::new(State::default()),
        }
    }

    /// The person and the run this keyring certifies.
    pub fn subject(&self) -> &RunSubject {
        &self.subject
    }

    /// The key and leaf now, issuing or renewing when the clock says so.
    ///
    /// A renewal the issuer fails at two thirds leaves the old leaf signing (its status is
    /// [`KeyStatus::Renewing`]); the same failure past the leaf's `notAfter` refuses, because there
    /// is nothing valid left to sign with.
    async fn live(&self) -> Result<Live, RunKeyError> {
        let mut state = self.state.lock().await;
        let now = self.clock.now();
        let (stale, expired, held) = match state.live.as_ref() {
            None => (true, false, None),
            Some(live) => (
                now >= live.renew_at,
                now >= live.not_after,
                Some(live.clone()),
            ),
        };
        if !stale && let Some(live) = &held {
            return Ok(live.clone());
        }
        if !expired
            && state.retry_at.is_some_and(|retry_at| now < retry_at)
            && let Some(live) = &held
        {
            // A renewal failed a moment ago: the old leaf still signs until the next attempt.
            return Ok(live.clone());
        }
        match self.certified(now).await {
            Ok(live) => {
                state.live = Some(live.clone());
                state.retry_at = None;
                Ok(live)
            }
            Err(why) if expired => Err(why),
            Err(why) => {
                state.retry_at = Some(now + jiff::SignedDuration::from_secs(RENEW_RETRY_SECS));
                held.ok_or(why)
            }
        }
    }

    /// A new key, a new CSR, and the leaf `issuer` answers for it, checked: the leaf is this key's
    /// and its window is a window.
    async fn certified(&self, now: Timestamp) -> Result<Live, RunKeyError> {
        let key = generate()?;
        let request = x509::request(&key, &self.subject.san())?;
        let chain = self
            .issuer
            .issue(&request)
            .await
            .map_err(RunKeyError::Renewal)?;
        let leaf = chain
            .certificates()
            .first()
            .ok_or_else(|| RunKeyError::Chain("a chain with no leaf".into()))?;
        if !x509::certifies(leaf, &key.verifying_key())? {
            return Err(RunKeyError::Chain(
                "the leaf certifies another key than the request's".into(),
            ));
        }
        let (not_before, not_after) = x509::window(leaf)?;
        if not_after <= not_before {
            return Err(RunKeyError::Chain(
                "the leaf's notAfter is not after its notBefore".into(),
            ));
        }
        if now >= not_after {
            return Err(RunKeyError::Chain(
                "the leaf is already expired when it arrives".into(),
            ));
        }
        let set = KeySet::new()
            .with_key(
                chain.kid().clone(),
                PublicKey::from_verifying(key.verifying_key()),
            )
            .map_err(|why| RunKeyError::Chain(Box::new(why)))?;
        Ok(Live {
            key,
            chain,
            set: Arc::new(set),
            not_after,
            renew_at: two_thirds(not_before, not_after),
        })
    }
}

impl Clone for Live {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            chain: self.chain.clone(),
            set: Arc::clone(&self.set),
            not_after: self.not_after,
            renew_at: self.renew_at,
        }
    }
}

#[async_trait]
impl TurnKeys for RunKeyring {
    async fn current(&self) -> Result<CurrentKey, TurnKeyError> {
        let live = self.live().await.map_err(fail)?;
        let status = if self.clock.now() >= live.renew_at {
            // A renewal failed within the retry window: the old leaf signs, and says so.
            KeyStatus::Renewing
        } else {
            KeyStatus::Active
        };
        Ok(CurrentKey {
            identity: KeyIdentity::Certificate(live.chain),
            status,
        })
    }

    async fn sign(&self, kid: &KeyId, input: &[u8]) -> Result<Signature, TurnKeyError> {
        let live = self.live().await.map_err(fail)?;
        if live.chain.kid() != kid {
            return Err(TurnKeyError::NotSigning { kid: kid.clone() });
        }
        if self.clock.now() >= live.not_after {
            // The leaf died between `current` and this call: never sign with an expired leaf.
            return Err(fail(RunKeyError::Expired));
        }
        Ok(Signature::from_bytes(live.key.sign(input).to_bytes()))
    }

    async fn key_set(&self) -> Result<Arc<KeySet>, TurnKeyError> {
        Ok(Arc::clone(&self.live().await.map_err(fail)?.set))
    }

    fn kind(&self) -> &'static str {
        "run-keyring"
    }
}

impl RunKeys for RunKeyring {}

impl fmt::Debug for RunKeyring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunKeyring")
            .field("issuer", &self.issuer)
            .field("subject", &self.subject)
            .field("kind", &self.kind())
            .finish_non_exhaustive()
    }
}

/// A [`RunKeyError`] as the key port's error: a refusal is `TurnKeyError::Refused`
/// (`Unauthenticated`), everything else `TurnKeyError::Unavailable` (`Transient`), because
/// `TurnKeyError` has no `Internal` and a permanent failure still must not send the call. The
/// precise class stays on the [`RunKeyError`] kept as the source.
fn fail(why: RunKeyError) -> TurnKeyError {
    match why.class() {
        ErrorClass::Unauthenticated => TurnKeyError::Refused(Box::new(why)),
        _ => TurnKeyError::Unavailable(Box::new(why)),
    }
}

/// The instant two thirds of the way from `not_before` to `not_after` ([ADR 0086] decision 5).
fn two_thirds(not_before: Timestamp, not_after: Timestamp) -> Timestamp {
    let life = not_after.as_nanosecond() - not_before.as_nanosecond();
    let at = not_before.as_nanosecond() + life * 2 / 3;
    jiff::Timestamp::from_nanosecond(at).map_or(not_after, Timestamp)
}

/// A new Ed25519 key from the system's random numbers, its seed wiped at once ([ADR 0086]
/// decision 3: made in memory, never written).
fn generate() -> Result<SigningKey, RunKeyError> {
    let mut seed = Zeroizing::new([0_u8; 32]);
    getrandom::fill(seed.as_mut_slice()).map_err(|_| RunKeyError::NoRandomness)?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Why a run's key could not sign, or could not be made ([`RunKeyring`]). Carries no key, no CSR
/// and no chain: only what went wrong.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunKeyError {
    /// The leaf's `notAfter` has passed and no renewal is in hand: the call is not sent.
    #[error("the run key's certificate expired and no renewal is in hand")]
    Expired,
    /// The certificate request could not be built (a name, a DER encoding).
    #[error("the run key's certificate request could not be built")]
    Request(#[source] BoxError),
    /// The issuer answered a chain the run cannot use: no leaf, a leaf for another key, a leaf
    /// with no usable lifetime.
    #[error("the certificate authority answered a chain the run key cannot use")]
    Chain(#[source] BoxError),
    /// The issuer refused or could not be reached: the leaf could not be made or renewed.
    #[error("the run key could not be renewed")]
    Renewal(#[source] RunCertError),
    /// The system gives no random numbers to make a key.
    #[error("no random numbers to make a run key")]
    NoRandomness,
}

impl Classify for RunKeyError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Expired | Self::NoRandomness => ErrorClass::Transient,
            Self::Renewal(why) => why.class(),
            Self::Request(_) | Self::Chain(_) => ErrorClass::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::error::Error as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jiff::SignedDuration;
    use x509_cert::der::Decode as _;
    use x509_cert::der::asn1::{Ia5String, ObjectIdentifier};
    use x509_cert::ext::pkix::{SubjectAltName, name::GeneralName};
    use x509_cert::request::{CertReq, ExtensionReq};

    use super::*;
    use crate::turn::run_cert::testkit::{FakeRunCertIssuer, TestClock};
    use crate::turn::run_cert::{CertificateRequest, RunCertIssuer, x509};

    const RUN: &str = "018f0f7a-0000-7000-8000-000000000001";
    const LIFE: SignedDuration = SignedDuration::from_hours(3);
    const EXTENSION_REQUEST: &str = "1.2.840.113549.1.9.14";
    const SUBJECT_ALT_NAME: &str = "2.5.29.17";

    fn run() -> RunId {
        RunId::from(uuid::Uuid::parse_str(RUN).unwrap())
    }

    fn subject() -> RunSubject {
        RunSubject::new("st-abc123", run()).unwrap()
    }

    fn clock() -> Arc<TestClock> {
        Arc::new(TestClock::at(Timestamp::UNIX_EPOCH))
    }

    fn issuer(clock: &Arc<TestClock>) -> Arc<FakeRunCertIssuer> {
        Arc::new(FakeRunCertIssuer::with_clock(clock.clone()).with_life(LIFE))
    }

    fn keyring(clock: &Arc<TestClock>) -> RunKeyring {
        RunKeyring::with_clock(issuer(clock), subject(), clock.clone())
    }

    fn at(seconds: i64) -> Timestamp {
        Timestamp::UNIX_EPOCH + SignedDuration::from_secs(seconds)
    }

    async fn public_of(keys: &RunKeyring, kid: &KeyId) -> [u8; 32] {
        keys.key_set().await.unwrap().key(kid).unwrap().to_bytes()
    }

    #[tokio::test]
    async fn a_run_key_signs_and_its_leaf_is_its_own() {
        let clock = clock();
        let keys = keyring(&clock);
        let current = keys.current().await.unwrap();
        let KeyIdentity::Certificate(chain) = &current.identity else {
            panic!("a run key is named by its certificate, got {current:?}");
        };
        assert_eq!(current.status, KeyStatus::Active);
        assert_eq!(current.kid(), chain.kid());

        let leaf = x509_cert::certificate::Certificate::from_der(&chain.certificates()[0]).unwrap();
        let set = keys.key_set().await.unwrap();
        let public = set.key(chain.kid()).unwrap();
        assert_eq!(
            leaf.tbs_certificate()
                .subject_public_key_info()
                .subject_public_key
                .raw_bytes(),
            &public.to_bytes(),
            "the leaf is not the run key's"
        );

        let signature = keys.sign(chain.kid(), b"a model call").await.unwrap();
        assert!(public.verifies(b"a model call", &signature));
        assert!(matches!(
            keys.sign(&KeyId::new("another").unwrap(), b"x").await,
            Err(TurnKeyError::NotSigning { .. })
        ));
    }

    #[tokio::test]
    async fn the_csr_names_the_person_and_the_run_with_a_critical_san() {
        let clock = clock();
        let issuer = Arc::new(RecordingIssuer::new(issuer(&clock)));
        let keys = RunKeyring::with_clock(issuer.clone(), subject(), clock.clone());
        keys.current().await.unwrap();

        let request = issuer.last().expect("the keyring sent a request");
        let csr = CertReq::from_der(request.der()).unwrap();
        let attribute = csr
            .info
            .attributes
            .iter()
            .find(|attribute| attribute.oid == ObjectIdentifier::new_unwrap(EXTENSION_REQUEST))
            .expect("the request carries an extensionRequest attribute");
        let extensions = attribute
            .values
            .get(0)
            .unwrap()
            .to_ref()
            .decode_as::<ExtensionReq>()
            .unwrap();
        let san = extensions
            .0
            .iter()
            .find(|extension| extension.extn_id == ObjectIdentifier::new_unwrap(SUBJECT_ALT_NAME))
            .expect("the request carries a subjectAltName");
        assert!(san.critical, "the subjectAltName is critical");
        let names = SubjectAltName::from_der(san.extn_value.as_bytes()).unwrap();
        assert_eq!(
            names.0,
            vec![GeneralName::UniformResourceIdentifier(
                Ia5String::new(&format!("urn:another:person:st-abc123:agent:{RUN}")).unwrap()
            )]
        );
    }

    #[tokio::test]
    async fn nothing_renews_before_two_thirds_and_a_new_key_comes_at_two_thirds() {
        let clock = clock();
        let keys = keyring(&clock);
        let first = keys.current().await.unwrap().kid().clone();
        let first_public = public_of(&keys, &first).await;

        // Two thirds of three hours is two hours: one second before, the same key still signs.
        clock.set(at(2 * 3600 - 1));
        let current = keys.current().await.unwrap();
        assert_eq!(current.kid(), &first, "the key renewed before two thirds");
        assert_eq!(current.status, KeyStatus::Active);

        // At two thirds, a new key and a new leaf replace them.
        clock.set(at(2 * 3600));
        let renewed = keys.current().await.unwrap();
        assert_ne!(
            renewed.kid(),
            &first,
            "the key was not renewed at two thirds"
        );
        assert_ne!(
            public_of(&keys, renewed.kid()).await,
            first_public,
            "the renewal reused the key pair"
        );
        // The old key no longer signs: the run never signs with two keys at once.
        assert!(matches!(
            keys.sign(&first, b"x").await,
            Err(TurnKeyError::NotSigning { .. })
        ));
        assert!(keys.sign(renewed.kid(), b"x").await.is_ok());
    }

    #[tokio::test]
    async fn a_failed_renewal_keeps_the_old_leaf_signing() {
        let clock = clock();
        let issuer = Arc::new(FlakyIssuer::new(issuer(&clock)));
        let keys = RunKeyring::with_clock(issuer.clone(), subject(), clock.clone());
        let first = keys.current().await.unwrap().kid().clone();

        issuer.fail_from(1);
        clock.set(at(2 * 3600));
        let current = keys.current().await.unwrap();
        assert_eq!(
            current.kid(),
            &first,
            "the old leaf must sign until the new one is in hand"
        );
        assert_eq!(current.status, KeyStatus::Renewing);
        assert!(keys.sign(&first, b"x").await.is_ok());

        // The retry window passes and the issuer answers again: the new key arrives.
        clock.set(at(2 * 3600 + RENEW_RETRY_SECS));
        issuer.fail_from(usize::MAX);
        assert_ne!(keys.current().await.unwrap().kid(), &first);
    }

    #[tokio::test]
    async fn an_expired_leaf_with_no_renewal_refuses_with_a_class() {
        let clock = clock();
        let issuer = Arc::new(FlakyIssuer::new(issuer(&clock)));
        let keys = RunKeyring::with_clock(issuer.clone(), subject(), clock.clone());
        let first = keys.current().await.unwrap().kid().clone();
        issuer.fail_from(1);

        clock.set(at(3 * 3600));
        let error = keys.current().await.unwrap_err();
        assert!(
            matches!(error, TurnKeyError::Unavailable(_)),
            "an expired leaf with no renewal refuses, got {error:?}"
        );
        assert_eq!(error.class(), ErrorClass::Transient);
        assert!(
            matches!(
                error
                    .source()
                    .and_then(|why| why.downcast_ref::<RunKeyError>()),
                Some(RunKeyError::Renewal(_))
            ),
            "the source says why: {error:?}"
        );
        assert!(matches!(
            keys.sign(&first, b"x").await,
            Err(TurnKeyError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_chain_for_another_key_is_refused() {
        let clock = clock();
        let keys = RunKeyring::with_clock(
            Arc::new(WrongKeyIssuer(issuer(&clock))),
            subject(),
            clock.clone(),
        );
        let error = keys.current().await.unwrap_err();
        assert!(
            matches!(
                error
                    .source()
                    .and_then(|why| why.downcast_ref::<RunKeyError>()),
                Some(RunKeyError::Chain(_))
            ),
            "a chain for another key is refused, got {error:?}"
        );
    }

    /// A seed whose tail spells a marker: if any rendering prints the key material, in text or in
    /// a common encoding, the assertions see it.
    const SENTINEL_SEED: [u8; 32] = *b"anr1309-key-material-SENTINEL!!!";

    #[tokio::test]
    async fn no_key_a_csr_or_a_chain_is_rendered() {
        assert!(String::from_utf8_lossy(&SENTINEL_SEED).contains("SENTINEL"));
        let clock = clock();
        let issuer = issuer(&clock);
        let key = SigningKey::from_bytes(&SENTINEL_SEED);
        let request = x509::request(&key, &subject().san()).unwrap();
        let chain = issuer.issue(&request).await.unwrap();
        let (not_before, not_after) = x509::window(&chain.certificates()[0]).unwrap();
        let live = Live {
            set: Arc::new(
                KeySet::new()
                    .with_key(
                        chain.kid().clone(),
                        PublicKey::from_verifying(key.verifying_key()),
                    )
                    .unwrap(),
            ),
            chain,
            key,
            not_after,
            renew_at: two_thirds(not_before, not_after),
        };
        let keys = RunKeyring::with_clock(issuer, subject(), clock);
        let error = RunKeyError::Request(Box::new(BadRunSubject));

        let rendered = [
            format!("{live:?}"),
            format!("{:?}", live.chain),
            format!("{:?}", live.key),
            format!("{request:?}"),
            format!("{keys:?}"),
            format!("{error}"),
            format!("{error:?}"),
        ];
        let needles = [
            "SENTINEL".to_owned(),
            URL_SAFE_NO_PAD.encode(SENTINEL_SEED),
            hex(&SENTINEL_SEED),
        ];
        for rendering in &rendered {
            for needle in &needles {
                assert!(
                    !rendering.contains(needle.as_str()),
                    "a rendering leaks the key material: {rendering}"
                );
            }
        }
        // The material really spells the marker, and the kid really is rendered: the check above
        // is neither vacuous nor passing because nothing is rendered.
        assert_eq!(&SENTINEL_SEED[21..29], b"SENTINEL");
        assert!(format!("{live:?}").contains(live.chain.kid().as_str()));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn a_subject_is_a_uri_component_or_it_is_refused() {
        assert_eq!(
            subject().san(),
            format!("urn:another:person:st-abc123:agent:{RUN}")
        );
        for bad in ["", "with space", "a:b", "a/b", "é", "a%41"] {
            assert_eq!(
                RunSubject::new(bad, run()),
                Err(BadRunSubject),
                "{bad:?} is not a URI unreserved component"
            );
        }
        assert_eq!(
            RunSubject::new("a".repeat(RunSubject::MAX_PERSON_LEN + 1), run()),
            Err(BadRunSubject)
        );
        assert_eq!(BadRunSubject.class(), ErrorClass::Invalid);
    }

    #[test]
    fn each_error_has_its_class() {
        assert_eq!(RunKeyError::Expired.class(), ErrorClass::Transient);
        assert_eq!(RunKeyError::NoRandomness.class(), ErrorClass::Transient);
        assert_eq!(
            RunKeyError::Renewal(RunCertError::Refused("no".into())).class(),
            ErrorClass::Unauthenticated
        );
        assert_eq!(
            RunKeyError::Renewal(RunCertError::Unavailable("down".into())).class(),
            ErrorClass::Transient
        );
        assert_eq!(
            RunKeyError::Renewal(RunCertError::Internal("bad".into())).class(),
            ErrorClass::Internal
        );
        assert_eq!(
            RunKeyError::Request("bad".into()).class(),
            ErrorClass::Internal
        );
        assert_eq!(
            RunKeyError::Chain("bad".into()).class(),
            ErrorClass::Internal
        );
    }

    /// An issuer that fails from its `n`th call on: a CA that is down after the first leaf.
    struct FlakyIssuer {
        inner: Arc<FakeRunCertIssuer>,
        calls: AtomicUsize,
        fail_from: AtomicUsize,
    }

    impl FlakyIssuer {
        fn new(inner: Arc<FakeRunCertIssuer>) -> Self {
            Self {
                inner,
                calls: AtomicUsize::new(0),
                fail_from: AtomicUsize::new(usize::MAX),
            }
        }

        fn fail_from(&self, from: usize) {
            self.fail_from.store(from, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl RunCertIssuer for FlakyIssuer {
        async fn issue(
            &self,
            request: &CertificateRequest,
        ) -> Result<CertificateChain, RunCertError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call >= self.fail_from.load(Ordering::SeqCst) {
                return Err(RunCertError::Unavailable("the fake CA is down".into()));
            }
            self.inner.issue(request).await
        }
    }

    /// An issuer that keeps the last request it saw, so a test can read the CSR the keyring sent.
    struct RecordingIssuer {
        inner: Arc<FakeRunCertIssuer>,
        seen: std::sync::Mutex<Option<CertificateRequest>>,
    }

    impl RecordingIssuer {
        fn new(inner: Arc<FakeRunCertIssuer>) -> Self {
            Self {
                inner,
                seen: std::sync::Mutex::new(None),
            }
        }

        fn last(&self) -> Option<CertificateRequest> {
            self.seen.lock().ok().and_then(|seen| seen.clone())
        }
    }

    #[async_trait]
    impl RunCertIssuer for RecordingIssuer {
        async fn issue(
            &self,
            request: &CertificateRequest,
        ) -> Result<CertificateChain, RunCertError> {
            if let Ok(mut seen) = self.seen.lock() {
                *seen = Some(request.clone());
            }
            self.inner.issue(request).await
        }
    }

    /// An issuer whose leaf certifies another key.
    struct WrongKeyIssuer(Arc<FakeRunCertIssuer>);

    #[async_trait]
    impl RunCertIssuer for WrongKeyIssuer {
        async fn issue(
            &self,
            _request: &CertificateRequest,
        ) -> Result<CertificateChain, RunCertError> {
            let other = SigningKey::from_bytes(&[9_u8; 32]);
            let request = x509::request(&other, &subject().san()).unwrap();
            self.0.issue(&request).await
        }
    }
}
