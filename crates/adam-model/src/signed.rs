//! Every model request signed: [`SignedRequests`] wraps the credentials of a model client and adds
//! adam's request signature ([`TURN_SIGNATURE_HEADER`], `adam-request/v1`) to each call
//! ([ADR 0068], *Amended 2026-11-25 (the request signature)*).
//!
//! [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md

use std::fmt;
use std::sync::Arc;

use adam_core::turn::{
    CurrentKey, DynRunKeys, DynTurnKeys, KeyId, KeySet, ModelCall, Signature, TurnKeyError,
    TurnKeys, sign_request,
};
use async_trait::async_trait;
use secrecy::SecretString;

pub use adam_core::turn::TURN_SIGNATURE_HEADER;

use crate::credentials::with_prompt;
use crate::{
    AfterRejection, CredentialHeaders, CredentialRequest, DynCredentials, ModelCredentials,
    ModelError, Park, RUN_HEADER, Refusal, Rejection,
};

/// The credentials of `inner`, and on **every** call the headers a verifier in front of the model
/// checks it with:
///
/// | Header | Value |
/// |---|---|
/// | `X-Run-Id` | the call's root run ([`CredentialRequest::run`]) |
/// | `Prompt-Statement`, `Prompt-User-Signature` | the run's values, when it has them, byte for byte ([`PromptStatement`](crate::PromptStatement)) |
/// | `Turn-Signature` | adam's signature of this request: its run, method, request target, the SHA-256 of its exact body, the prompt's id, the time and a nonce, signed by the [`KeySource`] ([`adam_core::turn::sign_request`]) |
///
/// Each call and each attempt is signed anew, so a retry after a `401` carries a new signature. The
/// key is named as the source names it: by `kid`, or by a certificate chain, which the header then
/// carries as `x5c` ([`KeySource`]).
///
/// **It fails closed**: a call it cannot sign is not sent. A call that names no run, a URL with no
/// request target, a prompt statement whose id cannot be read ([`PromptStatement::prompt_jti`](crate::PromptStatement::prompt_jti))
/// is [`ModelError::InvalidRequest`]; keys that cannot be reached are a transient error (the call is
/// retried as a model outage is); keys that refuse are [`ModelError::Auth`]. A run with no prompt
/// statement is signed with `prompt_jti` `null`.
///
/// Refusals and parks are `inner`'s. Its `Debug` names the source and the keys' kind, nothing else.
pub struct SignedRequests {
    inner: DynCredentials,
    source: KeySource,
}

/// Which key signs a model request ([ADR 0086] decision 4).
///
/// Closed on purpose: the request path handles each source, and a new one must fail to compile.
/// [`new`](SignedRequests::new) takes [`Process`](Self::Process), the process's turn keys, so a
/// host that gives none signs exactly as it did before a run had a certificate.
///
/// [ADR 0086]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md
#[derive(Clone, Debug)]
pub enum KeySource {
    /// The process's turn keys ([ADR 0068]): one key for every run the process steps.
    ///
    /// [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
    Process(DynTurnKeys),
    /// The run's own key, named by a certificate a CA issued for it ([ADR 0086]).
    ///
    /// [ADR 0086]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md
    Run(DynRunKeys),
}

#[async_trait]
impl TurnKeys for KeySource {
    async fn current(&self) -> Result<CurrentKey, TurnKeyError> {
        match self {
            Self::Process(keys) => keys.current().await,
            Self::Run(keys) => keys.current().await,
        }
    }

    async fn sign(&self, kid: &KeyId, input: &[u8]) -> Result<Signature, TurnKeyError> {
        match self {
            Self::Process(keys) => keys.sign(kid, input).await,
            Self::Run(keys) => keys.sign(kid, input).await,
        }
    }

    async fn key_set(&self) -> Result<Arc<KeySet>, TurnKeyError> {
        match self {
            Self::Process(keys) => keys.key_set().await,
            Self::Run(keys) => keys.key_set().await,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Process(keys) => keys.kind(),
            Self::Run(keys) => keys.kind(),
        }
    }
}

impl SignedRequests {
    /// The credentials of `inner`, each call signed with the process's turn keys `keys`.
    #[must_use]
    pub fn new(inner: DynCredentials, keys: DynTurnKeys) -> Self {
        Self::with_key_source(inner, KeySource::Process(keys))
    }

