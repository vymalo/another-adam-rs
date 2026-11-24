//! The resource budget and the admission port: what a process may use at once, and the permit
//! that gives a share back.
//!
//! [ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
//! has three layers: **admission** (is there room in the budget?), **bulkheads** (each kind of
//! work has its own pool) and the operating system's own limits on every process adam starts.
//! This crate is the first two.
//!
//! * [`Budget`] is one ceiling: how many runs, tool calls, processes, git processes, MCP servers
//!   and model calls at once, an optional token rate, the memory and CPU of a command, the disk
//!   budget and the share one conversation may hold. [`Budget::lower`] is the only way to combine
//!   two budgets, so a file or a person can lower the ceiling and never raise it.
//! * [`Admission`] is the port: [`admit`](Admission::admit) asks for a share of one [`Pool`] and
//!   answers a [`Permit`] (which gives it back when it is dropped, so a cancelled or panicking
//!   holder frees it) or a [`Refused`] (which implements [`Classify`](adam_error::Classify), so a
//!   caller reacts to the class and never to a variant).
//!
//! The crate is pure: no I/O, no file system, no clock of the wall. An implementation comes from
//! another crate ([`adam-admission-local`](../adam-admission-local/README.md) is the in-process
//! one), and its conformance is proven by
//! [`admission_conformance!`](../adam-admission-testkit/README.md).
//!
//! ```no_run
//! use adam_budget::{Admission, Ask, Pool};
//!
//! # async fn run(admission: &dyn Admission) -> Result<(), adam_budget::Refused> {
//! let permit = Ask::new(Pool::ModelCalls, "conversation-1").admit(admission).await?;
//! // ... one model call ...
//! drop(permit); // the share is back
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod admission;
mod budget;

pub use admission::{
    Admission, Ask, ConversationId, DynAdmission, Permit, Pool, PoolUsage, Refused, TokenUsage,
    Usage,
};
pub use budget::{Budget, BudgetBuilder, DiskReserve, Ratio};
