//! [`RemoteSubagentTool`]: the tool a parent calls to run a subagent that lives on another A2A
//! agent, and the bind-time checks that make it safe to build.
//!
//! The call is a journaled `SendMessage`; the parent then waits on the remote task and looks at it
//! with `GetTask` each time its wait timer fires ([`ToolError::AwaitRemote`] and
//! [`Tool::poll_remote`]). Everything that is a fact about the files or the deployment (the URL, the
//! token, the limits) is decided at [`RemoteSubagentTool::bind`]; the network is touched only when a
//! call needs it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use a2a::{
    A2AError, AgentCard, GetTaskRequest, Message, Part, PartContent, Role,
    SendMessageConfiguration, SendMessageRequest, SendMessageResponse, Task, TaskState, error_code,
};
use a2a_client::auth::AuthInterceptor;
use a2a_client::jsonrpc::JsonRpcTransportFactory;
use a2a_client::rest::RestTransportFactory;
use a2a_client::{A2AClient, A2AClientFactory, Transport};
use adam_agent_fs::{RemoteAgent, RemoteAuth};
use adam_llm_agent::{
    RemotePoll, StepIcon, StepKind, StepStyle, Tool, ToolCtx, ToolError, ToolOutput,
};
use adam_model::ToolSpec;
use adam_runtime::ReceivedFiles;
use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use tokio::sync::OnceCell;
use url::{Host, Url};

use crate::error::{Error, Origin, RemoteAuthProblem, RemoteUrlProblem};
use crate::subagent::{subagent_spec, title_of};

/// How long a parent waits for a remote task before it answers the call with an error result
/// (the default of [`AgentDef::remote_timeout`](crate::AgentDef::remote_timeout)).
pub(crate) const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(60 * 60);

/// The most bytes of a remote agent's answer that reach the model. What comes back is text a
/// stranger wrote, and it goes into a context window: it is cut here, on a character boundary,
/// with a note saying so.
const MAX_RESULT_BYTES: usize = 64 * 1024;

/// The biggest agent card that is read.
const MAX_CARD_BYTES: usize = 1024 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// What a deployment decides about remote subagents, given to
/// [`AgentDef`](crate::AgentDef) and used at bind.
#[derive(Debug, Clone)]
pub(crate) struct RemoteSettings {
    /// Values for environment variables, taken before the process environment.
    pub(crate) env: Arc<BTreeMap<String, SecretString>>,
    /// Allow `http` to hosts that are not this machine.
    pub(crate) allow_insecure: bool,
    /// How long a parent waits for a remote task.
    pub(crate) max_wait: Duration,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            env: Arc::default(),
            allow_insecure: false,
            max_wait: DEFAULT_MAX_WAIT,
        }
    }
}

