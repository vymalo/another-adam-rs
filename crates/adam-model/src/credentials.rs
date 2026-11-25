//! The credentials of a model call, asked of a provider the host gives ([ADR 0058]).
//!
//! A [`ModelClient`](crate::ModelClient) that talks HTTP asks its [`ModelCredentials`] for the headers of
//! **every** call, so a provider can mint, renew or prove a credential at the moment it is used: a
//! fixed key ([`StaticBearer`]), a token the host renews from the person's session, a proof bound to
//! the request (its method and URL are given), or a header that names the run for an injector in front
//! of the model, with no key at all. The client never stores what it is given and never journals it.
//!
//! [ADR 0058]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};

use crate::{ModelError, PROMPT_STATEMENT_HEADER, PROMPT_USER_SIGNATURE_HEADER, PromptStatement};

/// The call a provider is asked to authorise. Its `Debug` shows the body's length, never the body.
#[derive(Clone, Copy)]
pub struct CredentialRequest<'a> {
    /// The HTTP method, upper case (`POST`).
    pub method: &'a str,
    /// The full URL the call goes to, query included (a proof bound to the request needs it).
    pub url: &'a str,
    /// `0` for the first try, `1` for the one retry after [`AfterRejection::Retry`].
    pub attempt: u32,
    /// The root run the call belongs to ([`ModelRequest::run`](crate::ModelRequest::run)), when the
    /// caller named one: what [`InjectedRun`] sends ([ADR 0067]).
    ///
    /// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
    pub run: Option<&'a str>,
    /// The prompt statement and the person's envelope of the call's run
    /// ([`ModelRequest::prompt_statement`](crate::ModelRequest::prompt_statement)), when it has
    /// them: what [`InjectedRun`] sends beside the run ([ADR 0077]). Its `Debug` holds no value.
    ///
    /// [ADR 0077]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0077-a-model-call-carries-the-prompt-statement-and-the-persons-envelope.md
    pub prompt_statement: Option<&'a PromptStatement>,
    /// The exact bytes of the call's body, as they are sent: what a request signature hashes
    /// ([`SignedRequests`](crate::SignedRequests)). Empty for a call with no body.
    pub body: &'a [u8],
}

impl fmt::Debug for CredentialRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("attempt", &self.attempt)
            .field("run", &self.run)
            .field("prompt_statement", &self.prompt_statement)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// Why the endpoint refused a call: what a provider needs to decide whether a new credential would help.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rejection {
    /// The status code (`401`).
    pub status: u16,
    /// The `WWW-Authenticate` header, when there was one (`Bearer error="invalid_token"`).
    pub www_authenticate: Option<String>,
    /// The `DPoP-Nonce` header, when there was one (RFC 9449 §8: the server asks for a fresh nonce).
    pub dpop_nonce: Option<String>,
}

/// What a provider says after a [`Rejection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterRejection {
    /// A new credential may work: the client asks for headers again and sends the call once more.
    Retry,
    /// It would not: the call fails with the endpoint's answer.
    GiveUp,
}

/// What the endpoint answered when it refused a call (a status of 400 or above), as a provider is
/// asked about it ([`ModelCredentials::parks`], [ADR 0067] decision 3). It holds no body and no
/// header: the problem type is the only part of the body that is read, and only from a problem
/// document.
///
/// `#[non_exhaustive]`: build one with [`Refusal::new`] and the `with_` methods.
///
/// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Refusal {
    /// The status code (`403`).
    pub status: u16,
    /// The `type` of an `application/problem+json` body (RFC 9457), as far as it could be read.
    pub problem: ProblemType,
    /// The `Retry-After` header, when there was one.
    pub retry_after: Option<Duration>,
}

