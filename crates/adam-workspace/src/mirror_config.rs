//! The configuration of a mirror, judged before a command that carries a token uses it.
//!
//! A mirror is bare, and shared by every run. The commands that carry a token (`fetch`, `ls-remote`,
//! `push`) read its `config`, and **others can write it**: a `filter` that runs in the git sandbox is
//! a process of the coder's user on the work volume, and the mirror is on it. A `url.<x>.insteadOf`
//! rewrites the URL a command is given, an `http.extraHeader` or a `remote.origin.proxy` changes where
//! and how the request goes, a `credential.helper` or `core.sshCommand` is a program, an `include`
//! reads another file: each would turn the request that carries the token into one the workspace
//! never asked for.
//!
//! So the configuration is **checked, key by key, against what the workspace itself writes**, and a
//! mirror with any other key is refused: the command is not started, the error names the keys (never
//! their values) and the mirror, and the refusal is logged. It is **not repaired**: rewriting the
//! file would hide that something wrote it, and the next run would meet the same writer again. The
//! mirror is the operator's to look at.
//!
//! What the workspace writes (everything else is refused):
//!
//! | Key | Value |
//! |---|---|
//! | `core.repositoryformatversion` | `0` |
//! | `core.bare` | `true` |
//! | `core.filemode`, `core.ignorecase`, `core.precomposeunicode`, `core.symlinks` | `true` or `false` |
//! | `core.logallrefupdates` | `true`, `false` or `always` |
//! | `remote.origin.url` | the URL the workspace gives git for this repository, exactly |
//! | `remote.origin.fetch` | `+refs/heads/*:refs/remotes/origin/*`, exactly |
//! | `branch.agent/<name>.adam-run` | a run id |
//! | `branch.agent/<name>.remote` | `origin` |
//! | `branch.agent/<name>.merge` | `refs/heads/agent/<name>` |
//!
//! Each key once. `include.*`, `includeIf.*`, `url.*`, `http.*`, `credential.*` and `core.sshCommand`
//! are refused first, whatever else this table gets in the future ([`ALWAYS_REFUSED`]).
//!
//! The file is read as a regular file and not through a link ([`safe_fs`](crate::safe_fs)), parsed
//! by `git config --file --no-includes --list` on a private copy of the bytes that were read (so what is
//! judged is what was read, and parsing runs no program), and `HEAD`, `packed-refs` and `commondir`
//! are looked at as files too. **What this does not close**: a writer that changes the file between
//! the check and the command's read of it. The window is the time between two statements of one task,
//! under the mirror's lock, and the writer must be a process already running in the sandbox.

use std::path::Path;

use crate::workspace::{REMOTE_TRACKING_PREFIX, validate_run};

/// The most bytes of a mirror's `config` that are read.
pub(crate) const MAX_CONFIG: u64 = 1024 * 1024;

/// The keys that are refused whatever the table of allowed ones says.
pub(crate) const ALWAYS_REFUSED: &[&str] = &[
    "include.",
    "includeif.",
    "url.",
    "http.",
    "credential.",
    "core.sshcommand",
];

/// A key that is not allowed, and why, in words that hold no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Violation {
    pub(crate) key: String,
    pub(crate) why: &'static str,
}

/// The refspec the workspace writes as `remote.origin.fetch`.
pub(crate) fn fetch_refspec() -> String {
    format!("+refs/heads/*:{REMOTE_TRACKING_PREFIX}*")
}

/// The `(key, value)` pairs of `git config --list -z`: `key\nvalue\0`, or `key\0` for a key without
/// a value.
pub(crate) fn parse_list(stdout: &[u8]) -> Vec<(String, String)> {
    stdout
        .split(|b| *b == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let text = String::from_utf8_lossy(entry);
            match text.split_once('\n') {
                Some((key, value)) => (key.to_owned(), value.to_owned()),
                None => (text.into_owned(), String::new()),
            }
        })
        .collect()
}

