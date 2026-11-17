//! [`Scrubbed`]: a tool whose results and errors pass through the deployment's scrub function.

use std::sync::Arc;
use std::time::Duration;

use adam_llm_agent::{
    AnswerCheck, RemotePoll, StateKey, StepStyle, Tool, ToolAccess, ToolCtx, ToolError, ToolOutput,
};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde_json::Value;

/// A function from a text to the same text with its secrets replaced.
pub(crate) type Scrub = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// How long the longest value the scrub function replaces is, in bytes.
pub(crate) type Longest = Arc<dyn Fn() -> usize + Send + Sync>;

/// A tool whose content, errors and questions are scrubbed, and whose own scrubbing hook
/// ([`ToolCtx::scrub`]) is set, so that a tool may scrub before it cuts. Everything else is the
/// wrapped tool's.
pub(crate) struct Scrubbed {
    inner: Arc<dyn Tool>,
    scrub: Scrub,
    longest: Option<Longest>,
}

impl Scrubbed {
    pub(crate) fn new(inner: Arc<dyn Tool>, scrub: Scrub, longest: Option<Longest>) -> Self {
        Self {
            inner,
            scrub,
            longest,
        }
    }

    fn output(&self, mut out: ToolOutput) -> ToolOutput {
        out.content = (self.scrub)(&out.content);
        out
    }

    /// Every string of `value`, keys apart, passed through the scrub.
    fn scrub_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = (self.scrub)(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.scrub_value(item)),
            Value::Object(members) => members.values_mut().for_each(|item| self.scrub_value(item)),
            _ => {}
        }
    }

    fn error(&self, error: ToolError) -> ToolError {
        match error {
            ToolError::Transient(m) => ToolError::Transient((self.scrub)(&m)),
            ToolError::Permanent(m) => ToolError::Permanent((self.scrub)(&m)),
            // The waits carry an id and nothing to hide; a question is scrubbed like text.
            ToolError::NeedsInput { question, mut ui } => ToolError::NeedsInput {
                question: (self.scrub)(&question),
                ui: {
                    if let Some(ui) = &mut ui {
                        self.scrub_value(ui);
                    }
                    ui
                },
            },
            other @ (ToolError::AwaitRun { .. }
            | ToolError::AwaitRemote { .. }
            | ToolError::AwaitSession { .. }) => other,
            // `ToolError` is non_exhaustive: a variant added later must not slip past the scrub.
            other => ToolError::Permanent((self.scrub)(&other.to_string())),
        }
    }

    fn hooked(&self, ctx: &ToolCtx) -> ToolCtx {
        let scrub = Arc::clone(&self.scrub);
        match &self.longest {
            Some(longest) => {
                let longest = Arc::clone(longest);
                ctx.clone()
                    .with_scrub_longest(move |text| scrub(text), move || longest())
            }
            None => ctx.clone().with_scrub(move |text| scrub(text)),
        }
    }
}

#[async_trait]
impl Tool for Scrubbed {
    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    fn required_state(&self) -> Vec<StateKey> {
        self.inner.required_state()
    }

    fn asks_user(&self) -> bool {
        self.inner.asks_user()
    }

    fn paged(&self) -> bool {
        self.inner.paged()
    }

    fn step_style(&self) -> StepStyle {
        self.inner.step_style()
    }

    fn access(&self, args: &Value) -> ToolAccess {
        self.inner.access(args)
    }

    fn time_limit(&self) -> Option<Duration> {
        self.inner.time_limit()
    }

