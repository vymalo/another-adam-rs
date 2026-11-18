//! [`ToolAccess`]: what a tool call shares with the other calls of its model turn.
//!
//! The agent runs the calls of a turn at the same time, up to a limit
//! ([`LlmAgentBuilder::max_parallel`](crate::LlmAgentBuilder::max_parallel)). A tool says here which
//! calls may not overlap with it; the loop keeps the model's order between those and nothing else
//! (see ADR 0049).

/// Whether a call reads or changes what its scope names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UseMode {
    /// The call only looks: two reads of one scope may run together.
    Read,
    /// The call changes it: no other call of the scope runs beside it.
    Write,
}

/// One thing a call uses, and how.
///
/// A scope is an opaque key with `/` levels: `files`, `files/src/a.rs`, `commands`, `screen`.
/// Two uses **overlap** when one scope equals the other or is a prefix of it at a `/`
/// (`files` overlaps `files/src/a.rs`; `files/src` does not overlap `files/srcs`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Use {
    /// Read or write.
    pub mode: UseMode,
    /// What is used.
    pub scope: String,
}

impl Use {
    /// A read of `scope`.
    pub fn read(scope: impl Into<String>) -> Self {
        Self {
            mode: UseMode::Read,
            scope: scope.into(),
        }
    }

    /// A write of `scope`.
    pub fn write(scope: impl Into<String>) -> Self {
        Self {
            mode: UseMode::Write,
            scope: scope.into(),
        }
    }

    /// Whether the scopes of `self` and `other` overlap: equal, or one a prefix of the other at a `/`.
    pub fn overlaps(&self, other: &Use) -> bool {
        scopes_overlap(&self.scope, &other.scope)
    }

    /// Whether two calls using `self` and `other` must not run together: the scopes overlap and at
    /// least one writes.
    pub fn conflicts(&self, other: &Use) -> bool {
        (self.mode == UseMode::Write || other.mode == UseMode::Write) && self.overlaps(other)
    }
}

/// Whether the scope `a` equals `b` or is a prefix of it (or the reverse) at a `/`.
fn scopes_overlap(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    long == short || long.strip_prefix(short).is_some_and(|r| r.starts_with('/'))
}

/// What a call shares with the other calls of its turn: see [`Tool::access`](crate::Tool::access).
///
/// Two calls **conflict** when one of them is [`Alone`](Self::Alone), or when they have
/// overlapping [`Use`]s and at least one writes. Calls that conflict keep the model's order;
/// every other call starts as soon as fewer than the limit run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ToolAccess {
    /// Shares nothing with the run's other calls.
    Independent,
    /// Uses the named scopes; conflicts only with calls that use an overlapping one in a way that
    /// includes a write. An empty list is [`Independent`](Self::Independent).
    Uses(Vec<Use>),
    /// Runs by itself: every earlier call has finished and been applied, and nothing after it
    /// starts before it is done. The default, so that a tool that declares nothing is safe.
    #[default]
    Alone,
}

impl ToolAccess {
    /// A read of the one scope `scope`.
    pub fn reads(scope: impl Into<String>) -> Self {
        Self::Uses(vec![Use::read(scope)])
    }

    /// A write of the one scope `scope`.
    pub fn writes(scope: impl Into<String>) -> Self {
        Self::Uses(vec![Use::write(scope)])
    }

    /// A read of `base`, or of `base/<path>` when `path` names one place under it: see [`path_scope`].
    pub fn reads_path(base: &str, path: Option<&str>) -> Self {
        Self::reads(path_scope(base, path))
    }

    /// A write of `base`, or of `base/<path>`: see [`path_scope`].
    pub fn writes_path(base: &str, path: Option<&str>) -> Self {
        Self::writes(path_scope(base, path))
    }

    /// Whether a call with this access may run in the same segment as another call at all.
    pub fn is_alone(&self) -> bool {
        matches!(self, Self::Alone)
    }

    /// The uses of the access: none for [`Independent`](Self::Independent), `None` for
    /// [`Alone`](Self::Alone), which conflicts with everything.
    pub fn uses(&self) -> Option<&[Use]> {
        match self {
            Self::Independent => Some(&[]),
            Self::Uses(uses) => Some(uses),
            Self::Alone => None,
        }
    }

    /// Whether a call with this access and one with `other` must not run together.
    pub fn conflicts(&self, other: &ToolAccess) -> bool {
        match (self.uses(), other.uses()) {
            (Some(mine), Some(theirs)) => {
                mine.iter().any(|a| theirs.iter().any(|b| a.conflicts(b)))
            }
            _ => true,
        }
    }
}

