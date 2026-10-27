//! The directory a command runs in.
//!
//! The server runs git in a directory of the work volume and nowhere else. [`Root`] holds the
//! volume's directory, opened once, and resolves a requested directory **from it, one component at
//! a time, refusing a link at any of them** (`O_NOFOLLOW`): a slot or a mirror that a run replaced
//! by a link to somewhere else is not a place git is run. The check is made right before the
//! command starts; git is given the name and not the descriptor, so a swap in the moment between
//! is not seen. What that buys a run is a place in the sandbox, which holds nothing secret.

use std::io;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{Mode, OFlags};

/// Why a directory is not one the server runs git in.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CwdError {
    /// The path is not absolute, or is not below the root, or has a `.` or `..` after it.
    #[error("{0} is not a directory below the root")]
    Outside(PathBuf),
    /// A component is a link, a file, or not there.
    #[error("{0} is not a plain directory (it is missing, a file or a link)")]
    NotPlain(PathBuf),
}

/// The work volume, as the server runs git in it.
#[derive(Debug, Clone)]
pub struct Root {
    path: PathBuf,
    dir: Arc<OwnedFd>,
}

impl Root {
    /// Open `path`, an absolute path of an existing directory (links in it are followed: it is
    /// the operator's).
    ///
    /// # Errors
    ///
    /// When `path` is not absolute or cannot be opened as a directory.
    pub fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the root must be an absolute path",
            ));
        }
        let dir = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self {
            path: normalized(path),
            dir: Arc::new(dir),
        })
    }

    /// The root, as it was given.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `cwd` if it is the root or a plain directory below it; the check opens each component from
    /// the root without following a link.
    ///
    /// # Errors
    ///
    /// [`CwdError`] saying which of the two it is.
    pub fn resolve(&self, cwd: &Path) -> Result<PathBuf, CwdError> {
        let outside = || CwdError::Outside(cwd.to_owned());
        let relative = cwd.strip_prefix(&self.path).map_err(|_| outside())?;
        let mut parts = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(part) => parts.push(part),
                Component::CurDir => {}
                _ => return Err(outside()),
            }
        }
        let mut current: Option<OwnedFd> = None;
        for part in &parts {
            let from: &OwnedFd = current.as_ref().unwrap_or(&*self.dir);
            let next = rustix::fs::openat(
                from,
                *part,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| CwdError::NotPlain(cwd.to_owned()))?;
            current = Some(next);
        }
        Ok(parts.iter().fold(self.path.clone(), |p, part| p.join(part)))
    }
}

/// `path` without a trailing slash or `.` components (it is compared by components).
fn normalized(path: &Path) -> PathBuf {
    path.components().collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn volume() -> (tempfile::TempDir, Root) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("work");
        std::fs::create_dir_all(root.join("workspaces/run/slot")).unwrap();
        std::fs::write(root.join("file"), "x").unwrap();
        let root = Root::open(&root).unwrap();
        (tmp, root)
    }

    #[test]
    fn the_root_and_what_is_plainly_below_it_are_accepted() {
        let (_tmp, root) = volume();
        let base = root.path().to_owned();
        assert_eq!(root.resolve(&base).unwrap(), base);
        let slot = base.join("workspaces/run/slot");
        assert_eq!(root.resolve(&slot).unwrap(), slot);
        // A trailing slash and a `.` are the same place.
        assert_eq!(
            root.resolve(&base.join("workspaces/./run/")).unwrap(),
            base.join("workspaces/run")
        );
    }

    #[test]
    fn a_place_outside_the_root_is_refused() {
        let (tmp, root) = volume();
        let base = root.path().to_owned();
        for outside in [
            tmp.path().to_owned(),
            PathBuf::from("/"),
            PathBuf::from("/etc"),
            base.join("../etc"),
            base.join("workspaces/../.."),
            PathBuf::from("work"),
            PathBuf::from("workspaces/run"),
            // A sibling that shares the root's name as a prefix.
            PathBuf::from(format!("{}-other", base.display())),
        ] {
            assert!(
                matches!(root.resolve(&outside), Err(CwdError::Outside(_))),
                "{}",
                outside.display()
            );
        }
    }

    #[test]
    fn a_link_at_the_end_or_on_the_way_is_refused() {
        let (tmp, root) = volume();
        let base = root.path().to_owned();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("inside")).unwrap();
        symlink(&elsewhere, base.join("workspaces/run/link")).unwrap();
        symlink(base.join("workspaces/run/slot"), base.join("alias")).unwrap();

        for linked in [
            base.join("workspaces/run/link"),
            base.join("workspaces/run/link/inside"),
            base.join("alias"),
            base.join("alias/."),
        ] {
            assert!(
                matches!(root.resolve(&linked), Err(CwdError::NotPlain(_))),
                "{}",
                linked.display()
            );
        }
    }

    #[test]
    fn a_file_and_a_missing_place_are_not_directories() {
        let (_tmp, root) = volume();
        let base = root.path().to_owned();
        for not in [
            base.join("file"),
            base.join("workspaces/missing"),
            base.join("file/x"),
        ] {
            assert!(
                matches!(root.resolve(&not), Err(CwdError::NotPlain(_))),
                "{}",
                not.display()
            );
        }
    }

    #[test]
    fn a_root_that_is_not_a_directory_or_not_absolute_cannot_be_opened() {
        let (tmp, _root) = volume();
        assert!(Root::open(&tmp.path().join("work/file")).is_err());
        assert!(Root::open(&tmp.path().join("missing")).is_err());
        assert!(Root::open(Path::new("work")).is_err());
    }
}
