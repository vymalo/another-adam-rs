//! The one word of a command line that the server's log may hold.
//!
//! A log line says which git command ran and nothing else of the command line: the arguments of a
//! `git add` are paths, those of a `git commit` a message, those of a `git -c` a configuration
//! value. [`subcommand`] finds the subcommand after git's own options and returns it only if it
//! looks like one.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;

use crate::protocol::WireStr;

/// The most characters of a subcommand the log keeps.
const MAX_LEN: usize = 32;

/// The options of `git` itself that take their value as the next argument.
const TAKES_VALUE: &[&str] = &[
    "-c",
    "-C",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--super-prefix",
    "--config-env",
];

/// The subcommand of `git <argv>`: the first argument that is not an option of git and not the value
/// of one. `?` when that word is not made of letters, digits, `-` and `_` (an alias can be called
/// anything, a log line must not hold anything), `-` when there is none.
pub fn subcommand(argv: &[WireStr]) -> String {
    let mut words = argv.iter().map(|a| a.as_os_str());
    while let Some(word) = words.next() {
        let bytes = word.as_bytes();
        if bytes.first() == Some(&b'-') {
            if TAKES_VALUE.iter().any(|o| o.as_bytes() == bytes) {
                words.next();
            }
            continue;
        }
        let name = word.to_string_lossy();
        let plain = !name.is_empty()
            && name.chars().count() <= MAX_LEN
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        return if plain {
            name.into_owned()
        } else {
            "?".to_owned()
        };
    }
    "-".to_owned()
}

/// [`subcommand`] of arguments the operating system holds.
pub fn subcommand_of_os(argv: &[OsString]) -> String {
    let wire: Vec<WireStr> = argv.iter().cloned().map(WireStr::from).collect();
    subcommand(&wire)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(argv: &[&str]) -> String {
        subcommand_of_os(&argv.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn the_subcommand_is_found_after_the_options_of_git() {
        assert_eq!(sub(&["status", "--porcelain"]), "status");
        assert_eq!(sub(&["add", "-A"]), "add");
        assert_eq!(
            sub(&[
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "gc.auto=0",
                "--git-dir=/work/git/x.git",
                "commit-tree",
                "abc",
                "-m",
                "a message"
            ]),
            "commit-tree"
        );
        assert_eq!(sub(&["--git-dir", "/work/git/x.git", "fetch"]), "fetch");
        assert_eq!(sub(&["-C", "/work", "log"]), "log");
        assert_eq!(sub(&["--no-pager", "diff"]), "diff");
    }

    #[test]
    fn a_word_that_is_not_a_subcommand_is_not_logged() {
        assert_eq!(sub(&[]), "-");
        assert_eq!(sub(&["-c", "a=b"]), "-");
        assert_eq!(sub(&["sk-secret=abc/def"]), "?");
        assert_eq!(sub(&["has space"]), "?");
        assert_eq!(sub(&[&"x".repeat(33)]), "?");
        assert_eq!(sub(&[&"x".repeat(32)]), "x".repeat(32));
        assert_eq!(sub(&["status\nforged line"]), "?");
    }
}
