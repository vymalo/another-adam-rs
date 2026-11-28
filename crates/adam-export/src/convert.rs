//! A run and its `Conversation` to a [`Record`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use adam_core::RunId;
use adam_core::store::{RunRecord, RunStatus};
use adam_core::time::Timestamp;
use adam_llm_agent::Conversation;
use adam_model::{ContentPart, Message, ToolSpec};

use crate::facets::facets_of;
use crate::scrub::scrub_known_shapes;
use crate::{
    ChatMessage, ChatToolCall, FunctionCall, FunctionDef, NoSecrets, Outcome, OutcomeArtifact,
    Pseudonyms, Record, RunArtifact, SCHEMA, Samples, Scrub, ToolDef, ToolsSource, TrainOn,
    checks_of, pull_requests_of,
};

/// What an image part becomes: no image bytes are exported.
pub const IMAGE_PLACEHOLDER: &str = "[image]";

/// The system prompt and tools an agent's model is sent, as the export passes them
/// ([`ToolsSource::Current`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestShape {
    /// The system prompt.
    pub system: Option<String>,
    /// The tools.
    pub tools: Vec<ToolSpec>,
}

/// How runs become records.
#[derive(Clone)]
pub struct ExportOptions {
    /// Removes the deployment's secrets from every string ([`Scrub`]); the well-known shapes are
    /// removed after it whatever it is.
    pub scrub: Arc<dyn Scrub>,
    /// Turns ids into pseudonyms.
    pub pseudonyms: Pseudonyms,
    /// Keep an assistant turn's reasoning, when the run kept it, as `reasoning_content`.
    pub include_reasoning: bool,
    /// The current prompt and tools of each agent, by name, for the runs that recorded none.
    pub current: BTreeMap<String, RequestShape>,
    /// One record per run ([`Samples::Run`], the default) or one per assistant turn
    /// ([`Samples::Turn`]) in [`export_runs`](crate::export_runs).
    pub samples: Samples,
    /// The most tokens a record may hold, by [`estimated_tokens`](crate::estimated_tokens): a
    /// record over it is left out and counted ([`Summary::over_budget`](crate::Summary::over_budget)),
    /// never cut. `None` for no bound.
    pub max_tokens: Option<u32>,
}

impl ExportOptions {
    /// Options with `pseudonyms`, no deployment secret ([`NoSecrets`]), no reasoning and no
    /// current tools.
    pub fn new(pseudonyms: Pseudonyms) -> Self {
        Self {
            scrub: Arc::new(NoSecrets),
            pseudonyms,
            include_reasoning: false,
            current: BTreeMap::new(),
            samples: Samples::Run,
            max_tokens: None,
        }
    }

    /// One record per run or per assistant turn (see [`samples`](Self::samples)).
    #[must_use]
    pub fn samples(mut self, samples: Samples) -> Self {
        self.samples = samples;
        self
    }

    /// Leave out the records over `max_tokens` estimated tokens (see
    /// [`max_tokens`](Self::max_tokens)).
    #[must_use]
    pub fn max_tokens(mut self, max_tokens: Option<u32>) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// With `scrub` for the deployment's secrets.
    #[must_use]
    pub fn scrub(mut self, scrub: impl Scrub + 'static) -> Self {
        self.scrub = Arc::new(scrub);
        self
    }

    /// Keep reasoning (see [`include_reasoning`](Self::include_reasoning)).
    #[must_use]
    pub fn include_reasoning(mut self, yes: bool) -> Self {
        self.include_reasoning = yes;
        self
    }

    /// With `shape` as the current prompt and tools of `agent`.
    #[must_use]
    pub fn current(mut self, agent: impl Into<String>, shape: RequestShape) -> Self {
        self.current.insert(agent.into(), shape);
        self
    }

    /// `text` without the deployment's secrets nor the well-known shapes.
    pub(crate) fn clean(&self, text: &str) -> String {
        let scrubbed = self.scrub.scrub(text);
        match scrub_known_shapes(&scrubbed) {
            std::borrow::Cow::Borrowed(_) => scrubbed,
            std::borrow::Cow::Owned(cleaned) => cleaned,
        }
    }
}

impl std::fmt::Debug for ExportOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportOptions")
            .field("pseudonyms", &self.pseudonyms)
            .field("include_reasoning", &self.include_reasoning)
            .field("current", &self.current.keys().collect::<Vec<_>>())
            .field("samples", &self.samples)
            .field("max_tokens", &self.max_tokens)
            .finish_non_exhaustive()
    }
}

