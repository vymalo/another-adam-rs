//! [`Scrubbed`]: a tool whose results and errors pass through the deployment's scrub function.

use std::sync::Arc;
use std::time::Duration;

use adam_llm_agent::{RemotePoll, StateKey, StepStyle, Tool, ToolCtx, ToolError, ToolOutput};
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

    fn error(&self, error: ToolError) -> ToolError {
        match error {
            ToolError::Transient(m) => ToolError::Transient((self.scrub)(&m)),
            ToolError::Permanent(m) => ToolError::Permanent((self.scrub)(&m)),
            // The waits carry an id and nothing to hide; a question is scrubbed like text.
            ToolError::NeedsInput { question, ui } => ToolError::NeedsInput {
                question: (self.scrub)(&question),
                ui,
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

    fn time_limit(&self) -> Option<Duration> {
        self.inner.time_limit()
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
