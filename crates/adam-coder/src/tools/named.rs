//! Which repositories the person named.
//!
//! The coder works on the repository the person asked for and on no other. A model that is
//! given too little (a greeting, a vague task) may invent a repository, and a weak one does, so
//! the rule is code: `prepare_workspace` refuses a repository whose key is not among the keys of
//! the repositories named in the person's own messages (see
//! [`RunNotes::named_repos`](super::notes::RunNotes::named_repos)).
//!
//! A key is `host/owner/name`, lowercase, without scheme, credentials, `.git` or a trailing
//! slash; a port stays in the host as `host_port`, as the workspace layout spells it, and a
//! local repository is `local/<hash of the path>/<name>`. The same key comes out of every way of
//! writing the repository:
//!
//! | Written | Key |
//! |---|---|
//! | `https://github.com/Acme/Widgets.git` | `github.com/acme/widgets` |
//! | `github.com/acme/widgets/` | `github.com/acme/widgets` |
//! | `acme/widgets` (the default host) | `github.com/acme/widgets` |
//! | `git@github.com:acme/widgets.git` | `github.com/acme/widgets` |
//! | `http://git-server:8080/local/sandbox.git` | `git-server_8080/local/sandbox` |
//! | `/srv/git/sandbox.git` (a local path) | `local/<hash>/sandbox` |

use adam_workspace::{RepoLocation, RepoRef};

/// The host of a repository written as `owner/name`.
const DEFAULT_HOST: &str = "github.com";

/// The key of the repository a tool argument names, or `None` when the workspace would refuse
/// its form anyway (it is not an http(s) URL or an absolute path).
pub fn key_of_argument(url: &str) -> Option<String> {
    RepoRef::new(url.trim(), "main")
        .locate()
        .ok()
        .map(|loc| key_of_location(&loc))
}

fn key_of_location(loc: &RepoLocation) -> String {
    let key = format!("{}/{}/{}", loc.host, loc.owner, loc.name);
    if loc.is_local() {
        // A path is case-sensitive, and its hash is already lowercase.
        key
    } else {
        key.to_ascii_lowercase()
    }
}

/// The keys of every repository `text` names, in order of appearance, without repeats.
///
/// Reads words, not sentences: a word is a repository when it is an absolute path, a URL
/// (`scheme://host/owner/name`, anything after the name is ignored), `host/owner/name` with a
/// dotted or ported host, `git@host:owner/name`, or `owner/name`. A word that is a file path
/// also reads as `owner/name`; that only matters when the model asks for exactly that repository.
pub fn named_in(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for word in text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\''
                    | '`'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | ','
                    | ';'
                    | '|'
                    | '*'
            )
    }) {
        if let Some(key) = key_of_word(word)
            && !keys.contains(&key)
        {
            keys.push(key);
        }
    }
    keys
}

