//! The conformance suite against the in-process reference.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use adam_memory::{DynSiMemory, InProcessSiMemory};
use adam_memory_testkit::Plan;

fn make(plan: &Plan) -> DynSiMemory {
    Arc::new(InProcessSiMemory::new(plan.settings.clone()))
}

adam_memory_testkit::si_memory_conformance!(crate::make);
