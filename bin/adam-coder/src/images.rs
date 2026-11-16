//! Images on the workspace volume: what the model is sent when a person attaches a picture or a tool
//! reads one.
//!
//! The bytes of an image the model sees are kept under
//! `<WORKSPACE_ROOT>/images/<scope>/<sha256>.<ext>` ([`FileImages`], an
//! [`adam_llm_agent::KeptImages`]); the history and the journal hold only the reference (its SHA-256,
//! type, size, dimensions and origin).
//!
//! * **Per conversation.** The scope is 32 hexadecimal digits of a SHA-256 of the conversation's id (of
//!   the root run's id for a run with no conversation), so a later task of the conversation still
//!   sees the screenshot the person sent first. [`Janitor`](crate::Janitor) removes a scope when the
//!   conversation has been idle as long as it keeps scratch projects (`SCRATCH_IDLE_DAYS`).
//! * **Private, and no link is followed.** Like [`FileKept`](crate::kept::FileKept): the directories
//!   are opened one component at a time below the root without following a link, made `0700`; a file
//!   is written under a temporary name with `O_EXCL`, `0600`, and renamed; a link or a file in the way
//!   is refused, no path is in a message, and a file with more than one hard link is not read.
//! * **The hash is checked at load.** A command of the local environment runs as the same user and
//!   can rewrite a file: what is read back must be the bytes whose SHA-256 names the file, or it is
//!   [`ImageKeepError::Corrupt`] and the model is sent the stand-in line. An image that was changed is
//!   never sent.
//! * **Bounded.** One image is at most 4 MiB (the most `MODEL_IMAGE_MAX_BYTES` allows) and a scope at
//!   most [`SCOPE_MAX`] (64 MiB) of the files this process names; the check and the write are made one
//!   at a time in this process.
//! * **Not scrubbed.** An image is bytes, not text: nothing here, and nothing in the agent, looks for a
//!   secret in it. It reaches the volume only from the sources the deployment allows to reach the
//!   model (`MODEL_IMAGE_SOURCES`).

use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use adam_llm_agent::{ImageKeepError, KeptImages, is_hex, sha256_hex};
use adam_workspace::safe_fs::{Dir, SlotDir, WalkError};
use async_trait::async_trait;

/// The most bytes a scope keeps on the volume.
pub const SCOPE_MAX: u64 = 64 * 1024 * 1024;

/// The most bytes one kept image may be: the most the model is ever sent.
const IMAGE_MAX: u64 = 4 * 1024 * 1024;

/// A leftover temporary file is removed when it is older than this.
const STALE_TMP: Duration = Duration::from_secs(600);

/// One check-then-write at a time in this process.
static KEEP_LOCK: Mutex<()> = Mutex::new(());

/// The extensions of the types an image is kept as.
const EXTENSIONS: [&str; 4] = ["png", "jpg", "gif", "webp"];

/// Images as files under `<workspace_root>/images`.
#[derive(Debug, Clone)]
pub struct FileImages {
    root: std::path::PathBuf,
    scope_max: u64,
}

fn io_error(e: &io::Error) -> ImageKeepError {
    ImageKeepError::Io(format!("the volume refused it ({})", e.kind()))
}

fn walk_error(e: &WalkError) -> ImageKeepError {
    match e {
        WalkError::Link(_) | WalkError::NotDirectory(_) => ImageKeepError::Refused(
            "the place for images is not usable (something else is where its directory should be)"
                .to_owned(),
        ),
        WalkError::Missing(_) => ImageKeepError::NotFound(String::new()),
        WalkError::Io(e) => io_error(e),
    }
}

fn join_failed() -> ImageKeepError {
    ImageKeepError::Io("the volume could not be reached".to_owned())
}

/// `<64 hex>.<png|jpg|gif|webp>`: a file this process makes.
fn is_ours(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(sha, ext)| is_hex(sha, 64) && EXTENSIONS.contains(&ext))
}

fn extension(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    }
}