fn key_of_word(word: &str) -> Option<String> {
    let word = word.trim_end_matches(['.', ':', '!', '?']);
    let word = word.split(['?', '#']).next().unwrap_or(word);
    if word.starts_with('/') || word.starts_with("file://") {
        let loc = RepoRef::new(word, "main").locate().ok()?;
        return loc.is_local().then(|| key_of_location(&loc));
    }
    // `git@host:owner/name` is `host/owner/name`.
    let scp;
    let word = match word.split_once('@') {
        Some((_, rest)) if !word.contains("://") && rest.contains(':') => {
            scp = rest.replacen(':', "/", 1);
            scp.as_str()
        }
        _ => word,
    };
    let (scheme, rest) = match word.split_once("://") {
        Some((scheme, rest))
            if !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) =>
        {
            (true, rest)
        }
        Some(_) => return None,
        None => (false, word),
    };
    let mut segments = rest.split('/').filter(|s| !s.is_empty());
    let first = segments.next()?;
    // Credentials are not part of the host.
    let first = first.rsplit('@').next().unwrap_or(first);
    let hosted = scheme || first.contains('.') || first.contains(':') || first == "localhost";
    let (host, owner, name) = if hosted {
        (
            first.to_ascii_lowercase().replace(':', "_"),
            segments.next()?,
            segments.next()?,
        )
    } else {
        let name = segments.next()?;
        if segments.next().is_some() {
            return None;
        }
        (DEFAULT_HOST.to_owned(), first, name)
    };
    let name = name.strip_suffix(".git").unwrap_or(name);
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{host}/{owner}/{name}").to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> String {
        let keys = named_in(text);
        assert_eq!(keys.len(), 1, "{text:?} -> {keys:?}");
        keys.into_iter().next().unwrap()
    }

    #[test]
    fn every_way_of_writing_a_repository_gives_one_key() {
        for text in [
            "https://github.com/Acme/Widgets.git",
            "https://github.com/acme/widgets",
            "http://github.com/acme/widgets/",
            "github.com/acme/widgets",
            "GitHub.com/ACME/widgets.git",
            "acme/widgets",
            "Acme/Widgets.git",
            "git@github.com:acme/widgets.git",
            "ssh://git@github.com/acme/widgets.git",
            "https://user:pw@github.com/acme/widgets.git",
            "https://github.com/acme/widgets/tree/main/src",
            "In (https://github.com/acme/widgets), add a file.",
            "see `acme/widgets`, please",
            "fix it in acme/widgets.",
            "**https://github.com/acme/widgets**",
        ] {
            assert_eq!(one(text), "github.com/acme/widgets", "{text}");
        }
    }

    #[test]
    fn a_port_stays_in_the_host_as_the_workspace_layout_spells_it() {
        let expected = "git-server_8080/local/sandbox";
        assert_eq!(one("http://git-server:8080/local/sandbox.git"), expected);
        assert_eq!(one("git-server:8080/local/sandbox"), expected);
        assert_eq!(
            key_of_argument("http://git-server:8080/local/sandbox.git").as_deref(),
            Some(expected)
        );
        assert_eq!(
            one("In http://git-server:8080/local/sandbox.git (base branch main), add hello.txt."),
            expected
        );
    }

    #[test]
    fn the_argument_and_the_words_agree() {
        for arg in [
            "https://github.com/Acme/Widgets.git",
            " https://github.com/acme/widgets/ ",
            "http://github.com/acme/widgets",
        ] {
            assert_eq!(
                key_of_argument(arg).as_deref(),
                Some("github.com/acme/widgets"),
                "{arg}"
            );
        }
    }

    #[test]
    fn local_paths_are_named_by_the_path() {
        let key = one("the remote is at /srv/git/sandbox.git, use it");
        assert_eq!(key_of_argument("/srv/git/sandbox.git"), Some(key.clone()));
        assert_eq!(key_of_argument("/srv/git/sandbox.git/"), Some(key.clone()));
        assert_eq!(key_of_argument("file:///srv/git/sandbox.git"), Some(key));
        assert_ne!(
            key_of_argument("/srv/git/sandbox.git"),
            key_of_argument("/srv/other/sandbox.git"),
            "the same name elsewhere is another repository"
        );
        assert_ne!(
            key_of_argument("/srv/git/Sandbox.git"),
            key_of_argument("/srv/git/sandbox.git"),
            "paths are case-sensitive"
        );
    }

    #[test]
    fn plain_words_name_nothing() {
        for text in [
            "Hi",
            "Hi! I need a repository, a base branch and the task.",
            "add hello.txt containing hello",
            "use main",
            "",
            "https://example.com",
            "https://example.com/only-one-segment",
            "a/b/c",
        ] {
            let keys = named_in(text);
            assert!(keys.is_empty(), "{text:?} -> {keys:?}");
        }
    }

    #[test]
    fn several_repositories_are_all_named_once() {
        assert_eq!(
            named_in(
                "compare acme/widgets with https://github.com/acme/gadgets.git and acme/widgets"
            ),
            ["github.com/acme/widgets", "github.com/acme/gadgets"]
        );
    }

    #[test]
    fn the_key_of_an_unusable_argument_is_none() {
        for arg in [
            "",
            "acme/widgets",
            "relative/path.git",
            "ftp://h/o/r",
            "https://h/only",
        ] {
            assert_eq!(key_of_argument(arg), None, "{arg}");
        }
    }
}