/// A run's bookkeeping, without its state.
#[derive(Debug, Clone, PartialEq)]
pub struct RunMeta {
    /// The run.
    pub id: RunId,
    /// Its agent.
    pub agent: String,
    /// Its conversation.
    pub conversation_id: Option<String>,
    /// The run it works for.
    pub parent_id: Option<RunId>,
    /// Its status.
    pub status: RunStatus,
    /// When it was created.
    pub created_at: Timestamp,
    /// When it was last committed.
    pub updated_at: Timestamp,
    /// Why it failed.
    pub error: Option<String>,
    /// Its JSON artifacts, in the order its transitions committed them: what the outcome contract
    /// is read from ([`pull_requests_of`](crate::pull_requests_of), [`checks_of`](crate::checks_of)).
    /// Empty when the source has none, and then the outcome is the status, the error and the
    /// artifacts' names only.
    pub artifacts: Vec<RunArtifact>,
}

impl RunMeta {
    /// The bookkeeping of `record`, with the error its view reads, and no artifact.
    pub fn of(record: &RunRecord, error: Option<String>) -> Self {
        Self {
            id: record.id,
            agent: record.agent.clone(),
            conversation_id: record.conversation_id.clone(),
            parent_id: record.parent_id,
            status: record.status,
            created_at: record.created_at,
            updated_at: record.updated_at,
            error,
            artifacts: Vec::new(),
        }
    }

    /// With `artifacts` as the run's JSON artifacts (see [`artifacts`](Self::artifacts)).
    #[must_use]
    pub fn with_artifacts(mut self, artifacts: impl IntoIterator<Item = RunArtifact>) -> Self {
        self.artifacts = artifacts.into_iter().collect();
        self
    }
}

/// The record of `run`, whose agent state is `conversation`.
///
/// The conversation's `context`, `prompt_statement`, session header and caller are never read.
/// Every string is cleaned ([`ExportOptions::scrub`]); facets are read from the transcript before
/// it is cleaned, and are names, never values.
pub fn record_of(run: &RunMeta, conversation: &Conversation, options: &ExportOptions) -> Record {
    record_with(run, conversation, None, options)
}

/// [`record_of`], with the system prompt and tools the run's last model call was sent
/// (`Conversation::request`, read from the store): `recorded` wins over the agent's
/// [current](ExportOptions::current) ones, and the record says `tools_source: "recorded"`.
pub fn record_with(
    run: &RunMeta,
    conversation: &Conversation,
    recorded: Option<&RequestShape>,
    options: &ExportOptions,
) -> Record {
    let (tools_source, shape) = shape_for(run, recorded, options);
    let mut record = head_of(run, conversation, options);
    record.messages = Vec::with_capacity(conversation.messages.len() + 1);
    if let Some(system) = shape.and_then(|shape| shape.system.as_deref()) {
        record.messages.push(ChatMessage::System {
            content: options.clean(system),
        });
    }
    record.messages.extend(
        conversation
            .messages
            .iter()
            .map(|message| chat_message(message, options)),
    );
    record.tools = tools_of(shape, options);
    record.tools_source = tools_source;
    record
}

/// Which prompt and tools a record carries: `recorded` wins over the agent's
/// [current](ExportOptions::current) ones.
pub(crate) fn shape_for<'a>(
    run: &RunMeta,
    recorded: Option<&'a RequestShape>,
    options: &'a ExportOptions,
) -> (ToolsSource, Option<&'a RequestShape>) {
    match (recorded, options.current.get(&run.agent)) {
        (Some(shape), _) => (ToolsSource::Recorded, Some(shape)),
        (None, Some(shape)) => (ToolsSource::Current, Some(shape)),
        (None, None) => (ToolsSource::None, None),
    }
}

/// The tools of `shape`, cleaned; none without one.
pub(crate) fn tools_of(shape: Option<&RequestShape>, options: &ExportOptions) -> Vec<ToolDef> {
    shape
        .map(|shape| {
            shape
                .tools
                .iter()
                .map(|spec| tool_def(spec, options))
                .collect()
        })
        .unwrap_or_default()
}