impl Refusal {
    /// A refusal of status `status`, with no problem type and no `Retry-After`.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self {
            status,
            ..Self::default()
        }
    }

    /// With the problem type `problem`.
    #[must_use]
    pub fn with_problem(mut self, problem: ProblemType) -> Self {
        self.problem = problem;
        self
    }

    /// With the slug `slug` ([`ProblemType::Slug`]).
    #[must_use]
    pub fn with_slug(self, slug: impl Into<String>) -> Self {
        self.with_problem(ProblemType::Slug(slug.into()))
    }

    /// With the `Retry-After` `after`.
    #[must_use]
    pub fn with_retry_after(mut self, after: Option<Duration>) -> Self {
        self.retry_after = after;
        self
    }

    /// The problem type's slug, when it had one ([`ProblemType::Slug`]).
    #[must_use]
    pub fn slug(&self) -> Option<&str> {
        match &self.problem {
            ProblemType::Slug(slug) => Some(slug),
            ProblemType::Absent | ProblemType::Unreadable => None,
        }
    }
}

/// The problem type of a refusal ([`Refusal::problem`]): none, a slug, or one that cannot be read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ProblemType {
    /// No problem type: no problem document (another `Content-Type`, an empty body), no `type`
    /// member, or `about:blank` (RFC 9457 §4.2.1: the status says it all).
    #[default]
    Absent,
    /// The `type`'s slug (`run_not_bound`): its last path segment, or the member as is when it
    /// holds no `/`, when that is a word of at most 64 letters, digits, `_`, `-` and `.`.
    Slug(String),
    /// A problem document whose `type` is there but is no such word (`urn:gw:model_locked`, a
    /// query or a fragment, a member that is not a string, a body that is not a JSON object): the
    /// endpoint said what the refusal is, and adam cannot read it.
    Unreadable,
}

/// Whether a refused call parks its run, and how it wakes ([`ModelCredentials::parks`]).
///
/// Closed on purpose: whoever parks a run handles every way of waking it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Park {
    /// Not a park: the call fails with the error it has without a park (`Auth` for a `401`, and so on).
    No,
    /// Park, and make the same call again after at least this long.
    OnTimer(Duration),
    /// Park until something outside the run wakes it (a person, the orchestrator).
    UntilWoken,
}

/// The first wait of a park on a timer, when the endpoint says no longer one: the first step of the
/// agent's backoff ([ADR 0067] decision 4).
///
/// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
pub const FIRST_PARK: Duration = Duration::from_secs(30);

/// The longest wait of a park on a timer, whatever `Retry-After` says: 24 hours, so a wake time is
/// always one a clock can hold and a limit on parks applies ([ADR 0067] decision 4).
///
/// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
pub const MAX_PARK: Duration = Duration::from_secs(24 * 60 * 60);

/// The slug of a refusal that says the model is not allowed at all
/// ([`ModelError::ModelLocked`] for every provider, [ADR 0067] decision 7).
///
/// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
pub const MODEL_LOCKED_SLUG: &str = "model_locked";

/// The headers of one call. Names are plain; values are secrets, never shown by `Debug`.
#[derive(Default, Clone)]
pub struct CredentialHeaders(Vec<(String, SecretString)>);

impl CredentialHeaders {
    /// No header: the call goes out without credentials (a local server).
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// `Authorization: Bearer <token>`.
    #[must_use]
    pub fn bearer(token: &SecretString) -> Self {
        Self::none().with(
            "authorization",
            SecretString::from(format!("Bearer {}", token.expose_secret())),
        )
    }

    /// The same headers and one more; a later header of the same name replaces an earlier one.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: SecretString) -> Self {
        let name = name.into();
        self.0.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
        self.0.push((name, value));
        self
    }

    /// The headers, in the order they were added.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &SecretString)> {
        self.0.iter().map(|(n, v)| (n.as_str(), v))
    }

    /// Whether there is no header.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for CredentialHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(n, _)| n))
            .finish()
    }
}

