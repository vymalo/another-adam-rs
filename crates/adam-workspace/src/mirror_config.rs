//! The configuration of a mirror, put back to what the workspace writes before a command that carries
//! a token uses it.
//!
//! A mirror is bare, and shared by every run. The commands that carry a token (`fetch`, `ls-remote`,
//! `push`) read its `config`, and **others can write it**: a `filter` that runs in the git sandbox is
//! a process of the coder's user on the work volume, and the mirror is on it; so is a run's command in
//! local mode, which writes `user.email` with `git config` inside a worktree and lands in the mirror's
//! shared file. A `url.<x>.insteadOf` rewrites the URL a command is given, an `http.extraHeader` or a
//! `remote.origin.proxy` changes where and how the request goes, a `credential.helper` or
//! `core.sshCommand` is a program, an `include` reads another file: each would turn the request that
//! carries the token into one the workspace never asked for.
//!
//! So before every such command the configuration is **read, judged key by key against what the
//! workspace itself writes, and written back with only those entries** ([`repair`]). A list of what
//! stays, not of what goes, so a key nobody thought of goes too; and a mirror with a harmless key
//! (`user.name`, `gc.auto`) is put right rather than stopped: a refusal would block the repository
//! for every run until somebody edited the file by hand.
//!
//! What is kept (everything else is removed):
//!
//! | Key | Value |
//! |---|---|
//! | `core.repositoryformatversion` | `0` (anything else is reset to `0`) |
//! | `core.bare` | `true` (anything else is reset to `true`) |
//! | `core.filemode`, `core.ignorecase`, `core.precomposeunicode`, `core.symlinks` | `true` or `false` |
//! | `core.logallrefupdates` | `true`, `false` or `always` |
//! | `remote.origin.url` | the URL the workspace gives git for this repository (reset to it; always present) |
//! | `remote.origin.fetch` | `+refs/heads/*:refs/remotes/origin/*` (reset to it; always present) |
//! | `branch.agent/<name>.adam-run` | a run id |
//! | `branch.agent/<name>.remote` | `origin` |
//! | `branch.agent/<name>.merge` | `refs/heads/agent/<name>` |
//!
//! Each key once (the first valid one). `include.*`, `includeIf.*`, `url.*`, `http.*`, `credential.*`
//! and `core.sshCommand` are removed first, whatever else this table gets in the future
//! ([`ALWAYS_REFUSED`]).
//!
//! **How it is done.** The file is read as a regular file and not through a link
//! ([`safe_fs`](crate::safe_fs)), and parsed by `git config --file --no-includes --list` on a private
//! copy of the bytes that were read (parsing runs no program). When the entries to keep differ from
//! the ones found, a **new** file is built from them in a private directory, with `git config --file
//! <new> --add` on that new, empty file (git is never run on the hostile one), and it replaces
//! `config` atomically: a new regular file made next to it and renamed over it
//! ([`safe_fs::replace_file`](crate::safe_fs::replace_file)). A mirror that is already right is not
//! written. One warning per repair names the keys removed and reset, never a value.
//!
//! **What is still refused**, because git itself never makes it and only a planted file explains it:
//! a `config` that is not a regular file or is longer than [`MAX_CONFIG`], one git cannot parse, a
//! `HEAD` or `packed-refs` that is not a regular file, and a `commondir` in a bare mirror (git would
//! follow it to another repository's configuration).
//!
//! **What this does not close**: a writer in the sandbox that changes the file between the repair and
//! the network command's read of it. The window is the time between two statements of one task, under
//! the mirror's lock.

use std::path::Path;

use crate::workspace::{REMOTE_TRACKING_PREFIX, validate_run};

/// The most bytes of a mirror's `config` that are read.
pub(crate) const MAX_CONFIG: u64 = 1024 * 1024;

/// The keys that are removed whatever the table of kept ones says.
pub(crate) const ALWAYS_REFUSED: &[&str] = &[
    "include.",
    "includeif.",
    "url.",
    "http.",
    "credential.",
    "core.sshcommand",
];

/// What [`repair`] decided: the entries the configuration is to have, and which keys were removed
/// or reset to get there. The keys are names, never values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Repair {
    /// The entries to keep, in the order they were found, each key once; the two `remote.origin`
    /// keys at the end when they were missing.
    pub(crate) entries: Vec<(String, String)>,
    /// The keys that are not the workspace's, or occur again, once each.
    pub(crate) removed: Vec<String>,
    /// The keys that are the workspace's with a value it did not write, or were missing, once each.
    pub(crate) reset: Vec<String>,
}

