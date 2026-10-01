//! Where the coder's agent files come from: the copy `build.rs` embedded in the binary, or a
//! folder read when the process starts (`ADAM_AGENT_DIR`).
//!
//! The files are the prompt, the limits, the vars, the card, the skills, the subagents and the
//! `mcp.json` of the agent (`agent/instructions.md` and its neighbours). The tools, the
//! completion policy and the redaction stay in Rust, so a folder changes what the agent says and
//! offers, not what its tools do. A folder is read once, at startup, by every role, and a change
//! on disk applies at the next start ([ADR 0004](../../../docs/decisions/0004-agent-folders-at-run-time.md)).
//!
//! ```
//! use adam_coder::AgentFiles;
//!
//! // `ADAM_AGENT_DIR` unset: the embedded copy.
//! let files = AgentFiles::load(None).unwrap();
//! assert_eq!(files.describe().source, "embedded");
//! assert_eq!(files.describe().agent, "coder");
//! ```

use std::fmt;
use std::path::{Path, PathBuf};

use adam::agent_fs::{Diagnostic, Error as FilesError};
use adam::{AgentDef, AgentFolder, AssemblyError};
use adam_error::{Classify, ErrorClass};

use crate::agent::{AGENT, AGENT_NAME};

/// The agent files the process was started with.
#[derive(Debug, Clone)]
pub enum AgentFiles {
    /// The copy `build.rs` embedded from `agent/`: the default, and what a process without
    /// `ADAM_AGENT_DIR` runs.
    Embedded,
    /// A folder read at startup. Boxed: it holds the whole definition.
    Folder(Box<AgentFolder>),
}

/// Why the agent files cannot be used. Always [`ErrorClass::Invalid`]: the same folder never
/// works, so a supervisor must not restart the process (exit code 78).
#[derive(Debug, thiserror::Error)]
pub enum AgentFilesError {
    /// The folder cannot be read, has errors in its files, or holds more than one agent. Every
    /// diagnostic is in the message, as `path:line: error: what is wrong`.
    #[error("cannot read the agent folder `{}` (ADAM_AGENT_DIR): {}", path.display(), explain(reason))]
    Folder {
        /// The folder, as `ADAM_AGENT_DIR` named it.
        path: PathBuf,
        /// What the assembly reported (boxed: it is large). Not a `source`: the message already
        /// says all of it, so an error chain printed whole does not say it twice.
        reason: Box<AssemblyError>,
    },
    /// The folder is another agent's: the runs of the coder are stored under its name, so a
    /// folder that renames it would strand them.
    #[error(
        "the agent folder `{}` (ADAM_AGENT_DIR) holds the agent `{found}` ({}), but this process serves \
         `{expected}`: set `name: {expected}` in the frontmatter (the name is the key of the \
         stored runs)",
        root.display(),
        file.display()
    )]
    Name {
        /// The folder.
        root: PathBuf,
        /// The instructions file, relative to the folder.
        file: PathBuf,
        /// The name the file gives.
        found: String,
        /// The name this process serves: [`AGENT_NAME`].
        expected: &'static str,
    },
}

impl Classify for AgentFilesError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// The reason, with every diagnostic of a folder that has errors, one per line.
fn explain(reason: &AssemblyError) -> String {
    match diagnostics_of(reason) {
        [] => reason.to_string(),
        diagnostics => {
            let lines: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
            format!("\n  - {}", lines.join("\n  - "))
        }
    }
}

fn diagnostics_of(reason: &AssemblyError) -> &[Diagnostic] {
    match reason {
        AssemblyError::Manifest(FilesError::Invalid { diagnostics }) => diagnostics,
        _ => &[],
    }
}

impl AgentFilesError {
    /// The findings of a folder with errors, in discovery order (empty for any other reason).
    pub fn diagnostics(&self) -> &[Diagnostic] {
        match self {
            Self::Folder { reason, .. } => diagnostics_of(reason),
            Self::Name { .. } => &[],
        }
    }
}

