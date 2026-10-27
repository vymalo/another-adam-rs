//! What the environment of a command may hold.
//!
//! The server builds a command's environment from the request, but only from **names it allows**,
//! and adds its own `PATH`. A name that is not here is dropped, wherever it came from: the
//! environment of a client is full of things the sandbox must not receive, and the one place a
//! credential travels in the coder's `git` is `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>` and
//! `GIT_CONFIG_VALUE_<n>` (an `http.extraHeader`), which are not allowed. A command that carries a
//! token cannot be sent to the sandbox by mistake: the token is dropped on the way in.
//!
//! Both ends apply the same list: the client so that a secret never leaves its process, the server
//! because it does not trust its client.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;

/// The `PATH` every command gets, whatever the request says: what a filter or an alias needs
/// (`sh`, `cat`, `env`) and git's own directory, and nothing a client or a repository chose.
pub const FIXED_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// The names a command may be given.
///
/// * the locale and the time zone, and `HOME` (where git looks for the user's ignore file);
/// * the variables that **say which repository git works on and who wrote the commit**
///   (`GIT_DIR`, `GIT_COMMON_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, the object directories,
///   `GIT_CEILING_DIRECTORIES`, `GIT_AUTHOR_*`, `GIT_COMMITTER_*`);
/// * the ones that say which configuration files git reads (`GIT_CONFIG_GLOBAL`,
///   `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM`) and how it behaves (`GIT_TERMINAL_PROMPT`,
///   `GIT_OPTIONAL_LOCKS`, the pathspec switches, `GIT_NO_REPLACE_OBJECTS`).
///
/// Not here, on purpose: `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>`, `GIT_CONFIG_VALUE_<n>` and
/// `GIT_CONFIG_PARAMETERS` (configuration, and the way a token travels); `GIT_EXEC_PATH`,
/// `GIT_ASKPASS`, `SSH_ASKPASS`, `GIT_SSH`, `GIT_SSH_COMMAND`, `GIT_PROXY_COMMAND`, `GIT_EDITOR`,
/// `GIT_PAGER`, `GIT_EXTERNAL_DIFF` and `GIT_TEMPLATE_DIR` (programs and files git would run or
/// copy); `GIT_TRACE*` (output in files); `TMPDIR`, proxies, certificates and every other
/// variable of the client.
pub const ALLOWED: &[&str] = &[
    "HOME",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "TZ",
    "GIT_DIR",
    "GIT_COMMON_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_COMMITTER_DATE",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_TERMINAL_PROMPT",
    "GIT_OPTIONAL_LOCKS",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
];

/// Whether a command may be given a variable called `name`.
pub fn is_allowed(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    ALLOWED.iter().any(|allowed| allowed.as_bytes() == bytes)
}

/// The pairs of `offered` the command may have, once each (the last value of a name wins), and the
/// fixed `PATH` first. A name that cannot be a name (empty, with `=` or a NUL) and a value with a
/// NUL are dropped.
pub fn filter<I>(offered: I) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut kept: Vec<(OsString, OsString)> = Vec::new();
    for (name, value) in offered {
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes.contains(&b'=')
            || bytes.contains(&0)
            || value.as_bytes().contains(&0)
            || !is_allowed(&name)
        {
            continue;
        }
        match kept.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = value,
            None => kept.push((name, value)),
        }
    }
    let mut env = Vec::with_capacity(kept.len() + 1);
    env.push((OsString::from("PATH"), OsString::from(FIXED_PATH)));
    env.extend(kept);
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        list.iter()
            .map(|(n, v)| (OsString::from(n), OsString::from(v)))
            .collect()
    }

    #[test]
    fn only_the_allowed_names_and_a_fixed_path_survive() {
        let kept = filter(pairs(&[
            ("PATH", "/evil/bin"),
            ("MODEL_API_KEY", "sk-secret"),
            ("DATABASE_URL", "postgres://u:p@h/db"),
            ("GITHUB_APP_PRIVATE_KEY_PATH", "/secrets/key.pem"),
            ("GIT_DIR", "/work/git/x.git"),
            ("GIT_AUTHOR_NAME", "Ada"),
            ("HOME", "/home/agent"),
            ("TZ", "UTC"),
            ("LC_ALL", "C"),
        ]));
        let names: Vec<_> = kept.iter().map(|(n, _)| n.to_str().unwrap()).collect();
        assert_eq!(
            names,
            ["PATH", "GIT_DIR", "GIT_AUTHOR_NAME", "HOME", "TZ", "LC_ALL"]
        );
        assert_eq!(kept[0].1, FIXED_PATH, "the client's PATH is not kept");
    }

    #[test]
    fn the_way_a_token_travels_is_not_allowed() {
        for name in [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_PARAMETERS",
        ] {
            assert!(!is_allowed(OsStr::new(name)), "{name}");
        }
        let kept = filter(pairs(&[
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "http.https://github.com/.extraHeader"),
            ("GIT_CONFIG_VALUE_0", "Authorization: Basic eC1hY2Nlc3M="),
        ]));
        assert_eq!(kept.len(), 1, "only PATH is left: {kept:?}");
    }

    #[test]
    fn programs_and_files_git_would_run_are_not_allowed() {
        for name in [
            "GIT_EXEC_PATH",
            "GIT_ASKPASS",
            "SSH_ASKPASS",
            "GIT_SSH",
            "GIT_SSH_COMMAND",
            "GIT_PROXY_COMMAND",
            "GIT_EDITOR",
            "GIT_PAGER",
            "GIT_EXTERNAL_DIFF",
            "GIT_TEMPLATE_DIR",
            "GIT_TRACE",
            "TMPDIR",
            "HTTPS_PROXY",
            "LD_PRELOAD",
        ] {
            assert!(!is_allowed(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn a_name_is_kept_once_and_the_last_value_wins() {
        let kept = filter(pairs(&[("TZ", "UTC"), ("LC_ALL", "C"), ("TZ", "CET")]));
        let tz: Vec<_> = kept.iter().filter(|(n, _)| n == "TZ").collect();
        assert_eq!(tz.len(), 1);
        assert_eq!(tz[0].1, "CET");
    }

    #[test]
    fn what_cannot_be_an_environment_variable_is_dropped() {
        let offered = vec![
            (OsString::from(""), OsString::from("x")),
            (OsString::from("TZ=UTC"), OsString::from("x")),
            (OsString::from("TZ"), OsString::from("a\0b")),
            (OsString::from("LC_ALL"), OsString::from("C")),
        ];
        let kept = filter(offered);
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert_eq!(kept[1].0, "LC_ALL");
    }
}