/// Where the credentials of model calls come from. Asked for every call, so an implementation that
/// mints or renews must be cheap when it has a valid credential at hand (a cache and a lock, not a
/// network call per request).
///
/// Errors are the provider's choice of class: [`ModelError::Auth`] when the person must act (signed
/// out, consent withdrawn), a transient one when the issuer could not be reached, so the runtime
/// retries it as it retries the model's own failures.
#[async_trait]
pub trait ModelCredentials: Send + Sync + 'static {
    /// The headers to add to `call`. They replace any header of the same name the client would send.
    async fn headers(&self, call: CredentialRequest<'_>) -> Result<CredentialHeaders, ModelError>;

    /// The endpoint answered `401` to `call`, sent with the headers of [`headers`](Self::headers).
    /// The default gives up: a fixed credential will not get better by asking again. A provider
    /// that renews drops what it holds and answers [`AfterRejection::Retry`]; the client retries
    /// once, never more.
    async fn rejected(&self, call: CredentialRequest<'_>, rejection: &Rejection) -> AfterRejection {
        let _ = (call, rejection);
        AfterRejection::GiveUp
    }

    /// The endpoint refused a call (a status of 400 or above): whether the run parks on it
    /// ([ADR 0067](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md)
    /// decision 3). The default is [`Park::No`]: the call fails as it always has, so a wrong key is
    /// a failed task, not a run that waits for ever. Only a provider whose refusals are cured
    /// outside the run ([`InjectedRun`]) parks. Asked after [`rejected`](Self::rejected) gave up.
    fn parks(&self, refusal: &Refusal) -> Park {
        let _ = refusal;
        Park::No
    }
}

/// A shared, type-erased [`ModelCredentials`].
pub type DynCredentials = Arc<dyn ModelCredentials>;

#[async_trait]
impl<T: ModelCredentials + ?Sized> ModelCredentials for Arc<T> {
    async fn headers(&self, call: CredentialRequest<'_>) -> Result<CredentialHeaders, ModelError> {
        (**self).headers(call).await
    }

    async fn rejected(&self, call: CredentialRequest<'_>, rejection: &Rejection) -> AfterRejection {
        (**self).rejected(call, rejection).await
    }

    fn parks(&self, refusal: &Refusal) -> Park {
        (**self).parks(refusal)
    }
}

/// A fixed bearer token, the same for every call: what `MODEL_API_KEY` gives. An empty token sends
/// no `Authorization` header.
#[derive(Clone)]
pub struct StaticBearer(SecretString);

impl StaticBearer {
    /// A provider that always answers `token`.
    #[must_use]
    pub fn new(token: SecretString) -> Self {
        Self(token)
    }
}

impl fmt::Debug for StaticBearer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = if self.0.expose_secret().is_empty() {
            "empty"
        } else {
            "[REDACTED]"
        };
        f.debug_tuple("StaticBearer").field(&set).finish()
    }
}

#[async_trait]
impl ModelCredentials for StaticBearer {
    async fn headers(&self, _call: CredentialRequest<'_>) -> Result<CredentialHeaders, ModelError> {
        if self.0.expose_secret().is_empty() {
            Ok(CredentialHeaders::none())
        } else {
            Ok(CredentialHeaders::bearer(&self.0))
        }
    }
}

/// `headers` and the values of `prompt`: [`PROMPT_STATEMENT_HEADER`] when it has a statement,
/// [`PROMPT_USER_SIGNATURE_HEADER`] when it has the person's envelope, each byte for byte.
pub(crate) fn with_prompt(
    mut headers: CredentialHeaders,
    prompt: &PromptStatement,
) -> CredentialHeaders {
    if let Some(statement) = prompt.statement() {
        headers = headers.with(
            PROMPT_STATEMENT_HEADER,
            SecretString::from(statement.to_owned()),
        );
    }
    if let Some(user) = prompt.user() {
        headers = headers.with(
            PROMPT_USER_SIGNATURE_HEADER,
            SecretString::from(user.to_owned()),
        );
    }
    headers
}

/// The name of the header [`InjectedRun`] sends.
pub const RUN_HEADER: &str = "X-Run-Id";

