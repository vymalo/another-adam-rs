//! `{{name}}` placeholders: logic-free substitution into a prompt.
//!
//! The syntax is deliberately tiny. `{{name}}` (spaces inside the braces are ignored) is replaced
//! by the value of the var `name`. `{{{{` writes a literal `{{`, so a literal `{{name}}` is
//! written `{{{{name}}`: a `}}` in plain text is always itself. There are no conditionals, loops
//! or filters, and a placeholder that is not a valid var name is an error, never text.

use std::collections::BTreeMap;

use adam_agent_fs::is_env_name;

/// What is wrong with the `{{` a template contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateProblem {
    /// `{{` with no `}}` after it. To write the two characters, use `{{{{`.
    Unclosed,
    /// `{{}}`, or braces around blanks.
    Empty,
    /// Something between the braces that is not a var name (`^[A-Za-z_][A-Za-z0-9_]*$`).
    BadName(String),
}

/// How to write the two characters `{{` in a prompt.
const ESCAPE_HINT: &str = "(write `{{{{` for a literal `{{`)";

impl std::fmt::Display for TemplateProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unclosed => write!(f, "`{{{{` is never closed by `}}}}` {ESCAPE_HINT}"),
            Self::Empty => write!(f, "`{{{{}}}}` names no var {ESCAPE_HINT}"),
            Self::BadName(name) => write!(
                f,
                "`{{{{{name}}}}}` is not a var name: use letters, digits and `_` {ESCAPE_HINT}"
            ),
        }
    }
}

/// A piece of a parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    /// Text, with `{{{{` already turned into `{{`.
    Text(String),
    /// A placeholder, and the line it is on (1-based, within the template).
    Var { name: String, line: u32 },
}

/// A syntax error and the line it is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Syntax {
    pub(crate) line: u32,
    pub(crate) problem: TemplateProblem,
}

/// Split `text` into text and placeholders.
pub(crate) fn parse(text: &str) -> Result<Vec<Piece>, Syntax> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut rest = text;
    let mut line = 1_u32;
    while let Some(at) = rest.find("{{") {
        literal.push_str(&rest[..at]);
        line = line.saturating_add(newlines(&rest[..at]));
        let after = &rest[at..];
        if let Some(escaped) = after.strip_prefix("{{{{") {
            literal.push_str("{{");
            rest = escaped;
            continue;
        }
        let inner = &after[2..];
        let Some(end) = inner.find("}}") else {
            return Err(Syntax {
                line,
                problem: TemplateProblem::Unclosed,
            });
        };
        let name = inner[..end].trim();
        let problem = if name.is_empty() {
            Some(TemplateProblem::Empty)
        } else if !is_env_name(name) {
            Some(TemplateProblem::BadName(name.to_owned()))
        } else {
            None
        };
        if let Some(problem) = problem {
            return Err(Syntax { line, problem });
        }
        if !literal.is_empty() {
            pieces.push(Piece::Text(std::mem::take(&mut literal)));
        }
        pieces.push(Piece::Var {
            name: name.to_owned(),
            line,
        });
        line = line.saturating_add(newlines(&inner[..end]));
        rest = &inner[end + 2..];
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        pieces.push(Piece::Text(literal));
    }
    Ok(pieces)
}

fn newlines(text: &str) -> u32 {
    u32::try_from(text.matches('\n').count()).unwrap_or(u32::MAX)
}