    fn read_answer(
        &self,
        call_id: &str,
        args: &Value,
        ui: Option<&Value>,
        payload: &Value,
    ) -> AnswerCheck {
        // A refusal and an ending are made from the call's own arguments, which are not scrubbed:
        // they pass through the scrub like the question the call asked.
        self.inner.read_answer(call_id, args, ui, payload).scrubbed(
            |text: String| (self.scrub)(&text),
            |ui| self.scrub_value(ui),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        match self.inner.call(&self.hooked(ctx), args).await {
            Ok(out) => Ok(self.output(out)),
            Err(e) => Err(self.error(e)),
        }
    }

    async fn poll_remote(&self, ctx: &ToolCtx, task: &str) -> Result<RemotePoll, ToolError> {
        match self.inner.poll_remote(&self.hooked(ctx), task).await {
            Ok(RemotePoll::Ready(out)) => Ok(RemotePoll::Ready(self.output(out))),
            Ok(RemotePoll::Working) => Ok(RemotePoll::Working),
            Err(e) => Err(self.error(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use adam_runtime::NoopSink;
    use serde_json::json;

    use super::*;

    struct Leaky;

    #[async_trait]
    impl Tool for Leaky {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "leaky".into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            }
        }
        async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
            // The hook is set, so a tool can scrub before it cuts.
            let cut = ctx.scrub("hook: s3cr3t");
            Ok(ToolOutput::text(format!("{cut} and s3cr3t")))
        }
        async fn poll_remote(&self, _ctx: &ToolCtx, _task: &str) -> Result<RemotePoll, ToolError> {
            Ok(RemotePoll::Ready(ToolOutput::text("polled s3cr3t")))
        }
    }

    /// A tool that refuses the answer "no", so that a wrapper that forgets to forward
    /// `read_answer` is the one that accepts it.
    struct Picky;

    #[async_trait]
    impl Tool for Picky {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "picky".into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            }
        }
        async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("x"))
        }
        fn read_answer(
            &self,
            _call_id: &str,
            _args: &Value,
            _ui: Option<&Value>,
            payload: &Value,
        ) -> AnswerCheck {
            let leaky = |question: &str| {
                adam_llm_agent::Refusal::new(
                    question,
                    Some(json!([{"text": "uses s3cr3t", "n": 1}, ["s3cr3t"]])),
                    "no",
                )
            };
            match payload["text"].as_str() {
                Some("no") => AnswerCheck::Refuse(leaky("again? s3cr3t")),
                Some("end") => AnswerCheck::End(leaky("no more, s3cr3t")),
                Some("read") => AnswerCheck::Read("said s3cr3t".into()),
                _ => AnswerCheck::Accept,
            }
        }
    }

    /// A tool that reads the file its argument names, so that a wrapper that forgets to forward
    /// `access` is the one that runs it alone.
    struct Reader;

    #[async_trait]
    impl Tool for Reader {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "reader".into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            }
        }
        fn access(&self, args: &Value) -> ToolAccess {
            ToolAccess::reads_path("files", args.get("path").and_then(Value::as_str))
        }
        async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("x"))
        }
    }

    #[test]
    fn what_a_call_shares_is_what_the_wrapped_tool_says() {
        let wrapped = Scrubbed::new(
            Arc::new(Reader),
            Arc::new(|text: &str| text.to_owned()),
            None,
        );
        assert_eq!(
            wrapped.access(&json!({"path": "a/b.rs"})),
            ToolAccess::reads("files/a/b.rs")
        );
        let alone = Scrubbed::new(
            Arc::new(Picky),
            Arc::new(|text: &str| text.to_owned()),
            None,
        );
        assert_eq!(alone.access(&json!({})), ToolAccess::Alone);
    }

    #[test]
    fn the_answer_to_a_question_is_read_by_the_tool_that_asked_through_the_scrub() {
        let wrapped = Scrubbed::new(
            Arc::new(Picky),
            Arc::new(|text: &str| text.to_owned()),
            None,
        );
        let no = json!({"text": "no"});
        assert!(matches!(
            wrapped.read_answer("c1", &json!({}), None, &no),
            AnswerCheck::Refuse(_)
        ));
        assert_eq!(
            wrapped.read_answer("c1", &json!({}), None, &json!({"text": "yes"})),
            AnswerCheck::Accept
        );
    }

    #[test]
    fn what_a_refusal_an_ending_and_a_reading_say_is_scrubbed_like_a_question() {
        let wrapped = scrubbed_picky();
        let said = |text: &str| wrapped.read_answer("c1", &json!({}), None, &json!({"text": text}));
        let AnswerCheck::Refuse(refusal) = said("no") else {
            panic!("refused")
        };
        assert_eq!(refusal.question, "again? [redacted]");
        assert_eq!(
            refusal.ui,
            Some(json!([{"text": "uses [redacted]", "n": 1}, ["[redacted]"]]))
        );
        assert_eq!(refusal.rule, "no", "what is not text is as it was");
        let AnswerCheck::End(ending) = said("end") else {
            panic!("ended")
        };
        assert_eq!(ending.question, "no more, [redacted]");
        assert_eq!(
            ending.ui,
            Some(json!([{"text": "uses [redacted]", "n": 1}, ["[redacted]"]]))
        );
        assert_eq!(said("read"), AnswerCheck::Read("said [redacted]".into()));
        assert_eq!(said("yes"), AnswerCheck::Accept);
    }

    #[test]
    fn the_interface_of_a_question_is_scrubbed_too() {
        let ask = ToolError::NeedsInput {
            question: "q s3cr3t".into(),
            ui: Some(json!([{"text": "s3cr3t"}])),
        };
        assert_eq!(
            scrubbed().error(ask),
            ToolError::NeedsInput {
                question: "q [redacted]".into(),
                ui: Some(json!([{"text": "[redacted]"}])),
            }
        );
    }

    fn scrubbed_picky() -> Scrubbed {
        Scrubbed::new(
            Arc::new(Picky),
            Arc::new(|text: &str| text.replace("s3cr3t", "[redacted]")),
            None,
        )
    }

    fn scrubbed() -> Scrubbed {
        Scrubbed::new(
            Arc::new(Leaky),
            Arc::new(|text: &str| text.replace("s3cr3t", "[redacted]")),
            None,
        )
    }

    #[tokio::test]
    async fn results_and_polls_are_scrubbed_and_the_hook_is_set() {
        let ctx = ToolCtx::detached("leaky", "c1", Arc::new(NoopSink));
        let out = scrubbed().call(&ctx, json!({})).await.unwrap();
        assert_eq!(out.content, "hook: [redacted] and [redacted]");
        let RemotePoll::Ready(polled) = scrubbed().poll_remote(&ctx, "t").await.unwrap() else {
            panic!("ready");
        };
        assert_eq!(polled.content, "polled [redacted]");
    }

    #[test]
    fn errors_are_scrubbed_and_waits_pass_through() {
        let scrub = scrubbed();
        assert_eq!(
            scrub.error(ToolError::Permanent("leaked s3cr3t".into())),
            ToolError::Permanent("leaked [redacted]".into())
        );
        let wait = ToolError::AwaitSession {
            session: adam_core::RunId::new(),
        };
        assert_eq!(scrub.error(wait.clone()), wait);
    }
}
