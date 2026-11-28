//! The doubles and the conformance cases of [`adam_plugin::Plugin`] (ADR 0050, slice H1).
//!
//! [`plugin_conformance!`] takes a factory (`fn() -> HookSet`) and states, once, what every hook set
//! must do: run its plugins in order, let the first non-`Continue` verdict decide, refuse on an
//! error, catch a panic as `Invalid`, retry a transient failure once and then deny, and enforce the
//! deadline.
//!
//! Test code, not a product: the doubles are small and the macro is meant to be called from a crate's
//! own tests.

#![warn(missing_docs)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use adam_error::ErrorClass;
use adam_plugin::{
    Decision, HookError, HookEvent, HookResult, HookedCall, HookedMessage, HookedText, Plugin,
    Subscriptions,
};

/// A plugin that lets every call through, counting how often it was asked.
#[derive(Default)]
pub struct Allows {
    /// How often `message_received` ran.
    pub messages: AtomicUsize,
    /// How often `before_tool` ran.
    pub before: AtomicUsize,
    /// How often `after_tool` ran.
    pub after: AtomicUsize,
}

impl Plugin for Allows {
    fn name(&self) -> &str {
        "allows"
    }

    fn message_received(&self, _message: &mut HookedMessage<'_>) -> HookResult {
        self.messages.fetch_add(1, Ordering::SeqCst);
        Ok(Decision::Continue)
    }

    fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
        self.before.fetch_add(1, Ordering::SeqCst);
        Ok(Decision::Continue)
    }

    fn after_tool(&self, _call: &HookedCall<'_>, _output: &mut HookedText<'_>) -> HookResult {
        self.after.fetch_add(1, Ordering::SeqCst);
        Ok(Decision::Continue)
    }
}

/// A plugin that answers `verdict` at `before_tool`, again and again.
pub struct Answers(pub Decision);

impl Plugin for Answers {
    fn name(&self) -> &str {
        "answers"
    }

    fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
        Ok(self.0.clone())
    }
}

/// A plugin that answers `verdict` at `message_received`, again and again.
pub struct AnswersMessage(pub Decision);

impl Plugin for AnswersMessage {
    fn name(&self) -> &str {
        "answers-message"
    }

    fn message_received(&self, _message: &mut HookedMessage<'_>) -> HookResult {
        Ok(self.0.clone())
    }
}

/// A plugin that fails with `class` `times` times, then lets the call through. `u8::MAX`: never.
pub struct Fails {
    class: ErrorClass,
    times: AtomicUsize,
    remaining: usize,
}

impl Fails {
    /// A plugin failing `times` times with `class`.
    #[must_use]
    pub fn new(class: ErrorClass, times: usize) -> Self {
        Fails {
            class,
            times: AtomicUsize::new(0),
            remaining: times,
        }
    }

    /// A plugin that always fails with `class`.
    #[must_use]
    pub fn always(class: ErrorClass) -> Self {
        Self::new(class, usize::MAX)
    }
}

impl Plugin for Fails {
    fn name(&self) -> &str {
        "fails"
    }

    fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
        let seen = self.times.fetch_add(1, Ordering::SeqCst);
        if seen < self.remaining {
            Err(HookError::new("fails", self.class, "on purpose"))
        } else {
            Ok(Decision::Continue)
        }
    }
}

/// A plugin that panics at `before_tool`.
pub struct Panics;

impl Plugin for Panics {
    fn name(&self) -> &str {
        "panics"
    }

    fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
        panic!("the panics double panics on purpose");
    }
}

/// A plugin that rewrites the arguments: `arguments["rewritten"] = true`.
pub struct Rewrites;

impl Plugin for Rewrites {
    fn name(&self) -> &str {
        "rewrites"
    }

    fn before_tool(&self, call: &mut HookedCall<'_>) -> HookResult {
        if let Some(object) = call.arguments.as_object_mut() {
            object.insert("rewritten".into(), serde_json::Value::Bool(true));
        }
        Ok(Decision::Continue)
    }
}

/// A plugin that sleeps past any deadline it is given.
pub struct Sleeps(pub Duration);

