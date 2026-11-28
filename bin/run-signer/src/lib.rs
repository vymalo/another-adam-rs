#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod broker;
pub mod config;
mod error;
pub mod issuer;
pub mod keyrings;
pub mod proxy;
pub mod statement;

pub use broker::{Broker, from_lookup_env, from_process_env};
pub use config::{ConfigError, DEFAULT_LISTEN, Limits, SignerConfig};
pub use error::{EX_CONFIG, EX_OSERR, SignerError, exit_code};
pub use issuer::IssuerFactory;
pub use keyrings::{Keyrings, Refusal};
pub use proxy::{Signer, run, serve};
pub use statement::{PromptIdentity, StatementError, person_and_jti};
