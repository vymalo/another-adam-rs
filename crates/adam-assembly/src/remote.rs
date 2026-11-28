//! [`RemoteEndpoint`]: the connection to a subagent that lives on another A2A agent, and the
//! bind-time checks that make it safe to build.
//!
//! The agent that works the rounds of a session on it
//! ([`RemoteRoundAgent`](crate::RemoteRoundAgent)) holds one: everything that is a fact about the
//! files or the deployment (the URL, the token) is decided at [`RemoteEndpoint::bind`], and the
//! network is touched only when a round needs it. There is no tool named after a remote subagent
//! ([ADR 0089] slice C4): a parent starts it with `subagent_start`.
//!
//! [ADR 0089]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0089-every-child-is-a-session-and-the-one-shot-subagent-tool-goes.md

use std::sync::Arc;
use std::time::Duration;

use a2a::{A2AError, AgentCard, Message, Part, PartContent, Task, TaskState, error_code};
use a2a_client::auth::AuthInterceptor;
use a2a_client::jsonrpc::JsonRpcTransportFactory;
use a2a_client::rest::RestTransportFactory;
use a2a_client::{A2AClient, A2AClientFactory, Transport};
use adam_agent_fs::{RemoteAgent, RemoteAuth};
use adam_core::SecretError;
use adam_llm_agent::ToolError;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::RwLock;
use url::{Host, Url};

use crate::def::{RemoteSettings, Secrets};
use crate::error::{Error, Origin, RemoteAuthProblem, RemoteUrlProblem};

/// The most bytes of a remote agent's answer that reach the model. What comes back is text a
/// stranger wrote, and it goes into a context window: it is cut here, on a character boundary,
/// with a note saying so.
const MAX_RESULT_BYTES: usize = 64 * 1024;

/// The biggest agent card that is read.
const MAX_CARD_BYTES: usize = 1024 * 1024;

/// `adam_a2a::STEER_EXTENSION`, which this crate does not depend on without its `a2a` feature (a
/// test holds the two equal).
pub(crate) const STEER_EXTENSION_URI: &str = "https://agents.vymalo.com/a2a/extensions/steer/v1";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The token of `bearer:var`: the value given in code, else the secret source. Trimmed, and
/// refused when it could not be sent as a bearer token.
fn token(secrets: &Secrets, origin: &Origin, var: &str) -> Result<SecretString, Error> {
    let fail = |problem| Error::RemoteAuth {
        origin: origin.clone(),
        var: var.to_owned(),
        problem,
    };
    let raw = match secrets.env.get(var) {
        Some(value) => value.expose_secret().to_owned(),
        None => match secrets.source.secret(var) {
            Ok(value) => value.expose_secret().to_owned(),
            Err(SecretError::Missing) => return Err(fail(RemoteAuthProblem::Missing)),
            Err(SecretError::NotUnicode) => return Err(fail(RemoteAuthProblem::NotAToken)),
            // `Refused`, and any reason added later: not given (fail closed).
            Err(_) => return Err(fail(RemoteAuthProblem::Refused)),
        },
    };
    let token = raw.trim();
    if token.is_empty() {
        return Err(fail(RemoteAuthProblem::Empty));
    }
    // A bearer token is printable ASCII (RFC 6750 `b64token`, and the opaque tokens that
    // follow it): anything else would be an invalid header value at the first call.
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(fail(RemoteAuthProblem::NotAToken));
    }
    Ok(SecretString::from(token.to_owned()))
}

