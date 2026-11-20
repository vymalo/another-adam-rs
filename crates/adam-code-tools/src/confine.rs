//! Where a tool may read or write: a path the model chose, checked against the root of the place it
//! works in (moved from the coder's file tools, ADR 0047 decision 7).
//!
//! # The confinement rule ([`confine`])
//!
//! * An empty path, an absolute path, any `..` component and any component equal to `.git`
//!   (compared without regard to case: a case-insensitive file system has the same `.git`) are
//!   refused. The path is relative to the root of the place.
//! * To **read**, the path is resolved inside the place, following every symlink on the way that
//!   stays inside it, and the result must not be inside a `.git` either. So a symlink that stays
//!   inside is read, and one that leads out (an absolute one, or one that goes up past the root) is
//!   refused.
//! * To **write**, every component of the path that exists is looked at: any symlink on the way,
//!   the file itself included, is refused (a link that stays inside is refused too: a write through
//!   a link is a write to a place the model did not name).
//!
//! Everything is relative to the root **as it was opened** ([`SlotDir`]), never to its name: a swap
//! of the directory, or of one below it, is not followed.
//!
//! # Races
//!
//! A process the place's owner does not control (a command a tool ran, and what it left running)
//! can rename the place's directory, or one below it, and put a link at the old name while a tool
//! works: a path may be checked and then used. So nothing here uses a name for what it checks. The
//! check and the use are relative to the same descriptors (the root's, and the directory a file is
//! written in); a link that appears on a path after it was resolved is refused when the file is
//! opened (`RESOLVE_NO_SYMLINKS`, or `O_NOFOLLOW` one component at a time where `openat2` is
//! missing), and a write is a new file renamed over its target, which replaces a link that appeared
//! meanwhile instead of following it.
//!
//! What a descriptor cannot cover is `git`: `git apply` (`apply_patch`) writes the paths of a patch
//! by name, after they are checked here, and `git` finds its work tree by name in its own process.
//! A name is safe only where nothing can rename it, so that is the workspace's to ensure: the coder
//! makes each slot of a run pod a mount point of its own ([ADR 0041], decisions 1 and 3), and a host whose
//! directory a command can rename has the same gap.
//!
//! [ADR 0041]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0041-a-run-pod-sees-only-its-own-run.md

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use adam_workspace::safe_fs::{BeneathError, Dir, Meta, SlotDir, WalkError};

/// What a path is used for; see [`confine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The file is read. A symlink is followed, and must lead to something inside the root.
    Read,
    /// The file is created or replaced. No symlink may be on the way.
    Write,
}

/// Whether `name` is a `.git` (any case).
pub fn is_dot_git(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().eq_ignore_ascii_case(".git")
}

/// The normal components of `rel`, or why the path is refused (see the module docs).
pub fn parts_of(rel: &str) -> Result<Vec<OsString>, String> {
    let trimmed = rel.trim();
    if trimmed.is_empty() {
        return Err("path is required".to_owned());
    }
    if trimmed.contains('\0') {
        return Err(format!("`{}` is not a path", trimmed.escape_debug()));
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(format!(
            "`{trimmed}` is an absolute path: give a path relative to the root of the worktree"
        ));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) if is_dot_git(name) => {
                return Err(format!(
                    "`{trimmed}` is inside .git: the repository's own files are not for these tools"
                ));
            }
            Component::Normal(name) => parts.push(name.to_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "`{trimmed}` goes up with `..`: a path must stay inside the worktree"
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "`{trimmed}` is an absolute path: give a path relative to the root of the worktree"
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(format!(
            "`{trimmed}` is the root of the worktree, not a file: name a file"
        ));
    }
    Ok(parts)
}

/// A path that [`confine`] accepted: the components below the slot's root. For a read they are the
/// path with every link on the way followed (it has none), for a write the path as given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confined {
    parts: Vec<OsString>,
}

impl Confined {
    /// The path, relative to the slot's root.
    pub fn rel(&self) -> PathBuf {
        self.parts.iter().collect()
    }
}