    /// The credentials of `inner`, each call signed by `source`: the process's turn keys, or the
    /// run's certified key ([`KeySource`]).
    #[must_use]
    pub fn with_key_source(inner: DynCredentials, source: KeySource) -> Self {
        Self { inner, source }
    }
}

impl fmt::Debug for SignedRequests {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignedRequests")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ModelCredentials for SignedRequests {
    async fn headers(&self, call: CredentialRequest<'_>) -> Result<CredentialHeaders, ModelError> {
        let Some(run) = call.run.filter(|run| !run.is_empty()) else {
            return Err(ModelError::invalid_request(
                "a signed model call names no run",
            ));
        };
        let Some(path) = request_target(call.url) else {
            return Err(ModelError::invalid_request(
                "a signed model call has no request target",
            ));
        };
        let prompt_jti = match call.prompt_statement {
            None => None,
            Some(prompt) => Some(
                prompt
                    .prompt_jti()
                    .map_err(|why| ModelError::invalid_request(why.to_string()))?,
            ),
        };
        let mut headers = self
            .inner
            .headers(call)
            .await?
            .with(RUN_HEADER, SecretString::from(run.to_owned()));
        if let Some(prompt) = call.prompt_statement {
            headers = with_prompt(headers, prompt);
        }
        let request = ModelCall {
            run,
            method: call.method,
            path: &path,
            body: call.body,
            prompt_jti: prompt_jti.as_deref(),
        };
        let signature = match sign_request(&self.source, &request).await {
            // The key was renewed between naming it and signing: name the current one again, once.
            Err(TurnKeyError::NotSigning { .. }) => sign_request(&self.source, &request).await,
            signed => signed,
        }
        .map_err(key_error)?;
        Ok(headers.with(TURN_SIGNATURE_HEADER, SecretString::from(signature)))
    }

    async fn rejected(&self, call: CredentialRequest<'_>, rejection: &Rejection) -> AfterRejection {
        self.inner.rejected(call, rejection).await
    }

    fn parks(&self, refusal: &Refusal) -> Park {
        self.inner.parks(refusal)
    }
}

/// A [`TurnKeyError`] as the model call's error. Its text names no key and no input.
fn key_error(err: TurnKeyError) -> ModelError {
    match err {
        TurnKeyError::Refused(_) => {
            ModelError::Auth("the turn keys refuse to sign the model request".to_owned())
        }
        other => ModelError::transient("the turn keys could not sign the model request")
            .with_source(other),
    }
}