/// What the startup log says about the files: the `agent files` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilesInfo<'a> {
    /// `folder` or `embedded`.
    pub source: &'static str,
    /// For `folder`, the directory that holds `agent/` ([`AgentFolder::root`]): `ADAM_AGENT_DIR`
    /// itself, or its parent when it names the `agent/` directory (`/etc/adam` for
    /// `/etc/adam/agent`).
    pub path: Option<&'a Path>,
    /// `sha256:...` of the manifest and the files its skills bundle: the same files have the same
    /// digest whether they were embedded or read.
    pub digest: &'a str,
    /// The agent's name.
    pub agent: &'a str,
    /// How many warnings the load had.
    pub warnings: usize,
}

impl fmt::Display for FilesInfo<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} ({})", self.source, self.agent, self.digest)
    }
}

impl AgentFiles {
    /// The files of this process: the folder `path` names, or the embedded copy for `None`.
    ///
    /// The folder is read now, once, with the loader `build.rs` uses (lenient: warnings are kept,
    /// an error is refused), and must hold the coder (`name: coder`).
    ///
    /// # Errors
    ///
    /// [`AgentFilesError::Folder`] when the folder cannot be read, has errors or holds more than
    /// one agent; [`AgentFilesError::Name`] when its agent is not called [`AGENT_NAME`].
    pub fn load(path: Option<&Path>) -> Result<Self, AgentFilesError> {
        let Some(path) = path else {
            return Ok(Self::Embedded);
        };
        let folder = AgentFolder::load(path).map_err(|source| AgentFilesError::Folder {
            path: path.to_path_buf(),
            reason: Box::new(source),
        })?;
        let manifest = folder.def.manifest();
        if manifest.name != AGENT_NAME {
            return Err(AgentFilesError::Name {
                root: folder.root,
                file: manifest.path.clone(),
                found: manifest.name.clone(),
                expected: AGENT_NAME,
            });
        }
        Ok(Self::Folder(Box::new(folder)))
    }

    /// The definition to bind: the embedded manifest, or a copy of the folder's.
    ///
    /// # Errors
    ///
    /// [`AssemblyError::Manifest`] when the embedded manifest does not decode (the generated code
    /// and `adam-agent-fs` are not the same version; a unit test reads it).
    pub fn def(&self) -> Result<AgentDef, Box<AssemblyError>> {
        match self {
            Self::Embedded => AgentDef::from_manifest(AGENT).map_err(Box::new),
            Self::Folder(folder) => Ok(folder.def.clone()),
        }
    }

    /// What a load warned about (always empty for the embedded copy, which the build checked
    /// strictly).
    pub fn warnings(&self) -> &[Diagnostic] {
        match self {
            Self::Embedded => &[],
            Self::Folder(folder) => &folder.warnings,
        }
    }

