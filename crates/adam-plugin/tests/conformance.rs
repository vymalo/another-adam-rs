//! The conformance cases of `adam-plugin`, against a set with a 500 ms deadline and one retry. The deadline is well
//! above a loaded runner's scheduling delay, because a hook that misses it is retried and the counting cases count
//! every call; the deadline case sleeps 2 s, past it.
//!
//! The macro is invoked at module level: it defines the `#[test]` functions itself.
#![allow(clippy::expect_used, clippy::unwrap_used, missing_docs)]

use std::time::Duration;

use adam_plugin::{HookLimits, HookSet};

adam_plugin_testkit::plugin_conformance!(|| HookSet::new().with_limits(HookLimits {
    deadline: Duration::from_millis(500),
    retries: 1,
}));

adam_plugin_testkit::events_conformance!(|| HookSet::new().with_limits(HookLimits {
    deadline: Duration::from_millis(500),
    retries: 1,
}));