impl Plugin for Sleeps {
    fn name(&self) -> &str {
        "sleeps"
    }

    fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
        std::thread::sleep(self.0);
        Ok(Decision::Continue)
    }
}

/// An observer that records every event it is offered (ADR 0050, slice H5), with the subscriptions
/// it is given.
pub struct Records {
    subscriptions: Subscriptions,
    seen: Mutex<Vec<HookEvent>>,
}

impl Records {
    /// An observer that subscribes to `subscriptions`.
    #[must_use]
    pub fn new(subscriptions: Subscriptions) -> Self {
        Self {
            subscriptions,
            seen: Mutex::new(Vec::new()),
        }
    }

    /// The events it was offered, in order.
    pub fn events(&self) -> Vec<HookEvent> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The names of the events it was offered, in order.
    pub fn names(&self) -> Vec<String> {
        self.events().iter().map(|e| e.name().to_owned()).collect()
    }
}

impl Plugin for Records {
    fn name(&self) -> &str {
        "records"
    }

    fn subscriptions(&self) -> Subscriptions {
        self.subscriptions.clone()
    }

    fn observe(&self, event: &HookEvent) -> Result<(), HookError> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event.clone());
        Ok(())
    }
}

/// The cases every hook set must pass about custom events (ADR 0050, slice H5), against a factory:
/// what the set's plugins declare is valid and agrees with itself, every subscription can match
/// something, and an observer is offered a custom event only when its subscriptions name it.
///
/// Invoked at module level, like [`plugin_conformance!`]; the two do not share a name.
#[macro_export]
macro_rules! events_conformance {
    ($make:expr) => {
        #[test]
        fn the_declared_events_are_valid_and_agree_with_each_other() {
            let set = ($make)();
            adam_plugin::declared(set.declared_events().iter())
                .expect("every declared event is valid, and two of a name are equal");
        }

        #[test]
        fn every_subscription_can_match_something() {
            let set = ($make)();
            set.check_events(&[])
                .expect("each pattern's namespace is core or declared by the set");
        }

        #[test]
        fn an_observer_is_offered_a_custom_event_only_when_it_subscribes() {
            let header = || adam_plugin::EventHeader {
                key: "r/tool:c/probe.hit/0".into(),
                run: "r".into(),
                root_run: "r".into(),
                agent: "a".into(),
                at: "t".into(),
            };
            let custom = |name: &str| {
                adam_plugin::HookEvent::custom(name, 1, header(), &serde_json::json!({}), &|s| {
                    s.to_owned()
                })
            };
            let probe = std::sync::Arc::new($crate::Records::new(
                adam_plugin::Subscriptions::of(["probe.*"]).expect("a namespace pattern"),
            ));
            let core =
                std::sync::Arc::new($crate::Records::new(adam_plugin::Subscriptions::core()));
            let set = ($make)().with(probe.clone()).with(core.clone());
            let report = set.observe(&[
                custom("probe.hit"),
                custom("elsewhere.miss"),
                adam_plugin::HookEvent::new(adam_plugin::EventKind::ToolCompleted, header()),
            ]);
            assert_eq!(report.failed, 0);
            assert_eq!(
                probe.names(),
                ["probe.hit"],
                "its namespace, and no core event"
            );
            assert_eq!(
                core.names(),
                ["tool.completed"],
                "the default is the core catalogue"
            );
        }
    };
}

