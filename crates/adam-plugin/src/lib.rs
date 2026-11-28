//! Plugins around the agent loop (ADR 0050, slices **H1** and **H1b**): the vocabulary, and the
//! shapes the loop records when it uses them.
//!
//! A **plugin** is a compiled hook set: it may allow, rewrite or refuse a person's message
//! (`message.received`), rewrite a tool call before it runs, refuse it, ask a person, stop the run,
//! or rewrite what the tool answered. Three rules make it safe:
//!
//! 1. **Fail closed.** A hook that errors, panics or misses its deadline never lets the call or the
//!    message through: [`HookSet`] answers an error, and the caller refuses. A panic is
//!    [`ErrorClass::Invalid`]: a plugin that cannot run is buggy, not unlucky.
//! 2. **A transient failure is retried, once.** [`HookError::is_retryable`] says whether the hook
//!    may be tried again in the same step (a deadline is `Transient`); past the retries the call or
//!    the message is refused like any other error.
//! 3. **Order is the declaration order**, and the first verdict other than [`Verdict::Continue`] is
//!    the set's: a later plugin is not asked once one has decided.
//!
//! The loop runs a set **inside** the journaled step it affects, with `LlmAgentBuilder::hooks` of
//! `adam-llm-agent`: in the step that takes a message in for `message.received`, inside
//! `tool:<call id>` for `before_tool` (after a source's `withhold` and before the tool) and
//! `after_tool` (once the tool answered). The verdict is recorded in that step, so a replay that
//! finds it recorded asks no hook again. A hook gets no `Ctx` and cannot add a step.
//!
//! **The command environment is not a plugin point.** A command's environment has its own seam,
//! `adam_workspace::ExecHook`, composed by `adam_workspace::HookedEnvironment` (ADR 0050,
//! decision 13, slice H10): it sees an `ExecSpec` and no `Ctx`, store or secret, and
//! `HidingEnvironment` is one such hook. It is not a method of [`Plugin`] because this crate
//! depends on neither the loop, the runtime nor the workspace: an `exec.prepare` point would make
//! every plugin implementer depend on `adam-workspace`. `exec` stays a reserved event namespace.

#![warn(missing_docs)]

mod audit;
mod custom;
mod events;

pub use audit::Audit;
pub use custom::{
    DATA_MAX, DeclarationError, EventSpec, RESERVED_NAMESPACES, STEP_EVENTS_MAX, Subscriptions,
    TRANSITION_EVENTS_MAX, Visibility, check_event_name, declared, has_event_name_shape,
};
pub use events::{
    EVENT_MAX, EventHeader, EventKind, EventQueue, FIELD_MAX, FlushReport, HookEvent, JSON_MAX,
    ObserveLimits,
};

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_error::ErrorClass;
use serde_json::Value;

/// What a call is, as a hook sees it: the tool's name and the arguments the model sent, which a
/// plugin may rewrite in place.
pub struct HookedCall<'a> {
    /// The tool's name, as the model asked for it.
    pub tool: &'a str,
    /// The arguments, as JSON; a plugin that rewrites them changes what the tool runs with.
    pub arguments: &'a mut Value,
}

/// What a tool answered, as an `after_tool` hook sees it: the text the model will read, which a
/// plugin may rewrite in place.
pub struct HookedText<'a> {
    /// The tool's answer as text (an artifact or an image is summarised, as the loop does).
    pub text: &'a mut String,
}

/// The point where a person's message is about to reach a model (ADR 0050, decision 9). It may
/// allow the message, rewrite [`HookedMessage::text`] in place, or refuse it with [`Verdict::Deny`].
pub const MESSAGE_RECEIVED: &str = "message.received";

/// The point where the run would end with an answer (ADR 0050, decision 10): a plugin may allow the
/// end ([`Verdict::Continue`]) or keep the run going with [`Verdict::Deny`], whose reason the model is
/// asked again with. [`Verdict::Stop`] ends the run as it was going to end; [`Verdict::Ask`] is not a
/// verdict of this point and is a hook failure.
pub const RUN_ENDING: &str = "run.ending";

