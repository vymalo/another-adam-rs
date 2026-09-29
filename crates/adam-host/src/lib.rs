//! The contract between adam-rs and a host app.
//!
//! adam-rs is a library: a host app embeds it, the way a NestJS app embeds NestJS. The host
//! decides how a process starts. This crate holds the two things every host shares:
//!
//! * [`Role`], the closed enum of process roles (`all`, `control-plane`, `worker`). The host
//!   only says which role a process runs; it never invents its own role names.
//! * [`Host`] (feature `supervisor`), a small role-aware supervisor. The host registers its
//!   components, and the supervisor runs only those that match the role, stops them in a fixed
//!   order and reports which one failed.
//!
//! Nothing here reads the environment or the command line. The host owns the name of the
//! variable (`ROLE`, `ORCH_ROLE`) or the flag, and passes the value to [`Role::from_optional`]
//! or, with feature `clap`, lets clap parse it.
//!
//! Future work: a `runtime` feature with the `adam-runtime` worker as a ready-made component.

#![warn(missing_docs)]

mod role;
pub use role::{ParseRoleError, Role};

#[cfg(feature = "supervisor")]
mod supervisor;
#[cfg(feature = "supervisor")]
pub use supervisor::{Health, Host, HostError};
