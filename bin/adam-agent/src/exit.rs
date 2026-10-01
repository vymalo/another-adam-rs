//! How the binary ends: which exit code an error maps to.
//!
//! The codes, and the walk over the chain of causes from the outside in, are those of every agent
//! binary ([`adam_service::exit_code_with`], with the table in its README). What `adam-agent`
//! adds is the files: a folder that cannot be read, the files and the code disagreeing, and a
//! server of `mcp.json` that the policy refuses are the deployment's mistakes (78); an MCP server
//! that is down at startup is 69 (a supervisor restarts the process until it is up).

use std::error::Error;

use adam::AssemblyError;
use adam_error::{Classify, ErrorClass};

use crate::AgentError;

pub use adam_service::{EX_CONFIG, EX_GENERAL, EX_OSERR, EX_SOFTWARE, EX_UNAVAILABLE};

/// The exit code for `err`, found by walking its chain from the outside in.
pub fn exit_code(err: &(dyn Error + 'static)) -> u8 {
    adam_service::exit_code_with(err, class_of)
}

/// The class of `cause` when it is one of the errors of this binary's own steps.
fn class_of(cause: &(dyn Error + 'static)) -> Option<ErrorClass> {
    if let Some(e) = cause.downcast_ref::<AgentError>() {
        return e.own_class();
    }
    cause.downcast_ref::<AssemblyError>().map(Classify::class)
}