/// Substitute the values. A name without a value is left as its placeholder, which the caller
/// has already ruled out.
pub(crate) fn render(pieces: &[Piece], values: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Text(text) => out.push_str(text),
            Piece::Var { name, .. } => match values.get(name) {
                Some(value) => out.push_str(value),
                None => {
                    out.push_str("{{");
                    out.push_str(name);
                    out.push_str("}}");
                }
            },
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn names(pieces: &[Piece]) -> Vec<&str> {
        pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Var { name, .. } => Some(name.as_str()),
                Piece::Text(_) => None,
            })
            .collect()
    }

    #[test]
    fn placeholders_are_replaced_and_spaces_inside_are_ignored() {
        let pieces = parse("at most {{n}} of {{ m }} and {{n}}").unwrap();
        assert_eq!(names(&pieces), ["n", "m", "n"]);
        assert_eq!(
            render(&pieces, &values(&[("n", "3"), ("m", "x")])),
            "at most 3 of x and 3"
        );
    }

    #[test]
    fn text_without_placeholders_is_unchanged() {
        let text = "no braces\nor a } lone brace, or {single} ones, or }} closers";
        assert_eq!(render(&parse(text).unwrap(), &values(&[])), text);
        assert_eq!(parse("").unwrap(), []);
    }

    #[test]
    fn four_braces_write_two() {
        let text = "a {{{{name}} b {{n}}";
        let pieces = parse(text).unwrap();
        assert_eq!(names(&pieces), ["n"]);
        assert_eq!(render(&pieces, &values(&[("n", "1")])), "a {{name}} b 1");
        // Escapes next to each other, and at the ends.
        assert_eq!(render(&parse("{{{{{{{{").unwrap(), &values(&[])), "{{{{");
    }

    #[test]
    fn a_value_is_not_scanned_again() {
        let pieces = parse("{{a}}").unwrap();
        assert_eq!(render(&pieces, &values(&[("a", "{{b}}")])), "{{b}}");
    }

    #[test]
    fn a_missing_value_leaves_the_placeholder() {
        assert_eq!(render(&parse("x {{a}}").unwrap(), &values(&[])), "x {{a}}");
    }

    #[test]
    fn syntax_errors_carry_the_line() {
        let unclosed = parse("one\ntwo {{name").unwrap_err();
        assert_eq!(unclosed.line, 2);
        assert_eq!(unclosed.problem, TemplateProblem::Unclosed);

        let empty = parse("{{ }}").unwrap_err();
        assert_eq!((empty.line, empty.problem), (1, TemplateProblem::Empty));

        // A placeholder that spans lines moves the count for what follows it.
        let bad = parse("{{a\n}}\n{{not a var}}").unwrap_err();
        assert_eq!(bad.line, 3);
        assert_eq!(bad.problem, TemplateProblem::BadName("not a var".into()));

        assert_eq!(
            parse("{{{x}}}").unwrap_err().problem,
            TemplateProblem::BadName("{x".into())
        );
        assert_eq!(
            parse("{{1a}}").unwrap_err().problem,
            TemplateProblem::BadName("1a".into())
        );
    }

    #[test]
    fn problems_say_how_to_escape() {
        for problem in [
            TemplateProblem::Unclosed,
            TemplateProblem::Empty,
            TemplateProblem::BadName("a b".into()),
        ] {
            assert!(problem.to_string().contains("{{{{"), "{problem}");
        }
        assert!(
            TemplateProblem::BadName("a b".into())
                .to_string()
                .contains("`{{a b}}`")
        );
    }

    proptest::proptest! {
        /// Whatever the text, parsing returns; and text with no `{{` in it is left alone.
        #[test]
        fn parsing_never_panics_and_plain_text_is_unchanged(text in "[a-z{}\n ]{0,80}") {
            let parsed = parse(&text);
            if !text.contains("{{") {
                let pieces = parsed.unwrap();
                proptest::prop_assert_eq!(render(&pieces, &BTreeMap::new()), text);
            }
        }

        /// A declared placeholder between any text is replaced by exactly its value.
        #[test]
        fn a_placeholder_is_replaced_by_its_value(
            before in "[a-z \n]{0,20}",
            after in "[a-z \n]{0,20}",
            value in "[a-z{}]{0,10}",
        ) {
            let pieces = parse(&format!("{before}{{{{v}}}}{after}")).unwrap();
            let got = render(&pieces, &values(&[("v", &value)]));
            proptest::prop_assert_eq!(got, format!("{before}{value}{after}"));
        }
    }
}
