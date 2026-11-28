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
//! 3. **Order is the declaration order**, and the first verdict other than [`Decision::Continue`] is
//!    the set's: a later plugin is not asked once one has decided. An action ([`Decision::Act`]) is
//!    a decision too: it ends the set, and the loop performs it as its own journaled step
//!    (`Compact` as the next `compact:<n>:<turn>`, a declared action as `action:<turn>:<i>`).
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
    ActionSpec, DATA_MAX, DeclarationError, EventSpec, RESERVED_NAMESPACES, STEP_EVENTS_MAX,
    Subscriptions, TRANSITION_EVENTS_MAX, Visibility, check_event_name, declared,
    has_event_name_shape,
};
pub use events::{
    EVENT_MAX, EventHeader, EventKind, EventQueue, FIELD_MAX, FlushReport, HookEvent, JSON_MAX,
    ObserveLimits,
};

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_error::ErrorClass;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
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
/// allow the message, rewrite [`HookedMessage::text`] in place, or refuse it with [`Decision::Deny`].
pub const MESSAGE_RECEIVED: &str = "message.received";

/// The point where the run would end with an answer (ADR 0050, decision 10): a plugin may allow the
/// end ([`Decision::Continue`]) or keep the run going with [`Decision::Deny`], whose reason the model is
/// asked again with. [`Decision::Stop`] ends the run as it was going to end; [`Decision::Ask`] is not a
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

/// An action a deciding hook asks the loop to perform, as its own journaled step (ADR 0050,
/// decision 11, slice H1d).
///
/// The set is **open and versioned** on purpose, and this is the one place the workspace's closed
/// enums rule does not hold: a plugin declares an action of its own (`ActionSpec { name, version,
/// schema }`), and the loop performs it in `action:<turn>:<i>` with the declaring plugin's
/// `perform`. A new action is therefore a variant here and no trait change, so
/// `#[non_exhaustive]` is correct and is not to be "fixed": a host that matches this must carry an
/// arm for an action it does not know.
///
/// The `serde` derive is for the journal: a step's record carries the action it asked for, so
/// the record decides on a replay and the hook is not asked again (`#[serde(tag = "kind")]` so
/// the variants read with a tag, and the version field of `Declared` is versioned under the
/// record).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookAction {
    /// Summarise the history now, as the next `compact:<n>:<turn>` step, within ADR 0044's bound of
    /// compactions per run.
    Compact,
    /// A plugin-declared action (a name in [`Plugin::actions`](crate::Plugin::actions)). The loop
    /// looks up the plugin that declared `name`, runs its `perform`, and records the result.
    /// Declaring an action with no version is refused at build, and a plugin that does not
    /// declare the action the hook asked for returns [`HookError::unsupported`].
    Declared {
        /// The action's name, as declared.
        name: String,
        /// The version of its declaration.
        version: u32,
    },
}

/// What a deciding hook says about the call it runs at: a verdict, and the action it asks for.
///
/// The variants are the closed set the loop knows how to honour, so a new one must fail to compile
/// in every host: unlike [`HookAction`], this enum is not `#[non_exhaustive]` (rule 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Go on: the call runs with any rewrite the hook made.
    Continue,
    /// Refuse the call; the text is the reason a person reads.
    Deny(String),
    /// Ask a person before the call runs; the text is the question.
    Ask(String),
    /// Stop the run; the text is the reason the run ends with.
    Stop(String),
    /// Perform [`HookAction`] as its own journaled step after this one, and otherwise go on.
    ///
    /// The journaled step records the action, so a replay that finds it asks no hook again
    /// (ADR 0050, decision 11, slice H1d). `Compact` is performed as the next `compact:<n>:<turn>`
    /// step, with the reason `requested`; a declared action (a name a plugin declares with
    /// `actions()`) is performed in `action:<turn>:<i>` by the declaring plugin's `perform`.
    Act(HookAction),
}

