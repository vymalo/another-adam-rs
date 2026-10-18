//! Opening what a run's commands can write, without being led anywhere by it.
//!
//! The coder holds the whole volume (and the GitHub App's key). A run's pod holds its own run
//! directory and the worktree's directory in the mirror, and everything it puts there the coder
//! meets later: a link where the coder expects a file leads the coder's `open` to any file of the
//! volume or of the container, a pipe makes it wait for ever, and a file in a name the coder will
//! write is a link to wherever the run chose. So the coder's operations on those paths are these:
//!
//! * **read** a file with [`read_regular_file`]: not through a link, not a pipe or a device, of a
//!   size it is willing to read;
//! * **copy** one with [`private_copy_of`]: into a directory of the coder's own, made for the copy
//!   (mode `0700`, a name nobody chose, in the process's temporary directory and not on the
//!   volume), a file made with `O_EXCL` in it;
//! * **tell** a directory with [`is_real_directory`]: `lstat`, so a link to a directory is not one.
//!
//! A link is refused in the **last component** of the path only: every component above it is a
//! directory the coder made (the mirror, the run's directory) or the mount point of the run's pod,
//! which the run cannot replace.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

/// The most the coder reads from a small file it checks (`commondir`, `gitdir`, a `.git` file).
pub const MAX_SMALL_FILE: u64 = 4096;

/// The most the coder copies of a run's git index. A repository of a million files has an index of
/// about 120 MB.
pub const MAX_INDEX_COPY: u64 = 512 * 1024 * 1024;

/// Open `path` for reading if it is a regular file, and not otherwise: a link in the last component
/// (`O_NOFOLLOW`), a pipe (`O_NONBLOCK`: opening one does not wait for a writer, and it is refused
/// after `fstat`), a socket and a device are errors. The metadata is the open file's, not the
/// path's.
fn open_regular(path: &Path) -> io::Result<(File, Metadata)> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(not_regular(path));
    }
    let file = options.open(path).map_err(|e| {
        // `O_NOFOLLOW` on a link is `ELOOP`; say what it means.
        if is_link_error(&e) {
            not_regular(path)
        } else {
            e
        }
    })?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(not_regular(path));
    }
    Ok((file, meta))
}

#[cfg(unix)]
fn is_link_error(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ELOOP)
}

#[cfg(not(unix))]
fn is_link_error(_: &io::Error) -> bool {
    false
}

fn not_regular(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{} is not a regular file (a link, a pipe or a device)",
            path.display()
        ),
    )
}

/// The bytes of the regular file at `path`, at most `max` of them.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] with "not a regular file" for a link, a pipe, a socket, a
/// device or a directory, and for a file longer than `max`; the errors of `open` and `read`.
pub async fn read_regular_file(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let path = path.to_owned();
    blocking(move || {
        let (file, meta) = open_regular(&path)?;
        if meta.len() > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is longer than {max} bytes", path.display()),
            ));
        }
        let mut bytes = Vec::new();
        file.take(max + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is longer than {max} bytes", path.display()),
            ));
        }
        Ok(bytes)
    })
    .await
}

/// [`read_regular_file`] as text: lossy, for a file that holds a path or a line.
///
/// # Errors
///
/// As [`read_regular_file`].
pub async fn read_regular_text(path: &Path, max: u64) -> io::Result<String> {
    read_regular_file(path, max)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Whether `path` is a directory itself, not a link to one (`lstat`).
pub async fn is_real_directory(path: &Path) -> bool {
    matches!(tokio::fs::symlink_metadata(path).await, Ok(meta) if meta.is_dir())
}

/// Whether every component of `path` below `below` is a directory itself and not a link to one:
/// nothing on the way from `below` down to `path` leads anywhere else. `false` when `path` is not
/// below `below`.
pub async fn is_plain_path(below: &Path, path: &Path) -> bool {
    let Ok(rest) = path.strip_prefix(below) else {
        return false;
    };
    let mut at = below.to_owned();
    for part in rest.components() {
        match part {
            std::path::Component::Normal(part) => at.push(part),
            _ => return false,
        }
        if !is_real_directory(&at).await {
            return false;
        }
    }
    true
}

/// Whether `path` is a regular file itself, not a link to one (`lstat`).
pub async fn is_real_file(path: &Path) -> bool {
    matches!(tokio::fs::symlink_metadata(path).await, Ok(meta) if meta.is_file())
}

/// A copy of a file in a directory that only the coder's process can use, removed when this is
/// dropped. What a `git` that rewrites an index works on (`GIT_INDEX_FILE`), so that the real
/// index is not touched and nothing is made in a directory a run can write.
#[derive(Debug)]
pub struct PrivateCopy {
    path: PathBuf,
    /// Removes the directory (and the copy in it) on drop.
    _dir: tempfile::TempDir,
}

impl PrivateCopy {
    /// The copy.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Write `content` as the file at `path`, replacing what is there: written to a file of its own
/// next to it (a name nobody chose, made with `O_EXCL`) and renamed over `path`, so that a link or
/// a pipe at `path` is replaced and never written through. Mode `0644`.
///
/// # Errors
///
/// The errors of making, writing and renaming the file.
pub async fn replace_file(path: &Path, content: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let path = path.to_owned();
    let content = content.to_owned();
    blocking(move || {
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut tmp = tempfile::Builder::new()
            .prefix(".adam-replace-")
            .tempfile_in(dir)?;
        tmp.write_all(&content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            tmp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o644))?;
        }
        tmp.persist(&path).map_err(|e| e.error)?;
        Ok(())
    })
    .await
}