/// The answer a run is about to end with, as a `run.ending` hook sees it. Read only: a hook never
/// rewrites an answer, it asks for another try.
pub struct HookedEnd<'a> {
    /// The answer's text.
    pub text: &'a str,
    /// How many times a hook has already kept this run going.
    pub continuations: u32,
}

/// A file a person sent with a message, as a `message.received` hook sees it: **by reference**,
/// never its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageFile {
    /// The file name the sender gave it, when there is one.
    pub name: Option<String>,
    /// The media type the sender claims, checked against the bytes before the file is kept.
    pub media_type: Option<String>,
    /// The size in bytes.
    pub size: u64,
    /// The reference the loop keeps the file under (its SHA-256), when it has kept it or is about
    /// to; absent when the bytes could not be read or were not admitted.
    pub reference: Option<String>,
}

/// What a person's message is, as a `message.received` hook sees it: its text, which a plugin may
/// rewrite in place, and its files by reference.
///
/// A rewrite is what the model reads and what the run's history holds; the hook reads the text as
/// the loop will send it (scrubbed and bounded), never the bytes of a file.
pub struct HookedMessage<'a> {
    /// The message's text.
    pub text: &'a mut String,
    /// The files that came with it, in the order the person sent them.
    pub files: &'a [MessageFile],
}

/// The point where a model call is about to be made, inside the call's step (ADR 0050, slice H3).
pub const MODEL_REQUEST: &str = "model.request";

/// The point where a history is about to be summarised, inside the summary's journaled step
/// (ADR 0050, slice H9).
pub const COMPACTION_BEFORE: &str = "compaction.before";

/// Which model call a `model.request` hook runs at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelCallKind {
    /// A turn's call: the run's own answer.
    Turn,
    /// A compaction's summary call (ADR 0050, slice H9).
    Compaction,
}

/// What a model call is, as a `model.request` hook sees it: the sampling temperature and top-p,
/// the bound on the answer's length, the call's metadata, headers and extra body, and what kind of
/// call it is. A plugin may read the kind and rewrite the rest in place. The system prompt, the
/// history, the model, its endpoint and its key are not here.
pub struct HookedModelRequest<'a> {
    /// A turn's call, or a compaction's summary.
    pub kind: ModelCallKind,
    /// The sampling temperature; absent leaves the model's own.
    pub temperature: &'a mut Option<f32>,
    /// Nucleus sampling mass; absent leaves the model's own.
    pub top_p: &'a mut Option<f32>,
    /// Headers for this call. A credential-like name fails the call; the key is the deployment's.
    pub headers: &'a mut std::collections::BTreeMap<String, String>,
    /// Members merged over the call's body. The members the client owns are refused.
    pub extra_body: &'a mut Option<serde_json::Map<String, serde_json::Value>>,
    /// The upper bound on generated tokens; absent leaves the model's own.
    pub max_output_tokens: &'a mut Option<u32>,
    /// What is forwarded as request metadata where the backend supports it.
    pub metadata: &'a mut std::collections::BTreeMap<String, String>,
}

/// What a compaction is, as a `compaction.before` hook sees it: the context a plugin may add to the
/// summary request. The history, the cut and the model are not here, and a hook never writes the
/// summary itself.
pub struct HookedCompaction<'a> {
    /// Text a plugin adds to the summary request; empty adds nothing.
    pub context: &'a mut String,
}

/// What a deciding hook says about the call it runs at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Go on: the call runs with any rewrite the hook made.
    Continue,
    /// Refuse the call; the text is the reason a person reads.
    Deny(String),
    /// Ask a person before the call runs; the text is the question.
    Ask(String),
    /// Stop the run; the text is the reason the run ends with.
    Stop(String),
}

impl Verdict {
    /// The reason, for a verdict that carries one.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Verdict::Continue => None,
            Verdict::Deny(reason) | Verdict::Ask(reason) | Verdict::Stop(reason) => Some(reason),
        }
    }

    /// Whether the verdict ends the set's run of plugins: anything but [`Verdict::Continue`].
    #[must_use]
    pub fn decides(&self) -> bool {
        !matches!(self, Verdict::Continue)
    }
}

