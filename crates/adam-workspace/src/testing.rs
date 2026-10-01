//! Helpers for the tests of crates that use this one (feature `test-util`).

use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der};
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

/// A freshly generated RSA key, as the private key of a GitHub App is: made at run time so that no
/// key is ever committed, in the two PEM forms [`AppKey::from_pem`](crate::AppKey::from_pem) reads,
/// with the means to check that a JWT was signed by it.
pub struct TestAppKey {
    /// `-----BEGIN RSA PRIVATE KEY-----` (PKCS#1): the form GitHub gives the owner of an App.
    pub pkcs1_pem: String,
    /// `-----BEGIN PRIVATE KEY-----` (PKCS#8).
    pub pkcs8_pem: String,
    /// The public key (a PKCS#1 `RSAPublicKey`).
    public_key: Vec<u8>,
}

impl TestAppKey {
    /// A new 2048-bit key. Takes a moment.
    ///
    /// # Panics
    ///
    /// When the key cannot be generated (it is a test helper).
    #[allow(clippy::expect_used)]
    pub fn generate() -> Self {
        let pair = KeyPair::generate(KeySize::Rsa2048).expect("an RSA key is generated");
        let pkcs8 = AsDer::<Pkcs8V1Der<'static>>::as_der(&pair).expect("the key has a PKCS#8 form");
        let pkcs8 = pkcs8.as_ref().to_vec();
        let pkcs1 = pkcs1_of(&pkcs8);
        Self {
            pkcs1_pem: pem("RSA PRIVATE KEY", &pkcs1),
            pkcs8_pem: pem("PRIVATE KEY", &pkcs8),
            public_key: pair.public_key().as_ref().to_vec(),
        }
    }

    /// The claims of `jwt` if it is `RS256`, has three parts and its signature is this key's.
    ///
    /// # Errors
    ///
    /// Why it is not.
    pub fn verify_jwt(&self, jwt: &str) -> Result<serde_json::Value, String> {
        let parts: Vec<&str> = jwt.split('.').collect();
        let [header, claims, signature] = parts.as_slice() else {
            return Err("a JWT has three parts".to_owned());
        };
        let decode = |part: &str| URL_SAFE_NO_PAD.decode(part).map_err(|e| e.to_string());
        let header: serde_json::Value =
            serde_json::from_slice(&decode(header)?).map_err(|e| e.to_string())?;
        if header["alg"] != "RS256" || header["typ"] != "JWT" {
            return Err(format!("unexpected header {header}"));
        }
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &self.public_key)
            .verify(
                format!("{}.{}", parts[0], parts[1]).as_bytes(),
                &decode(signature)?,
            )
            .map_err(|_| "the signature is not this key's".to_owned())?;
        serde_json::from_slice(&decode(claims)?).map_err(|e| e.to_string())
    }
}

/// `der` as PEM with `label`, 64 characters to a line.
fn pem(label: &str, der: &[u8]) -> String {
    let body = STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in body.as_bytes().chunks(64) {
        out.push_str(&String::from_utf8_lossy(line));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// The next DER element of `input`: its tag, its content and what follows it.
fn der_next(input: &[u8]) -> (u8, &[u8], &[u8]) {
    let tag = input[0];
    let (len, header) = match input[1] {
        n if n < 0x80 => (usize::from(n), 2),
        n => {
            let bytes = usize::from(n & 0x7f);
            let len = input[2..2 + bytes]
                .iter()
                .fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
            (len, 2 + bytes)
        }
    };
    (tag, &input[header..header + len], &input[header + len..])
}

/// The PKCS#1 `RSAPrivateKey` inside a PKCS#8 `PrivateKeyInfo`: SEQUENCE { version, algorithm,
/// OCTET STRING { the key } }.
fn pkcs1_of(pkcs8: &[u8]) -> Vec<u8> {
    let (_, info, _) = der_next(pkcs8);
    let (_, _version, rest) = der_next(info);
    let (_, _algorithm, rest) = der_next(rest);
    let (tag, key, _) = der_next(rest);
    assert_eq!(tag, 0x04, "the key is an OCTET STRING");
    key.to_vec()
}
