//! Filesystem access on behalf of the agent, confined to one root.
//!
//! Every path the agent names is resolved against the real filesystem
//! (symlinks followed) and must land under the canonical root. `..` in the
//! not-yet-existing part of a path is refused outright, since it cannot be
//! resolved without the filesystem. A dangling symlink is refused too.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use crate::error::AcpError;

/// Largest file the agent may read or write through the client.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Why a filesystem request was refused or failed.
#[derive(Debug)]
pub(crate) enum FsError {
    /// Outside the root, or otherwise not allowed. The message is for the agent.
    Denied(String),
    /// Inside the root but absent.
    NotFound(String),
    /// Anything else.
    Io(String),
}

/// The root all agent file access is confined to.
#[derive(Debug, Clone)]
pub(crate) struct FsGuard {
    root: PathBuf,
}

impl FsGuard {
    /// Canonicalise `root`, which must exist and be a directory.
    pub(crate) async fn new(root: &Path) -> Result<Self, AcpError> {
        let canonical = tokio::fs::canonicalize(root).await.map_err(|e| {
            AcpError::Config(format!("fs_root {} is not usable: {e}", root.display()))
        })?;
        if !canonical.is_dir() {
            return Err(AcpError::Config(format!(
                "fs_root {} is not a directory",
                root.display()
            )));
        }
        Ok(Self { root: canonical })
    }