/// Copy the regular file at `source` (at most `max` bytes) into a new private directory, keeping
/// its modification time.
///
/// The time is kept for git: it takes an index entry whose file looks unchanged (same size, same
/// time) as unchanged, except when the entry is as new as the index file itself, then it reads the
/// file again ("racily clean"). A copy stamped with the time it was made would make every entry look
/// old, and a file rewritten within one tick of the index being written would keep its old content.
///
/// # Errors
///
/// As [`read_regular_file`]; and the errors of making the directory and the file.
pub async fn private_copy_of(source: &Path, max: u64) -> io::Result<PrivateCopy> {
    let source = source.to_owned();
    blocking(move || {
        let (mut from, meta) = open_regular(&source)?;
        if meta.len() > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is longer than {max} bytes", source.display()),
            ));
        }
        let mut dir = tempfile::Builder::new();
        dir.prefix("adam-index-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            dir.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let dir = dir.tempdir()?;
        let path = dir.path().join("index");
        let mut to = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let copied = io::copy(&mut (&mut from).take(max + 1), &mut to)?;
        if copied > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is longer than {max} bytes", source.display()),
            ));
        }
        to.set_modified(meta.modified()?)?;
        Ok(PrivateCopy { path, _dir: dir })
    })
    .await
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(io::Error::other)?
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[tokio::test]
    async fn a_regular_file_is_read_and_anything_else_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f");
        std::fs::write(&file, "../..\n").unwrap();
        assert_eq!(read_regular_text(&file, 64).await.unwrap(), "../..\n");

        let link = tmp.path().join("l");
        symlink(&file, &link).unwrap();
        let refused = read_regular_file(&link, 64).await.unwrap_err();
        assert!(
            refused.to_string().contains("not a regular file"),
            "{refused}"
        );

        let dir = tmp.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        assert!(read_regular_file(&dir, 64).await.is_err());

        let fifo = tmp.path().join("p");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let refused = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_regular_file(&fifo, 64),
        )
        .await
        .expect("a pipe is not waited on")
        .unwrap_err();
        assert!(
            refused.to_string().contains("not a regular file"),
            "{refused}"
        );

        std::fs::write(&file, "x".repeat(65)).unwrap();
        assert!(read_regular_file(&file, 64).await.is_err());
        assert!(
            read_regular_file(&tmp.path().join("missing"), 64)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_copy_is_private_keeps_the_time_and_goes_with_its_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("index");
        std::fs::write(&file, "entries").unwrap();
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(then)
            .unwrap();

        let copy = private_copy_of(&file, 1024).await.unwrap();
        assert_eq!(std::fs::read_to_string(copy.path()).unwrap(), "entries");
        assert_eq!(
            std::fs::metadata(copy.path()).unwrap().modified().unwrap(),
            then
        );
        assert!(!copy.path().starts_with(tmp.path()));
        let dir = copy.path().parent().unwrap().to_owned();
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(copy);
        assert!(!dir.exists(), "removed with its owner");

        // Not a link, not too big.
        let link = tmp.path().join("link");
        symlink(&file, &link).unwrap();
        assert!(private_copy_of(&link, 1024).await.is_err());
        assert!(private_copy_of(&file, 3).await.is_err());
    }

    #[tokio::test]
    async fn a_file_is_replaced_and_never_written_through() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let target = tmp.path().join("dir").join(".git");
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        symlink(&victim, &target).unwrap();
        replace_file(&target, b"gitdir: /x\n").await.unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "gitdir: /x\n");
        assert!(
            !std::fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let left: Vec<_> = std::fs::read_dir(tmp.path().join("dir")).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary file is left");
    }

    #[tokio::test]
    async fn a_link_is_not_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        let link = tmp.path().join("l");
        symlink(&dir, &link).unwrap();
        assert!(is_real_directory(&dir).await);
        assert!(!is_real_directory(&link).await);
        assert!(!is_real_directory(&tmp.path().join("missing")).await);
        let file = dir.join("f");
        std::fs::write(&file, "x").unwrap();
        symlink(&file, dir.join("fl")).unwrap();
        assert!(is_real_file(&file).await);
        assert!(!is_real_file(&dir.join("fl")).await);
    }
}
