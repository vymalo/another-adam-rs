//! Core traits and types for adam-rs.
//!
//! This crate is deliberately small: it defines the seams (starting with
//! [`store::Store`]) that every backend adapter implements, so a developer can
//! pick Postgres, MongoDB, or their own storage without touching agent code.

pub mod store;
// Test support for this workspace's database-gated suites, not part of the
// supported API.
#[doc(hidden)]
pub mod testing;

pub use store::memory::MemoryStore;
pub use store::{
    DynStore, JournalEntry, Lease, NewRun, RunId, RunRecord, RunStatus, RunUpdate, Store,
    StoreError, StoreResult,
};
