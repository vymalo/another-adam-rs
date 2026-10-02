//! What the output of a tool may say to a person or a model: scrubbed, short, and plain.
//!
//! The devcontainer CLI and Podman write build logs, and a repository's own commands write whatever
//! they like. Before any of it becomes the text of an [`EnvError`](adam_workspace::EnvError) or the
//! detail of a step, it passes through [`scrub`]: escape sequences are gone, a secret this process
//! knows is replaced, and a `NAME=value` whose name looks like a credential has no value.

use std::fmt::Write as _;

/// The text shown in place of a secret.
pub(crate) const MASK: &str = "***";

/// Lines of a log that an error carries.
pub(crate) const TAIL_LINES: usize = 40;

/// Longest line of a log that is kept whole.
const MAX_LINE: usize = 600;

/// Words that make the name of a variable one whose value is a secret.
const SECRET_NAMES: [&str; 8] = [
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "KEY",
    "CREDENTIAL",
    "AUTHORIZATION",
    "COOKIE",
];

/// `text` without escape sequences and carriage returns, with every known secret and every
/// credential-looking assignment masked.
///
/// `secrets` are values this process holds (the model key): a value shorter than 6 bytes is not
/// masked, because it would also mask ordinary words.
pub(crate) fn scrub(text: &str, secrets: &[&str]) -> String {
    let mut out = strip_escapes(text);
    for secret in secrets.iter().filter(|s| s.len() >= 6) {
        out = out.replace(secret, MASK);
    }
    out.lines()
        .map(mask_assignments)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The last `lines` lines of `text`, each at most `MAX_LINE` bytes.
pub(crate) fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = all.len().saturating_sub(lines);
    all[start..]
        .iter()
        .map(|l| clip(l, MAX_LINE))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `text` cut to at most `max` bytes at a character boundary, with an ellipsis when it was cut.
pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_owned();
    out.push('…');
    out
}

/// Remove ANSI escape sequences (colours, cursor moves) and carriage returns.
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // CSI: ESC [ ... final byte in @..~ ; OSC: ESC ] ... BEL or ESC \ ; else one more char.
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        for n in chars.by_ref() {
                            if ('@'..='~').contains(&n) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        while let Some(n) = chars.next() {
                            if n == '\u{7}' {
                                break;
                            }
                            if n == '\u{1b}' {
                                chars.next();
                                break;
                            }
                        }
                    }
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// One line with `NAME=value` and `Bearer value` masked where the name looks like a credential.
fn mask_assignments(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while !rest.is_empty() {
        // A word is what is between spaces; an assignment is a word with an `=` in it.
        let (word, tail) = match rest.find(' ') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if out.ends_with("Bearer ") || out.ends_with("bearer ") {
            out.push_str(MASK);
        } else if let Some((name, _)) = word.split_once('=') {
            let upper = name.to_ascii_uppercase();
            if !name.is_empty() && SECRET_NAMES.iter().any(|w| upper.contains(w)) {
                let _ = write!(out, "{name}={MASK}");
            } else {
                out.push_str(word);
            }
        } else {
            out.push_str(word);
        }
        // The separator and the following spaces are kept as they were.
        let spaces = tail.len() - tail.trim_start_matches(' ').len();
        out.push_str(&tail[..spaces]);
        rest = &tail[spaces..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_carriage_returns_are_removed() {
        let text = "\u{1b}[1mRunning the postCreateCommand...\u{1b}[0m\r\nnext\u{1b}]0;title\u{7}";
        assert_eq!(scrub(text, &[]), "Running the postCreateCommand...\nnext");
    }

    #[test]
    fn a_secret_this_process_holds_is_masked_wherever_it_is() {
        let text = "curl -H 'x-key: sk-live-0123456789' https://example.com/sk-live-0123456789";
        let out = scrub(text, &["sk-live-0123456789"]);
        assert!(!out.contains("sk-live"), "{out}");
        assert_eq!(out.matches(MASK).count(), 2);
    }

    #[test]
    fn a_short_secret_is_left_alone_so_that_words_survive() {
        assert_eq!(scrub("a key is here", &["key"]), "a key is here");
    }

    #[test]
    fn a_credential_looking_assignment_loses_its_value() {
        let out = scrub(
            "ENV GITHUB_TOKEN=ghp_abc PATH=/usr/bin API_KEY=abc  password=hunter2",
            &[],
        );
        assert_eq!(
            out,
            "ENV GITHUB_TOKEN=*** PATH=/usr/bin API_KEY=***  password=***"
        );
    }

    #[test]
    fn a_bearer_token_is_masked() {
        assert_eq!(
            scrub("Authorization: Bearer abc.def.ghi next", &[]),
            "Authorization: Bearer *** next"
        );
    }

    #[test]
    fn the_tail_is_the_last_lines_without_blanks_and_long_lines_are_cut() {
        let text = (1..=100)
            .map(|n| format!("line {n}\n\n"))
            .collect::<String>();
        let out = tail(&text, 3);
        assert_eq!(out, "line 98\nline 99\nline 100");
        let long = "x".repeat(5_000);
        assert!(tail(&long, 1).chars().count() <= MAX_LINE + 1);
    }

    #[test]
    fn clip_never_splits_a_character() {
        assert_eq!(clip("héllo", 2), "h…");
        assert_eq!(clip("abc", 10), "abc");
    }
}
