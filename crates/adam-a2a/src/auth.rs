//! Inbound authentication: an axum middleware that fails closed.
//!
//! Every route requires a valid credential except an explicit allow-list of
//! public ones ([`is_public`]), so a route added later is protected by default.
//! The middleware also owns caller identity: it strips any client-sent
//! identity header and, after authenticating, inserts the trusted one that the
//! request handler reads (the SDK exposes request headers to handlers, and
//! nothing else).

use std::sync::Arc;

use a2a::JsonRpcResponse;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use subtle::{Choice, ConditionallySelectable, ConstantTimeEq};

use crate::backend::Caller;

/// Header the auth layer uses to hand the authenticated subject to the
/// request handler. Stripped from every inbound request first, so a client can
/// never forge it.
pub(crate) const CALLER_HEADER: HeaderName = HeaderName::from_static("x-adam-caller-subject");

/// JSON-RPC error code for a rejected credential. A2A defines none; this is
/// the first code of the JSON-RPC implementation-defined range.
pub(crate) const UNAUTHORIZED_CODE: i32 = -32000;

/// How inbound requests are authenticated.
#[derive(Clone)]
pub enum AuthConfig {
    /// `Authorization: Bearer <t>` must match one configured token; anything
    /// else gets 401 (fail closed). With an empty list every request is
    /// rejected.
    BearerTokens(Vec<SecretString>),
    /// Explicit opt-out, for local development only; logs a warning at startup.
    AllowAnonymous,
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BearerTokens(tokens) => f
                .debug_struct("BearerTokens")
                .field("tokens", &tokens.len())
                .finish(),
            Self::AllowAnonymous => f.write_str("AllowAnonymous"),
        }
    }
}

/// Resolved, ready-to-check form of [`AuthConfig`].
pub(crate) struct Authenticator {
    mode: Mode,
    /// Whether `GET /.well-known/jwks.json` needs no credential (it exists only when the card
    /// is signed, and then it is the public half of the key that signs it).
    public_jwks: bool,
}

/// Where the public key set of the card signature is served, when there is one.
pub(crate) const JWKS_PATH: &str = "/.well-known/jwks.json";

enum Mode {
    Anonymous,
    /// SHA-256 digests of the configured tokens, `None` for unusable (empty)
    /// ones. Comparing fixed-size digests in constant time hides token length
    /// as well as content.
    Bearer(Vec<Option<[u8; 32]>>),
}

impl Authenticator {
    /// Resolve the config, logging what an operator needs to know.
    pub(crate) fn new(config: AuthConfig) -> Self {
        let mode = match config {
            AuthConfig::AllowAnonymous => {
                tracing::warn!(
                    "A2A server started with AuthConfig::AllowAnonymous: every request is \
                     accepted as `anonymous`. Use bearer tokens outside local development."
                );
                Mode::Anonymous
            }
            AuthConfig::BearerTokens(tokens) => {
                if tokens.is_empty() {
                    tracing::warn!(
                        "A2A server has no bearer tokens configured: every request will be rejected"
                    );
                }
                let digests = tokens
                    .iter()
                    .enumerate()
                    .map(|(index, token)| {
                        let token = token.expose_secret();
                        if token.is_empty() {
                            tracing::warn!(index, "ignoring empty bearer token");
                            None
                        } else {
                            Some(digest(token))
                        }
                    })
                    .collect();
                Mode::Bearer(digests)
            }
        };
        Self {
            mode,
            public_jwks: false,
        }
    }

    /// Serve the card's key set (`/.well-known/jwks.json`) without a credential.
    #[must_use]
    pub(crate) fn with_public_jwks(mut self) -> Self {
        self.public_jwks = true;
        self
    }

    /// Whether requests carry credentials (drives the card's security scheme).
    pub(crate) fn requires_bearer(&self) -> bool {
        matches!(self.mode, Mode::Bearer(_))
    }

    /// Authenticate from the request's `Authorization` header values.
    /// `Err(Unauthorized)` means reject with 401.
    pub(crate) fn authenticate(
        &self,
        authorization: &[&HeaderValue],
    ) -> Result<Caller, Unauthorized> {
        match &self.mode {
            Mode::Anonymous => Ok(Caller::anonymous()),
            Mode::Bearer(digests) => {
                // Exactly one Authorization header; more is ambiguous.
                let [value] = authorization else {
                    return Err(Unauthorized);
                };
                let token = bearer_token(value).ok_or(Unauthorized)?;
                let presented = digest(token);
                // Check every configured token, without early exit.
                let mut found = Choice::from(0);
                let mut index = 0u64;
                for (i, candidate) in digests.iter().enumerate() {
                    let Some(candidate) = candidate else { continue };
                    let eq = presented.ct_eq(candidate);
                    index.conditional_assign(&(i as u64), eq);
                    found |= eq;
                }
                if bool::from(found) {
                    Ok(Caller::new(format!("token-{index}")))
                } else {
                    Err(Unauthorized)
                }
            }
        }
    }
}

