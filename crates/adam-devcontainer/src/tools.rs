//! The directory of tools that every devcontainer gets, read-only, at `/opt/adam/bin`.
//!
//! `adam-exec` (the script of this crate, which starts and stops what the coder runs in a
//! container) and, when the coder has one, the `opencode` binary: OpenCode runs inside the
//! container, so that every command it starts does too, and it is the coder's own copy that is
//! mounted, so no image has to carry it. The directory is named after what is in it
//! (`<root>/environments/.tools/<first 12 hex of the sha256>`) and written once, to a temporary
//! directory that is then renamed, so that a run never sees half of it and every run shares it.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::state::environments_dir;

/// The script that starts and stops the coder's processes in a container.
pub(crate) const ADAM_EXEC: &str = include_str!("adam-exec.sh");

/// The name of the OpenCode binary in the tools directory.
const OPENCODE: &str = "opencode";

/// Write the tools directory if it is not there, and return it.
///
/// # Errors
///
/// I/O errors, including an `opencode` that cannot be read.
pub(crate) fn install(root: &Path, opencode: Option<&Path>) -> io::Result<PathBuf> {
    let version = version_of(opencode)?;
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
        if let Some(binary) = opencode {
            let mut source = fs::File::open(binary)?;
            let dest = tmp.join(OPENCODE);
            let mut out = fs::File::create(&dest)?;
            io::copy(&mut source, &mut out)?;
            out.flush()?;
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))?;
        }
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

/// What names the directory: the script and the binary.
fn version_of(opencode: Option<&Path>) -> io::Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(ADAM_EXEC.as_bytes());
    if let Some(binary) = opencode {
        let mut file = fs::File::open(binary)?;
        let mut chunk = vec![0u8; 1 << 16];
        loop {
            let n = file.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            hasher.update(&chunk[..n]);
        }
    }
    Ok(format!("{:x}", hasher.finalize())[..12].to_owned())
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
        let first = install(root.path(), None).unwrap();
        let again = install(root.path(), None).unwrap();
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
        assert!(!first.join("opencode").exists());
        // No temporary directory is left.
        let names: Vec<_> = fs::read_dir(first.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn the_opencode_binary_is_copied_and_changes_the_name() {
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("opencode-real");
        fs::write(&binary, b"\x7fELF not really").unwrap();
        let without = install(root.path(), None).unwrap();
        let with = install(root.path(), Some(&binary)).unwrap();
        assert_ne!(without, with);
        assert_eq!(
            fs::read(with.join("opencode")).unwrap(),
            b"\x7fELF not really"
        );
        assert_eq!(
            fs::metadata(with.join("opencode"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        // Another binary is another directory; the old one stays for the runs that mount it.
        fs::write(&binary, b"\x7fELF newer").unwrap();
        let newer = install(root.path(), Some(&binary)).unwrap();
        assert_ne!(newer, with);
        assert!(with.join("opencode").exists());
    }

    #[test]
    fn a_missing_binary_is_an_error_and_leaves_nothing() {
        let root = tempfile::tempdir().unwrap();
        let err = install(root.path(), Some(Path::new("/no/such/opencode"))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!environments_dir(root.path()).join(".tools").exists());
    }

    #[test]
    fn the_embedded_script_is_the_file() {
        assert!(ADAM_EXEC.starts_with("#!/bin/sh\n"));
        assert!(ADAM_EXEC.contains("adam-exec run"));
    }
}
