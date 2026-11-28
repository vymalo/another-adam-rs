//! The ASN.1 a run's certificate is made of ([ADR 0086]): the PKCS#10 request adam sends to a
//! [`RunCertIssuer`](super::RunCertIssuer), and the leaf it reads back for the key's lifetime.
//!
//! `x509-cert` gives the RFC 5280 and RFC 2986 types; the signatures are `ed25519-dalek`'s, through
//! `signature`'s traits, so no second crypto provider enters the tree. Nothing here is public: the
//! crate's own types ([`CertificateRequest`](super::CertificateRequest),
//! [`CertificateChain`](crate::turn::CertificateChain)) are what cross a port.
//!
//! [ADR 0086]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md

use std::str::FromStr;

use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use x509_cert::attr::Attribute;
use x509_cert::builder::profile::BuilderProfile;
use x509_cert::builder::{Builder as _, CertificateBuilder};
use x509_cert::certificate::{Certificate, TbsCertificate};
use x509_cert::der::asn1::{BitString, Ia5String, ObjectIdentifier, OctetString, SetOfVec};
use x509_cert::der::{Decode as _, Encode as _};
use x509_cert::ext::pkix::{SubjectAltName, name::GeneralName};
use x509_cert::name::Name;
use x509_cert::request::{CertReq, CertReqInfo, ExtensionReq, Version};
use x509_cert::serial_number::SerialNumber;
use x509_cert::spki::{AlgorithmIdentifierOwned, SubjectPublicKeyInfoOwned};
use x509_cert::time::{Time, Validity};

use super::{CertificateRequest, RunKeyError};
use crate::time::Timestamp;

/// Ed25519's `id-Ed25519` (RFC 8410 §3): the signature and public-key algorithm, no parameters.
const ED25519: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.112");
/// `id-ce-subjectAltName` (RFC 5280 §4.2.1.6).
const SUBJECT_ALT_NAME: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.17");

/// The distinguished name of a run's CSR: the CA writes the leaf's names from the request's URI
/// SAN, so the subject only says what the request is.
const REQUEST_SUBJECT: &str = "CN=adam run key";
/// The distinguished name a fake CA signs leaves under.
pub(super) const ISSUER_SUBJECT: &str = "CN=adam-testkit-ca";
/// The distinguished name a fake CA writes into the leaves it signs.
pub(super) const LEAF_SUBJECT: &str = "CN=adam run";

/// The DER of a PKCS#10 request for `key`'s public key, whose URI SAN is `san`, marked critical
/// (the PKI decision of 2026-11-26: the name is the only identity the leaf carries).
pub(super) fn request(key: &SigningKey, san: &str) -> Result<CertificateRequest, RunKeyError> {
    let subject = Name::from_str(REQUEST_SUBJECT).map_err(request_error)?;
    let name = Ia5String::new(san).map_err(request_error)?;
    let names = SubjectAltName(vec![GeneralName::UniformResourceIdentifier(name)]);
    let extension = x509_cert::ext::Extension {
        extn_id: SUBJECT_ALT_NAME,
        critical: true,
        extn_value: OctetString::new(names.to_der().map_err(request_error)?)
            .map_err(request_error)?,
    };
    let attribute = Attribute::try_from(ExtensionReq(vec![extension])).map_err(request_error)?;
    let info = CertReqInfo {
        version: Version::V1,
        subject,
        public_key: public_key_of(&key.verifying_key()).map_err(request_error)?,
        attributes: SetOfVec::try_from(vec![attribute]).map_err(request_error)?,
    };
    let signed = info.to_der().map_err(request_error)?;
    let signature = key.sign(&signed);
    let request = CertReq {
        info,
        algorithm: ed25519(),
        signature: BitString::from_bytes(&signature.to_bytes()).map_err(request_error)?,
    };
    CertificateRequest::new(request.to_der().map_err(request_error)?)
        .map_err(|why| RunKeyError::Request(Box::new(why)))
}

/// The window `der`'s leaf is valid in, as the CA wrote it: `notBefore` and `notAfter`.
pub(super) fn window(der: &[u8]) -> Result<(Timestamp, Timestamp), RunKeyError> {
    let certificate = Certificate::from_der(der).map_err(chain_error)?;
    let validity = certificate.tbs_certificate().validity();
    Ok((instant(validity.not_before)?, instant(validity.not_after)?))
}

/// Whether `der`'s leaf certifies `key`: the certificate is for the key the CSR asked about, not
/// for another one. A chain that certifies another key is refused, never cached.
pub(super) fn certifies(der: &[u8], key: &VerifyingKey) -> Result<bool, RunKeyError> {
    let certificate = Certificate::from_der(der).map_err(chain_error)?;
    let bits = &certificate
        .tbs_certificate()
        .subject_public_key_info()
        .subject_public_key;
    Ok(bits.raw_bytes() == key.as_bytes())
}