/// The cases every hook set must pass, against a factory.
///
/// ```ignore
/// #[test]
/// fn conformance() {
///     plugin_conformance!(|| HookSet::new().with_limits(HookLimits { deadline: Duration::from_millis(50), retries: 1 }));
/// }
/// ```
#[macro_export]
macro_rules! plugin_conformance {
    ($make:expr) => {
        fn a_call() -> serde_json::Value {
            serde_json::json!({"path": "/tmp"})
        }

        #[test]
        fn an_empty_set_lets_the_call_through() {
            let set = ($make)();
            let mut arguments = a_call();
            let mut call = adam_plugin::HookedCall {
                tool: "ls",
                arguments: &mut arguments,
            };
            assert_eq!(
                set.before_tool(&mut call).expect("no plugin, no failure"),
                adam_plugin::Decision::Continue
            );
        }

        #[test]
        fn plugins_run_in_order_and_the_first_verdict_decides() {
            let first = std::sync::Arc::new($crate::Allows::default());
            let second = std::sync::Arc::new($crate::Allows::default());
            let set = ($make)().with(first.clone()).with(second.clone());
            let mut arguments = a_call();
            let mut call = adam_plugin::HookedCall {
                tool: "ls",
                arguments: &mut arguments,
            };
            assert_eq!(set.before_tool(&mut call).expect("all continue"), adam_plugin::Decision::Continue);
            assert_eq!(first.before.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(second.before.load(std::sync::atomic::Ordering::SeqCst), 1);

            // A deny stops the set: the plugin after it is not asked.
            let denying = std::sync::Arc::new($crate::Answers(adam_plugin::Decision::Deny("no".into())));
            let after = std::sync::Arc::new($crate::Allows::default());
            let set = ($make)().with(denying).with(after.clone());
            assert_eq!(
                set.before_tool(&mut call).expect("a verdict, not a failure"),
                adam_plugin::Decision::Deny("no".into())
            );
            assert_eq!(after.before.load(std::sync::atomic::Ordering::SeqCst), 0);
        }

        #[test]
        fn ask_and_stop_are_verdicts_too() {
            for verdict in [adam_plugin::Decision::Ask("may I?".into()), adam_plugin::Decision::Stop("enough".into())] {
                let set = ($make)().with(std::sync::Arc::new($crate::Answers(verdict.clone())));
                let mut arguments = a_call();
                let mut call = adam_plugin::HookedCall {
                    tool: "ls",
                    arguments: &mut arguments,
                };
                assert_eq!(set.before_tool(&mut call).expect("a verdict"), verdict);
            }
        }

        #[test]
        fn an_ask_lets_the_plugins_after_it_refuse() {
            // An ask does not end the set (slice H8): the plugin after it is asked, and its denial wins.
            let after = std::sync::Arc::new($crate::Allows::default());
            let set = ($make)()
                .with(std::sync::Arc::new($crate::Answers(adam_plugin::Decision::Ask("may I?".into()))))
                .with(after.clone());
            let mut arguments = a_call();
            let mut call = adam_plugin::HookedCall {
                tool: "ls",
                arguments: &mut arguments,
            };
            assert_eq!(
                set.before_tool(&mut call).expect("a verdict"),
                adam_plugin::Decision::Ask("may I?".into()),
                "an ask that no plugin after it refuses is the set's"
            );
            assert_eq!(after.before.load(std::sync::atomic::Ordering::SeqCst), 1, "the plugin after the ask is asked");

            let set = ($make)()
                .with(std::sync::Arc::new($crate::Answers(adam_plugin::Decision::Ask("may I?".into()))))
                .with(std::sync::Arc::new($crate::Answers(adam_plugin::Decision::Deny("no".into()))));
            assert_eq!(
                set.before_tool(&mut call).expect("a verdict"),
                adam_plugin::Decision::Deny("no".into()),
                "a denial after an ask wins"
            );

            let set = ($make)()
                .with(std::sync::Arc::new($crate::Answers(adam_plugin::Decision::Ask("first?".into()))))
                .with(std::sync::Arc::new($crate::Answers(adam_plugin::Decision::Ask("second?".into()))));
            assert_eq!(
                set.before_tool(&mut call).expect("a verdict"),
                adam_plugin::Decision::Ask("first?".into()),
                "the first question stands"
            );
        }

        #[test]
        fn a_rewrite_is_visible_to_the_next_plugin() {
            let set = ($make)().with(std::sync::Arc::new($crate::Rewrites));
            let mut arguments = a_call();
            let mut call = adam_plugin::HookedCall {
                tool: "ls",
                arguments: &mut arguments,
            };
            assert_eq!(set.before_tool(&mut call).expect("a rewrite continues"), adam_plugin::Decision::Continue);
            assert_eq!(call.arguments["rewritten"], serde_json::json!(true));
        }

        #[test]
        fn a_panicking_hook_denies_with_invalid() {
            let set = ($make)().with(std::sync::Arc::new($crate::Panics));
            let mut arguments = a_call();
            let mut call = adam_plugin::HookedCall {
                tool: "ls",
                arguments: &mut arguments,
            };
            let error = set.before_tool(&mut call).expect_err("a panic denies");
            assert_eq!(error.class, adam_error::ErrorClass::Invalid);
            assert!(!error.is_retryable());
        }

        #[test]
        fn an_invalid_hook_denies_and_a_permanent_one_is_not_retried() {
            let set = ($make)().with(std::sync::Arc::new($crate::Fails::always(adam_error::ErrorClass::Invalid)));
            let mut arguments = a_call();
            let mut call = adam_plugin::HookedCall {
                tool: "ls",
                arguments: &mut arguments,
            };
            let error = set.before_tool(&mut call).expect_err("invalid denies");
            assert_eq!(error.class, adam_error::ErrorClass::Invalid);
        }

        #[test]
        fn a_transient_failure_is_retried_once_then_succeeds() {
            let set = ($make)().with(std::sync::Arc::new($crate::Fails::new(adam_error::ErrorClass::Transient, 1)));
            let mut arguments = a_call();
            let mut call = adam_plugin::HookedCall {
                tool: "ls",
                arguments: &mut arguments,
            };
            assert_eq!(
                set.before_tool(&mut call).expect("retried once, then continues"),
                adam_plugin::Decision::Continue
            );
        }

        #[test]
        fn a_transient_failure_past_its_retries_denies() {
            let set = ($make)().with(std::sync::Arc::new($crate::Fails::always(adam_error::ErrorClass::Transient)));
            let mut arguments = a_call();
            let mut call = adam_plugin::HookedCall {
                tool: "ls",
                arguments: &mut arguments,
            };
            let error = set.before_tool(&mut call).expect_err("still failing denies");
            assert!(error.is_retryable(), "{error}");
        }

        #[test]
        fn a_hook_past_its_deadline_is_transient_and_denies() {
            let set = ($make)().with(std::sync::Arc::new($crate::Sleeps(std::time::Duration::from_secs(2))));
            let mut arguments = a_call();
            let mut call = adam_plugin::HookedCall {
                tool: "ls",
                arguments: &mut arguments,
            };
            let error = set.before_tool(&mut call).expect_err("a sleeping hook denies");
            assert!(error.is_retryable(), "{error}");
        }

        #[test]
        fn after_tool_runs_its_plugins_too() {
            let plugin = std::sync::Arc::new($crate::Allows::default());
            let set = ($make)().with(plugin.clone());
            let mut arguments = a_call();
            let call = adam_plugin::HookedCall {
                tool: "ls",
                arguments: &mut arguments,
            };
            let mut text = String::from("answer");
            let mut output = adam_plugin::HookedText { text: &mut text };
            assert_eq!(
                set.after_tool(&call, &mut output).expect("allows"),
                adam_plugin::Decision::Continue
            );
            assert_eq!(plugin.after.load(std::sync::atomic::Ordering::SeqCst), 1);
        }

        #[test]
        fn a_message_runs_its_plugins_and_a_refusal_decides() {
            let plugin = std::sync::Arc::new($crate::Allows::default());
            let set = ($make)().with(plugin.clone());
            let mut text = String::from("hello");
            assert!(
                set.message_received(&mut adam_plugin::HookedMessage {
                    text: &mut text,
                    files: &[],
                })
                .expect("allows")
                .is_none(),
                "every plugin continued"
            );
            assert_eq!(plugin.messages.load(std::sync::atomic::Ordering::SeqCst), 1);

            let set = ($make)().with(std::sync::Arc::new($crate::AnswersMessage(adam_plugin::Decision::Deny("no".into()))));
            let mut text = String::from("hello");
            let decided = set
                .message_received(&mut adam_plugin::HookedMessage {
                    text: &mut text,
                    files: &[],
                })
                .expect("a verdict")
                .expect("a refusal");
            assert_eq!(decided.verdict, adam_plugin::Decision::Deny("no".into()));
            assert_eq!(decided.plugin, "answers-message");
        }
    };
}