/// Why a hook did not decide. A plugin that errors never lets the call through: the set answers this
/// and the caller denies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookError {
    /// The plugin that failed, by its name.
    pub plugin: String,
    /// The class of the failure: `Invalid` for a panic or a broken plugin, `Transient` for a deadline.
    pub class: ErrorClass,
    /// What happened, as a person reads it. Never holds an argument or a tool's answer.
    pub reason: String,
}

impl HookError {
    /// A plugin `plugin` failed with `class`, for `reason`.
    #[must_use]
    pub fn new(plugin: impl Into<String>, class: ErrorClass, reason: impl Into<String>) -> Self {
        HookError {
            plugin: plugin.into(),
            class,
            reason: reason.into(),
        }
    }

    /// Whether the call may be tried again in the same step (the class is retryable).
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.class.is_retryable()
    }
}

impl std::fmt::Display for HookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the plugin {:?} failed: {}", self.plugin, self.reason)
    }
}

impl std::error::Error for HookError {}

/// What a hook answers: a verdict, or the reason it could not decide.
pub type HookResult = Result<Verdict, HookError>;

/// A compiled hook set around one tool call. Every method has a do-nothing default, so a plugin that
/// cares about one point implements one method.
pub trait Plugin: Send + Sync {
    /// The plugin's name, as a report and an error name it.
    fn name(&self) -> &str;

    /// A person's message is about to reach a model: allow it, rewrite its text, or refuse it with
    /// [`Verdict::Deny`] and the reason the person reads. `Ask` and `Stop` are not verdicts of this
    /// point, and the loop treats them as a hook failure (a refusal).
    fn message_received(&self, _message: &mut HookedMessage<'_>) -> HookResult {
        Ok(Verdict::Continue)
    }

    /// The run would end with an answer: allow it, or return [`Verdict::Deny`] with the reason the
    /// model is asked again with. Called once per such end, in the journaled step `end:<turn>`.
    fn run_ending(&self, _end: &HookedEnd<'_>) -> HookResult {
        Ok(Verdict::Continue)
    }

    /// A model call is about to be made: rewrite its temperature, its bound on output tokens or its
    /// metadata, or stop the run with [`Verdict::Stop`]. `Deny` and `Ask` are not verdicts of this
    /// point, and the loop treats them as a hook failure.
    fn model_request(&self, _request: &mut HookedModelRequest<'_>) -> HookResult {
        Ok(Verdict::Continue)
    }

    /// A history is about to be summarised (ADR 0050, slice H9): add context to the summary request,
    /// or refuse the compaction with [`Verdict::Deny`], and the history is shortened instead.
    /// `Stop` fails the run; `Ask` is not a verdict of this point, so it is a failure of class
    /// `Invalid` and the run fails too (decision 8). A hook never writes the summary itself.
    fn compaction_before(&self, _compaction: &mut HookedCompaction<'_>) -> HookResult {
        Ok(Verdict::Continue)
    }

    /// Before the call runs: rewrite its arguments, refuse it, ask, or stop the run.
    fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
        Ok(Verdict::Continue)
    }

    /// After the tool answered: rewrite what the model will read, or refuse it.
    fn after_tool(&self, _call: &HookedCall<'_>, _output: &mut HookedText<'_>) -> HookResult {
        Ok(Verdict::Continue)
    }

    /// What the plugin is offered by [`observe`](Self::observe). The default is the core catalogue
    /// ([`Subscriptions::core`]), which is what an observer received before custom events existed;
    /// a custom event reaches only a plugin that names it (`Subscriptions::core().with("demo.*")`).
    /// A pattern whose namespace is neither core nor declared is refused when the agent is built.
    fn subscriptions(&self) -> Subscriptions {
        Subscriptions::core()
    }

    /// The custom events this plugin declares (ADR 0050, slice H5). The name rules, the core
    /// namespaces and the "declared twice, differently" check are the same as for a tool's
    /// ([`EventSpec`]); a declaration makes the namespace known to every subscription of the agent.
    /// The default declares none.
    fn events(&self) -> Vec<EventSpec> {
        Vec::new()
    }

    /// An event of the catalogue happened (ADR 0050, slice H2). An observer only reads: it runs
    /// outside the journal, its failure is counted and dropped, and it cannot change the run. The
    /// event's `key` is stable across replays: a plugin that writes elsewhere dedupes on it.
    fn observe(&self, _event: &HookEvent) -> Result<(), HookError> {
        Ok(())
    }
}

