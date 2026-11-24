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
use std::time::Duration;

use adam_error::ErrorClass;
use adam_plugin::{HookError, HookResult, HookedCall, HookedMessage, HookedText, Plugin, Verdict};

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
        Ok(Verdict::Continue)
    }

    fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
        self.before.fetch_add(1, Ordering::SeqCst);
        Ok(Verdict::Continue)
    }

    fn after_tool(&self, _call: &HookedCall<'_>, _output: &mut HookedText<'_>) -> HookResult {
        self.after.fetch_add(1, Ordering::SeqCst);
        Ok(Verdict::Continue)
    }
}

/// A plugin that answers `verdict` at `before_tool`, again and again.
pub struct Answers(pub Verdict);

impl Plugin for Answers {
    fn name(&self) -> &str {
        "answers"
    }

    fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
        Ok(self.0.clone())
    }
}

/// A plugin that answers `verdict` at `message_received`, again and again.
pub struct AnswersMessage(pub Verdict);

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
            Ok(Verdict::Continue)
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
        Ok(Verdict::Continue)
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
        Ok(Verdict::Continue)
    }
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
                adam_plugin::Verdict::Continue
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
            assert_eq!(set.before_tool(&mut call).expect("all continue"), adam_plugin::Verdict::Continue);
            assert_eq!(first.before.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(second.before.load(std::sync::atomic::Ordering::SeqCst), 1);

            // A deny stops the set: the plugin after it is not asked.
            let denying = std::sync::Arc::new($crate::Answers(adam_plugin::Verdict::Deny("no".into())));
            let after = std::sync::Arc::new($crate::Allows::default());
            let set = ($make)().with(denying).with(after.clone());
            assert_eq!(
                set.before_tool(&mut call).expect("a verdict, not a failure"),
                adam_plugin::Verdict::Deny("no".into())
            );
            assert_eq!(after.before.load(std::sync::atomic::Ordering::SeqCst), 0);
        }

        #[test]
        fn ask_and_stop_are_verdicts_too() {
            for verdict in [adam_plugin::Verdict::Ask("may I?".into()), adam_plugin::Verdict::Stop("enough".into())] {
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
        fn a_rewrite_is_visible_to_the_next_plugin() {
            let set = ($make)().with(std::sync::Arc::new($crate::Rewrites));
            let mut arguments = a_call();
            let mut call = adam_plugin::HookedCall {
                tool: "ls",
                arguments: &mut arguments,
            };
            assert_eq!(set.before_tool(&mut call).expect("a rewrite continues"), adam_plugin::Verdict::Continue);
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
                adam_plugin::Verdict::Continue
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
            let set = ($make)().with(std::sync::Arc::new($crate::Sleeps(std::time::Duration::from_millis(40))));
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
                adam_plugin::Verdict::Continue
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

            let set = ($make)().with(std::sync::Arc::new($crate::AnswersMessage(adam_plugin::Verdict::Deny("no".into()))));
            let mut text = String::from("hello");
            let decided = set
                .message_received(&mut adam_plugin::HookedMessage {
                    text: &mut text,
                    files: &[],
                })
                .expect("a verdict")
                .expect("a refusal");
            assert_eq!(decided.verdict, adam_plugin::Verdict::Deny("no".into()));
            assert_eq!(decided.plugin, "answers-message");
        }
    };
}
