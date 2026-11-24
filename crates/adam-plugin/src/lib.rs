//! Plugins around the agent loop (ADR 0050, slice **H1**): the vocabulary, and the shapes the loop
//! records when it uses them.
//!
//! A **plugin** is a compiled hook set: it may rewrite a tool call before it runs, refuse it, ask a
//! person, stop the run, or rewrite what the tool answered. Three rules make it safe:
//!
//! 1. **Fail closed.** A hook that errors, panics or misses its deadline never lets the call through:
//!    [`HookSet`] answers an error, and the caller denies. A panic is [`ErrorClass::Invalid`]: a
//!    plugin that cannot run is buggy, not unlucky.
//! 2. **A transient failure is retried, once.** [`HookError::is_retryable`] says whether the call may
//!    be tried again in the same step (a deadline is `Transient`); past the retries the call is
//!    denied like any other error.
//! 3. **Order is the declaration order**, and the first verdict other than [`Verdict::Continue`] is
//!    the set's: a later plugin is not asked once one has decided.
//!
//! The loop runs a set **inside** the journaled step `tool:<call id>`, with
//! `LlmAgentBuilder::hooks` of `adam-llm-agent`: after a source's `withhold` and before the tool for
//! `before_tool`, and once the tool answered for `after_tool`. The verdict is recorded beside the
//! call's output, so a replay that finds the step recorded asks no hook again. A hook gets no `Ctx`
//! and cannot add a step.

#![warn(missing_docs)]

mod events;

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

    /// Before the call runs: rewrite its arguments, refuse it, ask, or stop the run.
    fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
        Ok(Verdict::Continue)
    }

    /// After the tool answered: rewrite what the model will read, or refuse it.
    fn after_tool(&self, _call: &HookedCall<'_>, _output: &mut HookedText<'_>) -> HookResult {
        Ok(Verdict::Continue)
    }

    /// An event of the catalogue happened (ADR 0050, slice H2). An observer only reads: it runs
    /// outside the journal, its failure is counted and dropped, and it cannot change the run. The
    /// event's `key` is stable across replays: a plugin that writes elsewhere dedupes on it.
    fn observe(&self, _event: &HookEvent) -> Result<(), HookError> {
        Ok(())
    }
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

/// An ordered set of plugins, with their limits.
#[derive(Default)]
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

    /// Runs every plugin's `before_tool` in order, until one decides.
    ///
    /// # Errors
    /// The first plugin that panics, errors or misses its deadline, after a retryable failure has
    /// been tried again [`HookLimits::retries`] times. The caller denies on an error: a hook that
    /// cannot say `Continue` never lets the call through.
    pub fn before_tool(&self, call: &mut HookedCall<'_>) -> HookResult {
        self.run("before_tool", |plugin| {
            guard(plugin.name(), self.limits.deadline, || {
                plugin.before_tool(call)
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

    fn run(&self, point: &str, mut one: impl FnMut(&Arc<dyn Plugin>) -> HookResult) -> HookResult {
        for plugin in &self.plugins {
            let mut attempt = 0u8;
            loop {
                match one(plugin) {
                    Ok(verdict) if verdict.decides() => return Ok(verdict),
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
        Ok(Verdict::Continue)
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
}
