//! Core traits and types for adam-rs.
//!
//! This crate is deliberately small: it defines the seams (starting with
//! [`store::Store`]) that every backend adapter implements, so a developer can
//! pick Postgres, MongoDB, or their own storage without touching agent code.

pub mod jcs;
pub mod secrets;
pub mod store;
// Test support for this workspace's database-gated suites, not part of the
// supported API.
#[doc(hidden)]
pub mod testing;
pub mod time;
pub mod turn;

pub use jcs::canonicalize;
pub use secrets::{DynSecretSource, NoSecrets, ProcessEnv, SecretError, SecretSource};
pub use store::memory::MemoryStore;
pub use store::{
    BlobId, BodyRow, BodyWrite, ClaimScope, ConversationScope, DynStore, JournalEntry, Lease,
    NewPushConfig, NewRun, PushProgress, PushRecord, PushState, RunHeader, RunId, RunQuery,
    RunRecord, RunStatus, RunUpdate, Store, StoreError, StoreResult, claimed, head_body,
    set_head_body, storable, storable_owned, storable_text,
};
pub use time::Timestamp;
pub use turn::{DynTurnKeys, KeySet, TurnKeys};