/// Everything of a run's record but its messages and tools (empty here, `tools_source: none`):
/// what each of its records carries.
pub(crate) fn head_of(
    run: &RunMeta,
    conversation: &Conversation,
    options: &ExportOptions,
) -> Record {
    let pseudonyms = &options.pseudonyms;
    Record {
        schema: SCHEMA.to_owned(),
        id: pseudonyms.of("run", &run.id.to_string()),
        parent: run
            .parent_id
            .map(|parent| pseudonyms.of("run", &parent.to_string())),
        conversation: run
            .conversation_id
            .as_deref()
            .map(|conversation| pseudonyms.of("conversation", conversation)),
        // The exporter sets it from the source's label: the conversion does not know the source.
        source: None,
        agent: run.agent.clone(),
        status: run.status.as_str().to_owned(),
        created_at: run.created_at.to_string(),
        updated_at: run.updated_at.to_string(),
        turns: conversation.turns,
        tool_calls: conversation.tool_calls,
        usage: conversation.usage,
        models: models_of(conversation),
        compacted: is_compacted(conversation),
        facets: facets_of(&conversation.messages),
        outcome: Outcome {
            status: run.status.as_str().to_owned(),
            error: run.error.as_deref().map(|error| options.clean(error)),
            artifacts: conversation
                .artifacts
                .iter()
                .map(|artifact| OutcomeArtifact {
                    name: options.clean(&artifact.name),
                    mime_type: artifact.mime_type.clone(),
                })
                .collect(),
            pull_requests: pull_requests_of(&run.artifacts, pseudonyms),
            checks: checks_of(&run.artifacts),
        },
        tools_source: ToolsSource::None,
        train_on: TrainOn::Assistant,
        sample: None,
        messages: Vec::new(),
        tools: Vec::new(),
    }
}

/// Whether the history the record holds is not what the model saw at every turn.
fn is_compacted(conversation: &Conversation) -> bool {
    !conversation.compactions.is_empty()
        || !conversation.shortenings.is_empty()
        || conversation.omitted_turns > 0
        || conversation.summary.is_some()
}

/// The model aliases the assistant turns name, and the fallback's.
fn models_of(conversation: &Conversation) -> Vec<String> {
    let mut models: BTreeSet<String> = conversation
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant {
                provider: Some(provider),
                ..
            } if !provider.model.is_empty() => Some(provider.model.clone()),
            _ => None,
        })
        .collect();
    if let Some(switch) = &conversation.model_switch {
        models.insert(switch.from.clone());
        models.insert(switch.to.clone());
    }
    models.into_iter().collect()
}

pub(crate) fn chat_message(message: &Message, options: &ExportOptions) -> ChatMessage {
    match message {
        Message::User { content } => ChatMessage::User {
            content: options.clean(&text_of(content)),
        },
        Message::Assistant {
            content,
            tool_calls,
            reasoning,
            ..
        } => ChatMessage::Assistant {
            content: options.clean(&text_of(content)),
            tool_calls: tool_calls
                .iter()
                .map(|call| ChatToolCall {
                    id: call.id.clone(),
                    kind: "function".to_owned(),
                    function: FunctionCall {
                        name: call.name.clone(),
                        arguments: options.clean(&match &call.invalid_arguments {
                            Some(raw) => raw.clone(),
                            None => call.arguments.to_string(),
                        }),
                    },
                })
                .collect(),
            reasoning_content: reasoning
                .as_deref()
                .filter(|_| options.include_reasoning)
                .map(|reasoning| options.clean(reasoning)),
        },
        Message::Tool {
            call_id,
            content,
            images,
            ..
        } => {
            let mut text = options.clean(content);
            for _ in images {
                text.push('\n');
                text.push_str(IMAGE_PLACEHOLDER);
            }
            ChatMessage::Tool {
                tool_call_id: call_id.clone(),
                content: text,
            }
        }
    }
}

/// The text of a message's parts, an image as [`IMAGE_PLACEHOLDER`].
fn text_of(content: &[ContentPart]) -> String {
    content
        .iter()
        .map(|part| match part {
            ContentPart::Text { .. } => part.as_text().to_owned(),
            ContentPart::Image(_) => IMAGE_PLACEHOLDER.to_owned(),
        })
        .collect()
}