/// What a hook says, and which plugin said it: the record of a decision names both (ADR 0050,
/// decision 2), so a refusal a person reads can name the plugin that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided<'a> {
    /// The plugin that decided.
    pub plugin: &'a str,
    /// What it said.
    pub verdict: Verdict,
}

/// How long one hook may take, and how many times a transient failure is tried again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookLimits {
    /// The longest one plugin's hook may run before it is a `Transient` failure.
    pub deadline: Duration,
    /// How many times a retryable failure is tried again, at most. Zero: no retry.
    pub retries: u8,
}

impl Default for HookLimits {
    fn default() -> Self {
        // A local plugin is in-process code; one that blocks for a whole second has a bug.
        HookLimits {
            deadline: Duration::from_secs(1),
            retries: 1,
        }
    }
}

/// An ordered set of plugins, with their limits. A clone shares the plugins (`Arc`s): a host that
/// gives one set to several agents (`adam_assembly::BoundDef::hooks`) runs the same plugins in each.
#[derive(Clone, Default)]
pub struct HookSet {
    pub(crate) plugins: Vec<Arc<dyn Plugin>>,
    limits: HookLimits,
    pub(crate) observe_limits: ObserveLimits,
}

impl std::fmt::Debug for HookSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookSet")
            .field("plugins", &self.names())
            .field("limits", &self.limits)
            .finish()
    }
}

impl HookSet {
    /// A set with the default limits and no plugin.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same set with `limits`.
    #[must_use]
    pub fn with_limits(mut self, limits: HookLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The same set with `plugin` appended: the order is the order hooks run in.
    #[must_use]
    pub fn with(mut self, plugin: Arc<dyn Plugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// The plugins' names, in the order they run.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.name()).collect()
    }

    /// Whether the set holds no plugin, so the caller may skip it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Runs every plugin's `message_received` in order, until one decides. `None` when every plugin
    /// said [`Verdict::Continue`].
    ///
    /// # Errors
    /// As [`HookSet::before_tool`]. The caller refuses the message on an error: a hook that cannot
    /// say `Continue` never lets a message reach a model.
    pub fn message_received(
        &self,
        message: &mut HookedMessage<'_>,
    ) -> Result<Option<Decided<'_>>, HookError> {
        self.run_decided(MESSAGE_RECEIVED, |plugin| {
            guard(plugin.name(), self.limits.deadline, || {
                plugin.message_received(message)
            })
        })
    }

