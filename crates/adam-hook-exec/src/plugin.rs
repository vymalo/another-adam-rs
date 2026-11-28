//! The adapter itself: a [`Plugin`] whose hooks are asked of a program (ADR 0050, decision 15).

use std::collections::BTreeMap;
use std::sync::Arc;

use adam_error::{Classify, ErrorClass};
use adam_plugin::{
    Decision, HookError, HookEvent, HookResult, HookedCall, HookedCompaction, HookedEnd,
    HookedMessage, HookedModelRequest, HookedText, ModelCallKind, Plugin, Subscriptions,
};
use serde_json::{Value, json};

use crate::config::{Config, DecidePoint, Points, Scrub, scrub_value};
use crate::error::Error;
use crate::program::{Program, Spec};
use crate::protocol::{Mode, Reply, Word};

/// A program on stdio, seen as one [`Plugin`].
///
/// Every hook the program is configured at becomes one `event` request, answered within the
/// configured deadline; every event it subscribes to becomes one `observe` request. The program
/// gets the payloads scrubbed, no run context and only the safe variables of the environment. A
/// program that cannot decide never lets a call through: a missed deadline is `Transient`, an
/// answer the point does not allow is `Invalid`, and three misses in a row kill it and restart it
/// with backoff.
pub struct HookExec {
    name: String,
    points: Points,
    scrub: Arc<Scrub>,
    subscriptions: Subscriptions,
    program: Program,
}

impl HookExec {
    /// Start the program and say hello.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for an unusable configuration, [`Error::Spawn`] when the program cannot
    /// start, [`Error::Hello`] when it does not answer the hello in time, and [`Error::Protocol`]
    /// when it names another protocol. A binary reports these as exit 78 (ADR 0050, decision 15).
    pub fn start(config: Config) -> Result<Self, Error> {
        config.validate()?;
        let subscriptions = config.subscriptions()?;
        let spec = Arc::new(Spec {
            name: config.name.clone(),
            program: config.program.clone(),
            args: config.args.clone(),
            env: config.env.clone(),
            points: config.points.wire(),
            scrub: Arc::clone(&config.scrub),
        });
        let program = Program::start(spec, config.deadline, config.cwd.clone())?;
        Ok(HookExec {
            name: config.name,
            points: config.points,
            scrub: config.scrub,
            subscriptions,
            program,
        })
    }

    /// Ask the program one question, and turn its failure into the hook's. The payload is
    /// scrubbed whole, and a reply that asks for an action this build cannot perform is refused.
    fn ask(&self, point: DecidePoint, payload: Value) -> Result<Reply, HookError> {
        let payload = self.scrub_value(&payload);
        let reply = self
            .program
            .request(Mode::Decide, None, point.as_str(), payload)
            .map_err(|e| HookError::new(self.name.clone(), e.class(), e.to_string()))?;
        self.check(&reply)?;
        Ok(reply)
    }

    /// Refuse a reply this build cannot honour: an action is slice H1d, which is not built.
    fn check(&self, reply: &Reply) -> Result<(), HookError> {
        if reply.actions.is_empty() {
            return Ok(());
        }
        let error = Error::Action {
            program: self.name.clone(),
        };
        Err(HookError::new(
            self.name.clone(),
            error.class(),
            error.to_string(),
        ))
    }

    /// The reason a verdict carries, scrubbed: it is recorded and read by the model.
    fn reason(&self, reply: &Reply) -> String {
        self.scrub(&reply.reason())
    }

    /// A failure of this adapter's own making: an answer the point does not allow.
    fn refuse(&self, what: impl Into<String>) -> HookError {
        HookError::new(self.name.clone(), ErrorClass::Invalid, what.into())
    }

    /// `text` with the deployment's secrets replaced.
    fn scrub(&self, text: &str) -> String {
        (self.scrub.as_ref())(text)
    }

    /// `value` with every string inside it replaced.
    fn scrub_value(&self, value: &Value) -> Value {
        scrub_value(value, self.scrub.as_ref())
    }

    /// A map of strings with every key and value scrubbed.
    fn scrub_map(&self, map: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        map.iter()
            .map(|(key, value)| (self.scrub(key), self.scrub(value)))
            .collect()
    }

    /// The files of a message, by reference, as the protocol carries them.
    fn files(files: &[adam_plugin::MessageFile]) -> Value {
        Value::Array(
            files
                .iter()
                .map(|file| {
                    json!({
                        "name": file.name,
                        "media_type": file.media_type,
                        "size": file.size,
                        "reference": file.reference,
                    })
                })
                .collect(),
        )
    }
}

