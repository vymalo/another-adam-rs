//! The `adam-hooks/1` adapter (ADR 0050, slice **P1**): a program on stdio, behind the
//! [`Plugin`](adam_plugin::Plugin) trait.
//!
//! A **program** is a long-lived child process that speaks one JSON object per line: the adapter
//! says hello, asks one `event` per hook call, reads a `reply` with a verdict, and says `shutdown`
//! at the end. What the loop decides is unchanged — the verdict is the program's, recorded in the
//! step it affects, and a replay asks no program again.
//!
//! ```mermaid
//! sequenceDiagram
//!     participant L as the loop's step
//!     participant H as HookExec
//!     participant P as the program
//!     H->>P: hello (adam-hooks/1, the points)
//!     P-->>H: hello
//!     L->>H: before_tool(call)
//!     H->>P: event id, decide, key, deadline, scrubbed payload
//!     P-->>H: reply continue, rewrite, deny, ask or stop
//!     H-->>L: the verdict, applied
//! ```
//!
//! Four rules make it safe, and each one is a test:
//!
//! 1. **Fail closed.** A program that misses its deadline, writes a broken line or dies gives a
//!    [`HookError`](adam_plugin::HookError), never a `Continue`: the call is refused.
//! 2. **It is killed and restarted.** Three misses in a row kill the program's process group, and
//!    its deciding points are `Transient` until the backoff elapsed.
//! 3. **It sees only what it should.** Payloads go through the deployment's scrubber, a rewrite over
//!    a scrubbed span is refused, and the child gets an empty environment, then [`SAFE_VARS`], then
//!    the variables the configuration adds (a plain upper-case name, never `PATH` or `LD_*`).
//! 4. **Nothing is read from a repository.** The configuration is a value the embedder passes; the
//!    file and the switch that read it from a deployment are slice P2.
//!
//! ```no_run
//! use adam_hook_exec::{Config, DecidePoint, HookExec, Points};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let config = Config::new("policy", "/usr/local/bin/policy-hook")
//!     .points(Points {
//!         decide: vec![DecidePoint::ToolBefore],
//!         observe: Vec::new(),
//!     });
//! let plugin = HookExec::start(config)?;
//! # let _ = plugin;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod config;
mod error;
mod plugin;
mod program;
mod protocol;

pub use config::{
    Config, DEFAULT_DEADLINE_MS, DecidePoint, HELLO_DEADLINE_MS, MAX_DEADLINE_MS, Points,
    SAFE_VARS, Scrub,
};
pub use error::Error;
pub use plugin::HookExec;
pub use protocol::{Hello, LINE_MAX, Mode, Outgoing, PROTOCOL, Reply, VERSION, WirePoints, Word};