impl Decision {
    /// The reason, for a decision that carries one. A [`Decision::Act`] carries none.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Decision::Continue | Decision::Act(_) => None,
            Decision::Deny(reason) | Decision::Ask(reason) | Decision::Stop(reason) => Some(reason),
        }
    }

    /// Whether the decision ends the set's run of plugins: anything but [`Decision::Continue`], an
    /// action included (an action is a decision, even though the loop goes on).
    #[must_use]
    pub fn decides(&self) -> bool {
        !matches!(self, Decision::Continue)
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

    /// The default error a [`Plugin::perform`] returns when the plugin does not declare the
    /// action: a hook that does not declare an action never performs one, and the loop records
    /// the result of `perform` as this error. `class` is `Invalid` (a broken declaration is a
    /// build-time mistake, not a transient one), so the run fails for good on it.
    #[must_use]
    pub fn unsupported() -> Self {
        HookError {
            plugin: String::new(),
            class: ErrorClass::Invalid,
            reason: "this plugin does not declare the action".to_owned(),
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

/// What a hook answers: a decision, or the reason it could not decide.
pub type HookResult = Result<Decision, HookError>;

/// A compiled hook set around one tool call. Every method has a do-nothing default, so a plugin that
/// cares about one point implements one method.
pub trait Plugin: Send + Sync {
    /// The plugin's name, as a report and an error name it.
    fn name(&self) -> &str;

    /// A person's message is about to reach a model: allow it, rewrite its text, or refuse it with
    /// [`Decision::Deny`] and the reason the person reads. `Ask` and `Stop` are not verdicts of this
    /// point, and the loop treats them as a hook failure (a refusal).
    fn message_received(&self, _message: &mut HookedMessage<'_>) -> HookResult {
        Ok(Decision::Continue)
    }

    /// The run would end with an answer: allow it, or return [`Decision::Deny`] with the reason the
    /// model is asked again with. Called once per such end, in the journaled step `end:<turn>`.
    fn run_ending(&self, _end: &HookedEnd<'_>) -> HookResult {
        Ok(Decision::Continue)
    }

    /// A model call is about to be made: rewrite its temperature, its bound on output tokens or its
    /// metadata, or stop the run with [`Decision::Stop`]. `Deny` and `Ask` are not verdicts of this
    /// point, and the loop treats them as a hook failure.
    fn model_request(&self, _request: &mut HookedModelRequest<'_>) -> HookResult {
        Ok(Decision::Continue)
    }

    /// A history is about to be summarised (ADR 0050, slice H9): add context to the summary request,
    /// or refuse the compaction with [`Decision::Deny`], and the history is shortened instead.
    /// `Stop` fails the run; `Ask` is not a verdict of this point, so it is a failure of class
    /// `Invalid` and the run fails too (decision 8). A hook never writes the summary itself.
    fn compaction_before(&self, _compaction: &mut HookedCompaction<'_>) -> HookResult {
        Ok(Decision::Continue)
    }

    /// Before the call runs: rewrite its arguments, refuse it, ask, or stop the run.
    fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
        Ok(Decision::Continue)
    }

    /// After the tool answered: rewrite what the model will read, or refuse it.
    fn after_tool(&self, _call: &HookedCall<'_>, _output: &mut HookedText<'_>) -> HookResult {
        Ok(Decision::Continue)
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

    /// The actions this plugin performs (ADR 0050, decision 11, slice H1d), declared before any
    /// step can ask for one. The name rules and the namespace check are the same as for
    /// [`EventSpec`]: a name of the right shape outside the core namespaces, version of at least
    /// 1, two declarations of one name equal. The default declares none.
    fn actions(&self) -> Vec<ActionSpec> {
        Vec::new()
    }

    /// Perform `action` as the loop's own journaled step `action:<turn>:<i>`. **A hook never
    /// performs an action itself**: the loop performs it here, and the result (`Value`) is what
    /// the step records. The default returns [`HookError::unsupported`], because a plugin that
    /// does not declare the action cannot perform it.
    ///
    /// The step is the loop's, not the plugin's: a crash before this returns is replayed, and the
    /// replay runs `perform` again. A [`HookError`] of a retryable class is the step's failure,
    /// and the transition is tried again; an `Invalid` failure ends the run.
    ///
    /// Returns a boxed future so `Plugin` stays dyn-compatible (the trait is also held as
    /// `Arc<dyn Plugin>` by [`HookSet`]).
    fn perform(
        &self,
        _cx: &HookCtx,
        _action: &HookAction,
    ) -> BoxFuture<'_, Result<Value, HookError>> {
        Box::pin(async { Err(HookError::unsupported()) })
    }
}

/// What a hook says, and which plugin said it: the record of a decision names both (ADR 0050,
/// decision 2), so a refusal a person reads can name the plugin that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided<'a> {
    /// The plugin that decided.
    pub plugin: &'a str,
    /// What it said.
    pub verdict: Decision,
}

/// What a hook's [`Plugin::perform`] returned in the journaled `action:<turn>:<i>` step
/// (ADR 0050, decision 11, slice H1d): the plugin's `Value` on success, the error on failure,
/// and the plugin's name. The step records this so a replay that finds it recorded runs `perform`
/// no second time, and a host that reads the journal sees what happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionOutcome {
    /// The plugin that performed the action, by its name. Empty when no plugin declared the
    /// action (the step recorded the miss and the run went on).
    pub plugin: String,
    /// What `perform` returned. `None` when `perform` errored.
    pub result: Option<Value>,
}