/// Check `rel` against the slot `root` for `access`, or give the reason the model is told.
///
/// The rule is in the [module docs](self#the-confinement-rule-confine). Everything is looked at
/// relative to `root`, the slot's directory as the coder opened it, never by its name. For
/// [`Access::Read`] the path must exist; for [`Access::Write`] it may not exist yet (nor its
/// parents).
///
/// This is a **check**: it opens nothing the caller can use. A caller that goes on to read or write
/// does it with `open_for_read` and `locate_for_write`, which look at the path again as they
/// open it.
///
/// # Errors
///
/// A message for the model: the path is empty, absolute, goes up, names `.git`, leaves the
/// worktree through a symlink, goes through a symlink to write, or (to read) does not exist.
pub fn confine(root: &SlotDir, rel: &str, access: Access) -> Result<Confined, String> {
    let parts = parts_of(rel)?;
    let shown = rel.trim();
    match access {
        Access::Read => {
            let path: PathBuf = parts.iter().collect();
            let resolved = root
                .resolve_inside(&path, &is_dot_git)
                .map_err(|e| read_refusal(e, shown))?;
            Ok(Confined { parts: resolved })
        }
        Access::Write => {
            locate_parts(root, &parts, shown, false)?;
            Ok(Confined { parts })
        }
    }
}

/// What the model is told when a path cannot be resolved for a read.
fn read_refusal(e: BeneathError, shown: &str) -> String {
    match e {
        BeneathError::Escapes => {
            format!("`{shown}` leads outside the worktree (through a symlink): refused")
        }
        BeneathError::Avoided(_) => {
            format!("`{shown}` leads into .git (through a symlink): refused")
        }
        BeneathError::TooManyLinks => {
            format!("cannot resolve `{shown}`: too many levels of symbolic links")
        }
        BeneathError::Changed => {
            format!("`{shown}` changed while it was opened (a symlink is on its way now): refused")
        }
        BeneathError::Io(e) if e.kind() == io::ErrorKind::NotFound => {
            format!("`{shown}` does not exist")
        }
        BeneathError::Io(e) => format!("cannot resolve `{shown}`: {e}"),
    }
}

/// Open `rel` of the slot `root` for reading, and say what it is (a file, a directory or
/// something else: the caller tells the model).
///
/// The path is resolved inside the slot, following the links that stay inside it; the file is then
/// opened by the path that resolution found, which has no link in it, relative to `root`. So a
/// link put on the way after the resolution is refused and never followed, and so is a slot
/// swapped for a link meanwhile (the descriptor is the directory that was opened).
pub fn open_for_read(root: &SlotDir, rel: &str) -> Result<(fs::File, Meta), String> {
    let parts = parts_of(rel)?;
    let shown = rel.trim();
    let path: PathBuf = parts.iter().collect();
    root.open_following(&path, &is_dot_git)
        .map_err(|e| read_refusal(e, shown))
}

/// Where a write goes: the directory the file is in, as a descriptor, and the file.
pub struct Located {
    /// What the model called the file.
    shown: String,
    /// The slot, for the checkpoints of the tests.
    slot: PathBuf,
    /// The directory of the file; `None` when it (or a directory above it) is not there yet and
    /// nothing was made.
    parent: Option<Dir>,
    /// The file's name in that directory.
    name: OsString,
    /// What is at `name`, without following it.
    existing: Option<Meta>,
}

impl Located {
    /// What the model called the file.
    pub fn shown(&self) -> &str {
        &self.shown
    }

    /// The name of the root, for the checkpoints of a test's races; nothing is opened by it.
    pub fn slot(&self) -> &Path {
        &self.slot
    }

    /// The directory of the file, held; `None` when it (or one above it) is not there and nothing
    /// was made.
    pub fn parent(&self) -> Option<&Dir> {
        self.parent.as_ref()
    }

    /// The file's name in that directory.
    pub fn name(&self) -> &std::ffi::OsStr {
        &self.name
    }

    /// What is at the name now, without following it.
    pub fn existing(&self) -> Option<&Meta> {
        self.existing.as_ref()
    }
}

/// Find the directory a write to `parts` goes in, relative to `root`, making the directories that
/// are missing when `create` is set, and what is at the file now. `rel` is checked as [`confine`]
/// checks it (empty, absolute, `..`, `.git` and the root are refused) before anything is looked at;
/// then a link anywhere on the way, the file itself included, a file where a directory is needed and
/// a directory where the file is to be are refused.
///
/// # Errors
///
/// The reason the model is told.
pub fn locate_for_write(root: &SlotDir, rel: &str, create: bool) -> Result<Located, String> {
    let parts = parts_of(rel)?;
    locate_parts(root, &parts, rel.trim(), create)
}

