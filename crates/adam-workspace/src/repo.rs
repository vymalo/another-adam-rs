//! Repository references and the on-disk layout derived from them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{WorkspaceError, WorkspaceResult};

/// A repository to work on and the branch new work is based on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    /// `https://github.com/owner/repo(.git)`, or (for tests and local
    /// mirrors) an absolute filesystem path / `file://` URL.
    ///
    /// The URL must not embed credentials; they are rejected.
    pub url: String,
    /// Branch new worktrees start from (`origin/<base_branch>`) and pull
    /// requests target.
    pub base_branch: String,
}

impl RepoRef {
    /// Build a reference.
    pub fn new(url: impl Into<String>, base_branch: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            base_branch: base_branch.into(),
        }
    }

    /// Parse [`RepoRef::url`] into host, owner and name.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] for unsupported schemes, embedded
    /// credentials, relative paths, or URLs that are not `host/owner/repo`.
    pub fn locate(&self) -> WorkspaceResult<RepoLocation> {
        RepoLocation::parse(&self.url)
    }
}

/// Where a repository lives, in the form the mirror layout needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoLocation {
    /// Lowercased host (with `:port` as `_port`), or `local` for filesystem
    /// remotes.
    pub host: String,
    /// Owner or organisation. For filesystem remotes, a stable hash of the
    /// path (so two different `.../remote.git` never share a mirror).
    pub owner: String,
    /// Repository name without the `.git` suffix.
    pub name: String,
    remote: Remote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Remote {
    /// `http(s)://host[:port]/`; the scope for the auth header.
    Http {
        scope: String,
    },
    Local,
}

impl RepoLocation {
    fn parse(raw: &str) -> WorkspaceResult<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(invalid("repository url is empty"));
        }
        if !raw.contains("://") {
            return Self::local(Path::new(raw));
        }
        let url = Url::parse(raw).map_err(|e| invalid(format!("bad repository url: {e}")))?;
        match url.scheme() {
            "http" | "https" => {
                if !url.username().is_empty() || url.password().is_some() {
                    return Err(invalid(
                        "repository urls must not embed credentials; pass them through GitCredentials",
                    ));
                }
                if url.query().is_some() || url.fragment().is_some() {
                    return Err(invalid("repository url must not have a query or fragment"));
                }
                let host = url
                    .host_str()
                    .ok_or_else(|| invalid("repository url has no host"))?
                    .to_ascii_lowercase();
                let segments: Vec<&str> = url
                    .path_segments()
                    .map(|s| s.filter(|p| !p.is_empty()).collect())
                    .unwrap_or_default();
                let [owner, name] = segments[..] else {
                    return Err(invalid(format!(
                        "expected {}://{host}/<owner>/<repo>, got path {:?}",
                        url.scheme(),
                        url.path()
                    )));
                };
                let name = name.strip_suffix(".git").unwrap_or(name);
                let (host_dir, scope) = match url.port() {
                    Some(port) => (
                        format!("{host}_{port}"),
                        format!("{}://{host}:{port}/", url.scheme()),
                    ),
                    None => (host.clone(), format!("{}://{host}/", url.scheme())),
                };
                Ok(Self {
                    host: path_component(&host_dir)?,
                    owner: path_component(owner)?,
                    name: path_component(name)?,
                    remote: Remote::Http { scope },
                })
            }
            "file" => {
                let path = url
                    .to_file_path()
                    .map_err(|()| invalid("file url is not a local path"))?;
                Self::local(&path)
            }
            other => Err(invalid(format!(
                "unsupported repository scheme {other:?} (use https or a local path)"
            ))),
        }
    }

    fn local(path: &Path) -> WorkspaceResult<Self> {
        if !path.is_absolute() {
            return Err(invalid(format!(
                "repository {path:?} is neither a URL nor an absolute path"
            )));
        }
        let text = path.to_string_lossy();
        let text = text.trim_end_matches('/');
        let file = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| invalid("repository path has no final component"))?;
        let name = file.strip_suffix(".git").unwrap_or(&file);
        Ok(Self {
            host: "local".to_owned(),
            owner: format!("{:016x}", fnv1a(text.as_bytes())),
            name: lossy_component(name),
            remote: Remote::Local,
        })
    }

    /// `git/<host>/<owner>/<name>.git`, relative to the workspace root.
    pub fn mirror_relative(&self) -> PathBuf {
        PathBuf::from("git")
            .join(&self.host)
            .join(&self.owner)
            .join(format!("{}.git", self.name))
    }

    /// Whether the remote is a filesystem path rather than an http(s) server.
    pub fn is_local(&self) -> bool {
        self.remote == Remote::Local
    }

    /// `scheme://host[:port]/`, the scope credentials are limited to, for
    /// http(s) remotes.
    pub(crate) fn http_scope(&self) -> Option<&str> {
        match &self.remote {
            Remote::Http { scope } => Some(scope),
            Remote::Local => None,
        }
    }
}

fn invalid(message: impl Into<String>) -> WorkspaceError {
    WorkspaceError::Invalid(message.into())
}

/// A path component that is safe to use as a directory name.
fn path_component(part: &str) -> WorkspaceResult<String> {
    let ok = !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if ok {
        Ok(part.to_owned())
    } else {
        Err(invalid(format!(
            "repository url component {part:?} has characters outside [A-Za-z0-9._-]"
        )))
    }
}

fn lossy_component(part: &str) -> String {
    let s: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s == "." || s == ".." {
        "repo".to_owned()
    } else {
        s
    }
}

