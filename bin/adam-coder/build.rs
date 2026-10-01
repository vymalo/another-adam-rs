//! Embeds `agent/` (the coder's prompt and A2A card) into the crate; see `agent/instructions.md`.

fn main() -> Result<(), adam_agent_fs::BuildError> {
    adam_agent_fs::build("agent").emit()?;
    Ok(())
}
