//! A slot's directory, opened once and held: what the coder's own file operations are relative to.
//!
//! The directory of a slot (`<root>/workspaces/<run>/<dir>`) is the run's to rename: a command in
//! the run's pod, or a process it left running, can move the directory away and put a link to
//! another run's directory at its name, at any moment. The coder holds the whole volume, so a path
//! that is checked and then used by name may be followed there. A [`SlotDir`] makes the check and
//! the hold one step: [`SlotDir::open`] opens the directory without following a link, and everything
//! the coder does in the slot after that is relative to the descriptor, so the name no longer
//! matters. A directory that is moved is still the one the coder opened.
//!
//! # Rules
//!
//! * Every name below the slot is a **plain component**: no `/`, no `..`. The kernel's
//!   `openat2` is asked with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`, so a
//!   link anywhere on the way, the last component included, is refused (`ELOOP`); a walk, one
//!   component at a time with `O_NOFOLLOW`, does the same where `openat2` is not there (Linux before
//!   5.6, a seccomp profile that does not know it, other systems). [`Backend`] says which.
//! * **Reading** may follow a link that stays inside the slot, as the tools always did
//!   ([`SlotDir::open_following`]): the path is resolved first, in this process, over held
//!   directories, without ever asking the kernel to resolve a link or a `..` (an absolute link, or a
//!   `..` above the slot, is [`BeneathError::Escapes`]); the file is then opened by the path that
//!   resolution found, which has no link in it, so a link put there meanwhile is refused and never
//!   followed.
//! * **Writing** follows no link at all. Directories are made one at a time (`mkdirat`), a file is
//!   written to a name nobody chose (`O_EXCL`) and renamed over its target (`renameat`), all in the
//!   directory descriptor of the parent.
//!
//! # What this does not cover
//!
//! `git` finds its work tree by name inside its own process, whatever descriptor the coder holds,
//! so a slot that is swapped while `git` starts is not protected here: [`SlotDir::is_still_here`] is
//! a tripwire that narrows the window and does not close it. What closes it is that the slot's
//! directory cannot be renamed at all in the run's pod (a mount point).

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::ops::Deref;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, RawMode, Stat};
use rustix::io::Errno;

use super::race;

/// The most links one resolution follows before it gives up (Linux's own limit).
const MAX_LINKS: usize = 40;

/// How [`SlotDir`] opens a path below the slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Backend {
    /// `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`: the kernel
    /// refuses a link on the way, in one call (Linux 5.6 or later).
    Kernel,
    /// One component at a time, each opened relative to the one before with `O_NOFOLLOW`: the same
    /// refusals, in more calls, where the kernel has no `openat2`.
    Walk,
}

impl Backend {
    /// The backend this system supports, probed once.
    pub fn detect() -> Self {
        static DETECTED: OnceLock<Backend> = OnceLock::new();
        *DETECTED.get_or_init(|| {
            if kernel_has_openat2() {
                Backend::Kernel
            } else {
                Backend::Walk
            }
        })
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn no_links() -> rustix::fs::ResolveFlags {
    use rustix::fs::ResolveFlags;
    ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn kernel_has_openat2() -> bool {
    rustix::fs::openat2(
        CWD,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        no_links(),
    )
    .is_ok()
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn kernel_has_openat2() -> bool {
    false
}

/// What a name below a slot is, from `fstat` or `lstat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    mode: RawMode,
    size: u64,
}

impl Meta {
    fn of(stat: &Stat) -> Self {
        Self {
            mode: stat.st_mode,
            size: u64::try_from(stat.st_size).unwrap_or(0),
        }
    }

    fn kind(self) -> FileType {
        FileType::from_raw_mode(self.mode)
    }

    /// A directory.
    pub fn is_dir(self) -> bool {
        self.kind() == FileType::Directory
    }

    /// A regular file.
    pub fn is_file(self) -> bool {
        self.kind() == FileType::RegularFile
    }

    /// A symbolic link (only `lstat` says so: an open file is never one).
    pub fn is_symlink(self) -> bool {
        self.kind() == FileType::Symlink
    }

    /// The size in bytes.
    pub fn size(self) -> u64 {
        self.size
    }

    /// The permission bits (`0o7777`).
    #[allow(clippy::useless_conversion)]
    pub fn permissions(self) -> u32 {
        u32::from(self.mode & 0o7777)
    }
}

fn is(e: &io::Error, errno: Errno) -> bool {
    e.raw_os_error() == Some(errno.raw_os_error())
}

/// `name` is one plain component: not empty, not `.` or `..`, no `/`.
fn single(name: &OsStr) -> io::Result<&OsStr> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not the name of one entry of a directory"),
        ));
    }
    Ok(name)
}

/// The normal components of `rel`, which is a path below a slot: no root, no `..` (a `.` is
/// dropped).
fn components_of(rel: &Path) -> io::Result<Vec<&OsStr>> {
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(name) => parts.push(name),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a path below a slot", rel.display()),
                ));
            }
        }
    }
    Ok(parts)
}

fn not_regular(what: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{} is not a regular file (a link, a pipe or a device)",
            what.display()
        ),
    )
}

fn too_long(what: &Path, max: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{} is longer than {max} bytes", what.display()),
    )
}

/// An open directory. Every name is one plain component of it, and nothing is followed: the
/// operations are the `*at` calls on the descriptor.
#[derive(Debug)]
pub struct Dir {
    fd: OwnedFd,
}