/// The public key `der`'s CSR asks to have certified, or why `der` is not one.
pub(super) fn requested_key(der: &[u8]) -> Result<SubjectPublicKeyInfoOwned, RunKeyError> {
    let request = CertReq::from_der(der).map_err(chain_error)?;
    Ok(request.info.public_key)
}

/// A leaf certificate that certifies `subject`, valid from `not_before` to `not_after`, signed by
/// `issuer_key` under `issuer`: what a fake CA answers, so a test of the keyring has a real leaf to
/// read.
pub(super) fn mint(
    issuer_key: &SigningKey,
    issuer: &Name,
    subject: SubjectPublicKeyInfoOwned,
    serial: u64,
    not_before: Timestamp,
    not_after: Timestamp,
) -> Result<Vec<u8>, RunKeyError> {
    let profile = Leaf {
        issuer: issuer.clone(),
        subject: Name::from_str(LEAF_SUBJECT).map_err(chain_error)?,
    };
    let builder = CertificateBuilder::new(
        profile,
        SerialNumber::from(serial),
        Validity::new(time_of(not_before)?, time_of(not_after)?),
        subject,
    )
    .map_err(chain_error)?;
    let certificate = builder
        .build::<_, ed25519_dalek::Signature>(issuer_key)
        .map_err(chain_error)?;
    certificate.to_der().map_err(chain_error)
}

/// A certificate profile with no extension at all: the fake CA's, so a leaf carries exactly what a
/// test reads (its key and its window) and nothing that a real profile would police.
struct Leaf {
    issuer: Name,
    subject: Name,
}

impl BuilderProfile for Leaf {
    fn get_issuer(&self, _subject: &Name) -> Name {
        self.issuer.clone()
    }

    fn get_subject(&self) -> Name {
        self.subject.clone()
    }

    fn build_extensions(
        &self,
        _spk: x509_cert::spki::SubjectPublicKeyInfoRef<'_>,
        _issuer_spk: x509_cert::spki::SubjectPublicKeyInfoRef<'_>,
        _tbs: &TbsCertificate,
    ) -> x509_cert::builder::Result<Vec<x509_cert::ext::Extension>> {
        Ok(Vec::new())
    }
}

/// A self-signed certificate for `key`, named `issuer`: the fake CA's own certificate, valid from a
/// day before `now` to fifty years after it.
pub(super) fn self_signed(
    key: &SigningKey,
    issuer: &Name,
    serial: u64,
    now: Timestamp,
) -> Result<Vec<u8>, RunKeyError> {
    let spki = public_key_of(&key.verifying_key()).map_err(chain_error)?;
    mint(
        key,
        issuer,
        spki,
        serial,
        (now - jiff::SignedDuration::from_hours(24)).max(Timestamp::UNIX_EPOCH),
        now + jiff::SignedDuration::from_secs(60 * 60 * 24 * 365 * 50),
    )
}

fn public_key_of(key: &VerifyingKey) -> x509_cert::der::Result<SubjectPublicKeyInfoOwned> {
    Ok(SubjectPublicKeyInfoOwned {
        algorithm: ed25519(),
        subject_public_key: BitString::from_bytes(key.as_bytes())?,
    })
}

const fn ed25519() -> AlgorithmIdentifierOwned {
    AlgorithmIdentifierOwned {
        oid: ED25519,
        parameters: None,
    }
}

/// A certificate instant as adam's: the certificate's `Time`, in seconds (a CA writes whole
/// seconds), and `INFINITY` refused (there is no expiry to renew before).
fn instant(time: Time) -> Result<Timestamp, RunKeyError> {
    if time == Time::INFINITY {
        return Err(chain_error("a leaf that never expires has no renewal"));
    }
    let seconds = i64::try_from(time.to_unix_duration().as_secs())
        .map_err(|_| chain_error("a leaf's instant is out of range"))?;
    Timestamp::from_second(seconds).map_err(chain_error)
}

/// A certificate instant for `at`, at the whole second a CA writes (RFC 5280 §4.1.2.5 carries no
/// fraction).
fn time_of(at: Timestamp) -> Result<Time, RunKeyError> {
    let second = Timestamp::from_second(at.as_second()).map_err(chain_error)?;
    Time::from_str(&second.to_string()).map_err(chain_error)
}

fn request_error(why: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> RunKeyError {
    RunKeyError::Request(why.into())
}

fn chain_error(why: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> RunKeyError {
    RunKeyError::Chain(why.into())
}