impl RemoteSettings {
    /// The token of `bearer:var`: the value given in code, else the process environment. Trimmed,
    /// and refused when it could not be sent as a bearer token.
    fn token(&self, origin: &Origin, var: &str) -> Result<SecretString, Error> {
        let fail = |problem| Error::RemoteAuth {
            origin: origin.clone(),
            var: var.to_owned(),
            problem,
        };
        let raw = match self.env.get(var) {
            Some(value) => value.expose_secret().to_owned(),
            None => match std::env::var(var) {
                Ok(value) => value,
                Err(std::env::VarError::NotPresent) => {
                    return Err(fail(RemoteAuthProblem::Missing));
                }
                Err(std::env::VarError::NotUnicode(_)) => {
                    return Err(fail(RemoteAuthProblem::NotAToken));
                }
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

/// The tool that runs a subagent on another A2A agent, one per remote subagent, named after it.
///
/// `AgentDef::bind` adds one to the parent of every remote subagent (`a2a:` in the frontmatter) with
/// the same shape as [`SubagentTool`](crate::SubagentTool): input `{ "message": string }`, the
/// same description suffix, and no questions to the user. What differs is what a call does:
///
/// 1. one journaled `SendMessage` to the agent, with `returnImmediately`, whose message id is
///    derived from the parent's run and the tool call (`adam_runtime::child_run_id`), so a retry or
///    a replay sends the same id and a server that recognises a repeated message id (as
///    `adam-a2a-runtime` does) hands back the task it already made;
/// 2. an answer that is already final (a task in a final state, or a plain message) is the result;
///    otherwise the parent parks on the remote task id ([`ToolError::AwaitRemote`]) and looks at it
///    with `GetTask` each time its wait timer fires ([`Tool::poll_remote`]), each look a journaled
///    step, until the task is over or the wait passes the limit.
///
/// The client is built on the first call that needs it and kept: it fetches the agent card, picks
/// an interface, and adds `Authorization: Bearer <token>` to each request. Nothing here logs or
/// journals the token.
pub(crate) struct RemoteSubagentTool {
    spec: ToolSpec,
    /// The agent-card URL, as it can be shown.
    card_url: Url,
    /// The token, when the agent has `auth: bearer:VAR`.
    token: Option<SecretString>,
    allow_insecure: bool,
    max_wait: Duration,
    /// `files: true` in the file: the file parts of the answer are shared, not described.
    files: bool,
    client: OnceCell<A2AClient<Box<dyn Transport>>>,
}

impl std::fmt::Debug for RemoteSubagentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSubagentTool")
            .field("name", &self.spec.name)
            .field("card_url", &shown(&self.card_url))
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("connected", &self.client.initialized())
            .finish_non_exhaustive()
    }
}

impl RemoteSubagentTool {
    /// Check a remote subagent against the deployment's rules and build its tool. `origin` is the
    /// subagent (`coder/billing`) and its file.
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
            Some(RemoteAuth::Bearer { env }) => Some(settings.token(origin, env)?),
            None => None,
        };
        let description = [agent.description.trim(), agent.note.trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(Self {
            spec: subagent_spec(&agent.name, &description),
            card_url,
            token,
            allow_insecure: settings.allow_insecure,
            max_wait: settings.max_wait,
            files: agent.files,
            client: OnceCell::new(),
        })
    }

    fn name(&self) -> &str {
        &self.spec.name
    }

    /// The client, made on first use: the card is fetched, an interface that is safe to talk to is
    /// chosen, and the token is attached. A failure is not kept, so the next call tries again.
    async fn client(&self) -> Result<&A2AClient<Box<dyn Transport>>, ToolError> {
        self.client.get_or_try_init(|| self.dial()).await
    }

    async fn dial(&self) -> Result<A2AClient<Box<dyn Transport>>, ToolError> {
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
        Ok(client)
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
    fn failure(&self, action: &str, error: &A2AError) -> ToolError {
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

    /// Where a task stands, as the result of the call, or "still going".
    fn outcome(&self, task: &Task, files_left: u64) -> Outcome {
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
                let mut files = self.files.then(|| ReceivedFiles::new(name, files_left));
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
                    .map(|a| {
                        let one_part = a.parts.len() == 1;
                        let artifact_name = a.name.as_deref().filter(|_| one_part);
                        parts_text_sharing(&a.parts, files.as_mut(), artifact_name)
                    })
                    .filter(|text| !text.trim().is_empty())
                    .collect();
                // Files of the status message are shared too when nothing else answered.
                let text = if artifacts.is_empty() {
                    match (&task.status.message, files.as_mut()) {
                        (Some(message), Some(files)) => {
                            parts_text_sharing(&message.parts, Some(files), None)
                        }
                        _ => said,
                    }
                } else if artifacts_have_words || said.trim().is_empty() {
                    artifacts.join("\n\n")
                } else {
                    format!("{said}\n\n{}", artifacts.join("\n\n"))
                };
                let text = if text.trim().is_empty() {
                    "(the remote agent finished without output)".to_owned()
                } else {
                    cap_text(text, MAX_RESULT_BYTES)
                };
                Outcome::Done(with_files(text, files))
            }
            TaskState::Failed => Outcome::Done(ToolOutput::error(format!(
                "the remote agent `{name}` failed{}",
                with(&said)
            ))),
            TaskState::Canceled => Outcome::Done(ToolOutput::error(format!(
                "the remote task of `{name}` was canceled{}",
                with(&said)
            ))),
            TaskState::Rejected => Outcome::Done(ToolOutput::error(format!(
                "the remote agent `{name}` rejected the task{}",
                with(&said)
            ))),
            TaskState::InputRequired => Outcome::Done(ToolOutput::error(format!(
                "the remote agent `{name}` needs more input{}. A subagent cannot ask the user, so \
                 the task is over: call `{name}` again with a `message` that has everything the \
                 agent needs",
                with(&said)
            ))),
            TaskState::AuthRequired => Outcome::Done(ToolOutput::error(format!(
                "the remote agent `{name}` needs authorization{} that this deployment cannot give \
                 it: tell the user",
                with(&said)
            ))),
        }
    }
}

impl RemoteSubagentTool {
    /// A plain message that answered the call at once, as its result: its text, and with
    /// `files: true` its `raw` parts shared within `files_left` bytes.
    fn reply(&self, reply: &Message, files_left: u64) -> ToolOutput {
        let mut files = self
            .files
            .then(|| ReceivedFiles::new(self.name(), files_left));
        let text = match cap_text(
            parts_text_sharing(&reply.parts, files.as_mut(), None),
            MAX_RESULT_BYTES,
        ) {
            text if text.trim().is_empty() => "(the remote agent answered without text)".to_owned(),
            text => text,
        };
        with_files(text, files)
    }
}

/// A task's state, from the tool's side.
enum Outcome {
    Working,
    Done(ToolOutput),
}

/// The text of the parts of a message or an artifact, one per line. Data parts are their JSON; a
/// file is named, never included.
fn parts_text(parts: &[Part]) -> String {
    parts_text_sharing(parts, None, None)
}

/// [`parts_text`], except that with `files` (a remote subagent with `files: true`) a `raw` part is
/// shared as a file artifact of the calling run, its line saying so ([`ReceivedFiles`]): the
/// sender's filename and media type, checked against the bytes, and `artifact_name` when the part is
/// its artifact's only one. A file at a `url` is not fetched: it stays a line.
fn parts_text_sharing(
    parts: &[Part],
    mut files: Option<&mut ReceivedFiles>,
    artifact_name: Option<&str>,
) -> String {
    parts
        .iter()
        .map(|part| match &part.content {
            PartContent::Text(text) => text.clone(),
            PartContent::Data(value) => value.to_string(),
            PartContent::Raw(bytes) => match files.as_deref_mut() {
                // Every cap is checked on the length before the bytes are copied.
                Some(files) => match files.admit(part.media_type.as_deref(), bytes.len()) {
                    Ok(()) => files.share(
                        part.media_type.as_deref(),
                        part.filename.as_deref(),
                        artifact_name,
                        bytes.clone(),
                    ),
                    Err(line) => line,
                },
                None => format!(
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
            },
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

fn message_text(message: &Message) -> String {
    parts_text(&message.parts)
}

/// `text` as the result, with the files shared on the way; an error result when one was refused.
fn with_files(text: String, files: Option<ReceivedFiles>) -> ToolOutput {
    let (artifacts, refused) = files.map_or((Vec::new(), false), ReceivedFiles::into_parts);
    let mut output = if refused {
        ToolOutput::error(text)
    } else {
        ToolOutput::text(text)
    };
    output.artifacts = artifacts;
    output
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

#[async_trait]
impl Tool for RemoteSubagentTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    /// A call is an agent, on another system, working for this one: a `subagent` step drawn as an
    /// agent.
    fn step_style(&self) -> StepStyle {
        StepStyle::new(StepKind::Subagent)
            .with_label(title_of(&self.spec.name))
            .with_icon(StepIcon::Agent)
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let text = match args.get("message") {
            Some(Value::String(text)) if !text.trim().is_empty() => text,
            _ => {
                return Ok(ToolOutput::error(format!(
                    "`{}` needs `message`, a non-empty string with the whole task",
                    self.name()
                )));
            }
        };
        let client = self.client().await?;
        let mut message = Message::new(Role::User, vec![Part::text(text.as_str())]);
        // The same id for the same call of the same run, on every retry and replay: what lets a
        // server that recognises repeated messages hand back the task it already made.
        message.message_id = ctx.child_run_id().to_string();
        let request = SendMessageRequest {
            message,
            configuration: Some(SendMessageConfiguration {
                accepted_output_modes: None,
                task_push_notification_config: None,
                history_length: Some(0),
                return_immediately: Some(true),
            }),
            metadata: None,
            tenant: None,
        };
        let response = client
            .send_message(&request)
            .await
            .map_err(|e| self.failure("send the message to", &e))?;
        match response {
            SendMessageResponse::Message(reply) => Ok(self.reply(&reply, ctx.files_left())),
            SendMessageResponse::Task(task) => match self.outcome(&task, ctx.files_left()) {
                Outcome::Done(output) => Ok(output),
                Outcome::Working => {
                    tracing::debug!(tool = %self.name(), task = %task.id, "remote task started");
                    Err(ToolError::AwaitRemote {
                        task: task.id,
                        timeout_ms: Some(
                            u64::try_from(self.max_wait.as_millis()).unwrap_or(u64::MAX),
                        ),
                    })
                }
            },
        }
    }

    async fn poll_remote(&self, ctx: &ToolCtx, task: &str) -> Result<RemotePoll, ToolError> {
        let client = self.client().await?;
        let task = client
            .get_task(&GetTaskRequest {
                id: task.to_owned(),
                history_length: Some(0),
                tenant: None,
            })
            .await
            .map_err(|e| self.failure("look at the task on", &e))?;
        Ok(match self.outcome(&task, ctx.files_left()) {
            Outcome::Working => RemotePoll::Working,
            Outcome::Done(output) => RemotePoll::Ready(output),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN: u64 = adam_runtime::MAX_RUN_FILE_BYTES as u64;

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
            note: String::new(),
            path: "agent/subagents/billing.md".into(),
        }
    }

    fn settings(vars: &[(&str, &str)]) -> RemoteSettings {
        RemoteSettings {
            env: Arc::new(
                vars.iter()
                    .map(|(k, v)| ((*k).to_owned(), SecretString::from((*v).to_owned())))
                    .collect(),
            ),
            ..RemoteSettings::default()
        }
    }

    fn url_problem(url: &str, settings: &RemoteSettings) -> Option<RemoteUrlProblem> {
        match RemoteSubagentTool::bind(&origin(), &agent(url, None), settings) {
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
        let lax = RemoteSettings {
            allow_insecure: true,
            ..RemoteSettings::default()
        };
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
        let Err(Error::RemoteUrl { url, problem, .. }) = RemoteSubagentTool::bind(
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
        let tool = RemoteSubagentTool::bind(&origin(), &a, &good).unwrap();
        assert_eq!(
            tool.token.as_ref().unwrap().expose_secret(),
            "s3cret-token",
            "surrounding whitespace is trimmed"
        );
        let debug = format!("{tool:?}");
        assert!(!debug.contains("s3cret"), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");

        for (value, expected) in [
            ("", RemoteAuthProblem::Empty),
            ("   ", RemoteAuthProblem::Empty),
            ("two words", RemoteAuthProblem::NotAToken),
            ("tab\there", RemoteAuthProblem::NotAToken),
            ("caf\u{e9}", RemoteAuthProblem::NotAToken),
        ] {
            let error =
                RemoteSubagentTool::bind(&origin(), &a, &settings(&[("BILLING_TOKEN", value)]))
                    .unwrap_err();
            let Error::RemoteAuth { var, problem, .. } = &error else {
                panic!("{error}");
            };
            assert_eq!((var.as_str(), *problem), ("BILLING_TOKEN", expected));
            assert!(!error.to_string().contains(value.trim()) || value.trim().is_empty());
        }
        let missing = RemoteSubagentTool::bind(
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
    fn the_spec_has_the_shape_of_a_subagent_and_the_note_extends_the_description() {
        let mut a = agent("https://billing.example.com/card", None);
        a.note = "  Send invoice numbers.  ".into();
        let tool = RemoteSubagentTool::bind(&origin(), &a, &settings(&[])).unwrap();
        let spec = tool.spec();
        assert_eq!(spec.name, "billing");
        assert_eq!(
            spec.description,
            "Answers billing questions.\n\nSend invoice numbers. The agent does not see this \
             conversation; put everything it needs in `message`."
        );
        assert_eq!(spec.parameters["required"], serde_json::json!(["message"]));
        assert!(!tool.asks_user());
    }

    #[test]
    fn a_token_goes_only_to_the_origin_it_was_configured_for() {
        let a = agent("https://billing.example.com/card", Some("T"));
        let with = RemoteSubagentTool::bind(&origin(), &a, &settings(&[("T", "x")])).unwrap();
        assert!(with.interface_is_safe("https://billing.example.com/rpc"));
        assert!(!with.interface_is_safe("https://evil.example.net/rpc"));
        assert!(!with.interface_is_safe("https://billing.example.com:8443/rpc"));
        assert!(!with.interface_is_safe("http://billing.example.com/rpc"));
        assert!(!with.interface_is_safe("https://u:p@billing.example.com/rpc"));
        assert!(!with.interface_is_safe("not a url"));
        // Without a token the origin may differ, but not the transport rules.
        let without = RemoteSubagentTool::bind(
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
        let local = RemoteSubagentTool::bind(
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
        let lax = RemoteSettings {
            allow_insecure: true,
            ..RemoteSettings::default()
        };
        let https = RemoteSubagentTool::bind(
            &origin(),
            &agent("https://browser.example.com/card", None),
            &lax,
        )
        .unwrap();
        assert!(!https.interface_is_safe("http://browser.example.com/rpc"));
        assert!(!https.interface_is_safe("http://evil.example.net/rpc"));
        assert!(https.interface_is_safe("https://api.example.net/rpc"));
        let http = RemoteSubagentTool::bind(
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

    fn tool() -> RemoteSubagentTool {
        RemoteSubagentTool::bind(
            &origin(),
            &agent("https://billing.example.com/card", None),
            &settings(&[]),
        )
        .unwrap()
    }

    #[test]
    fn a_call_is_a_subagent_step_drawn_as_an_agent() {
        let style = tool().step_style();
        assert_eq!(style.kind, StepKind::Subagent);
        assert_eq!(style.icon, Some(StepIcon::Agent));
    }

    #[test]
    fn every_state_maps_to_a_result_or_to_more_waiting() {
        let tool = tool();
        for state in [
            TaskState::Unspecified,
            TaskState::Submitted,
            TaskState::Working,
        ] {
            assert!(matches!(
                tool.outcome(&task(state, None, &[]), RUN),
                Outcome::Working
            ));
        }
        let done = |t: Task| match tool.outcome(&t, RUN) {
            Outcome::Done(output) => output,
            Outcome::Working => panic!("still working"),
        };

        // The artifacts are the answer; the status message only when there are none.
        let out = done(task(TaskState::Completed, Some("Done."), &["one", "two"]));
        assert_eq!((out.content.as_str(), out.is_error), ("one\n\ntwo", false));
        let out = done(task(TaskState::Completed, Some("Done."), &[]));
        assert_eq!(out.content, "Done.");
        let out = done(task(TaskState::Completed, None, &[]));
        assert_eq!(out.content, "(the remote agent finished without output)");

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
            let out = done(task(state.clone(), Some("out of budget"), &[]));
            assert!(out.is_error, "{state:?}");
            assert!(out.content.contains(needle), "{state:?}: {}", out.content);
        }
        let out = done(task(TaskState::Failed, None, &[]));
        assert_eq!(out.content, "the remote agent `billing` failed");
        let out = done(task(TaskState::InputRequired, None, &[]));
        assert!(
            out.content.contains("cannot ask the user"),
            "{}",
            out.content
        );
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

    /// With `files: true`, the `raw` parts of a completed task are files of the calling run: the
    /// sender's filename and type (checked against the bytes), the artifact's name when the part is
    /// its only one, a line each for the model; a `url` part stays a line. Without it, nothing is kept.
    #[test]
    fn with_files_the_raw_parts_of_a_completed_task_are_shared() {
        const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        let mut shot = Part::raw(PNG.to_vec());
        shot.filename = Some("page.png".into());
        shot.media_type = Some("image/png".into());
        let mut pdf = Part::raw(b"%PDF-1.7".to_vec());
        pdf.media_type = Some("application/pdf".into());
        let artifact = |name: Option<&str>, parts: Vec<Part>| a2a::Artifact {
            artifact_id: a2a::new_artifact_id(),
            name: name.map(str::to_owned),
            description: None,
            parts,
            metadata: None,
            extensions: None,
        };
        let mut done = task(TaskState::Completed, Some("Done."), &[]);
        done.artifacts = Some(vec![
            artifact(Some("The page"), vec![shot]),
            artifact(
                Some("two parts"),
                vec![
                    Part::text("and a PDF:"),
                    pdf,
                    Part::url("https://x.example.com/f"),
                ],
            ),
        ]);

        let mut a = agent("https://billing.example.com/card", None);
        a.files = true;
        let sharing = RemoteSubagentTool::bind(&origin(), &a, &settings(&[])).unwrap();
        let Outcome::Done(out) = sharing.outcome(&done, RUN) else {
            panic!("the task is over");
        };
        assert!(!out.is_error, "{}", out.content);
        let names: Vec<(&str, &str)> = out
            .artifacts
            .iter()
            .map(|a| (a.name.as_str(), a.file.as_ref().unwrap().filename.as_str()))
            .collect();
        // The sender's base name and the extension of the checked type, made unique by a hash of
        // the bytes; the remote artifact's name stays the artifact's.
        let (page, pdf) = (names[0].1, names[1].1);
        assert_eq!(names[0].0, "The page");
        assert!(
            page.starts_with("page-") && page.ends_with(".png"),
            "{page}"
        );
        assert!(
            pdf.starts_with("billing-") && pdf.ends_with(".pdf"),
            "{pdf}"
        );
        assert_eq!(names[1].0, pdf);
        assert_eq!(
            out.content,
            format!(
                "Shared {page} (16 bytes, image/png). To show it in your answer, write \
                 ![description]({page}).\n\nand a PDF:\nShared {pdf} (8 bytes, application/pdf).\n\
                 [file at https://x.example.com/f]"
            )
        );
        assert_eq!(out.artifacts[0].file.as_ref().unwrap().bytes, PNG);

        // The same task to a subagent without the key: described, nothing kept.
        let Outcome::Done(plain) = tool().outcome(&done, RUN) else {
            panic!("the task is over");
        };
        assert!(plain.artifacts.is_empty());
        assert!(
            plain
                .content
                .starts_with("[file `page.png` not included: 16 bytes, image/png]")
        );

        // An adam agent's answer: its words in the status message, its screenshot the only
        // artifact. The words stay, then the file's line.
        let mut adam = task(TaskState::Completed, Some("The code is LH-7731."), &[]);
        let mut png = Part::raw(PNG.to_vec());
        png.filename = Some("shot.png".into());
        png.media_type = Some("image/png".into());
        adam.artifacts = Some(vec![artifact(None, vec![png])]);
        let Outcome::Done(out) = sharing.outcome(&adam, RUN) else {
            panic!("the task is over");
        };
        let shot = &out.artifacts[0].file.as_ref().unwrap().filename;
        assert!(
            out.content.starts_with("The code is LH-7731.\n\nShared ")
                && out.content.contains(shot.as_str()),
            "{}",
            out.content
        );
    }

    /// A remote that answers with a plain message, not a task: its file parts are shared the same
    /// way, within what the run may still share.
    #[test]
    fn with_files_the_raw_parts_of_a_message_reply_are_shared_too() {
        let mut shot = Part::raw(b"\x89PNG\r\n\x1a\nmore".to_vec());
        shot.filename = Some("evil.html".into());
        shot.media_type = Some("image/png".into());
        let reply = Message::new(Role::Agent, vec![Part::text("Here:"), shot.clone()]);
        let mut a = agent("https://billing.example.com/card", None);
        a.files = true;
        let sharing = RemoteSubagentTool::bind(&origin(), &a, &settings(&[])).unwrap();
        let out = sharing.reply(&reply, RUN);
        assert!(!out.is_error, "{}", out.content);
        let name = &out.artifacts[0].file.as_ref().unwrap().filename;
        // A real PNG keeps its base name and gets the extension of what it is.
        assert!(
            name.starts_with("evil-") && name.ends_with(".png"),
            "{name}"
        );
        assert!(
            out.content.starts_with("Here:\nShared evil-"),
            "{}",
            out.content
        );
        // With no room left, the file is refused before it is copied.
        let full = sharing.reply(&reply, 3);
        assert!(full.is_error && full.artifacts.is_empty());
        assert!(
            full.content
                .contains("is over what this run may still share"),
            "{}",
            full.content
        );
        // Without the key, described.
        let plain = tool().reply(&reply, RUN);
        assert!(plain.artifacts.is_empty());
        assert!(
            plain.content.contains("[file `evil.html` not included"),
            "{}",
            plain.content
        );
    }

    #[test]
    fn a_remote_file_over_the_cap_is_not_shared_and_the_result_says_so() {
        let mut big = Part::raw(vec![0; adam_runtime::MAX_ARTIFACT_FILE_BYTES + 1]);
        big.media_type = Some("application/zip".into());
        let mut done = task(TaskState::Completed, None, &[]);
        done.artifacts = Some(vec![a2a::Artifact {
            artifact_id: a2a::new_artifact_id(),
            name: None,
            description: None,
            parts: vec![big],
            metadata: None,
            extensions: None,
        }]);
        let mut a = agent("https://billing.example.com/card", None);
        a.files = true;
        let tool = RemoteSubagentTool::bind(&origin(), &a, &settings(&[])).unwrap();
        let Outcome::Done(out) = tool.outcome(&done, RUN) else {
            panic!("the task is over");
        };
        assert!(out.is_error);
        assert!(out.artifacts.is_empty());
        assert!(
            out.content
                .starts_with("Not shared: a file (application/zip) of 4194305 bytes is over"),
            "{}",
            out.content
        );
    }

    #[test]
    fn errors_from_the_remote_are_transient_only_when_trying_again_can_help() {
        let tool = tool();
        let internal = tool.failure("connect to", &A2AError::internal("HTTP request failed"));
        assert!(matches!(internal, ToolError::Transient(_)), "{internal:?}");
        for code in [
            error_code::TASK_NOT_FOUND,
            error_code::INVALID_PARAMS,
            -32000, // unauthorized
        ] {
            let error = tool.failure("send the message to", &A2AError::new(code, "no"));
            assert!(matches!(error, ToolError::Permanent(_)), "{code}");
        }
    }
}