/// The entries of `entries` that the workspace did not write, once per key, in the order they come.
/// `expected_url` is what `remote.origin.url` must be.
pub(crate) fn judge(entries: &[(String, String)], expected_url: &str) -> Vec<Violation> {
    let mut found: Vec<Violation> = Vec::new();
    let mut note = |key: &str, why: &'static str| {
        if !found.iter().any(|v| v.key == key) {
            found.push(Violation {
                key: key.to_owned(),
                why,
            });
        }
    };
    let refspec = fetch_refspec();
    for (key, value) in entries {
        if ALWAYS_REFUSED
            .iter()
            .any(|refused| key.starts_with(refused) || key == refused)
        {
            note(key, "a key that can redirect or run a credentialed command");
            continue;
        }
        let times = entries.iter().filter(|(k, _)| k == key).count();
        let allowed = match key.as_str() {
            "core.repositoryformatversion" => value == "0",
            "core.bare" => value == "true",
            "core.filemode" | "core.ignorecase" | "core.precomposeunicode" | "core.symlinks" => {
                matches!(value.as_str(), "true" | "false")
            }
            "core.logallrefupdates" => matches!(value.as_str(), "true" | "false" | "always"),
            "remote.origin.url" => same_remote(value, expected_url),
            "remote.origin.fetch" => *value == refspec,
            other => match other
                .strip_prefix("branch.")
                .and_then(|k| k.rsplit_once('.'))
            {
                Some((name, variable)) if branch_key_is_ours(name, variable) => {
                    branch_value_is_ours(name, variable, value)
                }
                _ => {
                    note(key, "a key the workspace does not write");
                    continue;
                }
            },
        };
        if !allowed {
            note(key, "a value the workspace did not write");
        } else if times > 1 {
            note(key, "a key that occurs more than once");
        }
    }
    found
}

/// Whether `branch.<name>.<variable>` is one of the three keys the workspace writes for a run's
/// branch (`agent/<...>`), whatever its value.
fn branch_key_is_ours(name: &str, variable: &str) -> bool {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/');
    name.strip_prefix("agent/")
        .is_some_and(|rest| !rest.is_empty() && name.chars().all(plain))
        && matches!(variable, "adam-run" | "remote" | "merge")
}

/// Whether the value of a key of [`branch_key_is_ours`] is the one the workspace writes.
fn branch_value_is_ours(name: &str, variable: &str, value: &str) -> bool {
    match variable {
        "adam-run" => validate_run(value).is_ok(),
        "remote" => value == "origin",
        "merge" => value.strip_prefix("refs/heads/") == Some(name),
        _ => false,
    }
}

/// Whether `value` is the address of the repository the workspace was asked for. Exactly, for an
/// http(s) remote (the workspace writes the canonical URL). A filesystem remote has two spellings
/// that are one mirror (`/srv/r.git` and `file:///srv/r.git`) and the mirror keeps the one it was
/// made with, so those are compared without the scheme and without a trailing slash.
fn same_remote(value: &str, expected: &str) -> bool {
    let is_path = |url: &str| !url.contains("://") || url.starts_with("file://");
    let path = |url: &str| {
        url.strip_prefix("file://")
            .unwrap_or(url)
            .trim_end_matches('/')
            .to_owned()
    };
    value == expected || (is_path(value) && is_path(expected) && path(value) == path(expected))
}