fn tool_def(spec: &ToolSpec, options: &ExportOptions) -> ToolDef {
    ToolDef {
        kind: "function".to_owned(),
        function: FunctionDef {
            name: spec.name.clone(),
            description: options.clean(&spec.description),
            parameters: spec.parameters.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use adam_llm_agent::{ArtifactRef, ModelSwitch};
    use adam_model::{ImageOrigin, ImageRef, ProviderTurn, ToolCall, Usage};
    use serde_json::json;

    use super::*;

    fn image() -> ImageRef {
        ImageRef {
            sha256: "0".repeat(64),
            media_type: "image/png".to_owned(),
            bytes: 10,
            width: 1,
            height: 1,
            name: Some("shot.png".to_owned()),
            origin: ImageOrigin::Person,
        }
    }

    fn meta(status: RunStatus) -> RunMeta {
        let at = Timestamp::from_second(1_800_000_000).expect("an instant");
        RunMeta {
            id: RunId::new(),
            agent: "coder".to_owned(),
            conversation_id: Some("conv-1".to_owned()),
            parent_id: Some(RunId::new()),
            status,
            created_at: at,
            updated_at: at,
            error: None,
            artifacts: Vec::new(),
        }
    }

    fn assistant(
        text: &str,
        calls: Vec<ToolCall>,
        reasoning: Option<&str>,
        model: Option<&str>,
    ) -> Message {
        Message::Assistant {
            content: vec![ContentPart::text(text)],
            tool_calls: calls,
            reasoning: reasoning.map(str::to_owned),
            provider: model.map(|model| ProviderTurn::new(model, "endpoint")),
            extra_content: None,
        }
    }

    fn conversation() -> Conversation {
        Conversation {
            messages: vec![
                Message::user_with_images("look at this", vec![image()]),
                assistant(
                    "",
                    vec![
                        ToolCall::new("c1", "read_file", json!({"path": "Cargo.toml"})),
                        ToolCall::with_invalid_arguments("c2", "run", "{not json"),
                    ],
                    Some("I should read the manifest"),
                    Some("small"),
                ),
                Message::tool_result_with_images(
                    "c1",
                    "[dependencies]\naxum = \"0.8\"",
                    vec![image()],
                ),
                Message::tool_error("c2", "arguments were not valid JSON"),
                assistant("done", Vec::new(), None, Some("big")),
            ],
            turns: 2,
            tool_calls: 2,
            usage: Usage::new(10, 5),
            artifacts: vec![ArtifactRef {
                name: "pull_request".to_owned(),
                mime_type: Some("application/json".to_owned()),
                id: None,
                bytes: None,
            }],
            ..Conversation::default()
        }
    }

    fn options() -> ExportOptions {
        ExportOptions::new(Pseudonyms::new(b"key"))
    }

    #[test]
    fn a_conversation_becomes_chat_messages() {
        let run = meta(RunStatus::Done);
        let record = record_of(&run, &conversation(), &options());
        assert_eq!(record.schema, SCHEMA);
        assert_eq!(
            record.id,
            options().pseudonyms.of("run", &run.id.to_string())
        );
        assert_ne!(record.id, run.id.to_string());
        assert!(record.parent.is_some());
        assert_eq!(
            record.conversation,
            Some(options().pseudonyms.of("conversation", "conv-1"))
        );
        assert_eq!(record.status, "done");
        assert_eq!(record.created_at, "2027-01-15T08:00:00Z");
        assert_eq!((record.turns, record.tool_calls), (2, 2));
        assert_eq!(record.usage, Usage::new(10, 5));
        assert_eq!(record.tools_source, ToolsSource::None);
        assert!(record.tools.is_empty());
        assert!(!record.compacted);
        assert!(record.facets.has(crate::facets::DEP, "axum"));
        assert!(record.facets.has(crate::facets::LANG, "toml"));
        assert_eq!(record.outcome.artifacts[0].name, "pull_request");

        let [user, first, read, failed, last] = record.messages.as_slice() else {
            panic!("five messages: {:?}", record.messages);
        };
        assert_eq!(
            user,
            &ChatMessage::User {
                content: format!("look at this{IMAGE_PLACEHOLDER}")
            }
        );
        let ChatMessage::Assistant {
            content,
            tool_calls,
            reasoning_content,
        } = first
        else {
            panic!("an assistant turn: {first:?}");
        };
        assert_eq!(content, "");
        assert_eq!(reasoning_content, &None);
        assert_eq!(tool_calls[0].kind, "function");
        assert_eq!(tool_calls[0].function.name, "read_file");
        assert_eq!(tool_calls[0].function.arguments, r#"{"path":"Cargo.toml"}"#);
        // The model's own text, not the marker the state holds in its place.
        assert_eq!(tool_calls[1].function.arguments, "{not json");
        assert_eq!(
            read,
            &ChatMessage::Tool {
                tool_call_id: "c1".to_owned(),
                content: format!("[dependencies]\naxum = \"0.8\"\n{IMAGE_PLACEHOLDER}"),
            }
        );
        assert!(matches!(failed, ChatMessage::Tool { tool_call_id, .. } if tool_call_id == "c2"));
        assert!(matches!(last, ChatMessage::Assistant { content, .. } if content == "done"));
    }

    #[test]
    fn reasoning_is_kept_only_when_asked() {
        let record = record_of(
            &meta(RunStatus::Done),
            &conversation(),
            &options().include_reasoning(true),
        );
        assert!(matches!(
            &record.messages[1],
            ChatMessage::Assistant { reasoning_content: Some(r), .. } if r == "I should read the manifest"
        ));
    }

    #[test]
    fn models_are_the_turns_and_the_fallback() {
        let mut conversation = conversation();
        assert_eq!(models_of(&conversation), ["big", "small"]);
        conversation.model_switch = Some(ModelSwitch {
            from: "big".to_owned(),
            to: "fallback".to_owned(),
            turn: 1,
        });
        assert_eq!(models_of(&conversation), ["big", "fallback", "small"]);
    }

    #[test]
    fn a_summary_marks_the_record_compacted() {
        let mut conversation = conversation();
        conversation.summary = Some("earlier, the run read the manifest".to_owned());
        assert!(record_of(&meta(RunStatus::Done), &conversation, &options()).compacted);
        let mut shortened = self::conversation();
        shortened.shortenings = vec![1];
        assert!(record_of(&meta(RunStatus::Done), &shortened, &options()).compacted);
    }

    #[test]
    fn the_current_shape_adds_a_system_message_and_tools() {
        let shape = RequestShape {
            system: Some(
                "you are a coder; token ghp_0123456789abcdefghijklmnopqrstuvwxyz".to_owned(),
            ),
            tools: vec![ToolSpec {
                name: "read_file".to_owned(),
                description: "read a file".to_owned(),
                parameters: json!({"type": "object"}),
            }],
        };
        let record = record_of(
            &meta(RunStatus::Done),
            &conversation(),
            &options().current("coder", shape),
        );
        assert_eq!(record.tools_source, ToolsSource::Current);
        let ChatMessage::System { content } = &record.messages[0] else {
            panic!("a system message first: {:?}", record.messages[0]);
        };
        assert!(content.starts_with("you are a coder") && !content.contains("ghp_"));
        assert_eq!(record.tools[0].kind, "function");
        assert_eq!(record.tools[0].function.name, "read_file");
    }

    #[test]
    fn a_failed_run_keeps_its_error_scrubbed() {
        let mut run = meta(RunStatus::Failed);
        run.error = Some("push refused for s3cret".to_owned());
        let options = options().scrub(|text: &str| text.replace("s3cret", "[gone]"));
        let record = record_of(&run, &conversation(), &options);
        assert_eq!(record.outcome.status, "failed");
        assert_eq!(
            record.outcome.error.as_deref(),
            Some("push refused for [gone]")
        );
    }

    #[test]
    fn the_outcome_contract_adds_the_pull_requests_and_the_checks() {
        let repository = "https://git.example.internal/someone/secret-project.git";
        let run = meta(RunStatus::Done).with_artifacts([
            RunArtifact::json(crate::CHECKS_ARTIFACT, json!({"passed": false})),
            RunArtifact::json(crate::CHECKS_ARTIFACT, json!({"passed": true})),
            RunArtifact::json(
                crate::PULL_REQUEST_ARTIFACT,
                json!({
                    "url": "https://git.example.internal/someone/secret-project/pull/12",
                    "number": "12",
                    "repository": repository,
                    "verified": true,
                }),
            ),
        ]);
        let record = record_of(&run, &conversation(), &options());
        assert_eq!(
            record.outcome.pull_requests,
            [crate::PullRequestOutcome {
                number: 12,
                repository: Some(options().pseudonyms.of("repository", repository)),
                verified: true,
            }]
        );
        assert_eq!(
            record.outcome.checks,
            Some(crate::ChecksOutcome {
                passed: true,
                reports: 2,
                failed: 1,
            })
        );
        assert!(record.outcome.verified());
        let line = serde_json::to_string(&record.outcome).expect("serializes");
        assert!(
            !line.contains("example.internal") && !line.contains("someone"),
            "neither the host nor the owner is written: {line}"
        );
        assert!(
            line.contains(r#""checks":{"passed":true,"reports":2,"failed":1}"#),
            "{line}"
        );
    }

    #[test]
    fn verified_only_keeps_a_run_whose_every_pull_request_was_verified() {
        let pull_request = |number: &str, verified: bool| {
            RunArtifact::json(
                crate::PULL_REQUEST_ARTIFACT,
                json!({"number": number, "repository": number, "verified": verified}),
            )
        };
        let record = |artifacts: Vec<RunArtifact>| {
            record_of(
                &meta(RunStatus::Done).with_artifacts(artifacts),
                &conversation(),
                &options(),
            )
        };
        let filter = crate::Filter::default().verified_only(true);
        assert!(filter.accepts(&record(vec![pull_request("1", true)])));
        assert!(filter.accepts(&record(vec![
            pull_request("1", false),
            pull_request("1", true),
        ])));
        for refused in [
            Vec::new(),
            vec![pull_request("1", false)],
            vec![pull_request("1", true), pull_request("2", false)],
            vec![RunArtifact::json(
                crate::CHECKS_ARTIFACT,
                json!({"passed": true}),
            )],
        ] {
            let record = record(refused);
            assert!(!filter.accepts(&record), "{:?}", record.outcome);
            assert!(crate::Filter::default().accepts(&record));
        }
    }

    /// The bytes of a record as the exporter wrote them before records could name their source
    /// (`Record::source`): a record with no source is the same line, so one unnamed source's export
    /// is byte-identical to the one before the ports (ADR 0085, *Amended*: ports and adapters).
    #[test]
    fn a_record_without_a_source_is_the_line_it_was() {
        let id = |text: &str| -> RunId { serde_json::from_value(json!(text)).expect("a run id") };
        let run = RunMeta {
            id: id("0195f0aa-0000-7000-8000-000000000001"),
            parent_id: Some(id("0195f0aa-0000-7000-8000-000000000002")),
            ..meta(RunStatus::Done)
        };
        let line = serde_json::to_string(&record_of(&run, &conversation(), &options()))
            .expect("a record serializes");
        assert_eq!(line, GOLDEN_LINE);
        // Nor does the outcome contract (slice E4) change the line of a run that reported no
        // pull request and no checks, whatever else its artifacts are.
        let branch = run.with_artifacts([RunArtifact::json(
            "branch",
            json!({"repository": "https://example.com/r.git", "branch": "agent/x"}),
        )]);
        let line = serde_json::to_string(&record_of(&branch, &conversation(), &options()))
            .expect("a record serializes");
        assert_eq!(line, GOLDEN_LINE);
    }

    /// Written by the exporter before `Record::source` existed (captured on 2026-11-27).
    const GOLDEN_LINE: &str = r##"{"schema":"adam.export/v1","id":"a65751c74b4df58489a4976ad73d8de1","parent":"8b2f43d9e12814ecd296fafc9672f138","conversation":"425258b40f480f116276830a75727003","agent":"coder","status":"done","created_at":"2027-01-15T08:00:00Z","updated_at":"2027-01-15T08:00:00Z","turns":2,"tool_calls":2,"usage":{"input_tokens":10,"output_tokens":5},"models":["big","small"],"compacted":false,"facets":{"dep":["axum"],"lang":["toml"]},"outcome":{"status":"done","artifacts":[{"name":"pull_request","mime_type":"application/json"}]},"tools_source":"none","train_on":"assistant","messages":[{"role":"user","content":"look at this[image]"},{"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"Cargo.toml\"}"}},{"id":"c2","type":"function","function":{"name":"run","arguments":"{not json"}}]},{"role":"tool","tool_call_id":"c1","content":"[dependencies]\naxum = \"0.8\"\n[image]"},{"role":"tool","tool_call_id":"c2","content":"arguments were not valid JSON"},{"role":"assistant","content":"done"}]}"##;
}