impl Plugin for HookExec {
    fn name(&self) -> &str {
        &self.name
    }

    fn subscriptions(&self) -> Subscriptions {
        self.subscriptions.clone()
    }

    fn message_received(&self, message: &mut HookedMessage<'_>) -> HookResult {
        if !self.points.decides(DecidePoint::MessageReceived) {
            return Ok(Decision::Continue);
        }
        let clean = self.scrub(message.text);
        let reply = self.ask(
            DecidePoint::MessageReceived,
            json!({"text": clean, "files": Self::files(message.files)}),
        )?;
        match reply.verdict {
            Word::Continue => Ok(Decision::Continue),
            Word::Deny => Ok(Decision::Deny(self.reason(&reply))),
            Word::Ask => Err(self
                .refuse("ask at message.received, where only allow, rewrite and deny are allowed")),
            Word::Stop => Err(self.refuse(
                "stop at message.received, where only allow, rewrite and deny are allowed",
            )),
            Word::Rewrite => {
                let Some(text) = reply.text else {
                    return Err(self.refuse("a rewrite without `text`"));
                };
                if clean != *message.text {
                    return Err(self.refuse(
                        "a rewrite over a scrubbed message is refused: the text holds a secret",
                    ));
                }
                *message.text = text;
                Ok(Decision::Continue)
            }
        }
    }

    fn run_ending(&self, end: &HookedEnd<'_>) -> HookResult {
        if !self.points.decides(DecidePoint::RunEnding) {
            return Ok(Decision::Continue);
        }
        let reply = self.ask(
            DecidePoint::RunEnding,
            json!({"text": self.scrub(end.text), "continuations": end.continuations}),
        )?;
        match reply.verdict {
            Word::Continue => Ok(Decision::Continue),
            Word::Deny => Ok(Decision::Deny(self.reason(&reply))),
            Word::Stop => Ok(Decision::Stop(self.reason(&reply))),
            Word::Ask => Err(self.refuse(
                "ask at run.ending, where only allow, deny (keep going) and stop are allowed",
            )),
            Word::Rewrite => {
                Err(self.refuse("rewrite at run.ending: an answer is never rewritten"))
            }
        }
    }

    fn model_request(&self, request: &mut HookedModelRequest<'_>) -> HookResult {
        if !self.points.decides(DecidePoint::ModelRequest) {
            return Ok(Decision::Continue);
        }
        let metadata = self.scrub_map(request.metadata);
        let headers = self.scrub_map(request.headers);
        let extra_body = request
            .extra_body
            .as_ref()
            .map(|body| self.scrub_value(&Value::Object(body.clone())));
        let original_body = request
            .extra_body
            .as_ref()
            .map(|body| Value::Object(body.clone()));
        let reply = self.ask(
            DecidePoint::ModelRequest,
            json!({
                "kind": match request.kind {
                    ModelCallKind::Turn => "turn",
                    ModelCallKind::Compaction => "compaction",
                },
                "temperature": request.temperature,
                "top_p": request.top_p,
                "max_output_tokens": request.max_output_tokens,
                "metadata": metadata,
                "headers": headers,
                "extra_body": extra_body,
            }),
        )?;
        match reply.verdict {
            Word::Continue => Ok(Decision::Continue),
            Word::Stop => Ok(Decision::Stop(self.reason(&reply))),
            Word::Deny | Word::Ask => Err(self.refuse(
                "deny or ask at model.request, where only allow, rewrite and stop are allowed",
            )),
            Word::Rewrite => {
                if let Some(value) = reply.temperature {
                    *request.temperature = Some(value);
                }
                if let Some(value) = reply.top_p {
                    *request.top_p = Some(value);
                }
                if let Some(value) = reply.max_output_tokens {
                    *request.max_output_tokens = Some(value);
                }
                if let Some(value) = reply.metadata {
                    if metadata != *request.metadata {
                        return Err(self.refuse("a rewrite over scrubbed metadata is refused"));
                    }
                    *request.metadata = value;
                }
                if let Some(value) = reply.headers {
                    if headers != *request.headers {
                        return Err(self.refuse("a rewrite over scrubbed headers is refused"));
                    }
                    *request.headers = value;
                }
                if let Some(value) = reply.extra_body {
                    if extra_body != original_body {
                        return Err(self.refuse("a rewrite over a scrubbed extra_body is refused"));
                    }
                    *request.extra_body = Some(value);
                }
                Ok(Decision::Continue)
            }
        }
    }