    /// Runs every plugin's `run_ending` in order, until one decides. `None` when every plugin allowed
    /// the end.
    ///
    /// # Errors
    /// As [`HookSet::before_tool`].
    pub fn run_ending(&self, end: &HookedEnd<'_>) -> Result<Option<Decided<'_>>, HookError> {
        self.run_decided(RUN_ENDING, |plugin| {
            guard(plugin.name(), self.limits.deadline, || {
                plugin.run_ending(end)
            })
        })
    }

    /// Runs every plugin's `before_tool` in order, until one denies or stops.
    ///
    /// An [`Verdict::Ask`] does not end the set (ADR 0050, decision 8 and slice H8): the plugins
    /// after it are still asked, each seeing the arguments the ones before it left, because a call a
    /// person approves runs, and a plugin that would have refused it must have had its say. A later
    /// `Deny` or `Stop`, or a later failure, is the set's answer; a later `Ask` adds nothing (the
    /// first question stands). The set answers the first `Ask` only when no plugin after it refused.
    ///
    /// # Errors
    /// The first plugin that panics, errors or misses its deadline, after a retryable failure has
    /// been tried again [`HookLimits::retries`] times. The caller denies on an error: a hook that
    /// cannot say `Continue` never lets the call through.
    pub fn before_tool(&self, call: &mut HookedCall<'_>) -> HookResult {
        let mut asked: Option<Verdict> = None;
        let decided = self.run_decided("before_tool", |plugin| {
            let verdict = guard(plugin.name(), self.limits.deadline, || {
                plugin.before_tool(call)
            })?;
            match verdict {
                // The first question stands; the plugins after it are asked all the same.
                Verdict::Ask(_) => {
                    asked.get_or_insert(verdict);
                    Ok(Verdict::Continue)
                }
                verdict => Ok(verdict),
            }
        })?;
        Ok(match decided {
            Some(one) => one.verdict,
            None => asked.unwrap_or(Verdict::Continue),
        })
    }

    /// Runs every plugin's `model_request` in order, until one decides.
    ///
    /// # Errors
    /// As [`HookSet::before_tool`]. The caller does not make the call on an error.
    pub fn model_request(&self, request: &mut HookedModelRequest<'_>) -> HookResult {
        self.run(MODEL_REQUEST, |plugin| {
            guard(plugin.name(), self.limits.deadline, || {
                plugin.model_request(request)
            })
        })
    }

    /// Runs every plugin's `compaction_before` in order, until one decides.
    ///
    /// # Errors
    /// As [`HookSet::before_tool`]. The caller does not summarise on an error: the run fails for
    /// good (decision 8); only a [`Verdict::Deny`] shortens the history instead.
    pub fn compaction_before(&self, compaction: &mut HookedCompaction<'_>) -> HookResult {
        self.run(COMPACTION_BEFORE, |plugin| {
            guard(plugin.name(), self.limits.deadline, || {
                plugin.compaction_before(compaction)
            })
        })
    }

    /// Runs every plugin's `after_tool` in order, until one decides.
    ///
    /// # Errors
    /// As [`HookSet::before_tool`].
    pub fn after_tool(&self, call: &HookedCall<'_>, output: &mut HookedText<'_>) -> HookResult {
        self.run("after_tool", |plugin| {
            guard(plugin.name(), self.limits.deadline, || {
                plugin.after_tool(call, output)
            })
        })
    }

    fn run(&self, point: &str, one: impl FnMut(&Arc<dyn Plugin>) -> HookResult) -> HookResult {
        self.run_decided(point, one)
            .map(|decided| decided.map_or(Verdict::Continue, |one| one.verdict))
    }

    /// [`run`](Self::run), naming the plugin whose verdict decided.
    fn run_decided(
        &self,
        point: &str,
        mut one: impl FnMut(&Arc<dyn Plugin>) -> HookResult,
    ) -> Result<Option<Decided<'_>>, HookError> {
        for plugin in &self.plugins {
            let mut attempt = 0u8;
            loop {
                match one(plugin) {
                    Ok(verdict) if verdict.decides() => {
                        return Ok(Some(Decided {
                            plugin: plugin.name(),
                            verdict,
                        }));
                    }
                    Ok(_) => break,
                    Err(error) if error.is_retryable() && attempt < self.limits.retries => {
                        attempt += 1;
                        continue;
                    }
                    Err(error) => {
                        return Err(HookError {
                            plugin: error.plugin,
                            class: error.class,
                            reason: format!("{} (at {point})", error.reason),
                        });
                    }
                }
            }
        }
        Ok(None)
    }
}

