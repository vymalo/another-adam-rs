//! Finding and reading a repository's `devcontainer.json`.
//!
//! The file is looked for in the first slot of the run, in the order the containers.dev
//! specification gives: `.devcontainer/devcontainer.json`, then `.devcontainer.json`, then
//! `.devcontainer/<folder>/devcontainer.json` (one level deep; the first folder in sorted order when
//! there are several, and the others are reported). A slot with none of them gets the default image.
//!
//! The file is JSON with comments and trailing commas, read with `jsonc-parser`. It is untrusted
//! input: it must be a regular file inside the slot, and not larger than [`MAX_CONFIG_BYTES`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use adam_workspace::EnvError;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The largest `devcontainer.json` that is read.
pub(crate) const MAX_CONFIG_BYTES: u64 = 1 << 20;

/// The places the specification names, relative to the root of the repository.
const FIRST: &str = ".devcontainer/devcontainer.json";
const SECOND: &str = ".devcontainer.json";

/// The environment a slot asks for.
#[derive(Debug, Clone)]
pub(crate) struct Discovered {
    /// The file, relative to the slot; `None` for the default image.
    pub source: Option<PathBuf>,
    /// What it says, as JSON: the file's, or `{"image": <default>}`.
    pub value: Value,
    /// Names the content: it changes when the file does (or the default image does).
    pub digest: String,
    /// The other files of `.devcontainer/<folder>/` that were not used, relative to the slot.
    pub others: Vec<PathBuf>,
}

impl Discovered {
    /// The default image as a configuration.
    fn default_image(image: &str) -> Self {
        Self {
            source: None,
            value: json!({ "image": image }),
            digest: format!("default:{image}"),
            others: Vec::new(),
        }
    }
}

/// What the first slot of a run asks for. `use_default` ignores the repository's own file.
///
/// # Errors
///
/// [`EnvError::Config`] for a file that cannot be read, is not a regular file inside the slot, is too
/// large, or is not one JSON object; [`EnvError::Io`] when the slot cannot be listed.
pub(crate) fn discover(
    slot: &Path,
    default_image: &str,
    use_default: bool,
) -> Result<Discovered, EnvError> {
    if use_default {
        return Ok(Discovered::default_image(default_image));
    }
    let Some((relative, others)) = find(slot)? else {
        return Ok(Discovered::default_image(default_image));
    };
    let (value, digest) = read(slot, &relative)?;
    Ok(Discovered {
        source: Some(relative),
        value,
        digest,
        others,
    })
}

/// Whether the slot has a devcontainer file of its own (it does not read it).
pub(crate) fn has_file(slot: &Path) -> Result<bool, EnvError> {
    Ok(find(slot)?.is_some())
}