    /// What the startup log says: the source, the digest, the agent, the warning count.
    pub fn describe(&self) -> FilesInfo<'_> {
        match self {
            Self::Embedded => FilesInfo {
                source: "embedded",
                path: None,
                digest: AGENT.digest,
                agent: AGENT.name,
                warnings: 0,
            },
            Self::Folder(folder) => FilesInfo {
                source: "folder",
                path: Some(&folder.root),
                digest: folder.digest.as_str(),
                agent: folder.def.name(),
                warnings: folder.warnings.len(),
            },
        }
    }

    /// Say which files this process runs: one `agent files` line, then each warning as
    /// `path:line: warning: ...`, then (for a folder) what the coder does not do with it.
    pub fn log(&self) {
        let info = self.describe();
        tracing::info!(
            source = info.source,
            path = info.path.map(|p| p.display().to_string()),
            digest = info.digest,
            agent = info.agent,
            warnings = info.warnings,
            "agent files"
        );
        for warning in self.warnings() {
            tracing::warn!("{warning}");
        }
        if let Self::Folder(folder) = self
            && !folder.def.manifest().schedules.is_empty()
        {
            tracing::warn!(
                "the agent folder has schedules, which the coder reads but does not run"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded agent is the coder, and `AGENT_NAME` is what the runs are stored under.
    #[test]
    fn the_embedded_files_are_the_coder() {
        let files = AgentFiles::load(None).unwrap();
        let info = files.describe();
        assert_eq!(info.source, "embedded");
        assert_eq!(info.agent, AGENT_NAME);
        assert_eq!(info.path, None);
        assert_eq!(info.warnings, 0);
        assert!(info.digest.starts_with("sha256:"), "{}", info.digest);
        assert!(files.warnings().is_empty());
        assert_eq!(files.def().unwrap().name(), AGENT_NAME);
    }

    /// The shipped `agent/` read from disk is the embedded copy: the same digest.
    #[test]
    fn the_shipped_folder_has_the_digest_of_the_embedded_copy() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/agent");
        let folder = AgentFiles::load(Some(Path::new(dir))).unwrap();
        let embedded = AgentFiles::load(None).unwrap();
        assert_eq!(folder.describe().source, "folder");
        assert_eq!(folder.describe().digest, embedded.describe().digest);
        assert_eq!(
            folder.describe().path,
            Some(
                Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/agent"))
                    .parent()
                    .unwrap()
            )
        );
    }

    #[test]
    fn a_missing_folder_is_refused_and_is_a_configuration_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = AgentFiles::load(Some(&dir.path().join("nowhere"))).unwrap_err();
        assert!(matches!(error, AgentFilesError::Folder { .. }), "{error}");
        assert!(error.to_string().contains("ADAM_AGENT_DIR"), "{error}");
        assert!(error.to_string().contains("nowhere"), "{error}");
        assert_eq!(error.class(), ErrorClass::Invalid);
        assert!(error.diagnostics().is_empty());
    }

    #[test]
    fn every_diagnostic_of_a_broken_folder_is_in_the_message() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("agent/subagents")).unwrap();
        std::fs::write(
            dir.path().join("agent/instructions.md"),
            "---\nname: coder\ndescription: The coder.\n---\nHi.\n",
        )
        .unwrap();
        for name in ["broken", "worse"] {
            std::fs::write(
                dir.path().join(format!("agent/subagents/{name}.md")),
                "---\n: : [\n---\nSub.\n",
            )
            .unwrap();
        }
        let error = AgentFiles::load(Some(dir.path())).unwrap_err();
        let text = error.to_string();
        for file in ["broken", "worse"] {
            assert!(
                text.contains(&format!("agent/subagents/{file}.md:")),
                "{file} missing from {text}"
            );
        }
        assert!(text.contains(": error: "), "{text}");
        assert_eq!(error.diagnostics().len(), 2, "{:?}", error.diagnostics());
        // Said once: nothing in the message repeats the list.
        assert_eq!(text.matches("broken.md").count(), 1, "{text}");
    }

    #[test]
    fn another_agents_folder_is_refused_naming_the_field() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("agent")).unwrap();
        std::fs::write(
            dir.path().join("agent/instructions.md"),
            "---\nname: other\ndescription: Not the coder.\n---\nHi.\n",
        )
        .unwrap();
        let error = AgentFiles::load(Some(dir.path())).unwrap_err();
        let AgentFilesError::Name {
            found,
            expected,
            file,
            ..
        } = &error
        else {
            panic!("{error}");
        };
        assert_eq!((found.as_str(), *expected), ("other", "coder"));
        assert_eq!(file, Path::new("agent/instructions.md"));
        let text = error.to_string();
        assert!(text.contains("name: coder"), "{text}");
        assert!(text.contains("`other`"), "{text}");
        assert_eq!(error.class(), ErrorClass::Invalid);
    }

    #[test]
    fn warnings_are_kept_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("agent")).unwrap();
        std::fs::write(
            dir.path().join("agent/instructions.md"),
            "---\nname: coder\ndescription: The coder.\nfavourite_colour: green\n---\nHi.\n",
        )
        .unwrap();
        let files = AgentFiles::load(Some(dir.path())).unwrap();
        assert_eq!(files.describe().warnings, 1);
        assert!(
            files.warnings()[0].to_string().contains("favourite_colour"),
            "{:?}",
            files.warnings()
        );
        files.log();
    }
}