fn checked(scope: &str, sha256: &str) -> Result<(), ImageKeepError> {
    if !is_hex(scope, 32) {
        return Err(ImageKeepError::Malformed(scope.chars().take(40).collect()));
    }
    if !is_hex(sha256, 64) {
        return Err(ImageKeepError::Malformed(sha256.chars().take(70).collect()));
    }
    Ok(())
}

impl FileImages {
    /// Images under `<workspace_root>/images`.
    pub fn new(workspace_root: &Path) -> Self {
        Self {
            root: workspace_root.to_path_buf(),
            scope_max: SCOPE_MAX,
        }
    }

    /// Keep at most `max` bytes per scope, not [`SCOPE_MAX`]: how a test reaches the bound with a
    /// little data.
    #[must_use]
    pub fn with_scope_max(mut self, max: u64) -> Self {
        self.scope_max = max;
        self
    }

    /// The directory `images/<scope>` below the root, one component at a time, no link followed.
    fn dir_of(&self, scope: &str, create: bool) -> Result<Dir, ImageKeepError> {
        if create {
            std::fs::create_dir_all(&self.root).map_err(|e| io_error(&e))?;
        }
        let root = SlotDir::open(&self.root).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound if !create => ImageKeepError::NotFound(String::new()),
            _ => io_error(&e),
        })?;
        if create {
            let images = root
                .walk(Path::new("images"), true)
                .map_err(|e| walk_error(&e))?;
            images.set_private().map_err(|e| io_error(&e))?;
        }
        let dir = root
            .walk(&Path::new("images").join(scope), create)
            .map_err(|e| walk_error(&e))?;
        if create {
            dir.set_private().map_err(|e| io_error(&e))?;
        }
        Ok(dir)
    }

    fn keep_blocking(
        &self,
        scope: &str,
        sha256: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<(), ImageKeepError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        checked(scope, sha256)?;
        if sha256_hex(bytes) != sha256 {
            return Err(ImageKeepError::Mismatch);
        }
        if bytes.len() as u64 > IMAGE_MAX {
            return Err(ImageKeepError::Refused(
                "an image of more than 4 MiB is not kept".to_owned(),
            ));
        }
        let _one_at_a_time = KEEP_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = self.dir_of(scope, true)?;
        let name = format!("{sha256}.{}", extension(media_type));
        let mut used = 0u64;
        for (entry, meta) in dir.entries().map_err(|e| io_error(&e))? {
            let entry_name = entry.to_string_lossy();
            if is_ours(&entry_name) && meta.is_file() {
                // The same image is kept once, under whatever extension it has.
                if entry_name.starts_with(sha256) {
                    return Ok(());
                }
                used += meta.size();
            } else if entry_name.starts_with('.') && entry_name.ends_with(".tmp") && meta.is_file()
            {
                let now = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let age = now.saturating_sub(u64::try_from(meta.mtime_secs()).unwrap_or(now));
                if age > STALE_TMP.as_secs() {
                    let _ = dir.unlink(&entry);
                }
            }
        }
        if used + bytes.len() as u64 > self.scope_max {
            return Err(ImageKeepError::Refused(format!(
                "a conversation keeps at most {} MiB of images, and this one holds {} MiB already",
                self.scope_max / (1024 * 1024),
                used / (1024 * 1024)
            )));
        }
        let tmp = std::ffi::OsString::from(format!(
            ".{sha256}.{}.{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let written = dir.create_new(&tmp).and_then(|mut file| {
            #[cfg(unix)]
            file.set_permissions(
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
            )?;
            file.write_all(bytes)?;
            file.flush()
        });
        if let Err(e) = written {
            let _ = dir.unlink(&tmp);
            return Err(io_error(&e));
        }
        if let Err(e) = dir.rename(&tmp, std::ffi::OsStr::new(&name)) {
            let _ = dir.unlink(&tmp);
            return Err(io_error(&e));
        }
        Ok(())
    }

    fn load_blocking(&self, scope: &str, sha256: &str) -> Result<Vec<u8>, ImageKeepError> {
        checked(scope, sha256)?;
        let missing = || ImageKeepError::NotFound(sha256.to_owned());
        let dir = match self.dir_of(scope, false) {
            Ok(dir) => dir,
            Err(ImageKeepError::NotFound(_)) => return Err(missing()),
            Err(other) => return Err(other),
        };
        let Some(name) = EXTENSIONS
            .iter()
            .map(|ext| format!("{sha256}.{ext}"))
            .find(|name| dir.lstat(std::ffi::OsStr::new(name)).is_ok())
        else {
            return Err(missing());
        };
        let (file, _) = match dir.open_regular(std::ffi::OsStr::new(&name)) {
            Ok(opened) => opened,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(missing()),
            // A link or something that is not a file is not an image of ours.
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                return Err(ImageKeepError::Corrupt);
            }
            Err(e) => return Err(io_error(&e)),
        };
        let meta = file.metadata().map_err(|e| io_error(&e))?;
        #[cfg(unix)]
        let linked = std::os::unix::fs::MetadataExt::nlink(&meta) != 1;
        #[cfg(not(unix))]
        let linked = false;
        if !meta.is_file() || linked || meta.len() > IMAGE_MAX {
            return Err(ImageKeepError::Corrupt);
        }
        let mut bytes = Vec::new();
        file.take(IMAGE_MAX + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| io_error(&e))?;
        // The file is the image its name says, or it is nothing: a local command can rewrite it.
        if sha256_hex(&bytes) != sha256 {
            return Err(ImageKeepError::Corrupt);
        }
        Ok(bytes)
    }

    /// The scopes that have images on the volume.
    ///
    /// # Errors
    ///
    /// I/O errors listing the directory.
    pub async fn scopes(&self) -> io::Result<Vec<String>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || -> io::Result<Vec<String>> {
            let root = match SlotDir::open(&this.root) {
                Ok(root) => root,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(e),
            };
            let images = match root.walk(Path::new("images"), false) {
                Ok(dir) => dir,
                Err(WalkError::Missing(_) | WalkError::Link(_) | WalkError::NotDirectory(_)) => {
                    return Ok(Vec::new());
                }
                Err(WalkError::Io(e)) => return Err(e),
            };
            let mut scopes: Vec<String> = images
                .entries()?
                .into_iter()
                .filter_map(|(name, _)| name.into_string().ok())
                .filter(|name| is_hex(name, 32))
                .collect();
            scopes.sort();
            Ok(scopes)
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Remove every image of `scope`. Idempotent. A link in the way is removed, not followed, and
    /// nothing is removed through one.
    ///
    /// # Errors
    ///
    /// I/O errors other than the directory not being there.
    pub async fn remove_scope(&self, scope: &str) -> io::Result<()> {
        if !is_hex(scope, 32) {
            return Ok(());
        }
        let (this, scope) = (self.clone(), scope.to_owned());
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            let root = match SlotDir::open(&this.root) {
                Ok(root) => root,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            };
            let images = match root.walk(Path::new("images"), false) {
                Ok(dir) => dir,
                Err(WalkError::Missing(_)) => return Ok(()),
                Err(WalkError::Link(_) | WalkError::NotDirectory(_)) => {
                    tracing::warn!(
                        "the place for images is not a directory of its own: images not removed"
                    );
                    return Ok(());
                }
                Err(WalkError::Io(e)) => return Err(e),
            };
            let name = std::ffi::OsStr::new(&scope);
            match images.lstat(name) {
                Ok(meta) if meta.is_dir() => {
                    let dir = match images.open_dir(name) {
                        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                            images.make_private(name)?;
                            images.open_dir(name)?
                        }
                        other => other?,
                    };
                    for (entry, meta) in dir.entries()? {
                        // Only files and links are ours: nothing nested is followed or removed.
                        if !meta.is_dir() {
                            dir.unlink(&entry)?;
                        }
                    }
                    images.rmdir(name)?;
                }
                Ok(_) => images.unlink(name)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }
}