/// [`locate_for_write`] for `parts` that [`parts_of`] gave: never called with parts of another
/// origin, so it is not public.
fn locate_parts(
    root: &SlotDir,
    parts: &[OsString],
    shown: &str,
    create: bool,
) -> Result<Located, String> {
    let Some((name, dirs)) = parts.split_last() else {
        return Err("path is required".to_owned());
    };
    let prefix = |upto: usize| -> String {
        parts[..=upto]
            .iter()
            .collect::<PathBuf>()
            .display()
            .to_string()
    };
    let linked = |upto: usize| {
        format!(
            "`{shown}` goes through the symlink `{}`: writing through a symlink is refused",
            prefix(upto)
        )
    };
    let dir_path: PathBuf = dirs.iter().collect();
    let parent = match root.walk(&dir_path, create) {
        Ok(dir) => Some(dir),
        Err(WalkError::Missing(_)) => None,
        Err(WalkError::Link(at)) => return Err(linked(at)),
        Err(WalkError::NotDirectory(at)) => {
            return Err(format!(
                "`{}` is a file, not a directory: `{shown}` cannot be inside it",
                prefix(at)
            ));
        }
        Err(WalkError::Io(e)) if create => {
            return Err(format!("cannot create the directories of `{shown}`: {e}"));
        }
        Err(WalkError::Io(e)) => return Err(format!("cannot look at `{shown}`: {e}")),
    };
    let existing = match &parent {
        Some(dir) => match dir.lstat(name) {
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("cannot look at `{shown}`: {e}")),
        },
        None => None,
    };
    match existing {
        Some(meta) if meta.is_symlink() => return Err(linked(parts.len() - 1)),
        Some(meta) if meta.is_dir() => {
            return Err(format!("`{shown}` is a directory: name a file"));
        }
        _ => {}
    }
    Ok(Located {
        shown: shown.to_owned(),
        slot: root.path().to_path_buf(),
        parent,
        name: name.clone(),
        existing,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn place() -> (tempfile::TempDir, SlotDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn main() {}\n").unwrap();
        let root = SlotDir::open(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn a_path_that_leaves_the_place_or_names_git_is_refused_for_both_uses() {
        let (_dir, root) = place();
        for access in [Access::Read, Access::Write] {
            for (path, said) in [
                ("", "path is required"),
                ("/etc/passwd", "absolute path"),
                ("../x", "goes up"),
                ("src/../../x", "goes up"),
                (".git/config", "inside .git"),
                ("src/.GIT/x", "inside .git"),
                (".", "the root of the worktree"),
            ] {
                let refused = confine(&root, path, access).unwrap_err();
                assert!(refused.contains(said), "{access:?} {path:?}: {refused}");
            }
        }
    }

    #[test]
    fn a_read_follows_a_link_that_stays_inside_and_refuses_one_that_leaves() {
        let (dir, root) = place();
        symlink("src/lib.rs", dir.path().join("inside")).unwrap();
        symlink("/etc/hostname", dir.path().join("outside")).unwrap();
        let read = confine(&root, "inside", Access::Read).unwrap();
        assert_eq!(read.rel(), PathBuf::from("src/lib.rs"));
        let refused = confine(&root, "outside", Access::Read).unwrap_err();
        assert!(refused.contains("leads outside"), "{refused}");
        let missing = confine(&root, "src/none.rs", Access::Read).unwrap_err();
        assert!(missing.contains("does not exist"), "{missing}");
    }

    /// `locate_for_write` is a door of its own: it checks the path as `confine` does, so a caller
    /// that never asked `confine` still cannot write into `.git` (a hook that runs on the next
    /// commit) or leave the place.
    #[test]
    fn locate_for_write_checks_the_path_itself() {
        let (dir, root) = place();
        std::fs::create_dir_all(dir.path().join(".git/hooks")).unwrap();
        for (path, said) in [
            (".git/hooks/pre-commit", "inside .git"),
            ("src/.Git/x", "inside .git"),
            ("../escape", "goes up"),
            ("/tmp/x", "absolute path"),
        ] {
            for create in [false, true] {
                let Err(refused) = locate_for_write(&root, path, create) else {
                    panic!("{path:?} create={create}: located");
                };
                assert!(
                    refused.contains(said),
                    "{path:?} create={create}: {refused}"
                );
            }
        }
        assert!(!dir.path().join(".git/hooks/pre-commit").exists());
        let located = locate_for_write(&root, " src/new.rs ", true).unwrap();
        assert_eq!(located.shown(), "src/new.rs");
        assert_eq!(located.name(), "new.rs");
        assert!(located.parent().is_some() && located.existing().is_none());
    }

    #[test]
    fn a_write_refuses_any_link_on_the_way_and_may_name_a_new_file() {
        let (dir, root) = place();
        symlink("src", dir.path().join("linked")).unwrap();
        let refused = confine(&root, "linked/new.rs", Access::Write).unwrap_err();
        assert!(refused.contains("link"), "{refused}");
        let new = confine(&root, "src/deep/new.rs", Access::Write).unwrap();
        assert_eq!(new.rel(), PathBuf::from("src/deep/new.rs"));
        assert!(
            !dir.path().join("src/deep").exists(),
            "a check makes nothing"
        );
    }
}

/// The rule's cases, moved from the coder with it: each use of a path, `.git` in any case, links
/// that stay inside or leave, and a root that is not a plain directory.
#[cfg(test)]
mod rules {
    use std::fs;
    use std::path::Path;

    use super::{Access, Confined, SlotDir};

    /// The slot at `root`, opened as the tools open it: the check and the hold of a slot.
    fn open(root: &Path) -> Result<SlotDir, String> {
        SlotDir::open(root).map_err(|e| {
            format!(
                "the worktree's directory is not a plain directory (a command replaced it by a link or a file): {e}"
            )
        })
    }

    fn confine(root: &Path, rel: &str, access: Access) -> Result<Confined, String> {
        super::confine(&open(root)?, rel, access)
    }

    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("README.md"), "hi\n").unwrap();
        // What a worktree has: a `.git` file.
        fs::write(dir.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        dir
    }

    fn refused(root: &Path, rel: &str, access: Access) -> String {
        confine(root, rel, access).expect_err(rel)
    }

    #[test]
    fn a_path_inside_the_worktree_is_accepted_for_both() {
        let dir = root();
        let accepted = |rel: &str, access| confine(dir.path(), rel, access).unwrap().rel();
        assert_eq!(
            accepted("src/main.rs", Access::Read),
            Path::new("src/main.rs")
        );
        assert_eq!(
            accepted("./src//main.rs", Access::Read),
            Path::new("src/main.rs")
        );
        // A file that is not there yet, in a directory that is not there yet.
        assert_eq!(
            accepted("new/deep/file.txt", Access::Write),
            Path::new("new/deep/file.txt")
        );
        assert_eq!(
            accepted("src/main.rs", Access::Write),
            Path::new("src/main.rs")
        );
        // Nothing was made by asking.
        assert!(!dir.path().join("new").exists());
    }

    #[test]
    fn an_empty_path_is_refused() {
        let dir = root();
        for rel in ["", "   ", ".", "./", "./."] {
            for access in [Access::Read, Access::Write] {
                let why = refused(dir.path(), rel, access);
                assert!(
                    why.contains("path is required") || why.contains("root of the worktree"),
                    "{rel:?}: {why}"
                );
            }
        }
    }

    #[test]
    fn a_path_that_goes_up_is_refused() {
        let dir = root();
        for rel in ["../x", "src/../../x", "..", "src/.."] {
            for access in [Access::Read, Access::Write] {
                let why = refused(dir.path(), rel, access);
                assert!(why.contains("`..`"), "{rel}: {why}");
            }
        }
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let dir = root();
        let inside = dir.path().join("README.md");
        for rel in ["/etc/passwd", inside.to_str().unwrap()] {
            for access in [Access::Read, Access::Write] {
                let why = refused(dir.path(), rel, access);
                assert!(why.contains("absolute"), "{rel}: {why}");
            }
        }
    }

    #[test]
    fn nothing_inside_dot_git_is_touched_whatever_the_case() {
        let dir = root();
        for rel in [
            ".git",
            ".git/config",
            ".GIT/x",
            ".Git/hooks/pre-commit",
            "a/.git/x",
            "src/.GiT",
        ] {
            for access in [Access::Read, Access::Write] {
                let why = refused(dir.path(), rel, access);
                assert!(why.contains(".git"), "{rel}: {why}");
            }
        }
        // `.github` is not `.git`.
        fs::create_dir_all(dir.path().join(".github")).unwrap();
        assert!(confine(dir.path(), ".github/workflows/ci.yml", Access::Write).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_file_outside_is_not_read() {
        let dir = root();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link")).unwrap();
        let why = refused(dir.path(), "link", Access::Read);
        assert!(why.contains("outside the worktree"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_outside_is_not_read_or_written() {
        let dir = root();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("dir")).unwrap();
        let why = refused(dir.path(), "dir/secret", Access::Read);
        assert!(why.contains("outside the worktree"), "{why}");
        let why = refused(dir.path(), "dir/new.txt", Access::Write);
        assert!(why.contains("symlink"), "{why}");
        assert!(!outside.path().join("new.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_write_through_a_symlink_is_refused_even_when_it_stays_inside() {
        let dir = root();
        std::os::unix::fs::symlink("README.md", dir.path().join("alias")).unwrap();
        std::os::unix::fs::symlink("src", dir.path().join("srclink")).unwrap();
        // Reading follows a link that stays inside.
        assert!(confine(dir.path(), "alias", Access::Read).is_ok());
        assert!(confine(dir.path(), "srclink/main.rs", Access::Read).is_ok());
        // Writing does not.
        let why = refused(dir.path(), "alias", Access::Write);
        assert!(why.contains("symlink `alias`"), "{why}");
        let why = refused(dir.path(), "srclink/main.rs", Access::Write);
        assert!(why.contains("symlink `srclink`"), "{why}");
        // A dangling link is a link too.
        std::os::unix::fs::symlink("nowhere", dir.path().join("dangling")).unwrap();
        assert!(refused(dir.path(), "dangling", Access::Write).contains("symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn a_root_that_is_a_link_to_another_directory_is_not_a_root() {
        let dir = root();
        let other = tempfile::tempdir().unwrap();
        fs::write(other.path().join(".env"), "TOKEN=another run's").unwrap();
        let link = other.path().join("slot");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        // A link to a worktree is canonicalised into that worktree: refused before. The refusal is
        // `SlotDir::open`'s own, not another failure the helper words the same way.
        for root in [link.clone(), dir.path().join("README.md")] {
            let refused = SlotDir::open(&root).expect_err("a link or a file is no root");
            assert_eq!(
                refused.kind(),
                std::io::ErrorKind::InvalidInput,
                "{refused}"
            );
        }
        for access in [Access::Read, Access::Write] {
            let why = refused(&link, "README.md", access);
            assert!(why.contains("not a plain directory"), "{why}");
        }
        // And the real directory is as it was.
        assert!(confine(dir.path(), "README.md", Access::Read).is_ok());
        // A file is no root either.
        let why = refused(&dir.path().join("README.md"), "x", Access::Write);
        assert!(why.contains("not a plain directory"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_dot_git_is_not_read_either() {
        let dir = root();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::os::unix::fs::symlink("../.git", dir.path().join("sub/g")).unwrap();
        let why = refused(dir.path(), "sub/g", Access::Read);
        assert!(why.contains(".git"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_is_absolute_is_refused_even_when_it_leads_into_the_worktree() {
        let dir = root();
        std::os::unix::fs::symlink(dir.path().join("README.md"), dir.path().join("abs")).unwrap();
        let why = refused(dir.path(), "abs", Access::Read);
        assert!(why.contains("outside the worktree"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_goes_up_past_the_root_is_refused_and_one_that_comes_back_is_not() {
        let dir = root();
        std::os::unix::fs::symlink("../x", dir.path().join("up")).unwrap();
        std::os::unix::fs::symlink("src/../README.md", dir.path().join("around")).unwrap();
        let why = refused(dir.path(), "up", Access::Read);
        assert!(why.contains("outside the worktree"), "{why}");
        assert_eq!(
            confine(dir.path(), "around", Access::Read).unwrap().rel(),
            Path::new("README.md")
        );
    }

    #[test]
    fn a_directory_is_not_a_file_to_write_and_a_file_is_not_a_directory() {
        let dir = root();
        assert!(refused(dir.path(), "src", Access::Write).contains("is a directory"));
        assert!(refused(dir.path(), "README.md/x", Access::Write).contains("is a file"));
    }

    #[test]
    fn reading_says_what_is_missing() {
        let dir = root();
        assert!(refused(dir.path(), "nope.txt", Access::Read).contains("does not exist"));
    }
}