/// Whether `url` points at this machine: `localhost`, a `*.localhost` name, or a loopback address.
fn is_local(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => {
            let name = name.to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

/// `url` without a user name, a password, a query or a fragment: what can be shown and logged.
fn shown(url: &Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

/// The A2A client of the agent: the connection and the rules about where it may go. The sessions on
/// one `a2a:` subagent share one.
///
/// The client is built on the first call that needs it and kept: it fetches the agent card, picks
/// an interface, and adds `Authorization: Bearer <token>` to each request. Nothing here logs or
/// journals the token.
pub(crate) struct RemoteEndpoint {
    name: String,
    /// The agent-card URL, as it can be shown.
    card_url: Url,
    /// The token, when the agent has `auth: bearer:VAR`.
    token: Option<SecretString>,
    allow_insecure: bool,
    client: RwLock<Option<Arc<Client>>>,
    /// One dial at a time, so a dial does not hold the lock the other sessions read the client under.
    dialing: tokio::sync::Mutex<()>,
}

pub(crate) type Client = A2AClient<Box<dyn Transport>>;

impl std::fmt::Debug for RemoteEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteEndpoint")
            .field("name", &self.name)
            .field("card_url", &shown(&self.card_url))
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field(
                "connected",
                &self.client.try_read().is_ok_and(|client| client.is_some()),
            )
            .finish_non_exhaustive()
    }
}

impl RemoteEndpoint {
    /// Check a remote subagent's URL and token against the deployment's rules.
    ///
    /// # Errors
    ///
    /// [`Error::RemoteUrl`] for a URL that is not http(s), carries a user name or password, or is
    /// plain `http` to a host that is not this machine (unless the settings allow it);
    /// [`Error::RemoteAuth`] for a token that cannot be had.
    pub(crate) fn bind(
        origin: &Origin,
        agent: &RemoteAgent,
        settings: &RemoteSettings,
        secrets: &Secrets,
    ) -> Result<Self, Error> {
        let refuse = |url: String, problem| Error::RemoteUrl {
            origin: origin.clone(),
            url,
            problem,
        };
        let Ok(card_url) = Url::parse(agent.url.trim()) else {
            return Err(refuse(
                "<not a URL>".to_owned(),
                RemoteUrlProblem::Unparseable,
            ));
        };
        let printable = shown(&card_url);
        if !matches!(card_url.scheme(), "http" | "https") || card_url.host().is_none() {
            return Err(refuse(printable, RemoteUrlProblem::NotHttp));
        }
        if !card_url.username().is_empty() || card_url.password().is_some() {
            return Err(refuse(printable, RemoteUrlProblem::Credentials));
        }
        if card_url.scheme() == "http" && !is_local(&card_url) && !settings.allow_insecure {
            return Err(refuse(printable, RemoteUrlProblem::Insecure));
        }
        let token = match &agent.auth {
            Some(RemoteAuth::Bearer { env }) => Some(token(secrets, origin, env)?),
            None => None,
        };
        Ok(Self {
            name: agent.name.clone(),
            card_url,
            token,
            allow_insecure: settings.allow_insecure,
            client: RwLock::new(None),
            dialing: tokio::sync::Mutex::new(()),
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The client, made on first use. A failure is not kept, so the next call tries again.
    pub(crate) async fn client(&self) -> Result<Arc<Client>, ToolError> {
        if let Some(client) = self.client.read().await.as_ref() {
            return Ok(client.clone());
        }
        let _dialing = self.dialing.lock().await;
        if let Some(client) = self.client.read().await.as_ref() {
            return Ok(client.clone());
        }
        let (client, _) = self.dial().await?;
        *self.client.write().await = Some(client.clone());
        Ok(client)
    }

    /// Fetch the card again and rebuild the client from it, so a card that changed is read. Says
    /// whether the card lists `steer/v1`. The network call holds no lock the readers need.
    pub(crate) async fn refresh(&self) -> Result<(Arc<Client>, bool), ToolError> {
        let _dialing = self.dialing.lock().await;
        let (client, steers) = self.dial().await?;
        *self.client.write().await = Some(client.clone());
        Ok((client, steers))
    }

    async fn dial(&self) -> Result<(Arc<Client>, bool), ToolError> {
        let http = reqwest::Client::builder()
            // A redirect could carry the request, and the token, somewhere nobody named.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("adam-assembly/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| {
                ToolError::Permanent(format!("cannot build an HTTP client: {}", e.without_url()))
            })?;
        let mut card = self.fetch_card(&http).await?;
        let steers = lists_steer(&card);
        let offered = card.supported_interfaces.len();
        card.supported_interfaces
            .retain(|interface| self.interface_is_safe(&interface.url));
        if card.supported_interfaces.is_empty() {
            return Err(ToolError::Permanent(format!(
                "the agent card of the remote agent `{}` ({}) offers no interface that can be used: \
                 {offered} offered, and each must be https (or local){}",
                self.name(),
                shown(&self.card_url),
                if self.token.is_some() {
                    " and on the same host and port as the card, so that the token goes only where \
                     it was configured"
                } else {
                    ""
                }
            )));
        }
        let mut factory = A2AClientFactory::builder()
            .no_defaults()
            .register(Arc::new(JsonRpcTransportFactory::new(Some(http.clone()))))
            .register(Arc::new(RestTransportFactory::new(Some(http))));
        if let Some(token) = &self.token {
            factory = factory.with_interceptor(Arc::new(AuthInterceptor::bearer(
                token.expose_secret().to_owned(),
            )));
        }
        let client = factory
            .build()
            .create_from_card(&card)
            .await
            .map_err(|e| self.failure("connect to", &e))?;
        tracing::debug!(tool = %self.name(), card = %shown(&self.card_url), "connected to a remote subagent");
        Ok((Arc::new(client), steers))
    }

    /// Whether the client may talk to the interface at `url`: no credentials in it; this machine only
    /// from a card on this machine (a remote card cannot aim calls at the caller's own loopback);
    /// https, or plain http to this machine, or (when the deployment allowed plain http) to the host
    /// of the card the operator configured, never to another; never plain http from an https card (a
    /// card cannot downgrade what the deployment chose); and, when a token is attached, the card's
    /// own origin.
    fn interface_is_safe(&self, url: &str) -> bool {
        let Ok(url) = Url::parse(url) else {
            return false;
        };
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return false;
        }
        if !url.username().is_empty() || url.password().is_some() {
            return false;
        }
        if is_local(&url) && !is_local(&self.card_url) {
            return false;
        }
        if url.scheme() == "http" && !is_local(&url) {
            let downgrade = self.card_url.scheme() == "https";
            let elsewhere = url.host() != self.card_url.host();
            if !self.allow_insecure || downgrade || elsewhere {
                return false;
            }
        }
        self.token.is_none() || url.origin() == self.card_url.origin()
    }

    /// Fetch the agent card, at most [`MAX_CARD_BYTES`] of it. The token goes with the request when
    /// there is one: the URL is the one the operator configured for it.
    async fn fetch_card(&self, http: &reqwest::Client) -> Result<AgentCard, ToolError> {
        let place = shown(&self.card_url);
        let mut request = http
            .get(self.card_url.clone())
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token.expose_secret());
        }
        let mut response = request.send().await.map_err(|e| {
            ToolError::Transient(format!(
                "cannot fetch the agent card of the remote agent `{}` from {place}: {}",
                self.name(),
                e.without_url()
            ))
        })?;
        let status = response.status();
        if !status.is_success() {
            let text = format!(
                "the agent card of the remote agent `{}` ({place}) answered HTTP {status}",
                self.name()
            );
            return Err(
                if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    ToolError::Transient(text)
                } else {
                    ToolError::Permanent(text)
                },
            );
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| {
            ToolError::Transient(format!(
                "cannot read the agent card from {place}: {}",
                e.without_url()
            ))
        })? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_CARD_BYTES {
                return Err(ToolError::Permanent(format!(
                    "the agent card at {place} is over {MAX_CARD_BYTES} bytes"
                )));
            }
        }
        serde_json::from_slice(&body).map_err(|e| {
            ToolError::Permanent(format!(
                "the agent card of the remote agent `{}` ({place}) is not a valid card: {e}",
                self.name()
            ))
        })
    }

    /// An A2A error as a tool error. A transport failure or a fault on the remote's side
    /// (JSON-RPC `internal error`) is worth trying again; anything the remote answered on purpose
    /// (unauthorized, task not found, invalid params) is not.
    pub(crate) fn failure(&self, action: &str, error: &A2AError) -> ToolError {
        let text = format!(
            "cannot {action} the remote agent `{}`: {}",
            self.name(),
            cap_text(error.message.clone(), 2_000)
        );
        if error.code == error_code::INTERNAL_ERROR {
            ToolError::Transient(text)
        } else {
            ToolError::Permanent(text)
        }
    }

    /// Where a task stands: its answer once it is over, or "still going". A file of the answer is
    /// described in a line, never shared.
    pub(crate) fn outcome(&self, task: &Task) -> Outcome {
        let said = task
            .status
            .message
            .as_ref()
            .map(message_text)
            .unwrap_or_default();
        let name = self.name();
        let with = |said: &str| {
            if said.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", cap_text(said.to_owned(), 4_000))
            }
        };
        match task.status.state {
            TaskState::Unspecified | TaskState::Submitted | TaskState::Working => Outcome::Working,
            TaskState::Completed => {
                // An adam agent answers in its status message and shares its files as artifacts:
                // artifacts with no words do not replace the words.
                let artifacts_have_words = task.artifacts.iter().flatten().any(|a| {
                    a.parts.iter().any(|p| match &p.content {
                        PartContent::Text(text) => !text.trim().is_empty(),
                        PartContent::Data(_) => true,
                        _ => false,
                    })
                });
                let artifacts: Vec<String> = task
                    .artifacts
                    .iter()
                    .flatten()
                    .map(|a| parts_text(&a.parts))
                    .filter(|text| !text.trim().is_empty())
                    .collect();
                let text = if artifacts.is_empty() {
                    said
                } else if artifacts_have_words || said.trim().is_empty() {
                    artifacts.join("\n\n")
                } else {
                    format!("{said}\n\n{}", artifacts.join("\n\n"))
                };
                Outcome::Completed(if text.trim().is_empty() {
                    "(the remote agent finished without output)".to_owned()
                } else {
                    cap_text(text, MAX_RESULT_BYTES)
                })
            }
            TaskState::Failed => {
                Outcome::Failed(format!("the remote agent `{name}` failed{}", with(&said)))
            }
            TaskState::Canceled => Outcome::Failed(format!(
                "the remote task of `{name}` was canceled{}",
                with(&said)
            )),
            TaskState::Rejected => Outcome::Failed(format!(
                "the remote agent `{name}` rejected the task{}",
                with(&said)
            )),
            TaskState::InputRequired => Outcome::Failed(format!(
                "the remote agent `{name}` needs more input{}",
                with(&said)
            )),
            TaskState::AuthRequired => Outcome::Failed(format!(
                "the remote agent `{name}` needs authorization{} that this deployment cannot give \
                 it: tell the user",
                with(&said)
            )),
        }
    }

    /// A plain message that answered a send at once, as text: its parts, a file described in a
    /// line.
    pub(crate) fn reply(reply: &Message) -> String {
        match cap_text(parts_text(&reply.parts), MAX_RESULT_BYTES) {
            text if text.trim().is_empty() => "(the remote agent answered without text)".to_owned(),
            text => text,
        }
    }
}