/// The request target of `url` as an HTTP client sends it (RFC 9112 §3.2.1, origin form): the path
/// and the query, `/` for an empty path, never the fragment. `None` when `url` has no scheme.
///
/// `url` is expected in the form a URL parser writes (`adam-model-openai` keeps it so): this does
/// not normalise percent-encoding or dot segments.
pub fn request_target(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let rest = rest.split('#').next().unwrap_or_default();
    Some(match rest.find(['/', '?']) {
        Some(at) if rest[at..].starts_with('?') => format!("/{}", &rest[at..]),
        Some(at) => rest[at..].to_owned(),
        None => "/".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::sync::Arc;

    use adam_core::turn::{KeySet, TurnKeys, TurnSignatureError, verify_request};
    use secrecy::ExposeSecret as _;

    use super::*;
    use crate::{InjectedRun, PromptStatement, StaticBearer};

    /// A statement whose attached payload names `jti`, and a device envelope whose header names a
    /// `nonce`: shapes only, no real signature.
    fn b64(json: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    }

    fn statement(jti: &str) -> String {
        format!(
            "{}.{}.c2ln",
            b64(r#"{"alg":"EdDSA"}"#),
            b64(&format!(r#"{{"jti":"{jti}"}}"#))
        )
    }

    fn device_envelope(nonce: &str) -> String {
        format!(
            "{}.{}.c2ln",
            b64(&format!(r#"{{"alg":"EdDSA","nonce":"{nonce}"}}"#)),
            b64(r#"{"typ":"prompt/v1"}"#)
        )
    }

    fn call<'a>(prompt: Option<&'a PromptStatement>, body: &'a [u8]) -> CredentialRequest<'a> {
        CredentialRequest {
            method: "POST",
            url: "https://models.example.test/v1/chat/completions",
            attempt: 0,
            run: Some("run-root-1"),
            prompt_statement: prompt,
            body,
        }
    }

    fn pairs(headers: &CredentialHeaders) -> Vec<(String, String)> {
        headers
            .iter()
            .map(|(n, v)| (n.to_owned(), v.expose_secret().to_owned()))
            .collect()
    }

    fn header(headers: &CredentialHeaders, name: &str) -> Option<String> {
        pairs(headers)
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }

    async fn keys() -> (DynTurnKeys, Arc<KeySet>) {
        let keys = <dyn TurnKeys>::ephemeral().unwrap();
        let set = keys.key_set().await.unwrap();
        (keys, set)
    }

    fn now() -> i64 {
        jiff::Timestamp::now().as_second()
    }

    #[test]
    fn the_request_target_is_the_path_and_query() {
        for (url, target) in [
            ("https://h.test/v1/chat/completions", "/v1/chat/completions"),
            ("https://h.test:8443/v1/c?x=1&y=2", "/v1/c?x=1&y=2"),
            ("http://h.test", "/"),
            ("http://h.test?q", "/?q"),
            ("http://h.test/a#frag", "/a"),
        ] {
            assert_eq!(request_target(url).as_deref(), Some(target), "{url}");
        }
        assert_eq!(request_target("/relative"), None);
    }

    /// Cloud: the injector's headers, and the signature binds the statement's `jti`.
    #[tokio::test]
    async fn a_cloud_call_is_signed_over_its_run_body_and_statement() {
        let (keys, set) = keys().await;
        let signed = SignedRequests::new(Arc::new(InjectedRun::new()), keys);
        let prompt = PromptStatement::new(statement("jti-1"), Some(statement("x"))).unwrap();
        let body = br#"{"model":"m","messages":[]}"#;
        let headers = signed.headers(call(Some(&prompt), body)).await.unwrap();
        let names: Vec<String> = pairs(&headers).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            [
                "X-Run-Id",
                "Prompt-Statement",
                "Prompt-User-Signature",
                "Turn-Signature"
            ]
        );
        let jws = header(&headers, TURN_SIGNATURE_HEADER).unwrap();
        let request = ModelCall {
            run: "run-root-1",
            method: "POST",
            path: "/v1/chat/completions",
            body,
            prompt_jti: Some("jti-1"),
        };
        verify_request(&set, &jws, &request, now()).unwrap();
        assert_eq!(
            verify_request(
                &set,
                &jws,
                &ModelCall {
                    body: b"{}",
                    ..request
                },
                now()
            ),
            Err(TurnSignatureError::Mismatch {
                member: "body_sha256"
            })
        );
        let again = signed.headers(call(Some(&prompt), body)).await.unwrap();
        assert_ne!(header(&again, TURN_SIGNATURE_HEADER).unwrap(), jws);
    }

    /// Device: a bearer of the host, the run named, the envelope sent and its nonce bound.
    #[tokio::test]
    async fn a_device_call_names_its_run_and_binds_the_envelope_nonce() {
        let (keys, set) = keys().await;
        let bearer = Arc::new(StaticBearer::new(SecretString::from("device-token")));
        let signed = SignedRequests::new(bearer, keys);
        let prompt = PromptStatement::envelope(device_envelope("nonce-7")).unwrap();
        let headers = signed.headers(call(Some(&prompt), b"{}")).await.unwrap();
        assert_eq!(
            header(&headers, "authorization").as_deref(),
            Some("Bearer device-token")
        );
        assert_eq!(header(&headers, RUN_HEADER).as_deref(), Some("run-root-1"));
        assert_eq!(header(&headers, "Prompt-Statement"), None);
        assert_eq!(
            header(&headers, "Prompt-User-Signature"),
            Some(device_envelope("nonce-7"))
        );
        let jws = header(&headers, TURN_SIGNATURE_HEADER).unwrap();
        verify_request(
            &set,
            &jws,
            &ModelCall {
                run: "run-root-1",
                method: "POST",
                path: "/v1/chat/completions",
                body: b"{}",
                prompt_jti: Some("nonce-7"),
            },
            now(),
        )
        .unwrap();
    }

    /// No prompt: signed, `prompt_jti` null. No run, an unreadable prompt id: nothing is signed
    /// and the call is refused before it is sent.
    #[tokio::test]
    async fn it_fails_closed_on_what_it_cannot_sign() {
        let (keys, set) = keys().await;
        let signed = SignedRequests::new(Arc::new(StaticBearer::new(SecretString::from(""))), keys);
        let headers = signed.headers(call(None, b"{}")).await.unwrap();
        let jws = header(&headers, TURN_SIGNATURE_HEADER).unwrap();
        verify_request(
            &set,
            &jws,
            &ModelCall {
                run: "run-root-1",
                method: "POST",
                path: "/v1/chat/completions",
                body: b"{}",
                prompt_jti: None,
            },
            now(),
        )
        .unwrap();

        let err = signed
            .headers(CredentialRequest {
                run: None,
                ..call(None, b"{}")
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ModelError::InvalidRequest { .. }), "{err:?}");

        let opaque = PromptStatement::new("eyJh.eyJw.c2ln", None).unwrap();
        let err = signed
            .headers(call(Some(&opaque), b"{}"))
            .await
            .unwrap_err();
        assert!(matches!(err, ModelError::InvalidRequest { .. }), "{err:?}");
        assert!(!err.to_string().contains("eyJ"), "{err}");
    }

    /// Keys that cannot sign: the call is not sent, with the class of the keys' failure.
    #[tokio::test]
    async fn keys_that_cannot_sign_send_nothing() {
        struct Down(bool);
        #[async_trait]
        impl TurnKeys for Down {
            async fn current(&self) -> Result<adam_core::turn::CurrentKey, TurnKeyError> {
                if self.0 {
                    Err(TurnKeyError::Refused("locked".into()))
                } else {
                    Err(TurnKeyError::Unavailable("down".into()))
                }
            }
            async fn sign(
                &self,
                kid: &adam_core::turn::KeyId,
                _input: &[u8],
            ) -> Result<adam_core::turn::Signature, TurnKeyError> {
                Err(TurnKeyError::NotSigning { kid: kid.clone() })
            }
            async fn key_set(&self) -> Result<Arc<KeySet>, TurnKeyError> {
                Ok(Arc::new(KeySet::new()))
            }
        }
        let bearer: DynCredentials = Arc::new(StaticBearer::new(SecretString::from("")));
        let down = SignedRequests::new(bearer.clone(), Arc::new(Down(false)));
        let err = down.headers(call(None, b"{}")).await.unwrap_err();
        assert!(adam_error::Classify::is_retryable(&err), "{err:?}");
        let locked = SignedRequests::new(bearer, Arc::new(Down(true)));
        let err = locked.headers(call(None, b"{}")).await.unwrap_err();
        assert!(matches!(err, ModelError::Auth(_)), "{err:?}");
        assert_eq!(
            format!("{locked:?}"),
            "SignedRequests { source: Process(TurnKeys(host)), .. }"
        );
    }

    /// A run's key: the signature carries the certificate chain, and it verifies with the run's
    /// key set. The process source is the default; a run source is chosen by name.
    #[tokio::test]
    async fn a_run_key_signs_with_its_certificate_chain() {
        use adam_core::turn::run_cert::testkit::{FakeRunCertIssuer, request_for};
        use adam_core::turn::testkit::Certified;
        use adam_core::turn::{Ed25519TurnKeys, RunCertIssuer};
        use base64::Engine as _;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let issuer = FakeRunCertIssuer::new();
        // A real request: the fake parses a PKCS#10 CSR since A2, so hand-written bytes are
        // refused before a leaf is minted.
        let chain = issuer.issue(&request_for("run-root-1")).await.unwrap();
        let run = Certified::new(Ed25519TurnKeys::ephemeral().unwrap(), chain);
        let set = run.key_set().await.unwrap();
        let signed = SignedRequests::with_key_source(
            Arc::new(StaticBearer::new(SecretString::from(""))),
            KeySource::Run(Arc::new(run)),
        );
        let headers = signed.headers(call(None, b"{}")).await.unwrap();
        let jws = header(&headers, TURN_SIGNATURE_HEADER).unwrap();
        verify_request(
            &set,
            &jws,
            &ModelCall {
                run: "run-root-1",
                method: "POST",
                path: "/v1/chat/completions",
                body: b"{}",
                prompt_jti: None,
            },
            now(),
        )
        .unwrap();
        let protected = jws.split('.').next().unwrap();
        let header_json: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(protected).unwrap()).unwrap();
        assert!(
            header_json.get("x5c").is_some(),
            "the run key's signature carries no certificate chain: {header_json}"
        );
        assert_eq!(
            format!("{signed:?}"),
            "SignedRequests { source: Run(RunKeys(certificate)), .. }"
        );
    }
}