#[async_trait]
impl KeptImages for FileImages {
    async fn keep(
        &self,
        scope: &str,
        sha256: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<(), ImageKeepError> {
        let (this, scope, sha256, media_type, bytes) = (
            self.clone(),
            scope.to_owned(),
            sha256.to_owned(),
            media_type.to_owned(),
            bytes.to_vec(),
        );
        tokio::task::spawn_blocking(move || {
            this.keep_blocking(&scope, &sha256, &media_type, &bytes)
        })
        .await
        .map_err(|_| join_failed())?
    }

    async fn load(&self, scope: &str, sha256: &str) -> Result<Vec<u8>, ImageKeepError> {
        let (this, scope, sha256) = (self.clone(), scope.to_owned(), sha256.to_owned());
        tokio::task::spawn_blocking(move || this.load_blocking(&scope, &sha256))
            .await
            .map_err(|_| join_failed())?
    }
}

/// The images store of an agent: what goes in its state.
pub fn store(images: &Arc<FileImages>) -> adam_llm_agent::ImageStore {
    adam_llm_agent::ImageStore(images.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use adam_llm_agent::image_scope;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nthe bytes of an image";

    #[tokio::test]
    async fn file_images_conformance() {
        let root = tempfile::tempdir().unwrap();
        let store = FileImages::new(root.path());
        let base = root.path().to_path_buf();
        adam_llm_agent::kept_images_conformance(
            &store,
            Some(&move |scope: &str, sha: &str| {
                let dir = base.join("images").join(scope);
                std::fs::write(dir.join(format!("{sha}.png")), b"changed behind its back").unwrap();
            }),
        )
        .await;
    }

    #[tokio::test]
    async fn a_rewritten_image_file_is_refused_by_its_hash() {
        let root = tempfile::tempdir().unwrap();
        let store = FileImages::new(root.path());
        let scope = image_scope(Some("c1"), "r1");
        let sha = sha256_hex(PNG);
        store.keep(&scope, &sha, "image/png", PNG).await.unwrap();
        assert_eq!(store.load(&scope, &sha).await.unwrap(), PNG);
        let file = root
            .path()
            .join("images")
            .join(&scope)
            .join(format!("{sha}.png"));
        std::fs::write(&file, b"rewritten").unwrap();
        assert!(matches!(
            store.load(&scope, &sha).await,
            Err(ImageKeepError::Corrupt)
        ));
        // A link planted in its place is not followed.
        std::fs::remove_file(&file).unwrap();
        let target = root.path().join("secret.txt");
        std::fs::write(&target, b"secret").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &file).unwrap();
            assert!(store.load(&scope, &sha).await.is_err());
        }
    }