/// FNV-1a, 64 bit. Stable across Rust versions (unlike `DefaultHasher`), which
/// matters because the result names directories that outlive the process.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(url: &str) -> WorkspaceResult<RepoLocation> {
        RepoRef::new(url, "main").locate()
    }

    #[test]
    fn parses_github_urls() {
        for url in [
            "https://github.com/vymalo/adam.git",
            "https://GitHub.com/vymalo/adam",
            "https://github.com/vymalo/adam/",
        ] {
            let l = loc(url).unwrap();
            assert_eq!(
                (l.host.as_str(), l.owner.as_str(), l.name.as_str()),
                ("github.com", "vymalo", "adam"),
                "{url}"
            );
            assert_eq!(
                l.mirror_relative(),
                PathBuf::from("git/github.com/vymalo/adam.git")
            );
            assert_eq!(l.http_scope(), Some("https://github.com/"));
            assert!(!l.is_local());
        }
    }

    #[test]
    fn keeps_the_port_out_of_the_host_dir_name() {
        let l = loc("http://127.0.0.1:8080/o/r.git").unwrap();
        assert_eq!(l.host, "127.0.0.1_8080");
        assert_eq!(l.http_scope(), Some("http://127.0.0.1:8080/"));
    }

    #[test]
    fn rejects_credentials_and_odd_urls() {
        for url in [
            "https://x-access-token:secret@github.com/o/r.git",
            "https://user@github.com/o/r.git",
            "https://github.com/o",
            "https://github.com/o/r/extra",
            "https://github.com/o/r?x=1",
            "ssh://git@github.com/o/r.git",
            "git@github.com:o/r.git",
            "relative/path",
            "",
        ] {
            assert!(
                matches!(loc(url), Err(WorkspaceError::Invalid(_))),
                "{url} should be rejected"
            );
        }
    }

    #[test]
    fn local_paths_get_a_stable_distinct_owner() {
        let a = loc("/tmp/a/remote.git").unwrap();
        let b = loc("/tmp/b/remote.git").unwrap();
        let a2 = loc("file:///tmp/a/remote.git").unwrap();
        assert_eq!(a.host, "local");
        assert_eq!(a.name, "remote");
        assert_ne!(a.owner, b.owner);
        assert_eq!(a.owner, a2.owner);
        assert!(a.is_local());
        assert_eq!(a.http_scope(), None);
    }

    mod prop {
        use proptest::prelude::*;

        use super::*;

        fn safe_component(part: &str) -> bool {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        }

        /// Whatever a location parsed from `raw` says, it is safe to use.
        fn assert_safe(loc: &RepoLocation) -> Result<(), TestCaseError> {
            prop_assert!(safe_component(&loc.host), "host {:?}", loc.host);
            prop_assert!(safe_component(&loc.owner), "owner {:?}", loc.owner);
            prop_assert!(safe_component(&loc.name), "name {:?}", loc.name);
            let rel = loc.mirror_relative();
            prop_assert!(rel.is_relative());
            prop_assert!(
                rel.components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
                "{rel:?}"
            );
            Ok(())
        }

        proptest! {
            /// A URL that embeds credentials is refused, however they are
            /// spelled (user only, user and password, percent-escapes, `@`
            /// and `:` inside the password): the secret never reaches a
            /// location, a mirror path or a scope.
            #[test]
            fn prop_parse_never_keeps_userinfo(
                scheme in prop_oneof![Just("http"), Just("https")],
                user in "[A-Za-z0-9._~%-]{1,12}",
                password in proptest::option::of("[A-Za-z0-9._~%:@!$&'()*+,;=-]{1,16}"),
                owner in "[a-z][a-z0-9-]{0,10}",
                name in "[a-z][a-z0-9._-]{0,10}",
            ) {
                let userinfo = match &password {
                    Some(p) => format!("{user}:{p}"),
                    None => user.clone(),
                };
                let url = format!("{scheme}://{userinfo}@github.com/{owner}/{name}.git");
                let parsed = loc(&url);
                prop_assert!(
                    matches!(parsed, Err(WorkspaceError::Invalid(_))),
                    "{url} was accepted: {parsed:?}"
                );
            }

            /// Any string that parses yields a location that is path-safe and
            /// whose http scope carries no userinfo, whatever URL tricks the
            /// input plays (backslashes, `@`, encoded dots, odd ports).
            #[test]
            fn prop_parse_accepts_only_safe_locations(
                rest in "[a-zA-Z0-9/:@.%#?\\\\_~ -]{0,40}",
                scheme in prop_oneof![Just("https://"), Just("http://"), Just("file://"), Just("")],
            ) {
                let raw = format!("{scheme}{rest}");
                if let Ok(location) = loc(&raw) {
                    assert_safe(&location)?;
                    if let Some(scope) = location.http_scope() {
                        let url = Url::parse(scope).expect("the scope is a URL");
                        prop_assert!(url.username().is_empty() && url.password().is_none());
                        prop_assert_eq!(url.path(), "/");
                    }
                    // Nothing after the credentials-free URL leaks: the
                    // original text with an `@` before the path never parses.
                    if raw.starts_with("http") && !location.is_local() {
                        let after_scheme = raw.split("://").nth(1).unwrap_or("");
                        let authority = after_scheme.split(['/', '\\', '?', '#']).next().unwrap_or("");
                        prop_assert!(!authority.contains('@'), "{raw}");
                    }
                }
            }

            /// Absolute local paths are always accepted as distinct-per-path
            /// mirrors and stay inside the mirror tree.
            #[test]
            fn prop_local_paths_stay_inside_the_mirror_tree(
                segments in proptest::collection::vec("[ -~]{1,12}", 1..5),
            ) {
                let raw = format!("/{}", segments.join("/"));
                if let Ok(location) = loc(&raw) {
                    prop_assert!(location.is_local());
                    assert_safe(&location)?;
                }
            }
        }
    }
}
