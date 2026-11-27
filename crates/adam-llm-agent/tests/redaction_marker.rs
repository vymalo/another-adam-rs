//! The redaction marker `adam-memory` copies is the one every redactor of adam-rs uses.
//!
//! `adam-memory` must not depend on `adam-llm-agent` (the agent will depend on the memory), so its
//! `REDACTION_MARKER` is a copy. This test is what keeps the copy honest (ADR 0070, ADR 0081
//! decision 5).

#[test]
fn the_memory_marker_is_the_agents_marker() {
    assert_eq!(
        adam_memory::REDACTION_MARKER,
        adam_llm_agent::REDACTED_MARKER
    );
}