    fn compaction_before(&self, compaction: &mut HookedCompaction<'_>) -> HookResult {
        if !self.points.decides(DecidePoint::CompactionBefore) {
            return Ok(Decision::Continue);
        }
        let clean = self.scrub(compaction.context);
        let reply = self.ask(DecidePoint::CompactionBefore, json!({"context": clean}))?;
        match reply.verdict {
            Word::Continue => Ok(Decision::Continue),
            Word::Deny => Ok(Decision::Deny(self.reason(&reply))),
            Word::Stop => Ok(Decision::Stop(self.reason(&reply))),
            Word::Ask => Err(self.refuse(
                "ask at compaction.before, where only allow, rewrite, deny and stop are allowed",
            )),
            Word::Rewrite => {
                let Some(context) = reply.context else {
                    return Err(self.refuse("a rewrite without `context`"));
                };
                if clean != *compaction.context {
                    return Err(self.refuse(
                        "a rewrite over a scrubbed context is refused: it holds a secret",
                    ));
                }
                *compaction.context = context;
                Ok(Decision::Continue)
            }
        }
    }

    fn before_tool(&self, call: &mut HookedCall<'_>) -> HookResult {
        if !self.points.decides(DecidePoint::ToolBefore) {
            return Ok(Decision::Continue);
        }
        let clean = self.scrub_value(call.arguments);
        let reply = self.ask(
            DecidePoint::ToolBefore,
            json!({"tool": call.tool, "arguments": clean}),
        )?;
        match reply.verdict {
            Word::Continue => Ok(Decision::Continue),
            Word::Deny => Ok(Decision::Deny(self.reason(&reply))),
            Word::Ask => Ok(Decision::Ask(self.reason(&reply))),
            Word::Stop => Ok(Decision::Stop(self.reason(&reply))),
            Word::Rewrite => {
                let Some(arguments) = reply.arguments else {
                    return Err(self.refuse("a rewrite without `arguments`"));
                };
                if clean != *call.arguments {
                    return Err(self.refuse(
                        "a rewrite over a scrubbed argument is refused: the call holds a secret",
                    ));
                }
                *call.arguments = arguments;
                Ok(Decision::Continue)
            }
        }
    }

    fn after_tool(&self, call: &HookedCall<'_>, output: &mut HookedText<'_>) -> HookResult {
        if !self.points.decides(DecidePoint::ToolAfter) {
            return Ok(Decision::Continue);
        }
        let clean = self.scrub(output.text);
        let reply = self.ask(
            DecidePoint::ToolAfter,
            json!({"tool": call.tool, "text": clean}),
        )?;
        match reply.verdict {
            Word::Continue => Ok(Decision::Continue),
            Word::Deny => Ok(Decision::Deny(self.reason(&reply))),
            Word::Stop => Ok(Decision::Stop(self.reason(&reply))),
            Word::Ask => Err(self
                .refuse("ask at tool.after, where only allow, rewrite, deny and stop are allowed")),
            Word::Rewrite => {
                let Some(text) = reply.text else {
                    return Err(self.refuse("a rewrite without `text`"));
                };
                if clean != *output.text {
                    return Err(self
                        .refuse("a rewrite over a scrubbed answer is refused: it holds a secret"));
                }
                *output.text = text;
                Ok(Decision::Continue)
            }
        }
    }

    fn observe(&self, event: &HookEvent) -> Result<(), HookError> {
        if !self.subscriptions.matches(event) {
            return Ok(());
        }
        let payload = json!({
            "kind": event.kind.name(),
            "name": event.name(),
            "custom": event.custom.clone(),
            "header": {
                "key": event.header.key.clone(),
                "run": event.header.run.clone(),
                "root_run": event.header.root_run.clone(),
                "agent": event.header.agent.clone(),
                "at": event.header.at.clone(),
            },
            "fields": event.fields.clone(),
            "cut": event.cut.clone(),
        });
        let reply = self
            .program
            .request(
                Mode::Observe,
                Some(event.header.key.as_str()),
                event.name(),
                payload,
            )
            .map_err(|e| HookError::new(self.name.clone(), e.class(), e.to_string()))?;
        self.check(&reply)?;
        if reply.verdict == Word::Continue {
            Ok(())
        } else {
            Err(self.refuse(format!(
                "{} at {}, where an observer may only continue",
                reply.verdict.as_str(),
                event.name()
            )))
        }
    }
}
