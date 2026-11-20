//! A repository's `devcontainer.json`, read for a run pod's image (ADR 0052 decision 6).
//!
//! The file is looked for where the containers.dev specification puts it, relative to a slot:
//! `.devcontainer/devcontainer.json`, then `.devcontainer.json`, then
//! `.devcontainer/<folder>/devcontainer.json` (one level deep; the first folder in sorted order when
//! there are several, and the others are reported so that the caller can say which it used).
//!
//! This module only **reads and chooses**; it starts nothing. The image of a run pod is the
//! repository's choice when it names one:
//!
//! | The configuration has | The image |
//! |---|---|
//! | `customizations.adam.image` | that image, whatever else the file says |
//! | `image`, and no `build` and no `features` | `image` |
//! | `build` or `features`, and no `customizations.adam.image` | refused: this coder runs no container engine |
//! | `dockerComposeFile` | refused: one image, not a Compose configuration |
//! | an image outside the allowed prefixes, a path, or a `${...}` variable | refused, naming the prefixes |
//!
//! The file is untrusted input: it must be a regular file inside the slot, at one of the places the
//! specification names, and not larger than [`MAX_CONFIG_BYTES`].

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::EnvError;

/// The largest `devcontainer.json` that is read.
pub const MAX_CONFIG_BYTES: u64 = 1 << 20;

/// `.devcontainer/devcontainer.json`: the first place the specification looks.
pub const FIRST: &str = ".devcontainer/devcontainer.json";

/// `.devcontainer.json`: the second place.
pub const SECOND: &str = ".devcontainer.json";

/// A configuration found in a slot.
#[derive(Debug, Clone)]
pub struct Devcontainer {
    /// The file, relative to the slot.
    pub source: PathBuf,
    /// What it says.
    pub value: Value,
    /// Names the content: it changes when the file does.
    pub digest: String,
    /// The other `.devcontainer/<folder>/devcontainer.json` files the lookup passed over, relative to
    /// the slot.
    pub others: Vec<PathBuf>,
}

impl Devcontainer {
    /// The image the run pod must use, checked against the deployment's allowed prefixes.
    ///
    /// `allowed_prefixes` are the prefixes the deployment allows (`RUN_POD_REPOSITORY_IMAGES`); each
    /// must end with `/` (a registry path) or `:` (a tag), so that a bare host cannot admit a
    /// look-alike. A prefix that does not is refused, not matched as a string. An empty list refuses
    /// every image, which is the deployment that allows no repository image at all.
    ///
    /// # Errors
    ///
    /// [`EnvError::Refused`] in words for a configuration that builds its image, uses Compose, names
    /// no image, names a path or a `${...}` variable, or names an image outside the allowed prefixes.
    pub fn image(&self, allowed_prefixes: &[String]) -> Result<String, EnvError> {
        let custom = self
            .value
            .get("customizations")
            .and_then(|customizations| customizations.get("adam"))
            .and_then(|adam| adam.get("image"));
        if let Some(custom) = custom {
            let image = image_string(custom, "customizations.adam.image")?;
            return check_image(&image, allowed_prefixes);
        }
        if self.value.get("dockerComposeFile").is_some() {
            return Err(refused(
                "a Compose configuration is not supported: name one image (`image` or \
                 `customizations.adam.image`)",
            ));
        }
        let build = build_of(&self.value);
        let features = self.value.get("features");
        if build.is_some() || features.is_some() {
            let what = [build, features.map(features_of)]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ");
            return Err(refused(format!(
                "this configuration builds its image ({what}); this coder runs no container engine. \
                 Build and publish the image in the repository's CI under one of {}, and name it in \
                 `customizations.adam.image`, or replace the build by `image`",
                prefixes(allowed_prefixes)
            )));
        }
        match self.value.get("image") {
            Some(image) => {
                let image = image_string(image, "image")?;
                check_image(&image, allowed_prefixes)
            }
            None => Err(refused(
                "this configuration names no image (neither `image` nor `customizations.adam.image`): \
                 a run pod needs one image to run",
            )),
        }
    }