/// The files of the mirror that git reads besides `config`, looked at without following a link:
/// `HEAD` must be a regular file, `packed-refs` a regular file when there is one, and there must be
/// no `commondir` (a bare mirror has none, and git would follow it to another repository's
/// configuration).
///
/// # Errors
///
/// What is wrong, in a sentence that names a file and no content.
pub(crate) async fn check_files(mirror: &Path) -> Result<(), String> {
    for (name, required) in [("HEAD", true), ("packed-refs", false)] {
        let path = mirror.join(name);
        match tokio::fs::symlink_metadata(&path).await {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => {
                return Err(format!(
                    "{} is not a regular file (a link, a pipe or a directory)",
                    path.display()
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => {}
            Err(e) => return Err(format!("cannot look at {}: {e}", path.display())),
        }
    }
    let commondir = mirror.join("commondir");
    match tokio::fs::symlink_metadata(&commondir).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot look at {}: {e}", commondir.display())),
        Ok(_) => Err(format!(
            "{} exists: a bare mirror has none, and git would follow it to another repository",
            commondir.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://github.com/acme/app.git";

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn keys(violations: &[Violation]) -> Vec<&str> {
        violations.iter().map(|v| v.key.as_str()).collect()
    }

    /// What `git init --bare` and the workspace leave.
    fn ours() -> Vec<(String, String)> {
        pairs(&[
            ("core.repositoryformatversion", "0"),
            ("core.filemode", "true"),
            ("core.bare", "true"),
            ("remote.origin.url", URL),
            ("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
            ("branch.agent/0190aaaa.adam-run", "0190aaaa-run-id"),
            ("branch.agent/0190aaaa.remote", "origin"),
            ("branch.agent/0190aaaa.merge", "refs/heads/agent/0190aaaa"),
        ])
    }

    #[test]
    fn what_the_workspace_writes_passes() {
        assert_eq!(judge(&ours(), URL), []);
        assert_eq!(
            judge(&[], URL),
            [],
            "a mirror with no keys at all is not a mirror to refuse"
        );
    }

    #[test]
    fn every_key_that_can_redirect_a_credentialed_command_is_refused() {
        for key in [
            "url.https://evil.example/.insteadof",
            "url.file:///evil.git.pushinsteadof",
            "include.path",
            "includeif.gitdir:/x.path",
            "http.extraheader",
            "http.https://github.com/.extraheader",
            "http.proxy",
            "credential.helper",
            "credential.https://github.com.username",
            "core.sshcommand",
            "remote.origin.pushurl",
            "remote.origin.proxy",
            "remote.origin.uploadpack",
            "remote.origin.receivepack",
            "remote.evil.url",
            "core.fsmonitor",
            "core.hookspath",
            "core.gitproxy",
            "core.askpass",
            "core.worktree",
            "core.alternaterefscommand",
            "extensions.worktreeconfig",
            "filter.x.clean",
            "diff.x.textconv",
            "alias.s",
            "user.name",
            "branch.main.remote",
            "branch.agent/x.pushremote",
            "branch.agent/x.rebase",
        ] {
            let mut entries = ours();
            entries.push((key.to_owned(), "x".to_owned()));
            assert_eq!(keys(&judge(&entries, URL)), [key], "{key}");
        }
    }

    #[test]
    fn a_key_that_is_ours_with_a_value_that_is_not_is_refused() {
        for (key, value) in [
            ("remote.origin.url", "https://evil.example/acme/app.git"),
            ("remote.origin.url", "https://github.com/acme/app"),
            ("remote.origin.fetch", "+refs/heads/*:refs/heads/*"),
            ("remote.origin.fetch", "+refs/*:refs/*"),
            ("core.bare", "false"),
            ("core.repositoryformatversion", "1"),
            ("core.filemode", "$(touch /x)"),
            ("core.logallrefupdates", "sometimes"),
            ("branch.agent/0190aaaa.remote", "https://evil.example/x.git"),
            ("branch.agent/0190aaaa.remote", "/work/other.git"),
            ("branch.agent/0190aaaa.merge", "refs/heads/main"),
            ("branch.agent/0190aaaa.adam-run", "../../etc"),
            ("branch.agent/0190aaaa.adam-run", ""),
        ] {
            let mut entries: Vec<_> = ours().into_iter().filter(|(k, _)| k != key).collect();
            entries.push((key.to_owned(), value.to_owned()));
            assert_eq!(keys(&judge(&entries, URL)), [key], "{key} = {value}");
        }
    }

    #[test]
    fn a_filesystem_remote_has_two_spellings_and_a_web_remote_has_one() {
        for (value, expected, same) in [
            ("/srv/git/app.git", "file:///srv/git/app.git", true),
            ("file:///srv/git/app.git", "/srv/git/app.git/", true),
            ("/srv/git/app.git", "/srv/git/other.git", false),
            ("/srv/git/app.git", "https://github.com/acme/app.git", false),
            (
                "https://github.com/acme/app.git",
                "https://github.com/acme/app.git",
                true,
            ),
            (
                "https://github.com/acme/app",
                "https://github.com/acme/app.git",
                false,
            ),
            (
                "https://evil.example/acme/app.git",
                "https://github.com/acme/app.git",
                false,
            ),
            (
                "ssh://git@github.com/acme/app.git",
                "/srv/git/app.git",
                false,
            ),
        ] {
            assert_eq!(same_remote(value, expected), same, "{value} vs {expected}");
        }
    }

    #[test]
    fn a_key_that_occurs_twice_is_refused() {
        for key in ["remote.origin.url", "remote.origin.fetch", "core.bare"] {
            let mut entries = ours();
            let again = entries.iter().find(|(k, _)| k == key).unwrap().clone();
            entries.push(again);
            assert_eq!(keys(&judge(&entries, URL)), [key], "{key}");
        }
    }

    #[test]
    fn branches_of_other_namespaces_are_not_the_workspaces() {
        for key in [
            "branch.main.adam-run",
            "branch.agent/.adam-run",
            "branch.feature/x.merge",
            "branch.agent/x y.adam-run",
        ] {
            let entries = pairs(&[(key, "origin")]);
            assert_eq!(keys(&judge(&entries, URL)), [key], "{key}");
        }
        // A branch name with dots in it is still read from its last dot.
        let entries = pairs(&[
            ("branch.agent/a.b.adam-run", "run-1"),
            ("branch.agent/a.b.merge", "refs/heads/agent/a.b"),
        ]);
        assert_eq!(judge(&entries, URL), []);
    }

    #[test]
    fn the_refused_keys_win_over_the_table_and_are_named_once() {
        let entries = pairs(&[
            ("http.extraheader", "a"),
            ("http.extraheader", "b"),
            ("url.x.insteadof", "c"),
        ]);
        let found = judge(&entries, URL);
        assert_eq!(keys(&found), ["http.extraheader", "url.x.insteadof"]);
        // None of the table's own keys could be one of them.
        for refused in ALWAYS_REFUSED {
            for allowed in [
                "core.repositoryformatversion",
                "core.bare",
                "remote.origin.url",
                "remote.origin.fetch",
                "branch.agent/x.merge",
            ] {
                assert!(!allowed.starts_with(refused), "{allowed} vs {refused}");
            }
        }
    }

    #[test]
    fn the_listing_of_git_is_read_as_pairs() {
        let listing =
            b"core.bare\ntrue\0remote.origin.url\nhttps://x/y.git\0some.flag\0multi.line\na\nb\0";
        assert_eq!(
            parse_list(listing),
            pairs(&[
                ("core.bare", "true"),
                ("remote.origin.url", "https://x/y.git"),
                ("some.flag", ""),
                ("multi.line", "a\nb"),
            ])
        );
        assert!(parse_list(b"").is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_files_of_a_mirror_are_regular_files_and_there_is_no_commondir() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let mirror = tmp.path().join("m.git");
        std::fs::create_dir(&mirror).unwrap();
        // No HEAD.
        assert!(check_files(&mirror).await.unwrap_err().contains("HEAD"));
        std::fs::write(mirror.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        check_files(&mirror).await.unwrap();
        std::fs::write(mirror.join("packed-refs"), "").unwrap();
        check_files(&mirror).await.unwrap();

        // A link in the place of either.
        let other = tmp.path().join("other");
        std::fs::write(&other, "x").unwrap();
        std::fs::remove_file(mirror.join("packed-refs")).unwrap();
        symlink(&other, mirror.join("packed-refs")).unwrap();
        assert!(
            check_files(&mirror)
                .await
                .unwrap_err()
                .contains("packed-refs")
        );
        std::fs::remove_file(mirror.join("packed-refs")).unwrap();
        std::fs::remove_file(mirror.join("HEAD")).unwrap();
        symlink(&other, mirror.join("HEAD")).unwrap();
        assert!(check_files(&mirror).await.unwrap_err().contains("HEAD"));
        std::fs::remove_file(mirror.join("HEAD")).unwrap();
        std::fs::write(mirror.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        // A directory where packed-refs goes, and a commondir.
        std::fs::create_dir(mirror.join("packed-refs")).unwrap();
        assert!(check_files(&mirror).await.is_err());
        std::fs::remove_dir(mirror.join("packed-refs")).unwrap();
        std::fs::write(mirror.join("commondir"), "../elsewhere\n").unwrap();
        assert!(
            check_files(&mirror)
                .await
                .unwrap_err()
                .contains("commondir")
        );
    }
}