/// The scope of a path argument under `base`: `base/<path>` with the path trimmed (the tools trim it
/// before they open it) and normalised lexically (a leading `./`, a `.` or an empty component, and a trailing `/` are dropped), and `base` itself
/// when there is no path, when it names nothing but the root, when it is absolute, or when it has a
/// `..` component (it may leave the place the scope names, so it widens to all of `base`).
///
/// What `#[tool(parallel = "read", path = "path")]` uses.
pub fn path_scope(base: &str, path: Option<&str>) -> String {
    let Some(path) = path.map(str::trim) else {
        return base.to_owned();
    };
    if path.starts_with('/') {
        return base.to_owned();
    }
    let mut kept: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return base.to_owned(),
            other => kept.push(other),
        }
    }
    if kept.is_empty() {
        base.to_owned()
    } else {
        format!("{base}/{}", kept.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_overlap_at_a_slash_only() {
        assert!(scopes_overlap("files", "files"));
        assert!(scopes_overlap("files", "files/src/a.rs"));
        assert!(scopes_overlap("files/src/a.rs", "files"));
        assert!(scopes_overlap("files/src", "files/src/a.rs"));
        assert!(!scopes_overlap("files/src", "files/srcs"));
        assert!(!scopes_overlap("files/src/a.rs", "files/src/b.rs"));
        assert!(!scopes_overlap("files", "commands"));
        assert!(!scopes_overlap("file", "files"));
    }

    #[test]
    fn two_reads_never_conflict_and_a_write_conflicts_with_an_overlapping_use() {
        let read = ToolAccess::reads("files");
        let write = ToolAccess::writes("files/a.rs");
        assert!(!read.conflicts(&read));
        assert!(read.conflicts(&write));
        assert!(write.conflicts(&write));
        assert!(!write.conflicts(&ToolAccess::writes("files/b.rs")));
        assert!(!write.conflicts(&ToolAccess::writes("commands")));
        assert!(!write.conflicts(&ToolAccess::Independent));
        assert!(!ToolAccess::Independent.conflicts(&ToolAccess::Independent));
    }

    #[test]
    fn alone_conflicts_with_everything_and_is_the_default() {
        assert_eq!(ToolAccess::default(), ToolAccess::Alone);
        assert!(ToolAccess::Alone.is_alone());
        for other in [
            ToolAccess::Independent,
            ToolAccess::reads("files"),
            ToolAccess::Alone,
        ] {
            assert!(ToolAccess::Alone.conflicts(&other));
            assert!(other.conflicts(&ToolAccess::Alone));
        }
    }

    #[test]
    fn a_path_is_normalised_lexically_and_widens_when_it_cannot_be_named() {
        assert_eq!(path_scope("files", None), "files");
        assert_eq!(path_scope("files", Some("src/a.rs")), "files/src/a.rs");
        assert_eq!(path_scope("files", Some("./src/./a.rs")), "files/src/a.rs");
        assert_eq!(path_scope("files", Some("src//a.rs/")), "files/src/a.rs");
        assert_eq!(path_scope("files", Some(".")), "files");
        assert_eq!(path_scope("files", Some("")), "files");
        assert_eq!(path_scope("files", Some("/etc/passwd")), "files");
        assert_eq!(path_scope("files", Some("../a.rs")), "files");
        assert_eq!(path_scope("files", Some("src/../a.rs")), "files");
    }

    /// The tools trim the path they are given, so `" src/a.rs"` is the file `src/a.rs`: the scope
    /// must name it too, or a write of one spelling would not wait for a read of the other.
    #[test]
    fn a_path_is_trimmed_before_it_names_a_scope() {
        let plain = ToolAccess::writes_path("files", Some("src/a.rs"));
        for spelled in [" src/a.rs", "src/a.rs ", "\tsrc/a.rs\n", "  ./src/a.rs  "] {
            let read = ToolAccess::reads_path("files", Some(spelled));
            assert_eq!(read, ToolAccess::reads("files/src/a.rs"), "{spelled:?}");
            assert!(plain.conflicts(&read), "{spelled:?}");
        }
        // A blank path names nothing, and an absolute one widens, trimmed or not.
        assert_eq!(path_scope("files", Some("   ")), "files");
        assert_eq!(path_scope("files", Some(" /etc/passwd")), "files");
    }
}
