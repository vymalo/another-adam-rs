//! The directory of tools that every devcontainer gets, read-only, at `/opt/adam/bin`.
//!
//! `adam-exec`, the script of this crate, which starts and stops what the coder runs in a
//! container. Nothing else: no agent binary and no secret enters a container (ADR 0038). The
//! directory is named after what is in it
//! (`<root>/environments/.tools/<first 12 hex of the sha256>`) and written once, to a temporary
//! directory that is then renamed, so that a run never sees half of it and every run shares it.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::state::environments_dir;

/// The script that starts and stops the coder's processes in a container.
pub(crate) const ADAM_EXEC: &str = include_str!("adam-exec.sh");

/// Write the tools directory if it is not there, and return it.
///
/// # Errors
///
/// I/O errors.
pub(crate) fn install(root: &Path) -> io::Result<PathBuf> {
    let version = version_of();
    let base = environments_dir(root).join(".tools");
    let target = base.join(&version);
    if target.join("adam-exec").is_file() {
        return Ok(target);
    }
    fs::create_dir_all(&base)?;
    let tmp = base.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    fs::create_dir_all(&tmp)?;
    let result = (|| {
        write_executable(&tmp.join("adam-exec"), ADAM_EXEC.as_bytes())?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        match fs::rename(&tmp, &target) {
            Ok(()) => Ok(()),
            // Another process made it in the meantime: the same content.
            Err(_) if target.join("adam-exec").is_file() => Ok(()),
            Err(e) => Err(e),
        }
    })();
    if result.is_err() || tmp.exists() {
        let _ = fs::remove_dir_all(&tmp);
    }
    result.map(|()| target)
}

/// What names the directory: the script.
fn version_of() -> String {
    let mut hasher = Sha256::new();
    hasher.update(ADAM_EXEC.as_bytes());
    format!("{:x}", hasher.finalize())[..12].to_owned()
}

fn write_executable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_directory_is_written_once_named_after_its_content_and_executable() {
        let root = tempfile::tempdir().unwrap();
        let first = install(root.path()).unwrap();
        let again = install(root.path()).unwrap();
        assert_eq!(first, again);
        assert_eq!(
            first.parent().unwrap(),
            root.path().join("environments/.tools")
        );
        assert_eq!(first.file_name().unwrap().len(), 12);
        let script = first.join("adam-exec");
        assert_eq!(fs::read_to_string(&script).unwrap(), ADAM_EXEC);
        assert_eq!(
            fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&first).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let files: Vec<_> = fs::read_dir(&first)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            files,
            ["adam-exec"],
            "only the exec script, no agent binary"
        );
        // No temporary directory is left.
        let names: Vec<_> = fs::read_dir(first.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn the_embedded_script_is_the_file() {
        assert!(ADAM_EXEC.starts_with("#!/bin/sh\n"));
        assert!(ADAM_EXEC.contains("adam-exec run"));
    }
}