/// No key, the run's id instead: what `MODEL_AUTH=injector` gives ([ADR 0067]). Every call carries
/// `X-Run-Id: <root run id>` and no `Authorization`; an injector in front of the model finds the
/// credential the run was leased and adds it upstream. The run id is an opaque capability, not a
/// secret.
///
/// A call whose run has a prompt statement ([`CredentialRequest::prompt_statement`], ADR 0077) also
/// carries [`PROMPT_STATEMENT_HEADER`] when it has a statement and [`PROMPT_USER_SIGNATURE_HEADER`]
/// when the person's envelope is there, each byte for byte; a run without one sends neither. Of the
/// providers here only this one and [`SignedRequests`](crate::SignedRequests) send them: a key or a
/// device grant alone talks to the model's own endpoint, which has no use for the person's
/// signature.
///
/// A call that names no run is refused with [`ModelError::InvalidRequest`] before anything is sent:
/// there is no fallback to a key.
///
/// Its refusals park the run ([`parks`](ModelCredentials::parks)), by the problem type's slug:
///
/// | Slug | Park |
/// |---|---|
/// | `unauthenticated`, `run_not_bound`, `lease_expired` | [`Park::OnTimer`]`(`[`FIRST_PARK`]`)` |
/// | `over_limit` | [`Park::UntilWoken`] |
/// | `rate_limited`, `unavailable` | [`Park::OnTimer`] of `Retry-After`, at least [`FIRST_PARK`] and at most [`MAX_PARK`] |
/// | none ([`ProblemType::Absent`]), on a `401` or `403` | [`Park::OnTimer`]`(`[`FIRST_PARK`]`)`, as `unauthenticated` (an edge's own denial may carry no problem body, *unverified*) |
/// | `model_locked`, any other slug, a `type` that cannot be read ([`ProblemType::Unreadable`]), none on any other status | [`Park::No`] |
///
/// A known slug decides whatever the status; `model_locked` is [`ModelError::ModelLocked`], which
/// the client gives for every provider.
///
/// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
#[derive(Debug, Clone, Copy, Default)]
pub struct InjectedRun;

impl InjectedRun {
    /// The provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ModelCredentials for InjectedRun {
    async fn headers(&self, call: CredentialRequest<'_>) -> Result<CredentialHeaders, ModelError> {
        match call.run {
            Some(run) if !run.is_empty() => {
                let mut headers =
                    CredentialHeaders::none().with(RUN_HEADER, SecretString::from(run.to_owned()));
                if let Some(prompt) = call.prompt_statement {
                    headers = with_prompt(headers, prompt);
                }
                Ok(headers)
            }
            _ => Err(ModelError::invalid_request(
                "a model call through the injector names no run",
            )),
        }
    }

    fn parks(&self, refusal: &Refusal) -> Park {
        let after_retry_after = || {
            Park::OnTimer(
                refusal
                    .retry_after
                    .map_or(FIRST_PARK, |after| after.clamp(FIRST_PARK, MAX_PARK)),
            )
        };
        match &refusal.problem {
            ProblemType::Slug(slug) => match slug.as_str() {
                "unauthenticated" | "run_not_bound" | "lease_expired" => Park::OnTimer(FIRST_PARK),
                "over_limit" => Park::UntilWoken,
                "rate_limited" | "unavailable" => after_retry_after(),
                // `model_locked` is no park (the client makes it `ModelLocked`), nor a slug it does
                // not know.
                _ => Park::No,
            },
            // The endpoint named the refusal and adam cannot read the name: it is not one of the
            // table's, so it parks nothing.
            ProblemType::Unreadable => Park::No,
            ProblemType::Absent if matches!(refusal.status, 401 | 403) => Park::OnTimer(FIRST_PARK),
            ProblemType::Absent => Park::No,
        }
    }
}

/// A test double that mints `token-1`, `token-2`, ... and renews on a rejection: every call to
/// [`headers`](ModelCredentials::headers) answers the current token, every [`rejected`](ModelCredentials::rejected)
/// moves to the next one and asks for the retry. It records what it was asked, for the tests of a
/// client: [`issued`](Self::issued), [`rejections`](Self::rejections).
#[derive(Debug, Default)]
pub struct RenewingCredentials {
    current: AtomicU32,
    issued: AtomicU32,
    rejections: std::sync::Mutex<Vec<Rejection>>,
}

