#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod allow;
pub mod config;
mod error;
mod head;
pub mod ip;
mod refusal;
pub mod resolve;
mod server;

pub use allow::{AllowError, AllowList, HostName, NameError};
pub use config::{Config, ConfigError, DEFAULT_LISTEN, Limits};
pub use error::{EX_CONFIG, EX_OSERR, EgressError, exit_code};
pub use ip::{AddressFilter, Cidr, CidrError, Dropped};
pub use refusal::Refusal;
pub use resolve::{Resolver, SystemResolver};
pub use server::{Server, run};