/// Whether the card lists `steer/v1`.
fn lists_steer(card: &AgentCard) -> bool {
    card.capabilities
        .extensions
        .iter()
        .flatten()
        .any(|extension| extension.uri == STEER_EXTENSION_URI)
}

/// A task's state, as a round reads it.
pub(crate) enum Outcome {
    Working,
    /// The task completed: the answer, as text.
    Completed(String),
    /// The task is over without an answer (failed, canceled, rejected, or waiting for what this
    /// deployment cannot give): what to say, as text.
    Failed(String),
}

/// The text of the parts of a message or an artifact, one per line. Data parts are their JSON; a
/// file is described in a line, never included: a `raw` part by its name, size and media type, a
/// `url` part by its address, which is never fetched.
fn parts_text(parts: &[Part]) -> String {
    parts
        .iter()
        .map(|part| match &part.content {
            PartContent::Text(text) => text.clone(),
            PartContent::Data(value) => value.to_string(),
            PartContent::Raw(bytes) => format!(
                "[file{} not included: {} bytes{}]",
                part.filename
                    .as_deref()
                    .map(|n| format!(" `{n}`"))
                    .unwrap_or_default(),
                bytes.len(),
                part.media_type
                    .as_deref()
                    .map(|m| format!(", {m}"))
                    .unwrap_or_default()
            ),
            PartContent::Url(url) => format!(
                "[file{} at {url}]",
                part.filename
                    .as_deref()
                    .map(|n| format!(" `{n}`"))
                    .unwrap_or_default()
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn message_text(message: &Message) -> String {
    parts_text(&message.parts)
}

/// `text`, cut to at most `max` bytes on a character boundary, with a note when it was cut.
fn cap_text(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let total = text.len();
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(&format!(
        "\n[cut here: {} of {total} bytes shown]",
        text.len()
    ));
    text
}

#[cfg(test)]
mod tests {
    use a2a::Role;

    use super::*;

    fn origin() -> Origin {
        Origin::new("coder/billing", "agent/subagents/billing.md")
    }

    fn agent(url: &str, auth: Option<&str>) -> RemoteAgent {
        RemoteAgent {
            name: "billing".into(),
            description: "Answers billing questions.".into(),
            url: url.into(),
            auth: auth.map(|env| RemoteAuth::Bearer { env: env.into() }),
            files: false,
            session: None,
            note: String::new(),
            path: "agent/subagents/billing.md".into(),
        }
    }

    /// What a deployment gives a remote subagent at bind: its rules and its secrets.
    #[derive(Default)]
    struct Given {
        settings: RemoteSettings,
        secrets: Secrets,
    }

    fn settings(vars: &[(&str, &str)]) -> Given {
        Given {
            secrets: Secrets {
                env: Arc::new(
                    vars.iter()
                        .map(|(k, v)| ((*k).to_owned(), SecretString::from((*v).to_owned())))
                        .collect(),
                ),
                ..Secrets::default()
            },
            ..Given::default()
        }
    }

    fn insecure() -> Given {
        Given {
            settings: RemoteSettings {
                allow_insecure: true,
                ..RemoteSettings::default()
            },
            ..Given::default()
        }
    }

    fn bind(origin: &Origin, agent: &RemoteAgent, given: &Given) -> Result<RemoteEndpoint, Error> {
        RemoteEndpoint::bind(origin, agent, &given.settings, &given.secrets)
    }

    fn url_problem(url: &str, settings: &Given) -> Option<RemoteUrlProblem> {
        match bind(&origin(), &agent(url, None), settings) {
            Err(Error::RemoteUrl { problem, .. }) => Some(problem),
            Ok(_) => None,
            Err(other) => panic!("{other}"),
        }
    }

    #[test]
    fn tls_is_required_unless_the_host_is_this_machine_or_the_deployment_says_otherwise() {
        let strict = settings(&[]);
        for ok in [
            "https://billing.example.com/.well-known/agent-card.json",
            "http://localhost:8080/card",
            "http://LOCALHOST/card",
            "http://agents.localhost/card",
            "http://127.0.0.1:9/card",
            "http://127.9.9.9/card",
            "http://[::1]:9/card",
        ] {
            assert_eq!(url_problem(ok, &strict), None, "{ok}");
        }
        for bad in [
            "http://billing.example.com/card",
            "http://10.0.0.5/card",
            "http://localhost.example.com/card",
            "http://notlocalhost/card",
            "http://[2001:db8::1]/card",
        ] {
            assert_eq!(
                url_problem(bad, &strict),
                Some(RemoteUrlProblem::Insecure),
                "{bad}"
            );
        }
        let lax = insecure();
        assert_eq!(url_problem("http://billing.example.com/card", &lax), None);
    }

    #[test]
    fn a_url_that_is_not_http_or_carries_credentials_is_refused_and_never_shown_with_them() {
        let s = settings(&[]);
        assert_eq!(
            url_problem("ftp://example.com/card", &s),
            Some(RemoteUrlProblem::NotHttp)
        );
        assert_eq!(
            url_problem("nonsense", &s),
            Some(RemoteUrlProblem::Unparseable)
        );
        let Err(Error::RemoteUrl { url, problem, .. }) = bind(
            &origin(),
            &agent("https://user:hunter2@example.com/card?key=abc#frag", None),
            &s,
        ) else {
            panic!("credentials in the URL must be refused");
        };
        assert_eq!(problem, RemoteUrlProblem::Credentials);
        assert_eq!(url, "https://example.com/card");
    }

    #[test]
    fn the_token_comes_from_the_settings_and_is_checked_without_ever_being_shown() {
        let a = agent("https://billing.example.com/card", Some("BILLING_TOKEN"));
        let good = settings(&[("BILLING_TOKEN", "  s3cret-token \n")]);
        let endpoint = bind(&origin(), &a, &good).unwrap();
        assert_eq!(
            endpoint.token.as_ref().unwrap().expose_secret(),
            "s3cret-token",
            "surrounding whitespace is trimmed"
        );
        let debug = format!("{endpoint:?}");
        assert!(!debug.contains("s3cret"), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");

        for (value, expected) in [
            ("", RemoteAuthProblem::Empty),
            ("   ", RemoteAuthProblem::Empty),
            ("two words", RemoteAuthProblem::NotAToken),
            ("tab\there", RemoteAuthProblem::NotAToken),
            ("caf\u{e9}", RemoteAuthProblem::NotAToken),
        ] {
            let error = bind(&origin(), &a, &settings(&[("BILLING_TOKEN", value)])).unwrap_err();
            let Error::RemoteAuth { var, problem, .. } = &error else {
                panic!("{error}");
            };
            assert_eq!((var.as_str(), *problem), ("BILLING_TOKEN", expected));
            assert!(!error.to_string().contains(value.trim()) || value.trim().is_empty());
        }
        let missing = bind(
            &origin(),
            &agent(
                "https://billing.example.com/card",
                Some("ADAM_ASSEMBLY_SURELY_UNSET_TOKEN"),
            ),
            &settings(&[]),
        )
        .unwrap_err();
        assert!(matches!(
            missing,
            Error::RemoteAuth {
                problem: RemoteAuthProblem::Missing,
                ..
            }
        ));
    }

    #[test]
    fn a_source_that_refuses_every_name_refuses_a_token_the_process_has() {
        // `PATH` is in every process environment, and a refusing source still does not give it.
        let a = agent("https://billing.example.com/card", Some("PATH"));
        let refusing = Given {
            secrets: Secrets {
                source: Arc::new(adam_core::NoSecrets),
                ..Secrets::default()
            },
            ..Given::default()
        };
        let error = bind(&origin(), &a, &refusing).unwrap_err();
        let Error::RemoteAuth { var, problem, .. } = &error else {
            panic!("{error}");
        };
        assert_eq!(
            (var.as_str(), *problem),
            ("PATH", RemoteAuthProblem::Refused)
        );
        assert!(
            error
                .to_string()
                .contains("this host gives no value for it"),
            "{error}"
        );
        // A value given in code is given: the source is asked only for the others.
        let mut given = settings(&[("PATH", "tok")]);
        given.secrets.source = Arc::new(adam_core::NoSecrets);
        let endpoint = bind(&origin(), &a, &given).unwrap();
        assert_eq!(endpoint.token.as_ref().unwrap().expose_secret(), "tok");
    }

    #[test]
    fn a_token_goes_only_to_the_origin_it_was_configured_for() {
        let a = agent("https://billing.example.com/card", Some("T"));
        let with = bind(&origin(), &a, &settings(&[("T", "x")])).unwrap();
        assert!(with.interface_is_safe("https://billing.example.com/rpc"));
        assert!(!with.interface_is_safe("https://evil.example.net/rpc"));
        assert!(!with.interface_is_safe("https://billing.example.com:8443/rpc"));
        assert!(!with.interface_is_safe("http://billing.example.com/rpc"));
        assert!(!with.interface_is_safe("https://u:p@billing.example.com/rpc"));
        assert!(!with.interface_is_safe("not a url"));
        // Without a token the origin may differ, but not the transport rules.
        let without = bind(
            &origin(),
            &agent("https://billing.example.com/card", None),
            &settings(&[]),
        )
        .unwrap();
        assert!(without.interface_is_safe("https://api.example.net/rpc"));
        assert!(!without.interface_is_safe("http://api.example.net/rpc"));
        // A remote card never points at this machine; a card on this machine may.
        assert!(!without.interface_is_safe("http://127.0.0.1:1/rpc"));
        assert!(!without.interface_is_safe("https://localhost/rpc"));
        assert!(!without.interface_is_safe("http://[::1]:1/rpc"));
        let local = bind(
            &origin(),
            &agent("http://127.0.0.1:8080/card", None),
            &settings(&[]),
        )
        .unwrap();
        assert!(local.interface_is_safe("http://127.0.0.1:9090/rpc"));
        assert!(local.interface_is_safe("http://localhost:9090/rpc"));
    }

    /// Plain http that the deployment allowed (`A2A_ALLOW_INSECURE_REMOTES`) is for the card's own
    /// host: an https card never gets a plain-http interface, and an http card never sends the
    /// client to plain http on another host, with or without a token.
    #[test]
    fn allowed_plain_http_is_never_a_downgrade_nor_another_host() {
        let lax = insecure();
        let https = bind(
            &origin(),
            &agent("https://browser.example.com/card", None),
            &lax,
        )
        .unwrap();
        assert!(!https.interface_is_safe("http://browser.example.com/rpc"));
        assert!(!https.interface_is_safe("http://evil.example.net/rpc"));
        assert!(https.interface_is_safe("https://api.example.net/rpc"));
        let http = bind(
            &origin(),
            &agent("http://browser.agents.svc:8080/card", None),
            &lax,
        )
        .unwrap();
        assert!(http.interface_is_safe("http://browser.agents.svc:8080/"));
        assert!(http.interface_is_safe("http://browser.agents.svc:9090/rpc"));
        assert!(!http.interface_is_safe("http://evil.example.net/rpc"));
        assert!(http.interface_is_safe("https://api.example.net/rpc"));
    }

    fn task(state: TaskState, status: Option<&str>, artifacts: &[&str]) -> Task {
        Task {
            id: "t-1".into(),
            context_id: "c-1".into(),
            status: a2a::TaskStatus {
                state,
                message: status.map(|t| Message::new(Role::Agent, vec![Part::text(t)])),
                timestamp: None,
            },
            artifacts: (!artifacts.is_empty()).then(|| {
                artifacts
                    .iter()
                    .map(|text| a2a::Artifact {
                        artifact_id: a2a::new_artifact_id(),
                        name: None,
                        description: None,
                        parts: vec![Part::text(*text)],
                        metadata: None,
                        extensions: None,
                    })
                    .collect()
            }),
            history: None,
            metadata: None,
        }
    }

    fn endpoint() -> RemoteEndpoint {
        bind(
            &origin(),
            &agent("https://billing.example.com/card", None),
            &settings(&[]),
        )
        .unwrap()
    }

    #[test]
    fn every_state_maps_to_an_answer_a_failure_or_more_waiting() {
        let endpoint = endpoint();
        for state in [
            TaskState::Unspecified,
            TaskState::Submitted,
            TaskState::Working,
        ] {
            assert!(matches!(
                endpoint.outcome(&task(state, None, &[])),
                Outcome::Working
            ));
        }
        let answer = |t: Task| match endpoint.outcome(&t) {
            Outcome::Completed(text) => text,
            Outcome::Failed(text) => panic!("failed: {text}"),
            Outcome::Working => panic!("still working"),
        };
        let failure = |t: Task| match endpoint.outcome(&t) {
            Outcome::Failed(text) => text,
            Outcome::Completed(text) => panic!("completed: {text}"),
            Outcome::Working => panic!("still working"),
        };

        // The artifacts are the answer; the status message only when there are none.
        let out = answer(task(TaskState::Completed, Some("Done."), &["one", "two"]));
        assert_eq!(out, "one\n\ntwo");
        let out = answer(task(TaskState::Completed, Some("Done."), &[]));
        assert_eq!(out, "Done.");
        let out = answer(task(TaskState::Completed, None, &[]));
        assert_eq!(out, "(the remote agent finished without output)");

        for (state, needle) in [
            (TaskState::Failed, "failed: out of budget"),
            (TaskState::Canceled, "was canceled: out of budget"),
            (TaskState::Rejected, "rejected the task: out of budget"),
            (TaskState::InputRequired, "needs more input: out of budget"),
            (
                TaskState::AuthRequired,
                "needs authorization: out of budget",
            ),
        ] {
            let out = failure(task(state.clone(), Some("out of budget"), &[]));
            assert!(out.contains(needle), "{state:?}: {out}");
        }
        let out = failure(task(TaskState::Failed, None, &[]));
        assert_eq!(out, "the remote agent `billing` failed");
    }

    #[test]
    fn a_long_answer_is_cut_on_a_character_boundary_with_a_note() {
        assert_eq!(cap_text("short".into(), 100), "short");
        let long = "\u{e9}".repeat(10); // two bytes each
        let cut = cap_text(long, 5);
        assert!(
            cut.starts_with("\u{e9}\u{e9}\n[cut here: 4 of 20 bytes shown]"),
            "{cut}"
        );
        let big = cap_text("a".repeat(MAX_RESULT_BYTES + 10), MAX_RESULT_BYTES);
        assert!(big.len() < MAX_RESULT_BYTES + 60);
        assert!(big.contains("[cut here:"));
    }

    #[test]
    fn parts_other_than_text_are_described_not_included() {
        let mut file = Part::raw(vec![0; 12]);
        file.filename = Some("report.pdf".into());
        file.media_type = Some("application/pdf".into());
        let parts = [
            Part::text("hello"),
            Part::data(serde_json::json!({"n": 2})),
            file,
            Part::url("https://files.example.com/x"),
        ];
        assert_eq!(
            parts_text(&parts),
            "hello\n{\"n\":2}\n[file `report.pdf` not included: 12 bytes, application/pdf]\n\
             [file at https://files.example.com/x]"
        );
    }

    /// A file of an answer, in a task's artifacts, beside the status message or in a plain message
    /// that answered at once, is a line for the model: nothing of it is kept (ADR 0089 slice C4:
    /// `files: true` is not honoured by a round).
    #[test]
    fn the_files_of_an_answer_are_described_and_never_kept() {
        const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        let mut shot = Part::raw(PNG.to_vec());
        shot.filename = Some("page.png".into());
        shot.media_type = Some("image/png".into());
        let mut done = task(TaskState::Completed, Some("The code is LH-7731."), &[]);
        done.artifacts = Some(vec![a2a::Artifact {
            artifact_id: a2a::new_artifact_id(),
            name: Some("The page".into()),
            description: None,
            parts: vec![shot.clone()],
            metadata: None,
            extensions: None,
        }]);
        let mut a = agent("https://billing.example.com/card", None);
        a.files = true;
        let asked = bind(&origin(), &a, &settings(&[])).unwrap();
        for endpoint in [endpoint(), asked] {
            let Outcome::Completed(out) = endpoint.outcome(&done) else {
                panic!("the task is over");
            };
            // An adam agent's answer: its words in the status message, its screenshot the only
            // artifact. The words stay, then the file's line.
            assert_eq!(
                out,
                "The code is LH-7731.\n\n[file `page.png` not included: 16 bytes, image/png]"
            );
        }
        let reply = Message::new(Role::Agent, vec![Part::text("Here:"), shot]);
        assert_eq!(
            RemoteEndpoint::reply(&reply),
            "Here:\n[file `page.png` not included: 16 bytes, image/png]"
        );
    }

    #[test]
    fn errors_from_the_remote_are_transient_only_when_trying_again_can_help() {
        let endpoint = endpoint();
        let internal = endpoint.failure("connect to", &A2AError::internal("HTTP request failed"));
        assert!(matches!(internal, ToolError::Transient(_)), "{internal:?}");
        for code in [
            error_code::TASK_NOT_FOUND,
            error_code::INVALID_PARAMS,
            -32000, // unauthorized
        ] {
            let error = endpoint.failure("send the message to", &A2AError::new(code, "no"));
            assert!(matches!(error, ToolError::Permanent(_)), "{code}");
        }
    }

    #[test]
    fn the_card_says_whether_a_working_task_takes_messages() {
        let mut card: AgentCard = serde_json::from_value(serde_json::json!({
            "name": "billing", "description": "d", "version": "1",
            "supportedInterfaces": [],
            "capabilities": {}, "defaultInputModes": [], "defaultOutputModes": [], "skills": []
        }))
        .unwrap();
        assert!(!lists_steer(&card));
        card.capabilities.extensions = Some(vec![a2a::AgentExtension {
            uri: STEER_EXTENSION_URI.to_owned(),
            description: None,
            required: None,
            params: None,
        }]);
        assert!(lists_steer(&card));
    }

    #[test]
    fn the_steer_uri_is_the_one_the_agents_declare() {
        assert_eq!(STEER_EXTENSION_URI, adam_a2a::STEER_EXTENSION);
    }
}