impl RenewingCredentials {
    /// Starts at `token-1`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: AtomicU32::new(1),
            ..Self::default()
        }
    }

    /// How many times headers were asked for.
    #[must_use]
    pub fn issued(&self) -> u32 {
        self.issued.load(Ordering::SeqCst)
    }

    /// The rejections it was told about, oldest first.
    #[must_use]
    pub fn rejections(&self) -> Vec<Rejection> {
        self.rejections
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl ModelCredentials for RenewingCredentials {
    async fn headers(&self, _call: CredentialRequest<'_>) -> Result<CredentialHeaders, ModelError> {
        self.issued.fetch_add(1, Ordering::SeqCst);
        let n = self.current.load(Ordering::SeqCst).max(1);
        Ok(CredentialHeaders::bearer(&SecretString::from(format!(
            "token-{n}"
        ))))
    }

    async fn rejected(
        &self,
        _call: CredentialRequest<'_>,
        rejection: &Rejection,
    ) -> AfterRejection {
        if let Ok(mut r) = self.rejections.lock() {
            r.push(rejection.clone());
        }
        self.current.fetch_add(1, Ordering::SeqCst);
        AfterRejection::Retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> CredentialRequest<'static> {
        CredentialRequest {
            method: "POST",
            url: "https://models.example.test/v1/chat/completions",
            attempt: 0,
            run: None,
            prompt_statement: None,
            body: b"",
        }
    }

    fn header_pairs(headers: &CredentialHeaders) -> Vec<(String, String)> {
        headers
            .iter()
            .map(|(n, v)| (n.to_owned(), v.expose_secret().to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn the_injector_names_the_run_and_sends_no_key() {
        for attempt in [0, 1] {
            let headers = InjectedRun::new()
                .headers(CredentialRequest {
                    run: Some("run-root-1"),
                    attempt,
                    ..call()
                })
                .await
                .unwrap();
            assert_eq!(
                header_pairs(&headers),
                vec![("X-Run-Id".to_owned(), "run-root-1".to_owned())],
                "exactly one header, and no authorization"
            );
        }
        assert_eq!(RUN_HEADER, "X-Run-Id");
    }

    #[tokio::test]
    async fn the_injector_sends_the_prompt_statement_and_the_envelope_when_the_run_has_them() {
        let (statement, user) = ("aGVhZA.cGF5.c2ln", "dXNlcg.ZW52.c2ln");
        let both = PromptStatement::new(statement, Some(user.to_owned())).unwrap();
        let headers = InjectedRun::new()
            .headers(CredentialRequest {
                run: Some("run-root-1"),
                prompt_statement: Some(&both),
                ..call()
            })
            .await
            .unwrap();
        assert_eq!(
            header_pairs(&headers),
            vec![
                ("X-Run-Id".to_owned(), "run-root-1".to_owned()),
                ("Prompt-Statement".to_owned(), statement.to_owned()),
                ("Prompt-User-Signature".to_owned(), user.to_owned()),
            ]
        );
        // No envelope: the statement alone. Neither value shows in a `Debug` of the headers.
        let only = PromptStatement::new(statement, None).unwrap();
        let headers = InjectedRun::new()
            .headers(CredentialRequest {
                run: Some("run-root-1"),
                prompt_statement: Some(&only),
                ..call()
            })
            .await
            .unwrap();
        assert_eq!(
            header_pairs(&headers),
            vec![
                ("X-Run-Id".to_owned(), "run-root-1".to_owned()),
                ("Prompt-Statement".to_owned(), statement.to_owned()),
            ]
        );
        assert!(!format!("{headers:?}").contains(statement));
        // A key never sends them.
        let headers = StaticBearer::new(SecretString::from("k"))
            .headers(CredentialRequest {
                run: Some("run-root-1"),
                prompt_statement: Some(&both),
                ..call()
            })
            .await
            .unwrap();
        assert_eq!(
            header_pairs(&headers),
            vec![("authorization".to_owned(), "Bearer k".to_owned())]
        );
    }

    #[tokio::test]
    async fn the_injector_refuses_a_call_that_names_no_run() {
        for run in [None, Some("")] {
            let err = InjectedRun::new()
                .headers(CredentialRequest { run, ..call() })
                .await
                .expect_err("no run, nothing sent");
            assert!(matches!(err, ModelError::InvalidRequest { .. }), "{err:?}");
            assert!(
                err.to_string()
                    .contains("a model call through the injector names no run"),
                "{err}"
            );
            assert!(!adam_error::Classify::is_retryable(&err));
        }
    }

    #[tokio::test]
    async fn the_injector_gives_up_after_a_rejection() {
        let after = InjectedRun::new()
            .rejected(
                CredentialRequest {
                    run: Some("run-root-1"),
                    ..call()
                },
                &Rejection {
                    status: 401,
                    ..Rejection::default()
                },
            )
            .await;
        assert_eq!(after, AfterRejection::GiveUp);
    }

    fn refusal(status: u16, slug: Option<&str>, retry_after: Option<u64>) -> Refusal {
        let refusal = Refusal::new(status).with_retry_after(retry_after.map(Duration::from_secs));
        match slug {
            Some(slug) => refusal.with_slug(slug),
            None => refusal,
        }
    }

    /// ADR 0067 decision 3, row by row.
    #[test]
    fn the_injector_parks_by_the_table() {
        let timer = |secs| Park::OnTimer(Duration::from_secs(secs));
        let table = [
            (refusal(401, Some("unauthenticated"), None), timer(30)),
            (refusal(403, Some("run_not_bound"), None), timer(30)),
            (refusal(403, Some("lease_expired"), None), timer(30)),
            // A known slug decides, whatever the status or a Retry-After with it.
            (refusal(403, Some("run_not_bound"), Some(600)), timer(30)),
            (refusal(402, Some("over_limit"), None), Park::UntilWoken),
            (
                refusal(402, Some("over_limit"), Some(600)),
                Park::UntilWoken,
            ),
            (refusal(429, Some("rate_limited"), None), timer(30)),
            (refusal(429, Some("rate_limited"), Some(5)), timer(30)),
            (refusal(429, Some("rate_limited"), Some(30)), timer(30)),
            (refusal(429, Some("rate_limited"), Some(120)), timer(120)),
            // Never past `MAX_PARK`, whatever the endpoint asks.
            (
                refusal(429, Some("rate_limited"), Some(86_400)),
                timer(86_400),
            ),
            (
                refusal(429, Some("rate_limited"), Some(86_401)),
                timer(86_400),
            ),
            (
                refusal(503, Some("unavailable"), Some(u64::MAX)),
                timer(86_400),
            ),
            (refusal(503, Some("unavailable"), None), timer(30)),
            (refusal(503, Some("unavailable"), Some(1)), timer(30)),
            (refusal(503, Some("unavailable"), Some(90)), timer(90)),
            (refusal(403, Some("model_locked"), None), Park::No),
            // A 401 or a 403 with no slug is `unauthenticated`.
            (refusal(401, None, None), timer(30)),
            (refusal(403, None, None), timer(30)),
            (refusal(403, None, Some(600)), timer(30)),
            // Anything else is no park.
            (refusal(401, Some("invalid_token"), None), Park::No),
            (refusal(400, None, None), Park::No),
            (refusal(402, None, None), Park::No),
            (refusal(404, None, None), Park::No),
            (refusal(429, None, Some(120)), Park::No),
            (refusal(500, None, None), Park::No),
            (refusal(503, None, Some(90)), Park::No),
        ];
        for (refusal, park) in table {
            assert_eq!(InjectedRun::new().parks(&refusal), park, "{refusal:?}");
        }
        assert_eq!(FIRST_PARK, Duration::from_secs(30));
        assert_eq!(MAX_PARK, Duration::from_secs(24 * 60 * 60));
        assert_eq!(MODEL_LOCKED_SLUG, "model_locked");
    }

    /// A problem type that is there but cannot be read is not "no problem type": it parks nothing,
    /// on a `401` or `403` too, where no type at all parks as `unauthenticated`.
    #[test]
    fn an_unreadable_problem_type_parks_nothing() {
        for status in [401, 402, 403, 429, 503] {
            let unreadable = Refusal::new(status)
                .with_problem(ProblemType::Unreadable)
                .with_retry_after(Some(Duration::from_secs(60)));
            assert_eq!(InjectedRun::new().parks(&unreadable), Park::No, "{status}");
            assert_eq!(unreadable.slug(), None);
        }
        assert_eq!(
            InjectedRun::new().parks(&Refusal::new(403).with_problem(ProblemType::Absent)),
            Park::OnTimer(FIRST_PARK)
        );
        assert_eq!(Refusal::new(403).problem, ProblemType::Absent);
        assert_eq!(
            Refusal::new(403).with_slug("over_limit").slug(),
            Some("over_limit")
        );
    }

    /// The default is no park: a wrong key fails as before (ADR 0058 decision 3), and an `Arc`
    /// forwards the provider's own answer.
    #[test]
    fn the_other_providers_never_park() {
        let shared: DynCredentials = Arc::new(StaticBearer::new(SecretString::from("sk-test")));
        let injected: DynCredentials = Arc::new(InjectedRun::new());
        for refusal in [
            refusal(401, Some("unauthenticated"), None),
            refusal(403, Some("run_not_bound"), None),
            refusal(402, Some("over_limit"), None),
            refusal(429, Some("rate_limited"), Some(120)),
            refusal(503, Some("unavailable"), Some(90)),
            refusal(401, None, None),
        ] {
            assert_eq!(
                StaticBearer::new(SecretString::from("sk-test")).parks(&refusal),
                Park::No
            );
            assert_eq!(RenewingCredentials::new().parks(&refusal), Park::No);
            assert_eq!(shared.parks(&refusal), Park::No);
            assert_eq!(
                Arc::new(shared.clone()).parks(&refusal),
                Park::No,
                "an Arc of an Arc"
            );
            assert_eq!(
                injected.parks(&refusal),
                InjectedRun::new().parks(&refusal),
                "an Arc forwards"
            );
        }
        assert_ne!(
            injected.parks(&refusal(401, None, None)),
            Park::No,
            "the forwarding is not the default"
        );
    }

    #[tokio::test]
    async fn static_bearer_sends_the_key_and_an_empty_one_sends_nothing() {
        let set = StaticBearer::new(SecretString::from("sk-test"))
            .headers(call())
            .await
            .unwrap();
        let got: Vec<_> = set
            .iter()
            .map(|(n, v)| (n.to_owned(), v.expose_secret().to_owned()))
            .collect();
        assert_eq!(
            got,
            vec![("authorization".to_owned(), "Bearer sk-test".to_owned())]
        );
        assert!(
            StaticBearer::new(SecretString::from(""))
                .headers(call())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_fixed_key_gives_up_after_a_rejection() {
        let after = StaticBearer::new(SecretString::from("sk-test"))
            .rejected(
                call(),
                &Rejection {
                    status: 401,
                    ..Rejection::default()
                },
            )
            .await;
        assert_eq!(after, AfterRejection::GiveUp);
    }

    #[test]
    fn debug_never_shows_a_value() {
        let headers = CredentialHeaders::bearer(&SecretString::from("sk-secret"))
            .with("dpop", SecretString::from("proof-secret"));
        let shown = format!(
            "{headers:?} {:?}",
            StaticBearer::new(SecretString::from("sk-secret"))
        );
        assert!(!shown.contains("secret"), "{shown}");
        assert!(
            shown.contains("authorization") && shown.contains("dpop"),
            "{shown}"
        );
    }

    #[test]
    fn a_later_header_replaces_one_of_the_same_name() {
        let headers = CredentialHeaders::bearer(&SecretString::from("a"))
            .with("Authorization", SecretString::from("Bearer b"));
        let values: Vec<_> = headers
            .iter()
            .map(|(_, v)| v.expose_secret().to_owned())
            .collect();
        assert_eq!(values, vec!["Bearer b".to_owned()]);
    }

    #[tokio::test]
    async fn the_double_renews_after_each_rejection() {
        let c = RenewingCredentials::new();
        let first = c.headers(call()).await.unwrap();
        assert_eq!(
            first.iter().next().unwrap().1.expose_secret(),
            "Bearer token-1"
        );
        let rejection = Rejection {
            status: 401,
            ..Rejection::default()
        };
        assert_eq!(c.rejected(call(), &rejection).await, AfterRejection::Retry);
        let second = c.headers(call()).await.unwrap();
        assert_eq!(
            second.iter().next().unwrap().1.expose_secret(),
            "Bearer token-2"
        );
        assert_eq!(c.issued(), 2);
        assert_eq!(c.rejections(), vec![rejection]);
    }
}
