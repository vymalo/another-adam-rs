// The macro pointed at `adam-llm-agent` directly: everything it emits goes
// through `::adam_llm_agent::__private`.
use adam_llm_agent::{Tool as _, ToolError, ToolOutput};

/// Echo.
#[adam::tool(crate = ::adam_llm_agent)]
async fn echo(
    /// Text
    text: String,
) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput::text(text))
}

fn main() {
    assert_eq!(Echo.spec().name, "echo");
}