    /// The keys of the file a run pod **ignores**, in a stable order, so that the result can list
    /// them: the pod is the deployment's, and its user, mounts, resources, network and runtime class
    /// do not move (ADR 0052 decision 5). `containerEnv` and `remoteEnv` are ignored too: this coder
    /// does not set them. The lifecycle commands are not run by this slice, so they are listed as
    /// well.
    pub fn ignored(&self) -> Vec<&'static str> {
        const IGNORED: [&str; 22] = [
            "initializeCommand",
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
            "postStartCommand",
            "postAttachCommand",
            "containerEnv",
            "remoteEnv",
            "privileged",
            "capAdd",
            "securityOpt",
            "runArgs",
            "mounts",
            "init",
            "forwardPorts",
            "appPort",
            "workspaceMount",
            "workspaceFolder",
            "remoteUser",
            "containerUser",
            "updateRemoteUserUID",
            "hostRequirements",
        ];
        IGNORED
            .into_iter()
            .filter(|key| self.value.get(*key).is_some())
            .collect()
    }
}

/// Find the configuration the specification's lookup names in `slot`.
///
/// # Errors
///
/// [`EnvError::Io`] when `.devcontainer` cannot be listed.
pub fn find(slot: &Path) -> Result<Option<PathBuf>, EnvError> {
    for candidate in [FIRST, SECOND] {
        if fs::symlink_metadata(slot.join(candidate)).is_ok() {
            return Ok(Some(PathBuf::from(candidate)));
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
        let relative = Path::new(".devcontainer")
            .join(entry.file_name())
            .join("devcontainer.json");
        if fs::symlink_metadata(slot.join(&relative)).is_ok() {
            found.push(relative);
        }
    }
    found.sort();
    Ok(found.into_iter().next())
}

/// Read the slot's configuration. `config` names the file when the slot has several (one of the
/// places the specification lists, relative to the slot); without it the lookup's first is used.
///
/// # Errors
///
/// [`EnvError::Refused`] when the slot has none, or `config` is not one of the specification's
/// places; [`EnvError::Config`] when the file cannot be read, is not a regular file inside the slot,
/// is too large, or is not one JSON object.
pub fn read(slot: &Path, config: Option<&str>) -> Result<Devcontainer, EnvError> {
    let others = candidates(slot)?;
    let relative = match config {
        Some(config) => {
            let relative = named_config(config)?;
            if !others.contains(&relative) {
                let listed = if others.is_empty() {
                    "none".to_owned()
                } else {
                    others
                        .iter()
                        .map(|path| format!("`{}`", path.display()))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                return Err(refused(format!(
                    "`{config}` is not a devcontainer configuration of this repository: the places \
                     the specification names are {listed}"
                )));
            }
            relative
        }
        None => others
            .first()
            .cloned()
            .ok_or_else(|| no_configuration(slot))?,
    };
    let (value, digest) = parse(slot, &relative)?;
    Ok(Devcontainer {
        source: relative.clone(),
        value,
        digest,
        others: others
            .into_iter()
            .filter(|candidate| *candidate != relative)
            .collect(),
    })
}

/// The places the specification names that this slot holds, in lookup order.
fn candidates(slot: &Path) -> Result<Vec<PathBuf>, EnvError> {
    let mut found = Vec::new();
    for candidate in [FIRST, SECOND] {
        if fs::symlink_metadata(slot.join(candidate)).is_ok() {
            found.push(PathBuf::from(candidate));
        }
    }
    let folders = match fs::read_dir(slot.join(".devcontainer")) {
        Ok(entries) => entries,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory =>
        {
            return Ok(found);
        }
        Err(e) => return Err(e.into()),
    };
    let mut nested = Vec::new();
    for entry in folders {
        let entry = entry?;
        let relative = Path::new(".devcontainer")
            .join(entry.file_name())
            .join("devcontainer.json");
        if fs::symlink_metadata(slot.join(&relative)).is_ok() {
            nested.push(relative);
        }
    }
    nested.sort();
    found.extend(nested);
    Ok(found)
}

/// `config` as one of the specification's places, relative to the slot.
fn named_config(config: &str) -> Result<PathBuf, EnvError> {
    let path = Path::new(config.trim());
    let mut parts = path.components();
    let well_formed = !config.trim().is_empty()
        && path.is_relative()
        && parts.all(|part| matches!(part, Component::Normal(_)))
        && path
            .file_name()
            .is_some_and(|name| name == "devcontainer.json")
        && (path == Path::new(FIRST)
            || path == Path::new(SECOND)
            || (path.components().count() == 3
                && path.starts_with(".devcontainer")
                && path.ends_with("devcontainer.json")));
    if well_formed {
        Ok(path.to_owned())
    } else {
        Err(refused(format!(
            "`{config}` is not one of the places the specification names: `{FIRST}`, `{SECOND}`, or \
             `.devcontainer/<folder>/devcontainer.json`"
        )))
    }
}

fn no_configuration(slot: &Path) -> EnvError {
    refused(format!(
        "`{}` has no devcontainer configuration: none of `{FIRST}`, `{SECOND}` or \
         `.devcontainer/<folder>/devcontainer.json` is there",
        slot.display()
    ))
}

fn refused(reason: impl Into<String>) -> EnvError {
    EnvError::Refused(reason.into())
}

/// Read and parse `relative` (a path inside `slot`).
fn parse(slot: &Path, relative: &Path) -> Result<(Value, String), EnvError> {
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

/// What a `build` asks to build, in a phrase, or `None`.
fn build_of(value: &Value) -> Option<String> {
    let build = value.get("build")?;
    let mut said = Vec::new();
    if let Some(dockerfile) = build.get("dockerfile").and_then(Value::as_str) {
        said.push(format!("`build.dockerfile`: {dockerfile}"));
    }
    if let Some(context) = build.get("context").and_then(Value::as_str) {
        said.push(format!("`build.context`: {context}"));
    }
    if let Some(args) = build.get("args").and_then(Value::as_object) {
        said.push(format!("`build.args`: {}", args.len()));
    }
    if said.is_empty() {
        said.push("`build`".to_owned());
    }
    Some(said.join(", "))
}

/// The features a configuration asks for, in a phrase.
fn features_of(features: &Value) -> String {
    match features.as_object() {
        Some(features) if !features.is_empty() => format!(
            "`features`: {}",
            features.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
        _ => "`features`".to_owned(),
    }
}

/// The prefixes as a sentence, for a refusal.
fn prefixes(allowed: &[String]) -> String {
    if allowed.is_empty() {
        "no prefix (RUN_POD_REPOSITORY_IMAGES is empty: this deployment allows no repository image)"
            .to_owned()
    } else {
        allowed
            .iter()
            .map(|prefix| format!("`{prefix}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn image_string(value: &Value, key: &str) -> Result<String, EnvError> {
    let Some(image) = value.as_str() else {
        return Err(refused(format!(
            "`{key}` must be a string (the image to run)"
        )));
    };
    let image = image.trim();
    if image.is_empty() {
        return Err(refused(format!("`{key}` is empty")));
    }
    Ok(image.to_owned())
}

/// An image reference the deployment may run: not a variable, not a path, under an allowed prefix.
fn check_image(image: &str, allowed: &[String]) -> Result<String, EnvError> {
    if image.contains("${") || image.contains("$(") {
        return Err(refused(format!(
            "the image `{image}` holds a variable, which this coder does not expand: name the image \
             as it is"
        )));
    }
    if image.starts_with(['.', '/', '~'])
        || image.contains('\\')
        || image.contains("://")
        || image.split('/').any(|part| part == "..")
    {
        return Err(refused(format!(
            "the image `{image}` is a path, not an image reference: name an image the repository's \
             CI publishes"
        )));
    }
    if image.chars().any(char::is_whitespace) {
        return Err(refused(format!(
            "the image `{image}` holds white space, which an image reference cannot"
        )));
    }
    if allowed.is_empty() {
        return Err(refused(format!(
            "the image `{image}` is refused: this deployment allows no repository image \
             (RUN_POD_REPOSITORY_IMAGES is empty)"
        )));
    }
    // A prefix is matched on a path boundary: `ghcr.io/acme/` admits `ghcr.io/acme/dev` and not
    // `ghcr.io/acme.evil/dev`, and `ghcr.io/acme/tool:` admits a tag of that one repository. A
    // prefix that does not end with `/` or `:` is a bare host or repository name, and matching it
    // as a string would admit a look-alike; the deployment's value is refused instead of trusted.
    let malformed: Vec<String> = allowed
        .iter()
        .filter(|prefix| !is_prefix(prefix))
        .cloned()
        .collect();
    if !malformed.is_empty() {
        return Err(refused(format!(
            "this deployment's image prefixes are not usable ({}): each must end with `/` (a \
             registry path) or `:` (a tag), so that a bare host cannot admit a look-alike; ask the \
             person who runs this deployment",
            prefixes(&malformed)
        )));
    }
    if !allowed.iter().any(|prefix| image.starts_with(prefix)) {
        return Err(refused(format!(
            "the image `{image}` is outside the prefixes this deployment allows ({}): build and \
             publish it under one of them, or ask the person who runs this deployment",
            prefixes(allowed)
        )));
    }
    Ok(image.to_owned())
}

/// Whether `prefix` ends on a path boundary: a registry path (`ghcr.io/acme/`) or a tag
/// (`ghcr.io/acme/tool:`). A bare host, a repository name or an empty string does not, and matching
/// it as a string prefix would admit a look-alike.
fn is_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && !prefix.chars().any(char::is_whitespace)
        && (prefix.ends_with('/') || prefix.ends_with(':'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ALLOWED: [&str; 2] = ["ghcr.io/acme/", "registry.example:5000/team/"];

    fn allowed() -> Vec<String> {
        ALLOWED.iter().map(|prefix| (*prefix).to_owned()).collect()
    }

    fn slot_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        dir
    }

    fn image_of(content: &str) -> Result<String, EnvError> {
        let slot = slot_with(&[(".devcontainer/devcontainer.json", content)]);
        read(slot.path(), None).unwrap().image(&allowed())
    }

    fn refusal(content: &str) -> String {
        match image_of(content) {
            Err(EnvError::Refused(reason)) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_lookup_order_is_the_specifications() {
        let all = slot_with(&[
            (
                ".devcontainer/devcontainer.json",
                r#"{"image":"ghcr.io/acme/a"}"#,
            ),
            (".devcontainer.json", r#"{"image":"ghcr.io/acme/b"}"#),
            (
                ".devcontainer/x/devcontainer.json",
                r#"{"image":"ghcr.io/acme/c"}"#,
            ),
        ]);
        assert_eq!(
            find(all.path()).unwrap(),
            Some(PathBuf::from(FIRST)),
            "the first place wins"
        );
        let found = read(all.path(), None).unwrap();
        assert_eq!(found.source, Path::new(FIRST));
        assert_eq!(found.value["image"], "ghcr.io/acme/a");
        assert_eq!(
            found.others,
            [
                PathBuf::from(".devcontainer.json"),
                PathBuf::from(".devcontainer/x/devcontainer.json")
            ],
            "the ones it passed over are reported"
        );

        let second = slot_with(&[
            (".devcontainer.json", r#"{"image":"ghcr.io/acme/b"}"#),
            (
                ".devcontainer/x/devcontainer.json",
                r#"{"image":"ghcr.io/acme/c"}"#,
            ),
        ]);
        assert_eq!(read(second.path(), None).unwrap().source, Path::new(SECOND));
    }

    #[test]
    fn several_folders_use_the_first_in_sorted_order_and_config_names_another() {
        let slot = slot_with(&[
            (
                ".devcontainer/rust/devcontainer.json",
                r#"{"image":"ghcr.io/acme/r"}"#,
            ),
            (
                ".devcontainer/go/devcontainer.json",
                r#"{"image":"ghcr.io/acme/g"}"#,
            ),
            (
                ".devcontainer/node/devcontainer.json",
                r#"{"image":"ghcr.io/acme/n"}"#,
            ),
        ]);
        let found = read(slot.path(), None).unwrap();
        assert_eq!(
            found.source,
            Path::new(".devcontainer/go/devcontainer.json")
        );
        assert_eq!(
            found.others,
            [
                PathBuf::from(".devcontainer/node/devcontainer.json"),
                PathBuf::from(".devcontainer/rust/devcontainer.json"),
            ]
        );
        let chosen = read(slot.path(), Some(".devcontainer/rust/devcontainer.json")).unwrap();
        assert_eq!(chosen.value["image"], "ghcr.io/acme/r");
        assert_eq!(
            chosen.others.len(),
            2,
            "the other two are reported: {:?}",
            chosen.others
        );
        // A name that is not one of the specification's places is refused, and so is a path out.
        for bad in [
            "devcontainer.json",
            ".devcontainer/devcontainer.json/../devcontainer.json",
            "/etc/devcontainer.json",
            "../devcontainer.json",
            ".devcontainer/rust/other.json",
        ] {
            let error = read(slot.path(), Some(bad)).unwrap_err();
            assert!(matches!(error, EnvError::Refused(_)), "{bad}: {error}");
        }
    }

    #[test]
    fn comments_and_trailing_commas_are_read() {
        let slot = slot_with(&[(
            ".devcontainer.json",
            "// a comment\n{\n  /* block */ \"image\": \"ghcr.io/acme/x\", // trailing\n  \"remoteUser\": \"vscode\",\n}\n",
        )]);
        let found = read(slot.path(), None).unwrap();
        assert_eq!(
            found.value,
            json!({"image": "ghcr.io/acme/x", "remoteUser": "vscode"})
        );
    }

    #[test]
    fn a_slot_with_no_file_is_refused_and_a_file_that_does_not_parse_names_itself() {
        let slot = slot_with(&[("README.md", "hi")]);
        let error = read(slot.path(), None).unwrap_err();
        assert!(matches!(error, EnvError::Refused(_)), "{error}");
        assert!(error.to_string().contains("no devcontainer configuration"));

        for (content, want) in [
            ("{ \"image\": ", "not valid JSON"),
            ("", "empty"),
            ("[1]", "one JSON object"),
        ] {
            let slot = slot_with(&[(".devcontainer.json", content)]);
            let error = read(slot.path(), None).unwrap_err();
            match &error {
                EnvError::Config { file, reason } => {
                    assert_eq!(file, Path::new(SECOND));
                    assert!(reason.contains(want), "{content:?}: {reason}");
                }
                other => panic!("{content:?}: {other}"),
            }
        }
        assert!(refusal(r#"{"image": 3}"#).contains("must be a string"));
    }

    #[test]
    fn a_file_over_the_limit_or_a_link_that_leaves_the_repository_is_refused() {
        let big = format!(r#"{{"image":"{}"}}"#, "x".repeat(MAX_CONFIG_BYTES as usize));
        let slot = slot_with(&[(".devcontainer.json", &big)]);
        assert!(matches!(
            read(slot.path(), None),
            Err(EnvError::Config { .. })
        ));

        let outside = slot_with(&[("secret.json", r#"{"image":"ghcr.io/acme/stolen"}"#)]);
        let slot = tempfile::tempdir().unwrap();
        fs::create_dir_all(slot.path().join(".devcontainer")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.json"), slot.path().join(FIRST))
            .unwrap();
        let error = read(slot.path(), None).unwrap_err();
        assert!(
            error.to_string().contains("leads out of the repository"),
            "{error}"
        );
    }

    #[test]
    fn the_digest_follows_the_content() {
        let slot = slot_with(&[(".devcontainer.json", r#"{"image":"ghcr.io/acme/a"}"#)]);
        let a = read(slot.path(), None).unwrap().digest;
        assert_eq!(a, read(slot.path(), None).unwrap().digest);
        fs::write(slot.path().join(SECOND), r#"{"image":"ghcr.io/acme/b"}"#).unwrap();
        assert_ne!(a, read(slot.path(), None).unwrap().digest);
    }

    #[test]
    fn an_image_is_used_when_it_is_under_an_allowed_prefix() {
        assert_eq!(
            image_of(r#"{"image":"ghcr.io/acme/dev:1"}"#).unwrap(),
            "ghcr.io/acme/dev:1"
        );
        assert_eq!(
            image_of(r#"{"image":"registry.example:5000/team/dev@sha256:abc"}"#).unwrap(),
            "registry.example:5000/team/dev@sha256:abc",
            "a digest is an image reference too"
        );
    }

    #[test]
    fn customizations_adam_image_wins_whatever_else_the_file_says() {
        assert_eq!(
            image_of(
                r#"{"image":"ghcr.io/acme/ignored","build":{"dockerfile":"Dockerfile"},"features":{"ghcr.io/devcontainers/features/node:1":{}},"customizations":{"adam":{"image":"ghcr.io/acme/built"}}}"#
            )
            .unwrap(),
            "ghcr.io/acme/built"
        );
        // A customizations.adam.image that is not a string is refused, naming it.
        assert!(refusal(r#"{"customizations":{"adam":{"image":3}}}"#).contains("must be a string"));
    }

    #[test]
    fn a_configuration_that_builds_is_refused_and_says_what_to_add() {
        let dockerfile = refusal(r#"{"build":{"dockerfile":"Dockerfile"}}"#);
        assert!(
            dockerfile.contains("builds its image")
                && dockerfile.contains("build.dockerfile")
                && dockerfile.contains("Dockerfile")
                && dockerfile.contains("customizations.adam.image")
                && dockerfile.contains("ghcr.io/acme/"),
            "{dockerfile}"
        );
        let features = refusal(
            r#"{"features":{"ghcr.io/devcontainers/features/node:1":{},"ghcr.io/devcontainers/features/rust:1":{}}}"#,
        );
        assert!(
            features.contains("builds its image")
                && features.contains("features")
                && features.contains("node")
                && features.contains("rust"),
            "{features}"
        );
    }

    #[test]
    fn a_compose_configuration_is_refused() {
        let said = refusal(r#"{"dockerComposeFile":"docker-compose.yml","service":"app"}"#);
        assert!(
            said.contains("Compose configuration is not supported")
                && said.contains("name one image"),
            "{said}"
        );
    }

    #[test]
    fn a_configuration_with_no_image_is_refused() {
        let said = refusal(r#"{"remoteUser":"vscode"}"#);
        assert!(
            said.contains("names no image") && said.contains("customizations.adam.image"),
            "{said}"
        );
    }

    #[test]
    fn a_relative_image_is_refused() {
        for image in [
            "./image",
            "../image",
            "/image",
            "~/image",
            "ghcr.io/acme/../evil",
            "https://ghcr.io/acme/dev",
            r"ghcr.io\\acme\\dev",
        ] {
            let said = refusal(&format!(r#"{{"image":"{image}"}}"#));
            assert!(
                said.contains("is a path, not an image reference"),
                "{image}: {said}"
            );
        }
    }

    #[test]
    fn a_variable_in_an_image_is_refused() {
        for image in ["ghcr.io/${OWNER}/dev:1", "ghcr.io/acme/$(tag)"] {
            let said = refusal(&format!(r#"{{"image":"{image}"}}"#));
            assert!(said.contains("holds a variable"), "{image}: {said}");
        }
    }

    #[test]
    fn an_image_outside_the_prefixes_is_refused_and_the_prefixes_are_named() {
        let said = refusal(r#"{"image":"docker.io/library/node:24"}"#);
        assert!(
            said.contains("outside the prefixes this deployment allows")
                && said.contains("ghcr.io/acme/")
                && said.contains("registry.example:5000/team/"),
            "{said}"
        );
        // A look-alike of a bare host is not admitted: the prefixes here end with `/`.
        assert!(
            refusal(r#"{"image":"ghcr.io/acme.evil/dev:1"}"#).contains("outside the prefixes"),
            "a sibling host is not under ghcr.io/acme/"
        );
        // With no prefix at all, every image is refused, and the refusal says why.
        let slot = slot_with(&[(".devcontainer.json", r#"{"image":"ghcr.io/acme/dev:1"}"#)]);
        let error = read(slot.path(), None).unwrap().image(&[]).unwrap_err();
        assert!(
            error.to_string().contains("allows no repository image"),
            "{error}"
        );
    }

    /// ADR 0052: a prefix is matched on a path boundary. A caller that hands the reader a bare
    /// host (`ghcr.io/acme`, not `ghcr.io/acme/`) must not get a look-alike admitted: the prefix is
    /// refused rather than matched as a string, and the image that really is under it is refused
    /// with it (the deployment's value is wrong, and the reader fails closed).
    #[test]
    fn a_prefix_that_is_a_bare_host_is_refused_instead_of_admitting_a_look_alike() {
        for image in ["ghcr.io/acme.evil/dev:1", "ghcr.io/acme/dev:1"] {
            let slot = slot_with(&[(".devcontainer.json", &format!(r#"{{"image":"{image}"}}"#))]);
            let error = read(slot.path(), None)
                .unwrap()
                .image(&["ghcr.io/acme".to_owned()])
                .unwrap_err();
            assert!(matches!(error, EnvError::Refused(_)), "{image}: {error}");
            assert!(
                error.to_string().contains("not usable")
                    && error.to_string().contains("ghcr.io/acme")
                    && error.to_string().contains("look-alike"),
                "{image}: {error}"
            );
        }
        // The same image under the path prefix the deployment should have written.
        let slot = slot_with(&[(".devcontainer.json", r#"{"image":"ghcr.io/acme/dev:1"}"#)]);
        assert_eq!(
            read(slot.path(), None)
                .unwrap()
                .image(&["ghcr.io/acme/".to_owned()])
                .unwrap(),
            "ghcr.io/acme/dev:1"
        );
        // An empty prefix would be a string prefix of every image.
        let error = read(slot.path(), None)
            .unwrap()
            .image(&[String::new()])
            .unwrap_err();
        assert!(error.to_string().contains("not usable"), "{error}");
    }

    #[test]
    fn a_case_variant_or_another_path_of_the_same_host_is_not_admitted() {
        for image in [
            "GHCR.IO/acme/dev:1",
            "ghcr.io/Acme/dev:1",
            "ghcr.io/acme-evil/dev:1",
            "ghcr.io/acme/dev/../evil:1",
            "localhost:5000/team/dev:1",
        ] {
            let said = refusal(&format!(r#"{{"image":"{image}"}}"#));
            assert!(
                said.contains("outside the prefixes") || said.contains("is a path"),
                "{image}: {said}"
            );
        }
        let slot = slot_with(&[(
            ".devcontainer.json",
            r#"{"image":"localhost:5000/team/dev:1"}"#,
        )]);
        assert_eq!(
            read(slot.path(), None)
                .unwrap()
                .image(&["localhost:5000/team/".to_owned()])
                .unwrap(),
            "localhost:5000/team/dev:1"
        );
        let slot = slot_with(&[(
            ".devcontainer.json",
            r#"{"image":"localhost:5000/other/dev:1"}"#,
        )]);
        assert!(
            read(slot.path(), None)
                .unwrap()
                .image(&["localhost:5000/team/".to_owned()])
                .is_err(),
            "another path of the same registry is outside the prefix"
        );
    }

    #[test]
    fn an_image_beside_a_build_is_refused_too() {
        for content in [
            r#"{"image":"ghcr.io/acme/dev:1","build":{"dockerfile":"Dockerfile"}}"#,
            r#"{"image":"ghcr.io/acme/dev:1","features":{"ghcr.io/devcontainers/features/node:1":{}}}"#,
        ] {
            let said = refusal(content);
            assert!(said.contains("builds its image"), "{content}: {said}");
        }
    }

    #[test]
    fn the_keys_a_run_pod_ignores_are_listed() {
        let slot = slot_with(&[(
            ".devcontainer.json",
            r#"{"image":"ghcr.io/acme/dev:1","remoteUser":"vscode","privileged":true,"mounts":[],"containerEnv":{"A":"b"},"postCreateCommand":"make","runArgs":["--init"],"updateRemoteUserUID":true,"hostRequirements":{"cpus":4}}"#,
        )]);
        let found = read(slot.path(), None).unwrap();
        assert_eq!(
            found.ignored(),
            [
                "postCreateCommand",
                "containerEnv",
                "privileged",
                "runArgs",
                "mounts",
                "remoteUser",
                "updateRemoteUserUID",
                "hostRequirements"
            ]
        );
        let plain = slot_with(&[(".devcontainer.json", r#"{"image":"ghcr.io/acme/dev:1"}"#)]);
        assert!(read(plain.path(), None).unwrap().ignored().is_empty());
    }
}