/// Marker: the credential was missing or wrong.
#[derive(Debug)]
pub(crate) struct Unauthorized;

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// The token from `Authorization: Bearer <token>` (scheme is case-insensitive,
/// RFC 7235).
fn bearer_token(value: &HeaderValue) -> Option<&str> {
    let value = value.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim_start();
    (!token.is_empty()).then_some(token)
}

/// Routes that need no credential: the public card (discovery must work before
/// a client has credentials) and the liveness probe.
pub(crate) fn is_public(method: &Method, path: &str) -> bool {
    method == Method::GET && (path == a2a_server::WELL_KNOWN_AGENT_CARD_PATH || path == "/healthz")
}

/// Axum middleware: strip forged identity, authenticate, inject the trusted one.
pub(crate) async fn authenticate(
    State(authenticator): State<Arc<Authenticator>>,
    mut request: Request,
    next: Next,
) -> Response {
    request.headers_mut().remove(CALLER_HEADER);

    let path = request.uri().path();
    if is_public(request.method(), path)
        || (authenticator.public_jwks && request.method() == Method::GET && path == JWKS_PATH)
    {
        return next.run(request).await;
    }

    let caller = {
        let values: Vec<&HeaderValue> = request.headers().get_all(AUTHORIZATION).iter().collect();
        authenticator.authenticate(&values)
    };
    let subject = match caller {
        Ok(caller) => HeaderValue::from_str(&caller.subject).ok(),
        Err(Unauthorized) => None,
    };
    match subject {
        Some(subject) => {
            request.headers_mut().insert(CALLER_HEADER, subject);
            next.run(request).await
        }
        None => {
            tracing::debug!(
                path = request.uri().path(),
                "rejected unauthenticated request"
            );
            unauthorized()
        }
    }
}

/// 401 with `WWW-Authenticate: Bearer` and a JSON-RPC error envelope, so an
/// A2A client surfaces a typed error instead of failing to parse the body.
fn unauthorized() -> Response {
    let body = JsonRpcResponse::error(
        a2a::JsonRpcId::Null,
        a2a::JsonRpcError {
            code: UNAUTHORIZED_CODE,
            message: "unauthorized: missing or invalid bearer token".to_owned(),
            data: None,
        },
    );
    let body = serde_json::to_vec(&body).unwrap_or_default();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(value: &str) -> HeaderValue {
        HeaderValue::from_str(value).unwrap()
    }

    fn bearer(tokens: &[&str]) -> Authenticator {
        Authenticator::new(AuthConfig::BearerTokens(
            tokens.iter().map(|t| SecretString::from(*t)).collect(),
        ))
    }

    #[test]
    fn matches_the_right_token_and_names_it_by_index() {
        let auth = bearer(&["alpha", "beta", "gamma"]);
        let h = header("Bearer beta");
        assert_eq!(auth.authenticate(&[&h]).unwrap().subject, "token-1");
        let h = header("bearer gamma");
        assert_eq!(auth.authenticate(&[&h]).unwrap().subject, "token-2");
    }

    #[test]
    fn rejects_missing_wrong_malformed_and_duplicated_credentials() {
        let auth = bearer(&["alpha"]);
        assert!(auth.authenticate(&[]).is_err());
        for bad in [
            "Bearer nope",
            "Bearer ",
            "Bearer",
            "Basic alpha",
            "alpha",
            "Bearer alph",
            "Bearer alphaa",
        ] {
            let h = header(bad);
            assert!(
                auth.authenticate(&[&h]).is_err(),
                "{bad:?} must be rejected"
            );
        }
        let good = header("Bearer alpha");
        assert!(auth.authenticate(&[&good, &good]).is_err());
    }

    #[test]
    fn empty_token_list_and_empty_tokens_reject_everything() {
        let h = header("Bearer x");
        assert!(bearer(&[]).authenticate(&[&h]).is_err());
        assert!(bearer(&[""]).authenticate(&[&header("Bearer ")]).is_err());
    }

    #[test]
    fn anonymous_mode_accepts_everything_as_anonymous() {
        let auth = Authenticator::new(AuthConfig::AllowAnonymous);
        assert_eq!(auth.authenticate(&[]).unwrap(), Caller::anonymous());
        assert!(!auth.requires_bearer());
    }

    #[test]
    fn debug_never_prints_tokens() {
        let config = AuthConfig::BearerTokens(vec![SecretString::from("super-secret")]);
        assert!(!format!("{config:?}").contains("super-secret"));
    }

    #[test]
    fn only_card_and_healthz_are_public() {
        assert!(is_public(&Method::GET, "/.well-known/agent-card.json"));
        assert!(is_public(&Method::GET, "/healthz"));
        assert!(!is_public(&Method::POST, "/"));
        assert!(!is_public(&Method::POST, "/healthz"));
        assert!(!is_public(&Method::GET, "/"));
        assert!(!is_public(&Method::GET, "/anything-else"));
    }
}
