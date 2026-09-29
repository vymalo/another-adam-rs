//! The name patterns, written as code so that no regex crate is needed.

/// `^[a-z0-9][a-z0-9_-]{0,63}$`: the name of an agent or a subagent.
pub fn is_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    first_ok
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The Agent Skills name rule: 1 to 64 characters of `a-z0-9-`, with no leading, trailing or
/// doubled hyphen.
pub fn is_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `^[a-z][a-z0-9_]{0,63}$`: the name of a tool.
pub fn is_tool_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `^[A-Za-z_][A-Za-z0-9_]*$`: an environment variable name.
pub fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names() {
        for ok in ["a", "coder", "a-b_c", "0x", &"a".repeat(64)] {
            assert!(is_agent_name(ok), "{ok}");
        }
        for bad in ["", "-a", "_a", "A", "a b", "a.b", &"a".repeat(65), "é"] {
            assert!(!is_agent_name(bad), "{bad}");
        }
    }

    #[test]
    fn skill_names() {
        for ok in ["a", "pdf-processing", "a1", &"a".repeat(64)] {
            assert!(is_skill_name(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "a--b", "A", "a_b", &"a".repeat(65)] {
            assert!(!is_skill_name(bad), "{bad}");
        }
    }

    #[test]
    fn tool_and_env_names() {
        assert!(is_tool_name("get_weather") && !is_tool_name("Read") && !is_tool_name("a-b"));
        assert!(!is_tool_name("_a") && !is_tool_name("1a") && !is_tool_name(""));
        assert!(
            is_env_name("_A1") && !is_env_name("1A") && !is_env_name("A-B") && !is_env_name("")
        );
    }
}