impl Repair {
    /// Whether the configuration found is not the one to keep.
    pub(crate) fn changes_something(&self) -> bool {
        !self.removed.is_empty() || !self.reset.is_empty()
    }
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

/// What a key found in the configuration is.
enum Verdict {
    /// The workspace's, with a value it writes.
    Keep,
    /// The workspace's, with a value it did not write: the entry is kept with this value instead.
    Reset(String),
    /// Not the workspace's, or its value is none it could have written.
    Drop,
}

/// Judge one entry. `expected_url` is what `remote.origin.url` must be.
fn judge(key: &str, value: &str, expected_url: &str) -> Verdict {
    if ALWAYS_REFUSED
        .iter()
        .any(|refused| key.starts_with(refused) || key == *refused)
    {
        return Verdict::Drop;
    }
    let reset_unless = |valid: bool, to: String| {
        if valid {
            Verdict::Keep
        } else {
            Verdict::Reset(to)
        }
    };
    let keep_if = |valid: bool| if valid { Verdict::Keep } else { Verdict::Drop };
    match key {
        "core.repositoryformatversion" => reset_unless(value == "0", "0".to_owned()),
        "core.bare" => reset_unless(value == "true", "true".to_owned()),
        "core.filemode" | "core.ignorecase" | "core.precomposeunicode" | "core.symlinks" => {
            keep_if(matches!(value, "true" | "false"))
        }
        "core.logallrefupdates" => keep_if(matches!(value, "true" | "false" | "always")),
        "remote.origin.url" => {
            reset_unless(same_remote(value, expected_url), expected_url.to_owned())
        }
        "remote.origin.fetch" => reset_unless(value == fetch_refspec(), fetch_refspec()),
        other => match other
            .strip_prefix("branch.")
            .and_then(|k| k.rsplit_once('.'))
        {
            Some((name, variable)) if branch_key_is_ours(name, variable) => {
                keep_if(branch_value_is_ours(name, variable, value))
            }
            _ => Verdict::Drop,
        },
    }
}

/// What the configuration is to be, given what was found in it: only the entries the workspace
/// writes, with valid values, each key once; `remote.origin.url` and `remote.origin.fetch` as they
/// must be, and present. `expected_url` is what `remote.origin.url` must be.
pub(crate) fn repair(entries: &[(String, String)], expected_url: &str) -> Repair {
    let mut out = Repair::default();
    let note = |list: &mut Vec<String>, key: &str| {
        if !list.iter().any(|k| k == key) {
            list.push(key.to_owned());
        }
    };
    for (key, value) in entries {
        if out.entries.iter().any(|(k, _)| k == key) {
            // The key is already kept: this is a second value.
            note(&mut out.removed, key);
            continue;
        }
        match judge(key, value, expected_url) {
            Verdict::Keep => out.entries.push((key.clone(), value.clone())),
            Verdict::Reset(to) => {
                out.entries.push((key.clone(), to));
                note(&mut out.reset, key);
            }
            Verdict::Drop => note(&mut out.removed, key),
        }
    }
    for (key, value) in [
        ("remote.origin.url", expected_url.to_owned()),
        ("remote.origin.fetch", fetch_refspec()),
    ] {
        if !out.entries.iter().any(|(k, _)| k == key) {
            out.entries.push((key.to_owned(), value));
            note(&mut out.reset, key);
        }
    }
    out
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
            ("core.logallrefupdates", "true"),
        ])
    }

    #[test]
    fn what_the_workspace_writes_is_left_exactly_as_it_is() {
        let repair = repair(&ours(), URL);
        assert_eq!(repair.entries, ours(), "same entries, same order");
        assert!(!repair.changes_something(), "{repair:?}");
    }

    #[test]
    fn a_mirror_with_no_keys_gets_the_two_remote_keys() {
        let repair = repair(&[], URL);
        assert_eq!(
            repair.entries,
            pairs(&[
                ("remote.origin.url", URL),
                ("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
            ])
        );
        assert_eq!(repair.reset, ["remote.origin.url", "remote.origin.fetch"]);
        assert!(repair.removed.is_empty());
        assert!(repair.changes_something());
    }

    #[test]
    fn every_key_that_can_redirect_or_run_a_credentialed_command_is_removed() {
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
            "branch.main.remote",
            "branch.agent/x.pushremote",
            "branch.agent/x.rebase",
        ] {
            let mut entries = ours();
            entries.insert(2, (key.to_owned(), "x".to_owned()));
            entries.push((key.to_owned(), "y".to_owned()));
            let repair = repair(&entries, URL);
            assert_eq!(
                repair.entries,
                ours(),
                "{key}: only the workspace's keys are left"
            );
            assert_eq!(repair.removed, [key], "{key}");
            assert!(repair.reset.is_empty(), "{key}");
        }
    }

    /// What a person's command writes with `git config` in a worktree (it lands in the mirror's
    /// shared file), and what an earlier version left: harmless, and not the workspace's.
    #[test]
    fn a_harmless_key_is_removed_and_the_runs_branch_entries_stay() {
        let mut entries = ours();
        entries.insert(3, ("user.email".to_owned(), "ada@example.com".to_owned()));
        entries.insert(3, ("user.name".to_owned(), "Ada".to_owned()));
        entries.push(("gc.auto".to_owned(), "0".to_owned()));
        let repair = repair(&entries, URL);
        assert_eq!(repair.entries, ours());
        assert_eq!(repair.removed, ["user.name", "user.email", "gc.auto"]);
    }

    #[test]
    fn a_key_that_is_ours_with_a_value_that_is_not_is_reset_or_removed() {
        // The two remote keys and the two that decide what kind of repository git thinks it has:
        // put back, in place.
        for (key, value, to) in [
            (
                "remote.origin.url",
                "https://evil.example/acme/app.git",
                URL,
            ),
            ("remote.origin.url", "https://github.com/acme/app", URL),
            (
                "remote.origin.fetch",
                "+refs/heads/*:refs/heads/*",
                "+refs/heads/*:refs/remotes/origin/*",
            ),
            (
                "remote.origin.fetch",
                "+refs/*:refs/*",
                "+refs/heads/*:refs/remotes/origin/*",
            ),
            ("core.bare", "false", "true"),
            ("core.repositoryformatversion", "1", "0"),
        ] {
            let mut entries = ours();
            let at = entries.iter().position(|(k, _)| k == key).unwrap();
            entries[at].1 = value.to_owned();
            let repair = repair(&entries, URL);
            let mut want = ours();
            want[at].1 = to.to_owned();
            assert_eq!(repair.entries, want, "{key} = {value}");
            assert_eq!(repair.reset, [key], "{key} = {value}");
            assert!(repair.removed.is_empty(), "{key}");
        }
        // The others have no value to be put back to: they go.
        for (key, value) in [
            ("core.filemode", "$(touch /x)"),
            ("core.logallrefupdates", "sometimes"),
            ("branch.agent/0190aaaa.remote", "https://evil.example/x.git"),
            ("branch.agent/0190aaaa.remote", "/work/other.git"),
            ("branch.agent/0190aaaa.merge", "refs/heads/main"),
            ("branch.agent/0190aaaa.adam-run", "../../etc"),
            ("branch.agent/0190aaaa.adam-run", ""),
        ] {
            let mut entries = ours();
            let at = entries.iter().position(|(k, _)| k == key).unwrap();
            entries[at].1 = value.to_owned();
            let repair = repair(&entries, URL);
            let mut want = ours();
            want.remove(at);
            assert_eq!(repair.entries, want, "{key} = {value}");
            assert_eq!(repair.removed, [key], "{key} = {value}");
            assert!(repair.reset.is_empty(), "{key}");
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
        // The spelling the mirror was made with is kept.
        let mut entries = ours();
        entries[3].1 = "file:///srv/git/app.git".to_owned();
        let repair = repair(&entries, "/srv/git/app.git");
        assert_eq!(repair.entries, entries);
        assert!(!repair.changes_something());
    }

    #[test]
    fn a_key_that_occurs_twice_is_kept_once() {
        for key in ["remote.origin.url", "remote.origin.fetch", "core.bare"] {
            let mut entries = ours();
            let again = entries.iter().find(|(k, _)| k == key).unwrap().clone();
            entries.push(again);
            let repair = repair(&entries, URL);
            assert_eq!(repair.entries, ours(), "{key}");
            assert_eq!(repair.removed, [key], "{key}");
        }
        // The first value that is valid is the one kept: a wrong one first does not shadow it.
        let entries = pairs(&[("core.filemode", "maybe"), ("core.filemode", "false")]);
        let repair = repair(&entries, URL);
        assert_eq!(
            repair.entries[0],
            ("core.filemode".to_owned(), "false".to_owned())
        );
        assert_eq!(repair.removed, ["core.filemode"]);
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
            let repair = repair(&entries, URL);
            assert_eq!(repair.removed, [key], "{key}");
        }
        // A branch name with dots in it is still read from its last dot.
        let mut entries = pairs(&[
            ("branch.agent/a.b.adam-run", "run-1"),
            ("branch.agent/a.b.merge", "refs/heads/agent/a.b"),
        ]);
        let repair = repair(&entries, URL);
        entries.extend(pairs(&[
            ("remote.origin.url", URL),
            ("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
        ]));
        assert_eq!(repair.entries, entries);
        assert!(repair.removed.is_empty());
    }

    #[test]
    fn the_removed_keys_win_over_the_table_and_are_named_once() {
        let entries = pairs(&[
            ("http.extraheader", "a"),
            ("http.extraheader", "b"),
            ("url.x.insteadof", "c"),
        ]);
        let repair = repair(&entries, URL);
        assert_eq!(repair.removed, ["http.extraheader", "url.x.insteadof"]);
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
    fn repairing_what_was_repaired_changes_nothing() {
        let mut entries = ours();
        entries.push(("http.extraheader".to_owned(), "x".to_owned()));
        entries.push(("core.bare".to_owned(), "false".to_owned()));
        let first = repair(&entries, URL);
        assert!(first.changes_something());
        let second = repair(&first.entries, URL);
        assert_eq!(second.entries, first.entries);
        assert!(!second.changes_something(), "{second:?}");
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
