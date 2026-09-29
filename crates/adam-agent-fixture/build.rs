//! The whole build script of an agent crate: nothing but the call.

fn main() -> Result<(), adam_agent_fs::BuildError> {
    // The fixture shares the agent directory of the `adam-agent-fs` tests, so that both prove
    // their claims on the same files.
    adam_agent_fs::build("agent")
        .root("../adam-agent-fs/tests/fixtures/valid")
        .emit()?;
    Ok(())
}