    #[tokio::test]
    async fn the_files_are_private_and_a_scope_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        let store = FileImages::new(root.path()).with_scope_max(40);
        let scope = image_scope(Some("c1"), "r1");
        let sha = sha256_hex(PNG);
        store.keep(&scope, &sha, "image/png", PNG).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = root.path().join("images").join(&scope);
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            let file = dir.join(format!("{sha}.png"));
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let more = vec![9u8; 30];
        let refused = store
            .keep(&scope, &sha256_hex(&more), "image/png", &more)
            .await;
        assert!(
            matches!(refused, Err(ImageKeepError::Refused(_))),
            "{refused:?}"
        );
        // The same image again is kept once.
        store.keep(&scope, &sha, "image/png", PNG).await.unwrap();
        assert_eq!(
            std::fs::read_dir(root.path().join("images").join(&scope))
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn scopes_are_listed_and_removed() {
        let root = tempfile::tempdir().unwrap();
        let store = FileImages::new(root.path());
        assert!(store.scopes().await.unwrap().is_empty());
        let (a, b) = (image_scope(Some("a"), "r"), image_scope(Some("b"), "r"));
        for scope in [&a, &b] {
            store
                .keep(scope, &sha256_hex(PNG), "image/png", PNG)
                .await
                .unwrap();
        }
        let mut listed = store.scopes().await.unwrap();
        listed.sort();
        let mut want = vec![a.clone(), b.clone()];
        want.sort();
        assert_eq!(listed, want);
        store.remove_scope(&a).await.unwrap();
        store.remove_scope(&a).await.unwrap();
        assert_eq!(store.scopes().await.unwrap(), vec![b.clone()]);
        assert!(matches!(
            store.load(&a, &sha256_hex(PNG)).await,
            Err(ImageKeepError::NotFound(_))
        ));
    }
}