/// The file the specification's lookup finds, and the files it passed over.
fn find(slot: &Path) -> Result<Option<(PathBuf, Vec<PathBuf>)>, EnvError> {
    for candidate in [FIRST, SECOND] {
        if fs::symlink_metadata(slot.join(candidate)).is_ok() {
            return Ok(Some((PathBuf::from(candidate), Vec::new())));
        }
    }
    let folders = match fs::read_dir(slot.join(".devcontainer")) {
        Ok(entries) => entries,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let mut found = Vec::new();
    for entry in folders {
        let entry = entry?;
        let name = entry.file_name();
        let relative = Path::new(".devcontainer")
            .join(&name)
            .join("devcontainer.json");
        if fs::symlink_metadata(slot.join(&relative)).is_ok() {
            found.push(relative);
        }
    }
    found.sort();
    let mut found = found.into_iter();
    Ok(found.next().map(|first| (first, found.collect())))
}

/// Read and parse `relative` (a path inside `slot`).
fn read(slot: &Path, relative: &Path) -> Result<(Value, String), EnvError> {
    let config = |reason: String| EnvError::Config {
        file: relative.to_owned(),
        reason,
    };
    let path = slot.join(relative);
    let resolved = fs::canonicalize(&path).map_err(|e| config(format!("cannot be read: {e}")))?;
    let inside = fs::canonicalize(slot)?;
    if !resolved.starts_with(&inside) {
        return Err(config(
            "is a link that leads out of the repository, which is not followed".to_owned(),
        ));
    }
    let meta = fs::metadata(&resolved).map_err(|e| config(format!("cannot be read: {e}")))?;
    if !meta.is_file() {
        return Err(config("is not a regular file".to_owned()));
    }
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(config(format!(
            "is {} bytes; the limit is {MAX_CONFIG_BYTES}",
            meta.len()
        )));
    }
    let bytes = fs::read(&resolved).map_err(|e| config(format!("cannot be read: {e}")))?;
    let text = String::from_utf8(bytes).map_err(|_| config("is not valid UTF-8".to_owned()))?;
    let value = jsonc_parser::parse_to_serde_value::<Option<Value>>(
        &text,
        &jsonc_parser::ParseOptions::default(),
    )
    .map_err(|e| config(format!("is not valid JSON with comments: {e}")))?
    .ok_or_else(|| config("is empty".to_owned()))?;
    if !value.is_object() {
        return Err(config("must hold one JSON object".to_owned()));
    }
    let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
    Ok((value, digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        dir
    }

    const DEFAULT: &str = "registry.example/base:1";

    #[test]
    fn a_slot_without_a_file_gets_the_default_image() {
        let slot = slot_with(&[("README.md", "hi")]);
        let found = discover(slot.path(), DEFAULT, false).unwrap();
        assert_eq!(found.source, None);
        assert_eq!(found.value, json!({"image": DEFAULT}));
        assert!(found.others.is_empty());
        assert!(!has_file(slot.path()).unwrap());
    }

    #[test]
    fn the_lookup_order_is_the_specifications() {
        let all = slot_with(&[
            (".devcontainer/devcontainer.json", r#"{"image":"a"}"#),
            (".devcontainer.json", r#"{"image":"b"}"#),
            (".devcontainer/x/devcontainer.json", r#"{"image":"c"}"#),
        ]);
        let found = discover(all.path(), DEFAULT, false).unwrap();
        assert_eq!(found.source, Some(PathBuf::from(FIRST)));
        assert_eq!(found.value["image"], "a");

        let second = slot_with(&[
            (".devcontainer.json", r#"{"image":"b"}"#),
            (".devcontainer/x/devcontainer.json", r#"{"image":"c"}"#),
        ]);
        let found = discover(second.path(), DEFAULT, false).unwrap();
        assert_eq!(found.source, Some(PathBuf::from(SECOND)));
        assert!(has_file(second.path()).unwrap());
    }

    #[test]
    fn several_folders_use_the_first_in_sorted_order_and_report_the_others() {
        let slot = slot_with(&[
            (".devcontainer/rust/devcontainer.json", r#"{"image":"r"}"#),
            (".devcontainer/go/devcontainer.json", r#"{"image":"g"}"#),
            (".devcontainer/node/devcontainer.json", r#"{"image":"n"}"#),
            (".devcontainer/empty/readme.md", "no config here"),
        ]);
        let found = discover(slot.path(), DEFAULT, false).unwrap();
        assert_eq!(
            found.source,
            Some(PathBuf::from(".devcontainer/go/devcontainer.json"))
        );
        assert_eq!(found.value["image"], "g");
        assert_eq!(
            found.others,
            [
                PathBuf::from(".devcontainer/node/devcontainer.json"),
                PathBuf::from(".devcontainer/rust/devcontainer.json"),
            ]
        );
    }

    #[test]
    fn comments_and_trailing_commas_are_read() {
        let slot = slot_with(&[(
            ".devcontainer/devcontainer.json",
            "// a comment\n{\n  /* block */ \"image\": \"x\", // trailing\n  \"remoteUser\": \"vscode\",\n}\n",
        )]);
        let found = discover(slot.path(), DEFAULT, false).unwrap();
        assert_eq!(found.value, json!({"image": "x", "remoteUser": "vscode"}));
    }

    #[test]
    fn a_file_that_does_not_parse_names_itself() {
        let slot = slot_with(&[(".devcontainer/devcontainer.json", "{ \"image\": ")]);
        let err = discover(slot.path(), DEFAULT, false).unwrap_err();
        let EnvError::Config { file, reason } = &err else {
            panic!("{err:?}")
        };
        assert_eq!(file, Path::new(FIRST));
        assert!(reason.contains("not valid JSON"), "{reason}");
    }

    #[test]
    fn an_empty_file_and_a_file_that_is_not_an_object_are_config_errors() {
        for (content, want) in [
            ("", "empty"),
            ("// nothing\n", "empty"),
            ("[1]", "one JSON object"),
        ] {
            let slot = slot_with(&[(".devcontainer.json", content)]);
            let err = discover(slot.path(), DEFAULT, false).unwrap_err();
            let EnvError::Config { reason, .. } = &err else {
                panic!("{err:?}")
            };
            assert!(reason.contains(want), "{content:?}: {reason}");
        }
    }

    #[test]
    fn a_file_over_the_limit_is_refused() {
        let big = format!(r#"{{"image":"{}"}}"#, "x".repeat(MAX_CONFIG_BYTES as usize));
        let slot = slot_with(&[(".devcontainer.json", &big)]);
        let err = discover(slot.path(), DEFAULT, false).unwrap_err();
        assert!(matches!(err, EnvError::Config { .. }), "{err:?}");
    }

    #[test]
    fn a_link_that_leaves_the_repository_is_not_followed() {
        let outside = slot_with(&[("secret.json", r#"{"image":"stolen"}"#)]);
        let slot = tempfile::tempdir().unwrap();
        fs::create_dir_all(slot.path().join(".devcontainer")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.json"), slot.path().join(FIRST))
            .unwrap();
        let err = discover(slot.path(), DEFAULT, false).unwrap_err();
        let EnvError::Config { reason, .. } = &err else {
            panic!("{err:?}")
        };
        assert!(reason.contains("leads out of the repository"), "{reason}");
    }

    #[test]
    fn a_link_inside_the_repository_is_read() {
        let slot = slot_with(&[("shared/dc.json", r#"{"image":"inner"}"#)]);
        fs::create_dir_all(slot.path().join(".devcontainer")).unwrap();
        std::os::unix::fs::symlink("../shared/dc.json", slot.path().join(FIRST)).unwrap();
        let found = discover(slot.path(), DEFAULT, false).unwrap();
        assert_eq!(found.value["image"], "inner");
    }

    #[test]
    fn the_digest_follows_the_content_and_use_default_ignores_the_file() {
        let slot = slot_with(&[(".devcontainer.json", r#"{"image":"a"}"#)]);
        let a = discover(slot.path(), DEFAULT, false).unwrap().digest;
        assert_eq!(a, discover(slot.path(), DEFAULT, false).unwrap().digest);
        fs::write(slot.path().join(SECOND), r#"{"image":"b"}"#).unwrap();
        assert_ne!(a, discover(slot.path(), DEFAULT, false).unwrap().digest);
        let ignored = discover(slot.path(), DEFAULT, true).unwrap();
        assert_eq!(ignored.source, None);
        assert_eq!(ignored.value, json!({"image": DEFAULT}));
    }
}