/// Runs `hook`, turning a panic into an `Invalid` error and a deadline into a `Transient` one.
fn guard(plugin: &str, deadline: Duration, hook: impl FnOnce() -> HookResult) -> HookResult {
    let started = Instant::now();
    // A plugin is not `UnwindSafe` in general (it may hold a `Mutex`); the rule of the loop is that
    // a panic in a hook denies the call and changes nothing else, so the state is not observed here.
    let outcome = catch_unwind(AssertUnwindSafe(hook));
    let elapsed = started.elapsed();
    match outcome {
        Ok(Ok(_verdict)) if elapsed > deadline => Err(HookError::new(
            plugin,
            ErrorClass::Transient,
            "the hook missed its deadline",
        )),
        Ok(Ok(verdict)) => Ok(verdict),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(HookError::new(
            plugin,
            ErrorClass::Invalid,
            "the hook panicked",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter(std::sync::atomic::AtomicUsize);

    impl Plugin for Counter {
        fn name(&self) -> &str {
            "counter"
        }

        fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Verdict::Continue)
        }
    }

    #[test]
    fn a_verdict_carries_its_reason_and_only_continue_lets_the_set_go_on() {
        assert_eq!(Verdict::Continue.reason(), None);
        assert_eq!(Verdict::Deny("no".into()).reason(), Some("no"));
        assert!(!Verdict::Continue.decides());
        assert!(Verdict::Stop("done".into()).decides());
    }

    #[test]
    fn an_empty_set_continues_and_a_plugin_runs_once() {
        let mut args = serde_json::json!({});
        let mut call = HookedCall {
            tool: "ls",
            arguments: &mut args,
        };
        assert_eq!(
            HookSet::new().before_tool(&mut call).unwrap(),
            Verdict::Continue
        );

        let counter = Arc::new(Counter(0.into()));
        let set = HookSet::new().with(counter.clone());
        assert_eq!(set.names(), vec!["counter"]);
        assert!(!set.is_empty());
        assert_eq!(set.before_tool(&mut call).unwrap(), Verdict::Continue);
        assert_eq!(counter.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_panic_is_invalid_and_an_error_is_not_a_verdict() {
        struct Panics;
        impl Plugin for Panics {
            fn name(&self) -> &str {
                "panics"
            }
            fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
                panic!("on purpose");
            }
        }
        let mut args = serde_json::json!({});
        let mut call = HookedCall {
            tool: "ls",
            arguments: &mut args,
        };
        let error = HookSet::new()
            .with(Arc::new(Panics))
            .before_tool(&mut call)
            .expect_err("a panicking hook denies");
        assert_eq!(error.class, ErrorClass::Invalid);
        assert!(error.reason.contains("before_tool"), "{error}");
    }

    struct RefusesMessages;

    impl Plugin for RefusesMessages {
        fn name(&self) -> &str {
            "refuses-messages"
        }

        fn message_received(&self, message: &mut HookedMessage<'_>) -> HookResult {
            if message.text.contains("secret") {
                return Ok(Verdict::Deny("no secrets here".into()));
            }
            message.text.push_str(" (read)");
            Ok(Verdict::Continue)
        }
    }

    #[test]
    fn a_message_hook_may_rewrite_or_refuse_and_sees_files_by_reference() {
        let files = [MessageFile {
            name: Some("note.txt".into()),
            media_type: Some("text/plain".into()),
            size: 12,
            reference: Some("abc".into()),
        }];
        let mut text = String::from("hello");
        let set = HookSet::new().with(Arc::new(RefusesMessages));
        let decided = set
            .message_received(&mut HookedMessage {
                text: &mut text,
                files: &files,
            })
            .expect("a verdict");
        assert!(
            decided.is_none(),
            "a rewrite continues: the text is the decision"
        );
        assert_eq!(text, "hello (read)", "a rewrite is made in place");

        let mut text = String::from("a secret");
        let decided = set
            .message_received(&mut HookedMessage {
                text: &mut text,
                files: &files,
            })
            .expect("a verdict")
            .expect("a refusal");
        assert_eq!(decided.verdict, Verdict::Deny("no secrets here".into()));
        assert_eq!(decided.plugin, "refuses-messages");
    }

    #[test]
    fn an_empty_set_allows_a_message() {
        let mut text = String::from("hello");
        let set = HookSet::new();
        let decided = set
            .message_received(&mut HookedMessage {
                text: &mut text,
                files: &[],
            })
            .expect("no plugin, no failure");
        assert!(decided.is_none(), "an empty set decides nothing");
        assert_eq!(text, "hello");
    }
}
