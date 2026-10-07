//! The `agent-registry/v1` registry of the v0 operator: the document
//! builder ([`document`]) and an axum router ([`Registry::router`]) that serves it from an
//! [`adam_operator_ports::AgentDirectory`].
//!
//! * [`document::build`] is pure: directory entries in, the linkset's bytes and its `ETag` out, with the
//!   contract's limits refused rather than truncated.
//! * [`Registry`] is the server's state and [`Registry::router`] its routes: `GET` and `HEAD` of
//!   [`PATH`], one static bearer ([`Token`]) compared in constant time, a strong `ETag` with `304`,
//!   `Cache-Control: private, max-age=30` and `Vary: Authorization`.
//!
//! The crate depends on the ports and not on the controller or any provider: the directory is
//! whatever implements the trait, and the controller learns that the registry is full through a flag the
//! composition root shares between the two ([`Registry::with_full_flag`]).

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod document;
mod server;
mod token;

pub use document::{Built, Overflow, Skipped, build};
pub use server::{MAX_AGE_SECS, Outcome, PATH, Registry};
pub use token::{Token, TokenError};
