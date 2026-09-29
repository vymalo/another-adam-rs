//! The coder's system prompt.

/// The prompt template; `{{MAX_CHECK_CYCLES}}` is replaced by [`instructions`].
const TEMPLATE: &str = include_str!("instructions.md");

/// The system prompt for an agent allowed `max_check_cycles` failed
/// `run_checks` calls.
///
/// The limit is also enforced in code (see [`crate::tools`]): the prompt tells
/// the model the rules, the tools make them hold.
pub fn instructions(max_check_cycles: u32) -> String {
    TEMPLATE.replace("{{MAX_CHECK_CYCLES}}", &max_check_cycles.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_is_templated_in() {
        let text = instructions(7);
        assert!(text.contains("at most 7 times"), "{text}");
        assert!(!text.contains("{{"), "unreplaced placeholder");
    }

    #[test]
    fn the_prompt_carries_the_rules_the_code_relies_on() {
        let text = instructions(3);
        for needle in [
            "Never open a pull request while the last check run failed",
            "accept_red_checks: true",
            "ask_user",
            "CLAUDE.md",
            "justfile",
            "small, focused commits",
            "verification section",
        ] {
            assert!(text.contains(needle), "prompt lost: {needle}");
        }
    }
}