impl ActionOutcome {
    /// An outcome for a plugin's success.
    #[must_use]
    pub fn from_result(plugin: impl Into<String>, result: Value) -> Self {
        Self {
            plugin: plugin.into(),
            result: Some(result),
        }
    }

    /// An outcome for a plugin's failure.
    #[must_use]
    pub fn from_error(plugin: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
            result: None,
        }
    }

    /// An outcome for an action nobody declared.
    #[must_use]
    pub fn undeclared(name: impl Into<String>) -> Self {
        Self {
            plugin: format!("no plugin declares the action `{}`", name.into()),
            result: None,
        }
    }

    /// The error class of the recorded failure, when one is recorded.
    #[must_use]
    pub fn into_error(self) -> Option<HookError> {
        if self.result.is_none() {
            Some(HookError {
                plugin: self.plugin,
                class: ErrorClass::Invalid,
                reason: "the action's perform returned an error".to_owned(),
            })
        } else {
            None
        }
    }
}

impl From<Result<Value, HookError>> for ActionOutcome {
    fn from(result: Result<Value, HookError>) -> Self {
        match result {
            Ok(value) => ActionOutcome {
                plugin: String::new(),
                result: Some(value),
            },
            Err(error) => ActionOutcome {
                plugin: error.plugin,
                result: None,
            },
        }
    }
}

impl ActionOutcome {
    /// The plugin that performed the action, by its name. Set by the loop after `perform` runs
    /// (the loop knows which plugin declared the action).
    #[must_use]
    pub fn with_plugin(mut self, plugin: impl Into<String>) -> Self {
        self.plugin = plugin.into();
        self
    }
}

