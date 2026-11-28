//! The skills section of an agent's system prompt ([ADR 0054], slice 7).
//!
//! The loop knows no skill file. It renders what a [`SkillCatalog`] gives it, and the assembly
//! implements that from its own skill set. **One function** renders the section for the folder's
//! own selection and for a session's narrower one ([`SkillCatalog::section`] with `None` or with
//! the names), so the two can never say different things about the same skill.
//!
//! [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md

use std::sync::Arc;

/// What an agent's system prompt says about its skills: the catalog of the skills left to load and
/// the bodies of the preloaded ones.
///
/// `selected` is the session's `skills` ([`adam_runtime::SessionConfig::skills`]), or `None` for
/// the folder's own selection. A selection narrows the catalog and **leaves the preloaded skills
/// alone**: they are the folder's own instructions, in the prompt before any session said
/// anything.
pub trait SkillCatalog: Send + Sync {
    /// The section for `selected`, the folder's own selection when it is `None`. An empty string
    /// when the selection leaves nothing to say (it is then left out of the prompt).
    fn section(&self, selected: Option<&[String]>) -> String;
}

/// A shared [`SkillCatalog`].
pub type DynSkillCatalog = Arc<dyn SkillCatalog>;

/// The system prompt of a request: the folder's own text, then the skills section, then whatever
/// follows it in every request (the session tools' guide), each after a blank line.
///
/// It is the one composition of the three parts: the assembly asks it for the prompt it reports,
/// and the loop asks it again for each request with the session's own selection, so a root's
/// prompt and a session's are the same bytes for the same parts ([ADR 0044]'s stable prefix
/// included). `None` when every part is empty or absent.
///
/// [ADR 0044]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0044-paged-tools-a-stable-prompt-prefix-a-plan-skills-commands-and-project-context.md
#[must_use]
pub fn system_prompt(
    base: Option<&str>,
    catalog: Option<&str>,
    tail: Option<&str>,
) -> Option<String> {
    let parts: Vec<&str> = [base, catalog, tail]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_parts_are_joined_with_a_blank_line_and_an_empty_one_is_left_out() {
        assert_eq!(
            system_prompt(Some("a"), Some("b"), Some("c")).as_deref(),
            Some("a\n\nb\n\nc")
        );
        assert_eq!(system_prompt(Some("a"), None, None).as_deref(), Some("a"));
        assert_eq!(
            system_prompt(Some("a"), Some(""), Some("c")).as_deref(),
            Some("a\n\nc")
        );
        assert_eq!(system_prompt(None, Some(""), None), None);
    }
}