impl Dir {
    /// Another descriptor of the same directory.
    ///
    /// # Errors
    ///
    /// `dup` failing.
    pub fn try_clone(&self) -> io::Result<Dir> {
        Ok(Dir {
            fd: self.fd.try_clone()?,
        })
    }

    /// What `name` is, without following it (`lstat`).
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::NotFound`] when there is nothing of that name; `InvalidInput` when `name` is
    /// not one plain component.
    pub fn lstat(&self, name: &OsStr) -> io::Result<Meta> {
        let stat = rustix::fs::statat(&self.fd, single(name)?, AtFlags::SYMLINK_NOFOLLOW)?;
        Ok(Meta::of(&stat))
    }

    /// The directory `name` of this one, opened as [`open_dir`](Self::open_dir) does, with the
    /// kernel's refusal of links (`openat2`) when `backend` is [`Backend::Kernel`].
    fn open_dir_using(&self, name: &OsStr, backend: Backend) -> io::Result<Dir> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if backend == Backend::Kernel {
            let name = single(name)?;
            match rustix::fs::openat2(
                &self.fd,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
                no_links(),
            ) {
                Err(Errno::NOSYS) => {}
                done => {
                    return done
                        .map(|fd| Dir { fd })
                        .map_err(|e| self.classified(name, e.into()));
                }
            }
        }
        let _ = backend;
        self.open_dir(name)
    }

    /// `err` of opening `name`, with the cause told: `O_NOFOLLOW` with `O_DIRECTORY` says
    /// `ENOTDIR` for a link, and a caller wants to tell a link from a file.
    fn classified(&self, name: &OsStr, err: io::Error) -> io::Error {
        if is(&err, Errno::LOOP) || is(&err, Errno::NOTDIR) {
            return match self.lstat(name) {
                Ok(meta) if meta.is_symlink() => Errno::LOOP.into(),
                Ok(_) => Errno::NOTDIR.into(),
                Err(_) => err,
            };
        }
        err
    }

    /// Open the directory `name` of this one. A link is `ELOOP`, a file `ENOTDIR`.
    ///
    /// # Errors
    ///
    /// The errors of `openat`.
    pub fn open_dir(&self, name: &OsStr) -> io::Result<Dir> {
        let name = single(name)?;
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| self.classified(name, e.into()))?;
        Ok(Dir { fd })
    }

    /// `name` with `oflags` (and `O_NOFOLLOW`): a link is `ELOOP`.
    fn open_name(&self, name: &OsStr, oflags: OFlags, mode: Mode) -> io::Result<OwnedFd> {
        let name = single(name)?;
        rustix::fs::openat(
            &self.fd,
            name,
            oflags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            mode,
        )
        .map_err(|e| self.classified(name, e.into()))
    }

    /// Open the regular file `name` for reading, and what it is. Not through a link, not a pipe
    /// (opened without waiting for a writer, and refused after `fstat`), a device or a directory.
    ///
    /// # Errors
    ///
    /// `InvalidInput` ("not a regular file") for anything but a regular file; the errors of `open`.
    pub fn open_regular(&self, name: &OsStr) -> io::Result<(File, Meta)> {
        let fd = self
            .open_name(
                name,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY,
                Mode::empty(),
            )
            .map_err(|e| {
                if is(&e, Errno::LOOP) || is(&e, Errno::NXIO) {
                    not_regular(Path::new(name))
                } else {
                    e
                }
            })?;
        let file = File::from(fd);
        let meta = Meta::of(&rustix::fs::fstat(&file)?);
        if !meta.is_file() {
            return Err(not_regular(Path::new(name)));
        }
        Ok((file, meta))
    }

    /// Make the file `name`, which must not exist (`O_EXCL`: a link there is not followed, it is
    /// in the way), for writing. Mode `0666` less the umask.
    ///
    /// # Errors
    ///
    /// The errors of `open`; `AlreadyExists` when there is something of that name.
    pub fn create_new(&self, name: &OsStr) -> io::Result<File> {
        self.open_name(
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            Mode::from_raw_mode(0o666),
        )
        .map(File::from)
    }

    /// Make the directory `name` (mode `0777` less the umask).
    ///
    /// # Errors
    ///
    /// The errors of `mkdirat`.
    pub fn mkdir(&self, name: &OsStr) -> io::Result<()> {
        rustix::fs::mkdirat(&self.fd, single(name)?, Mode::from_raw_mode(0o777))?;
        Ok(())
    }

    /// Rename `from` to `to`, both in this directory, replacing what is at `to` (a link is replaced,
    /// not followed).
    ///
    /// # Errors
    ///
    /// The errors of `renameat`.
    pub fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        rustix::fs::renameat(&self.fd, single(from)?, &self.fd, single(to)?)?;
        Ok(())
    }

    /// Remove the file or the link `name`; a link is removed and not what it leads to.
    ///
    /// # Errors
    ///
    /// The errors of `unlinkat`.
    pub fn unlink(&self, name: &OsStr) -> io::Result<()> {
        rustix::fs::unlinkat(&self.fd, single(name)?, AtFlags::empty())?;
        Ok(())
    }

    /// Make the link `name` to `target`.
    ///
    /// # Errors
    ///
    /// The errors of `symlinkat`.
    pub fn symlink(&self, target: &Path, name: &OsStr) -> io::Result<()> {
        rustix::fs::symlinkat(target, &self.fd, single(name)?)?;
        Ok(())
    }

    /// Where the link `name` points.
    ///
    /// # Errors
    ///
    /// The errors of `readlinkat`.
    pub fn read_link(&self, name: &OsStr) -> io::Result<PathBuf> {
        let target = rustix::fs::readlinkat(&self.fd, single(name)?, Vec::new())?;
        Ok(PathBuf::from(OsString::from_vec(target.into_bytes())))
    }

    /// A new file in this directory under a name nobody chose (`O_EXCL`): the file and its name.
    fn create_temp(&self, prefix: &str) -> io::Result<(File, OsString)> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut last = io::Error::from(Errno::EXIST);
        for _ in 0..32 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let name = OsString::from(format!(
                "{prefix}{}-{nanos:x}-{}.tmp",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            match self.create_new(&name) {
                Ok(file) => return Ok((file, name)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    /// Write `content` as the file `name` of this directory, replacing what is there: written to a
    /// file of its own next to it and renamed over it, so a link or a pipe at `name` is replaced
    /// and never written through. Mode `0644`.
    ///
    /// # Errors
    ///
    /// The errors of making, writing and renaming the file.
    pub fn replace_file(&self, name: &OsStr, content: &[u8]) -> io::Result<()> {
        let name = single(name)?;
        let (mut tmp, tmp_name) = self.create_temp(".adam-replace-")?;
        let done = (|| -> io::Result<()> {
            tmp.write_all(content)?;
            rustix::fs::fchmod(&tmp, Mode::from_raw_mode(0o644))?;
            drop(tmp);
            self.rename(&tmp_name, name)
        })();
        if done.is_err() {
            let _ = self.unlink(&tmp_name);
        }
        done
    }
}

/// Why a path could not be walked ([`SlotDir::walk`]); the numbers count the path's components
/// from 0.
#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    /// This component is a symbolic link.
    #[error("component {0} is a symbolic link")]
    Link(usize),
    /// This component is a file (or anything but a directory) with something below it.
    #[error("component {0} is not a directory")]
    NotDirectory(usize),
    /// This component is not there (only when nothing is made).
    #[error("component {0} does not exist")]
    Missing(usize),
    /// Anything else.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Why a path could not be resolved inside a slot ([`SlotDir::resolve_inside`]).
#[derive(Debug, thiserror::Error)]
pub enum BeneathError {
    /// An absolute link, or a `..` above the slot: it leads out.
    #[error("it leads outside the slot")]
    Escapes,
    /// It goes through a name the caller does not want entered.
    #[error("it goes through {0:?}")]
    Avoided(OsString),
    /// More links than one resolution follows.
    #[error("too many levels of symbolic links")]
    TooManyLinks,
    /// A link was put on the path after it was resolved: the file was not opened.
    #[error("a symbolic link appeared on the path after it was resolved")]
    Changed,
    /// Anything else, `NotFound` among it.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A slot's directory, held open. Cheap to share: keep it in an [`Arc`].
///
/// See [`safe_fs`](crate::safe_fs). The methods are blocking system calls on a local file
/// system: call them from `spawn_blocking` ([`blocking`](Self::blocking)).
pub struct SlotDir {
    dir: Dir,
    path: PathBuf,
    dev: u64,
    ino: u64,
    backend: Backend,
}

impl Deref for SlotDir {
    type Target = Dir;

    fn deref(&self) -> &Dir {
        &self.dir
    }
}

impl fmt::Debug for SlotDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlotDir")
            .field("path", &self.path)
            .field("inode", &(self.dev, self.ino))
            .finish_non_exhaustive()
    }
}

impl PartialEq for SlotDir {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.dev == other.dev && self.ino == other.ino
    }
}

impl Eq for SlotDir {}

/// The device and the inode of a `stat`, whatever their types are here.
#[allow(clippy::unnecessary_cast)]
fn identity(stat: &Stat) -> (u64, u64) {
    (stat.st_dev as u64, stat.st_ino as u64)
}

impl SlotDir {
    /// Open the directory at `path`, which must be a directory itself and not a link. The
    /// components above the last are the coder's own (the volume, the run's directory) and are
    /// followed as they are; the last one is opened without following a link, and is the one
    /// everything afterwards is relative to.
    ///
    /// # Errors
    ///
    /// `InvalidInput` ("not a plain directory") for a link or a file, `NotFound` when there is
    /// nothing, and the errors of `open`.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_with(path, Backend::detect())
    }

    /// [`open`](Self::open) with the backend chosen: for a test that runs the fallback.
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open).
    #[cfg(any(test, feature = "test-util"))]
    pub fn open_using(path: &Path, backend: Backend) -> io::Result<Self> {
        Self::open_with(path, backend)
    }

    fn open_with(path: &Path, backend: Backend) -> io::Result<Self> {
        let plain = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is not a plain directory (a command replaced it by a link or a file)",
                    path.display()
                ),
            )
        };
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(plain());
        };
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let above = Dir {
            fd: rustix::fs::openat(
                CWD,
                parent,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?,
        };
        let held = above.open_dir_using(name, backend).map_err(|e| {
            if is(&e, Errno::LOOP) || is(&e, Errno::NOTDIR) {
                plain()
            } else {
                e
            }
        })?;
        let (dev, ino) = identity(&rustix::fs::fstat(&held.fd)?);
        Ok(Self {
            dir: held,
            path: path.to_owned(),
            dev,
            ino,
            backend,
        })
    }

    /// [`open`](Self::open) from async code.
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open).
    pub async fn open_async(path: &Path) -> io::Result<Self> {
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || Self::open(&path))
            .await
            .map_err(io::Error::other)?
    }

    /// The path the directory was opened with. A name, for messages and for `git` (which can only
    /// be given a name): the directory held is the one that was at it then.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run `work` on the blocking pool, with this directory.
    ///
    /// # Errors
    ///
    /// What `work` returns; a task that did not finish is an `io::Error`.
    pub async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl FnOnce(&SlotDir) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let root = Arc::clone(self);
        tokio::task::spawn_blocking(move || work(&root))
            .await
            .map_err(io::Error::other)?
    }

    /// Whether the name this directory was opened with is still this directory: the descriptor's
    /// inode against `lstat` of the name, which must be a directory and not a link. `false` when
    /// the directory was moved, replaced, removed or the name became a link.
    ///
    /// A **tripwire**, for the callers that can only use the slot by name (`git`): it narrows the
    /// time in which a swap goes unseen and does not close it.
    pub fn is_still_here(&self) -> bool {
        let Ok(held) = rustix::fs::fstat(&self.dir.fd) else {
            return false;
        };
        let Ok(named) = rustix::fs::statat(CWD, self.path.as_path(), AtFlags::SYMLINK_NOFOLLOW)
        else {
            return false;
        };
        identity(&held) == (self.dev, self.ino)
            && identity(&named) == (self.dev, self.ino)
            && Meta::of(&named).is_dir()
    }

    /// Open `rel` below the slot with `oflags`, refusing a link anywhere on the way (the last
    /// component included).
    fn open_below(&self, rel: &Path, oflags: OFlags, mode: Mode) -> io::Result<OwnedFd> {
        // Plain components only, whatever the backend: `..` is refused here and not left to the
        // kernel to refuse in its own words.
        let parts = components_of(rel)?;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if self.backend == Backend::Kernel {
            let path: PathBuf = if parts.is_empty() {
                PathBuf::from(".")
            } else {
                parts.iter().collect()
            };
            match rustix::fs::openat2(
                &self.dir.fd,
                path.as_path(),
                oflags | OFlags::CLOEXEC,
                mode,
                no_links(),
            ) {
                // A kernel that lost the call since the probe: walk.
                Err(Errno::NOSYS) => {}
                done => return done.map_err(Into::into),
            }
        }
        let Some((last, above)) = parts.split_last() else {
            return Ok(rustix::fs::openat(
                &self.dir.fd,
                ".",
                oflags | OFlags::CLOEXEC,
                mode,
            )?);
        };
        let mut at = self.dir.try_clone()?;
        for name in above {
            at = at.open_dir(name)?;
        }
        at.open_name(last, oflags, mode)
    }

    /// The directory at `rel` below the slot, which must be there and have no link on the way
    /// (`rel` empty: the slot itself).
    ///
    /// # Errors
    ///
    /// `ELOOP` for a link, `ENOTDIR` for a file, `NotFound`; `InvalidInput` for a `rel` that is not
    /// below the slot.
    pub fn dir_below(&self, rel: &Path) -> io::Result<Dir> {
        Ok(Dir {
            fd: self.open_below(rel, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?,
        })
    }

    /// The parent directory of `rel` and its last component.
    fn parent_of<'a>(&self, rel: &'a Path) -> io::Result<(Dir, &'a OsStr)> {
        let parts = components_of(rel)?;
        let Some((last, above)) = parts.split_last() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the slot itself has no parent below it",
            ));
        };
        let above: PathBuf = above.iter().collect();
        Ok((self.dir_below(&above)?, last))
    }

    /// What `rel` is, without following it (`lstat`); nothing on the way is a link.
    ///
    /// # Errors
    ///
    /// As [`dir_below`](Self::dir_below).
    pub fn lstat_below(&self, rel: &Path) -> io::Result<Meta> {
        if components_of(rel)?.is_empty() {
            return Ok(Meta::of(&rustix::fs::fstat(&self.dir.fd)?));
        }
        let (parent, name) = self.parent_of(rel)?;
        parent.lstat(name)
    }

    /// Where the link `rel` points; nothing above it is a link.
    ///
    /// # Errors
    ///
    /// As [`dir_below`](Self::dir_below).
    pub fn read_link_below(&self, rel: &Path) -> io::Result<PathBuf> {
        let (parent, name) = self.parent_of(rel)?;
        parent.read_link(name)
    }

    /// Open the regular file `rel` for reading, and what it is. Nothing on the way is a link, and
    /// a pipe, a device or a directory is `InvalidInput` ("not a regular file").
    ///
    /// # Errors
    ///
    /// As [`Dir::open_regular`], and as [`dir_below`](Self::dir_below).
    pub fn open_regular_below(&self, rel: &Path) -> io::Result<(File, Meta)> {
        let fd = self
            .open_below(
                rel,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY,
                Mode::empty(),
            )
            .map_err(|e| {
                if is(&e, Errno::LOOP) || is(&e, Errno::NXIO) {
                    not_regular(&self.path.join(rel))
                } else {
                    e
                }
            })?;
        let file = File::from(fd);
        let meta = Meta::of(&rustix::fs::fstat(&file)?);
        if !meta.is_file() {
            return Err(not_regular(&self.path.join(rel)));
        }
        Ok((file, meta))
    }

    /// The bytes of the regular file `rel`, at most `max` of them
    /// ([`safe_fs::read_regular_file`](super::read_regular_file) for a file below the slot).
    ///
    /// # Errors
    ///
    /// As [`open_regular_below`](Self::open_regular_below); `InvalidInput` for a file longer than
    /// `max`.
    pub fn read_regular_below(&self, rel: &Path, max: u64) -> io::Result<Vec<u8>> {
        let (file, meta) = self.open_regular_below(rel)?;
        if meta.size() > max {
            return Err(too_long(&self.path.join(rel), max));
        }
        let mut bytes = Vec::new();
        file.take(max + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max {
            return Err(too_long(&self.path.join(rel), max));
        }
        Ok(bytes)
    }

    /// [`read_regular_below`](Self::read_regular_below) as text: lossy.
    ///
    /// # Errors
    ///
    /// As [`read_regular_below`](Self::read_regular_below).
    pub fn read_regular_text_below(&self, rel: &Path, max: u64) -> io::Result<String> {
        self.read_regular_below(rel, max)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Write `content` as the file `rel`, replacing what is there
    /// ([`Dir::replace_file`]); nothing above it is a link.
    ///
    /// # Errors
    ///
    /// As [`dir_below`](Self::dir_below) and [`Dir::replace_file`].
    pub fn replace_file_below(&self, rel: &Path, content: &[u8]) -> io::Result<()> {
        let (parent, name) = self.parent_of(rel)?;
        parent.replace_file(name, content)
    }

    /// The directory `rel` below the slot, walked one component at a time and, with `create`,
    /// made as it goes; a link or a file on the way is told with the component it is. For a
    /// caller that writes, or that must say which component is wrong.
    ///
    /// # Errors
    ///
    /// [`WalkError`].
    pub fn walk(&self, rel: &Path, create: bool) -> Result<Dir, WalkError> {
        let mut at = self.dir.try_clone()?;
        for (i, name) in components_of(rel)?.into_iter().enumerate() {
            at = match at.open_dir(name) {
                Ok(next) => next,
                Err(e) if is(&e, Errno::NOENT) => {
                    if !create {
                        return Err(WalkError::Missing(i));
                    }
                    match at.mkdir(name) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e.into()),
                    }
                    at.open_dir(name).map_err(|e| walk_error(i, e))?
                }
                Err(e) => return Err(walk_error(i, e)),
            };
        }
        Ok(at)
    }

    /// Resolve `rel` inside the slot, following the links that stay inside it, and say the path
    /// without any: the components from the slot's root (empty: the root itself). `avoid` names
    /// the components that must not be entered, however they are reached (`.git`).
    ///
    /// Done over held directories and never by asking the kernel to resolve a link or a `..`:
    /// each link is read and its text is walked as more components, an absolute one is
    /// [`BeneathError::Escapes`], and so is a `..` at the slot's root.
    ///
    /// # Errors
    ///
    /// [`BeneathError`]; `NotFound` for a name that is not there (a link to nothing included).
    pub fn resolve_inside(
        &self,
        rel: &Path,
        avoid: &dyn Fn(&OsStr) -> bool,
    ) -> Result<Vec<OsString>, BeneathError> {
        let up = OsString::from("..");
        let mut queue: VecDeque<OsString> = components_of(rel)?
            .into_iter()
            .map(OsStr::to_owned)
            .collect();
        let mut names: Vec<OsString> = Vec::new();
        let mut dirs = vec![self.dir.try_clone()?];
        let mut links = 0;
        while let Some(token) = queue.pop_front() {
            if token == up {
                if names.pop().is_none() {
                    return Err(BeneathError::Escapes);
                }
                dirs.pop();
                continue;
            }
            if avoid(&token) {
                return Err(BeneathError::Avoided(token));
            }
            let Some(at) = dirs.last() else {
                return Err(BeneathError::Escapes);
            };
            let meta = at.lstat(&token)?;
            if meta.is_symlink() {
                links += 1;
                if links > MAX_LINKS {
                    return Err(BeneathError::TooManyLinks);
                }
                let target = at.read_link(&token)?;
                let mut text = Vec::new();
                for component in target.components() {
                    match component {
                        Component::Normal(name) => text.push(name.to_owned()),
                        Component::ParentDir => text.push(up.clone()),
                        Component::CurDir => {}
                        Component::RootDir | Component::Prefix(_) => {
                            return Err(BeneathError::Escapes);
                        }
                    }
                }
                for part in text.into_iter().rev() {
                    queue.push_front(part);
                }
            } else if meta.is_dir() {
                let next = at.open_dir(&token)?;
                dirs.push(next);
                names.push(token);
            } else {
                // Only the last component may be something other than a directory.
                if !queue.is_empty() {
                    return Err(io::Error::from(Errno::NOTDIR).into());
                }
                names.push(token);
            }
        }
        Ok(names)
    }

    /// Open `rel` for reading, following the links that stay inside the slot: the path is
    /// resolved ([`resolve_inside`](Self::resolve_inside)) and the file is opened by the path that
    /// resolution found, with no link allowed in it. A directory, a pipe or a device is opened as
    /// well (without waiting): the metadata says what it is.
    ///
    /// # Errors
    ///
    /// [`BeneathError`]; a link put on the resolved path meanwhile is `ELOOP`.
    pub fn open_following(
        &self,
        rel: &Path,
        avoid: &dyn Fn(&OsStr) -> bool,
    ) -> Result<(File, Meta), BeneathError> {
        let resolved: PathBuf = self.resolve_inside(rel, avoid)?.iter().collect();
        race::checkpoint(&self.path, "read");
        let fd = self
            .open_below(
                &resolved,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY,
                Mode::empty(),
            )
            .map_err(|e| {
                if is(&e, Errno::LOOP) {
                    BeneathError::Changed
                } else {
                    e.into()
                }
            })?;
        let file = File::from(fd);
        let meta = Meta::of(&rustix::fs::fstat(&file).map_err(io::Error::from)?);
        Ok((file, meta))
    }
}

fn walk_error(at: usize, e: io::Error) -> WalkError {
    if is(&e, Errno::LOOP) {
        WalkError::Link(at)
    } else if is(&e, Errno::NOTDIR) {
        WalkError::NotDirectory(at)
    } else {
        WalkError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn backends() -> Vec<Backend> {
        let mut all = vec![Backend::Walk];
        if Backend::detect() == Backend::Kernel {
            all.push(Backend::Kernel);
        }
        all
    }

    fn never(_: &OsStr) -> bool {
        false
    }

    fn dot_git(name: &OsStr) -> bool {
        name.to_string_lossy().eq_ignore_ascii_case(".git")
    }

    /// A slot with `a.txt`, `sub/b.txt` and `.git`; and next to it `outside/secret`.
    fn slot() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let slot = tmp.path().join("slot");
        std::fs::create_dir_all(slot.join("sub/deep")).unwrap();
        std::fs::write(slot.join("a.txt"), "a\n").unwrap();
        std::fs::write(slot.join("sub/b.txt"), "b\n").unwrap();
        std::fs::write(slot.join(".git"), "gitdir: x\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("outside")).unwrap();
        std::fs::write(tmp.path().join("outside/secret"), "s3cr3t\n").unwrap();
        tmp
    }

    fn open(tmp: &tempfile::TempDir, backend: Backend) -> SlotDir {
        SlotDir::open_using(&tmp.path().join("slot"), backend).unwrap()
    }

    fn errno_of<T: fmt::Debug>(result: io::Result<T>) -> Option<i32> {
        result.unwrap_err().raw_os_error()
    }

    #[test]
    fn a_slot_is_a_directory_itself_and_not_a_link_or_a_file() {
        let tmp = slot();
        for backend in backends() {
            let slot = tmp.path().join("slot");
            assert!(SlotDir::open_using(&slot, backend).is_ok());
            symlink(&slot, tmp.path().join("link")).unwrap();
            let refused = SlotDir::open_using(&tmp.path().join("link"), backend).unwrap_err();
            assert!(
                refused.to_string().contains("not a plain directory"),
                "{refused}"
            );
            let refused = SlotDir::open_using(&slot.join("a.txt"), backend).unwrap_err();
            assert!(
                refused.to_string().contains("not a plain directory"),
                "{refused}"
            );
            let missing = SlotDir::open_using(&tmp.path().join("missing"), backend).unwrap_err();
            assert_eq!(missing.kind(), io::ErrorKind::NotFound);
            std::fs::remove_file(tmp.path().join("link")).unwrap();
        }
    }

    #[test]
    fn a_slot_that_was_moved_or_replaced_is_not_still_here() {
        let tmp = slot();
        let path = tmp.path().join("slot");
        let held = open(&tmp, Backend::detect());
        assert!(held.is_still_here());
        // Moved away, a link to another directory at its name.
        std::fs::rename(&path, tmp.path().join("slot.away")).unwrap();
        symlink(tmp.path().join("outside"), &path).unwrap();
        assert!(!held.is_still_here(), "a link");
        std::fs::remove_file(&path).unwrap();
        assert!(!held.is_still_here(), "nothing");
        // A directory of the same name is not the directory that was opened.
        std::fs::create_dir(&path).unwrap();
        assert!(!held.is_still_here(), "another directory");
        // Put back, it is again.
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(tmp.path().join("slot.away"), &path).unwrap();
        assert!(held.is_still_here(), "put back");
    }

    #[test]
    fn what_is_done_after_the_slot_moved_is_done_in_the_directory_that_was_opened() {
        let tmp = slot();
        for backend in backends() {
            let path = tmp.path().join("slot");
            let held = open(&tmp, backend);
            std::fs::rename(&path, tmp.path().join("slot.away")).unwrap();
            symlink(tmp.path().join("outside"), &path).unwrap();

            assert_eq!(
                held.read_regular_text_below(Path::new("a.txt"), 64)
                    .unwrap(),
                "a\n"
            );
            held.replace_file_below(Path::new("sub/new.txt"), b"new\n")
                .unwrap();
            assert!(!tmp.path().join("outside/new.txt").exists());
            assert_eq!(
                std::fs::read_to_string(tmp.path().join("slot.away/sub/new.txt")).unwrap(),
                "new\n"
            );

            std::fs::remove_file(&path).unwrap();
            std::fs::rename(tmp.path().join("slot.away"), &path).unwrap();
            std::fs::remove_file(path.join("sub/new.txt")).unwrap();
        }
    }

    #[test]
    fn a_link_anywhere_on_the_way_is_refused_and_a_file_is_not_a_directory() {
        let tmp = slot();
        let path = tmp.path().join("slot");
        symlink("a.txt", path.join("alias")).unwrap();
        symlink("sub", path.join("srclink")).unwrap();
        symlink(tmp.path().join("outside"), path.join("out")).unwrap();
        for backend in backends() {
            let held = open(&tmp, backend);
            let read = |rel: &str| held.open_regular_below(Path::new(rel));
            assert!(read("a.txt").is_ok());
            assert!(read("sub/b.txt").is_ok());
            // The last component, a middle one, one that leaves the slot.
            for rel in ["alias", "srclink/b.txt", "out/secret"] {
                let refused = read(rel).unwrap_err();
                assert!(
                    refused.to_string().contains("not a regular file")
                        || refused.raw_os_error() == Some(Errno::LOOP.raw_os_error()),
                    "{rel}: {refused}"
                );
            }
            assert_eq!(
                errno_of(held.dir_below(Path::new("srclink"))),
                Some(Errno::LOOP.raw_os_error())
            );
            assert_eq!(
                errno_of(held.dir_below(Path::new("a.txt/x"))),
                Some(Errno::NOTDIR.raw_os_error())
            );
            assert_eq!(
                held.dir_below(Path::new("nope")).unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
            // Not a path below the slot.
            for rel in ["../outside", "/etc"] {
                assert_eq!(
                    held.dir_below(Path::new(rel)).unwrap_err().kind(),
                    io::ErrorKind::InvalidInput,
                    "{rel}"
                );
            }
            // A directory, a pipe.
            assert!(
                read("sub")
                    .unwrap_err()
                    .to_string()
                    .contains("not a regular file")
            );
        }
    }

    #[test]
    fn a_pipe_is_not_waited_on() {
        let tmp = slot();
        let fifo = tmp.path().join("slot/pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        for backend in backends() {
            let held = open(&tmp, backend);
            let refused = held.open_regular_below(Path::new("pipe")).unwrap_err();
            assert!(
                refused.to_string().contains("not a regular file"),
                "{refused}"
            );
            let (file, meta) = held
                .open_following(Path::new("pipe"), &never)
                .map_err(|e| e.to_string())
                .unwrap();
            assert!(!meta.is_file());
            drop(file);
        }
    }

    #[test]
    fn a_file_is_read_up_to_its_limit() {
        let tmp = slot();
        for backend in backends() {
            let held = open(&tmp, backend);
            assert_eq!(
                held.read_regular_below(Path::new("a.txt"), 2).unwrap(),
                b"a\n"
            );
            assert!(held.read_regular_below(Path::new("a.txt"), 1).is_err());
        }
    }

    #[test]
    fn a_file_replaces_what_is_there_and_is_never_written_through() {
        let tmp = slot();
        let path = tmp.path().join("slot");
        for backend in backends() {
            let held = open(&tmp, backend);
            std::fs::remove_file(path.join(".git")).unwrap();
            symlink(tmp.path().join("outside/secret"), path.join(".git")).unwrap();
            held.replace_file_below(Path::new(".git"), b"gitdir: /x\n")
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(tmp.path().join("outside/secret")).unwrap(),
                "s3cr3t\n"
            );
            let meta = std::fs::symlink_metadata(path.join(".git")).unwrap();
            assert!(meta.is_file());
            {
                use std::os::unix::fs::PermissionsExt as _;
                assert_eq!(meta.permissions().mode() & 0o777, 0o644);
            }
            // Nothing is left beside it, and a directory above it that is a link is not entered.
            let mut left: Vec<_> = std::fs::read_dir(&path)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            left.sort();
            assert_eq!(left, [".git", "a.txt", "sub"]);
            symlink(tmp.path().join("outside"), path.join("out")).unwrap();
            assert!(
                held.replace_file_below(Path::new("out/secret"), b"x")
                    .is_err()
            );
            assert_eq!(
                std::fs::read_to_string(tmp.path().join("outside/secret")).unwrap(),
                "s3cr3t\n"
            );
            std::fs::remove_file(path.join("out")).unwrap();
        }
    }

    #[test]
    fn walking_says_which_component_is_wrong_and_makes_the_missing_ones() {
        let tmp = slot();
        let path = tmp.path().join("slot");
        symlink(tmp.path().join("outside"), path.join("out")).unwrap();
        for backend in backends() {
            let held = open(&tmp, backend);
            assert!(held.walk(Path::new("sub/deep"), false).is_ok());
            assert!(matches!(
                held.walk(Path::new("sub/nope/x"), false),
                Err(WalkError::Missing(1))
            ));
            assert!(matches!(
                held.walk(Path::new("sub/out"), false),
                Err(WalkError::Missing(1))
            ));
            assert!(matches!(
                held.walk(Path::new("out/x"), true),
                Err(WalkError::Link(0))
            ));
            assert!(matches!(
                held.walk(Path::new("sub/b.txt/x"), true),
                Err(WalkError::NotDirectory(1))
            ));
            let made = held.walk(Path::new("new/er/dir"), true).unwrap();
            made.create_new(OsStr::new("f.txt"))
                .unwrap()
                .write_all(b"x")
                .unwrap();
            assert!(path.join("new/er/dir/f.txt").is_file());
            assert!(!tmp.path().join("outside/new").exists());
            std::fs::remove_dir_all(path.join("new")).unwrap();
        }
    }

    #[test]
    fn a_link_that_stays_inside_is_followed_and_one_that_leaves_or_enters_git_is_not() {
        let tmp = slot();
        let path = tmp.path().join("slot");
        symlink("a.txt", path.join("alias")).unwrap();
        symlink("sub", path.join("srclink")).unwrap();
        symlink("../a.txt", path.join("sub/up")).unwrap();
        symlink("../../a.txt", path.join("sub/too-far")).unwrap();
        symlink(tmp.path().join("outside/secret"), path.join("abs")).unwrap();
        symlink(".git", path.join("sub/g")).unwrap();
        symlink("../.git", path.join("sub/h")).unwrap();
        symlink("loop2", path.join("loop1")).unwrap();
        symlink("loop1", path.join("loop2")).unwrap();
        symlink("nowhere", path.join("dangling")).unwrap();
        symlink(".", path.join("sub/here")).unwrap();
        for backend in backends() {
            let held = open(&tmp, backend);
            let resolve = |rel: &str| held.resolve_inside(Path::new(rel), &dot_git);
            let names = |rel: &str| -> Vec<String> {
                resolve(rel)
                    .unwrap()
                    .iter()
                    .map(|n| n.to_string_lossy().into_owned())
                    .collect()
            };
            assert_eq!(names("a.txt"), ["a.txt"]);
            assert_eq!(names("alias"), ["a.txt"]);
            assert_eq!(names("srclink/b.txt"), ["sub", "b.txt"]);
            assert_eq!(names("sub/up"), ["a.txt"]);
            assert_eq!(names("sub/here/b.txt"), ["sub", "b.txt"]);
            assert_eq!(names("sub/here"), ["sub"]);
            assert!(names(".").is_empty());
            assert!(matches!(resolve("sub/too-far"), Err(BeneathError::Escapes)));
            assert!(matches!(resolve("abs"), Err(BeneathError::Escapes)));
            assert!(matches!(resolve(".git"), Err(BeneathError::Avoided(_))));
            assert!(matches!(resolve("sub/g"), Err(BeneathError::Avoided(_))));
            assert!(matches!(resolve("sub/h"), Err(BeneathError::Avoided(_))));
            assert!(matches!(resolve("loop1"), Err(BeneathError::TooManyLinks)));
            for rel in ["dangling", "nope", "sub/nope/x"] {
                assert!(
                    matches!(resolve(rel), Err(BeneathError::Io(e)) if e.kind() == io::ErrorKind::NotFound),
                    "{rel}"
                );
            }
            // A file with something below it.
            assert!(matches!(resolve("a.txt/x"), Err(BeneathError::Io(_))));
            // And the open is by that resolution.
            let (mut file, meta) = held
                .open_following(Path::new("srclink/up"), &dot_git)
                .map_err(|e| e.to_string())
                .unwrap();
            let mut text = String::new();
            file.read_to_string(&mut text).unwrap();
            assert_eq!((text.as_str(), meta.is_file()), ("a\n", true));
            let (_, meta) = held
                .open_following(Path::new("sub/here"), &dot_git)
                .map_err(|e| e.to_string())
                .unwrap();
            assert!(meta.is_dir());
            let (_, meta) = held
                .open_following(Path::new("."), &dot_git)
                .map_err(|e| e.to_string())
                .unwrap();
            assert!(meta.is_dir());
        }
    }

    #[test]
    fn a_directory_swapped_for_a_link_after_the_resolution_is_not_followed() {
        let tmp = slot();
        let path = tmp.path().join("slot");
        for backend in backends() {
            let held = open(&tmp, backend);
            let hook = race::on_checkpoint(&path, {
                let (path, outside) = (path.clone(), tmp.path().join("outside"));
                move |at, _| {
                    if at == "read" {
                        std::fs::rename(path.join("sub"), path.join("sub.away")).unwrap();
                        symlink(&outside, path.join("sub")).unwrap();
                    }
                }
            });
            std::fs::write(tmp.path().join("outside/b.txt"), "SECRET\n").unwrap();
            let refused = held
                .open_following(Path::new("sub/b.txt"), &never)
                .expect_err("the link is not followed");
            assert_eq!(hook.reached("read"), 1);
            assert!(matches!(&refused, BeneathError::Changed), "{refused}");
            drop(hook);
            std::fs::remove_file(path.join("sub")).unwrap();
            std::fs::rename(path.join("sub.away"), path.join("sub")).unwrap();
        }
    }

    #[test]
    fn the_operations_of_a_directory_do_not_follow_what_they_are_given() {
        let tmp = slot();
        let held = open(&tmp, Backend::detect());
        // Only one plain component is a name.
        for bad in ["", ".", "..", "a/b", "/etc"] {
            assert_eq!(
                held.lstat(OsStr::new(bad)).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{bad:?}"
            );
        }
        // `O_EXCL`: an existing name, a link included, is in the way and is not followed.
        symlink(tmp.path().join("outside/secret"), tmp.path().join("slot/l")).unwrap();
        assert_eq!(
            held.create_new(OsStr::new("l")).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            held.create_new(OsStr::new("a.txt")).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        // A link is removed, not what it leads to; read; and made.
        assert_eq!(
            held.read_link(OsStr::new("l")).unwrap(),
            tmp.path().join("outside/secret")
        );
        held.unlink(OsStr::new("l")).unwrap();
        assert!(tmp.path().join("outside/secret").exists());
        held.symlink(Path::new("a.txt"), OsStr::new("l2")).unwrap();
        assert!(held.lstat(OsStr::new("l2")).unwrap().is_symlink());
        // A rename replaces a link at the target.
        held.rename(OsStr::new("a.txt"), OsStr::new("l2")).unwrap();
        assert!(held.lstat(OsStr::new("l2")).unwrap().is_file());
        // The metadata.
        let meta = held.lstat(OsStr::new("sub")).unwrap();
        assert!(meta.is_dir() && !meta.is_file() && !meta.is_symlink());
        assert_eq!(held.lstat(OsStr::new("l2")).unwrap().size(), 2);
        assert_eq!(
            held.lstat(OsStr::new("l2")).unwrap().permissions() & 0o600,
            0o600
        );
    }
}