    /// Resolve `path` to a location under the root. The flag says whether the
    /// final path already exists.
    async fn resolve(&self, path: &Path) -> Result<(PathBuf, bool), FsError> {
        let denied = |why: &str| {
            FsError::Denied(format!(
                "access to {} denied: {why} (allowed root: {})",
                path.display(),
                self.root.display()
            ))
        };
        if !path.is_absolute() {
            return Err(denied("path must be absolute"));
        }
        let mut existing = path.to_path_buf();
        let mut tail: Vec<OsString> = Vec::new();
        loop {
            match tokio::fs::symlink_metadata(&existing).await {
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if !matches!(
                        existing.components().next_back(),
                        Some(Component::Normal(_))
                    ) {
                        return Err(denied(
                            "`..` is not allowed in a path that does not exist yet",
                        ));
                    }
                    let Some(name) = existing.file_name().map(OsString::from) else {
                        return Err(denied("no existing ancestor"));
                    };
                    tail.push(name);
                    existing.pop();
                }
                Err(e) => return Err(FsError::Io(format!("{}: {e}", path.display()))),
            }
        }
        let canonical = tokio::fs::canonicalize(&existing)
            .await
            .map_err(|_| denied("cannot resolve the path (dangling symlink?)"))?;
        if !canonical.starts_with(&self.root) {
            return Err(denied("resolves outside the allowed root"));
        }
        let exists = tail.is_empty();
        let mut full = canonical;
        full.extend(tail.iter().rev());
        Ok((full, exists))
    }

    /// Whether every path resolves under the root (fails closed on any error).
    pub(crate) async fn contains_all(&self, paths: &[PathBuf]) -> bool {
        for p in paths {
            if self.resolve(p).await.is_err() {
                return false;
            }
        }
        true
    }

    /// `fs/read_text_file`: 1-based `line` to start at, `limit` lines.
    pub(crate) async fn read_text(
        &self,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, FsError> {
        let (full, exists) = self.resolve(path).await?;
        if !exists {
            return Err(FsError::NotFound(path.display().to_string()));
        }
        let meta = tokio::fs::metadata(&full)
            .await
            .map_err(|e| FsError::Io(format!("{}: {e}", path.display())))?;
        if !meta.is_file() {
            return Err(FsError::Io(format!("{} is not a file", path.display())));
        }
        if meta.len() > MAX_FILE_BYTES {
            return Err(FsError::Denied(format!(
                "{} is larger than {MAX_FILE_BYTES} bytes",
                path.display()
            )));
        }
        let text = tokio::fs::read_to_string(&full)
            .await
            .map_err(|e| FsError::Io(format!("{}: {e}", path.display())))?;
        if line.is_none() && limit.is_none() {
            return Ok(text);
        }
        let skip = line.map_or(0, |l| l.saturating_sub(1) as usize);
        let take = limit.map_or(usize::MAX, |l| l as usize);
        Ok(text.split_inclusive('\n').skip(skip).take(take).collect())
    }

    /// `fs/write_text_file`. Creates missing parent directories (inside the
    /// root by construction). Writing identical content to an existing file
    /// is a no-op, so a duplicate write (OpenCode sends one after its own
    /// edit) is harmless.
    pub(crate) async fn write_text(&self, path: &Path, content: &str) -> Result<(), FsError> {
        if content.len() as u64 > MAX_FILE_BYTES {
            return Err(FsError::Denied(format!(
                "content for {} is larger than {MAX_FILE_BYTES} bytes",
                path.display()
            )));
        }
        let (full, exists) = self.resolve(path).await?;
        if exists {
            let meta = tokio::fs::metadata(&full)
                .await
                .map_err(|e| FsError::Io(format!("{}: {e}", path.display())))?;
            if !meta.is_file() {
                return Err(FsError::Io(format!("{} is not a file", path.display())));
            }
            if meta.len() == content.len() as u64
                && tokio::fs::read(&full)
                    .await
                    .is_ok_and(|b| b == content.as_bytes())
            {
                return Ok(());
            }
        } else if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| FsError::Io(format!("{}: {e}", parent.display())))?;
        }
        tokio::fs::write(&full, content)
            .await
            .map_err(|e| FsError::Io(format!("{}: {e}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn guard() -> (tempfile::TempDir, FsGuard) {
        let dir = tempfile::tempdir().unwrap();
        let g = FsGuard::new(dir.path()).await.unwrap();
        (dir, g)
    }

    #[tokio::test]
    async fn write_inside_creates_parents_and_is_idempotent() {
        let (dir, g) = guard().await;
        let p = dir.path().join("a/b/c.txt");
        g.write_text(&p, "x").await.unwrap();
        g.write_text(&p, "x").await.unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "x");
        g.write_text(&p, "y").await.unwrap();
        assert_eq!(g.read_text(&p, None, None).await.unwrap(), "y");
    }

    #[tokio::test]
    async fn dotdot_escape_is_denied() {
        let (dir, g) = guard().await;
        let outside = dir.path().parent().unwrap().join("adam-acp-escape.txt");
        for p in [
            dir.path().join("../adam-acp-escape.txt"),
            dir.path().join("new/../../adam-acp-escape.txt"),
        ] {
            assert!(matches!(
                g.write_text(&p, "x").await,
                Err(FsError::Denied(_))
            ));
        }
        assert!(!outside.exists());
    }

    #[tokio::test]
    async fn relative_and_outside_paths_are_denied() {
        let (_dir, g) = guard().await;
        assert!(matches!(
            g.write_text(Path::new("rel.txt"), "x").await,
            Err(FsError::Denied(_))
        ));
        let other = tempfile::tempdir().unwrap();
        assert!(matches!(
            g.read_text(&other.path().join("f"), None, None).await,
            Err(FsError::Denied(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escapes_are_denied() {
        let (dir, g) = guard().await;
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(other.path(), dir.path().join("dirlink")).unwrap();
        std::os::unix::fs::symlink(other.path().join("secret.txt"), dir.path().join("filelink"))
            .unwrap();
        std::os::unix::fs::symlink(other.path().join("nope.txt"), dir.path().join("dangling"))
            .unwrap();

        for p in [
            "dirlink/new.txt",
            "dirlink/secret.txt",
            "filelink",
            "dangling",
        ] {
            let p = dir.path().join(p);
            assert!(
                matches!(g.write_text(&p, "x").await, Err(FsError::Denied(_))),
                "{p:?}"
            );
            assert!(
                matches!(g.read_text(&p, None, None).await, Err(FsError::Denied(_))),
                "{p:?}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(other.path().join("secret.txt")).unwrap(),
            "s"
        );
        assert!(!other.path().join("new.txt").exists());
        assert!(!other.path().join("nope.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_inside_root_is_fine() {
        let (dir, g) = guard().await;
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("alias")).unwrap();
        g.write_text(&dir.path().join("alias/f.txt"), "ok")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("real/f.txt")).unwrap(),
            "ok"
        );
    }

    #[tokio::test]
    async fn read_missing_inside_is_not_found_and_line_limit_works() {
        let (dir, g) = guard().await;
        assert!(matches!(
            g.read_text(&dir.path().join("missing"), None, None).await,
            Err(FsError::NotFound(_))
        ));
        let p = dir.path().join("l.txt");
        std::fs::write(&p, "1\n2\n3\n4\n").unwrap();
        assert_eq!(g.read_text(&p, Some(2), Some(2)).await.unwrap(), "2\n3\n");
        assert_eq!(g.read_text(&p, Some(4), None).await.unwrap(), "4\n");
    }

    #[tokio::test]
    async fn contains_all_fails_closed() {
        let (dir, g) = guard().await;
        assert!(
            g.contains_all(&[dir.path().join("a"), dir.path().join("b/c")])
                .await
        );
        assert!(
            !g.contains_all(&[dir.path().join("a"), PathBuf::from("/etc/passwd")])
                .await
        );
        assert!(g.contains_all(&[]).await);
    }

    #[tokio::test]
    async fn bad_root_is_a_config_error() {
        assert!(matches!(
            FsGuard::new(Path::new("/definitely/not/here")).await,
            Err(AcpError::Config(_))
        ));
    }
}
