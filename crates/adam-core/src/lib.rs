//! Core traits and types for adam-rs.
//!
//! This crate is deliberately small: it defines the seams (starting with
//! [`store::Store`]) that every backend adapter implements, so a developer can
//! pick Postgres, MongoDB, or their own storage without touching agent code.

pub mod secrets;
pub mod store;
// Test support for this workspace's database-gated suites, not part of the
// supported API.
#[doc(hidden)]
pub mod testing;
pub mod time;

pub use secrets::{DynSecretSource, NoSecrets, ProcessEnv, SecretError, SecretSource};
pub use store::memory::MemoryStore;
pub use store::{
    BlobId, ClaimScope, ConversationScope, DynStore, JournalEntry, Lease, NewPushConfig, NewRun,
    PushProgress, PushRecord, PushState, RunHeader, RunId, RunQuery, RunRecord, RunStatus,
    RunUpdate, Store, StoreError, StoreResult, claimed, storable, storable_owned, storable_text,
};
pub use time::Timestamp;