/// The context a plugin's [`Plugin::perform`] runs with: identifying the run, root run, agent,
/// point and attempt, the deadline the step has, and the cancel token the loop has set. **No
/// `Ctx`, store or run context** (ADR 0050, decision 13): a hook never reads the state or asks
/// the store for one.
///
/// `emit_event` is **not built** here (slice H5 has it for tools, not plugins yet): a plugin's
/// `observe` is the path today, and a plugin that needs to emit from `perform` is added with
/// that seam, not with a stopgap. The field is reserved by the trait shape, not by this struct.
#[derive(Debug, Clone)]
pub struct HookCtx {
    /// The run the hook runs for.
    pub run: String,
    /// The root of the run's parent chain (the top of the chain, the run itself for one that is
    /// nobody's child).
    pub root_run: String,
    /// The agent's name.
    pub agent: String,
    /// The point the hook runs at (`"before_tool"`, `"action"`, ...).
    pub point: &'static str,
    /// The step the hook runs in (e.g. `tool:c1`, `action:3:0`).
    pub key: String,
    /// Which attempt of the step (0 the first time, then retries).
    pub attempt: u8,
    /// The deadline the hook may take.
    pub deadline: Duration,
    /// Whether the run has been cancelled meanwhile.
    pub cancelled: bool,
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
    /// said [`Decision::Continue`].
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
    /// A [`Decision::Act`] ends the set like any other decision: the action is the first one asked
    /// for, and the plugins after it are not asked. The loop performs the action as its own
    /// journaled step after the call's step (`Compact` as the next `compact:<n>:<turn>`, a
    /// declared action as `action:<turn>:<i>`), so the call runs with any rewrite the hooks
    /// left.
    ///
    /// A [`Decision::Ask`] does not end the set (ADR 0050, decision 8 and slice H8): the plugins
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
        let mut asked: Option<Decision> = None;
        let decided = self.run_decided("before_tool", |plugin| {
            let verdict = guard(plugin.name(), self.limits.deadline, || {
                plugin.before_tool(call)
            })?;
            match verdict {
                // The first question stands; the plugins after it are asked all the same.
                Decision::Ask(_) => {
                    asked.get_or_insert(verdict);
                    Ok(Decision::Continue)
                }
                verdict => Ok(verdict),
            }
        })?;
        Ok(match decided {
            Some(one) => one.verdict,
            None => asked.unwrap_or(Decision::Continue),
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
    /// good (decision 8); only a [`Decision::Deny`] shortens the history instead.
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

    /// The plugin that declared the action `name`, or `None` when no plugin declares it. Two
    /// plugins that declared the same name at different versions conflict at build (the same
    /// check as for [`EventSpec`]); a plugin whose `actions` does not include the action is not
    /// the one to perform it.
    #[must_use]
    pub fn declarer_of(&self, name: &str) -> Option<&Arc<dyn Plugin>> {
        self.plugins.iter().find(|plugin| {
            plugin
                .actions()
                .iter()
                .any(|spec| spec.name == name)
        })
    }

    fn run(&self, point: &str, one: impl FnMut(&Arc<dyn Plugin>) -> HookResult) -> HookResult {
        self.run_decided(point, one)
            .map(|decided| decided.map_or(Decision::Continue, |one| one.verdict))
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
            Ok(Decision::Continue)
        }
    }

    #[test]
    fn a_verdict_carries_its_reason_and_only_continue_lets_the_set_go_on() {
        assert_eq!(Decision::Continue.reason(), None);
        assert_eq!(Decision::Deny("no".into()).reason(), Some("no"));
        assert!(!Decision::Continue.decides());
        assert!(Decision::Stop("done".into()).decides());
    }

    #[test]
    fn an_action_is_a_decision_and_carries_no_reason() {
        let action = Decision::Act(HookAction::Compact);
        assert_ne!(action, Decision::Continue);
        assert!(
            action.decides(),
            "an action is a decision, not a continuation"
        );
        assert_eq!(action.reason(), None, "an action carries no reason");
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
            Decision::Continue
        );

        let counter = Arc::new(Counter(0.into()));
        let set = HookSet::new().with(counter.clone());
        assert_eq!(set.names(), vec!["counter"]);
        assert!(!set.is_empty());
        assert_eq!(set.before_tool(&mut call).unwrap(), Decision::Continue);
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
                return Ok(Decision::Deny("no secrets here".into()));
            }
            message.text.push_str(" (read)");
            Ok(Decision::Continue)
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
        assert_eq!(decided.verdict, Decision::Deny("no secrets here".into()));
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

    /// A plugin that does not implement `actions` declares no actions, and a `perform` call on it
    /// returns the unsupported error of class `Invalid`.
    #[test]
    fn an_unaction_plugin_unsupports_perform() {
        struct Quiet;
        impl Plugin for Quiet {
            fn name(&self) -> &str {
                "quiet"
            }
        }
        let plugin = Quiet;
        assert!(plugin.actions().is_empty());
        let cx = HookCtx {
            run: "r".into(),
            root_run: "r".into(),
            agent: "a".into(),
            point: "action",
            key: "action:0:0".into(),
            attempt: 0,
            deadline: Duration::from_secs(1),
            cancelled: false,
        };
        let error = futures::executor::block_on(plugin.perform(&cx, &HookAction::Compact))
            .expect_err("a hook that does not declare an action cannot perform it");
        assert_eq!(error.class, ErrorClass::Invalid);
        assert!(
            error.reason.contains("does not declare"),
            "{error}"
        );
    }

    /// A declared action is a `HookAction::Declared { name, version }` with the version the plugin
    /// declared. An action has no reason and is a decision.
    #[test]
    fn a_declared_action_carries_name_and_version() {
        let action = HookAction::Declared {
            name: "demo.refresh".into(),
            version: 2,
        };
        match &action {
            HookAction::Declared { name, version } => {
                assert_eq!(name, "demo.refresh");
                assert_eq!(*version, 2);
            }
            _ => panic!("expected Declared"),
        }
    }
}
