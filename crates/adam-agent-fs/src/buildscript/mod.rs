//! The code generator for `build.rs`: `adam_agent_fs::build("agent").emit()`.
//!
//! It loads and validates the directory with [`Dir`] (the same parser and validator the run-time
//! path uses), reports what it found as build errors and warnings with file and line, and writes
//! the agent as a `static` to `OUT_DIR/adam_agent.rs` for `adam::include_agent!()`. It also tells
//! cargo to rerun the script when any file, or the directory, changes.
//!
//! Everything but [`Build::emit`] takes its inputs from the builder, not from the environment, so
//! the logic runs in unit tests.

mod codegen;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::manifest::{AgentManifest, Layout, Package, Report, Strictness};
use crate::{Diagnostic, Dir, Error, ManifestSource};

/// The most bytes of resources (`scripts/`, `references/`, `assets/`) one skill may embed.
const RESOURCE_LIMIT: u64 = 1024 * 1024;

/// The name of the generated source in `OUT_DIR`.
const SOURCE_FILE: &str = "adam_agent.rs";

/// The name of the manifest as JSON in `OUT_DIR`, for people and tools to read.
const MANIFEST_FILE: &str = "adam_manifest.json";

/// Start a build script step for the agent directory `dir`: `"agent"` (one agent) or `"agents"`
/// (`agents/<name>/`, several). The directory is relative to the package root.
///
/// ```no_run
/// // build.rs
/// fn main() -> Result<(), adam_agent_fs::BuildError> {
///     adam_agent_fs::build("agent").emit()?;
///     Ok(())
/// }
/// ```
pub fn build(dir: &str) -> Build {
    Build {
        dir: dir.to_owned(),
        root: None,
        out_dir: None,
        name: None,
        optional: false,
        strictness: Strictness::Lenient,
        crate_path: "::adam::agent_fs".to_owned(),
    }
}

/// The settings of one build script step. Create it with [`build`].
#[derive(Debug, Clone)]
pub struct Build {
    dir: String,
    root: Option<PathBuf>,
    out_dir: Option<PathBuf>,
    name: Option<String>,
    optional: bool,
    strictness: Strictness,
    crate_path: String,
}

impl Build {
    /// Accept a package with neither `agent/` nor `agents/`: the generated `AGENTS` is empty.
    #[must_use]
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    /// Fail the build on warnings too (an unknown key, a name that had to be repaired).
    #[must_use]
    pub fn strict(mut self) -> Self {
        self.strictness = Strictness::Strict;
        self
    }

    /// The directory that holds the agent directory. Default: `CARGO_MANIFEST_DIR`.
    #[must_use]
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Where the generated files go. Default: `OUT_DIR`.
    #[must_use]
    pub fn out_dir(mut self, out_dir: impl Into<PathBuf>) -> Self {
        self.out_dir = Some(out_dir.into());
        self
    }

    /// The name of the root agent when its frontmatter has none. Default: `CARGO_PKG_NAME`.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The path of the crate that holds the `Embedded*` types, as the generated code writes it.
    /// Default `::adam::agent_fs` (the facade); use `::adam_agent_fs` for a crate that depends on
    /// this one directly.
    #[must_use]
    pub fn crate_path(mut self, path: impl Into<String>) -> Self {
        self.crate_path = path.into();
        self
    }

    /// Load, validate and generate; print the cargo directives to standard output; write the
    /// files. This is what a build script calls.
    ///
    /// # Errors
    ///
    /// [`BuildError::Load`] with [`Error::Invalid`] when the files have errors (or warnings under
    /// [`strict`](Self::strict)); each finding has been printed as `cargo::error=path:line: ...`
    /// by then. The other variants are I/O and environment problems.
    pub fn emit(&self) -> Result<Emitted, BuildError> {
        self.emit_to(&mut io::stdout().lock())
    }

    /// [`emit`](Self::emit) with the directives written to `out` instead of standard output.
    ///
    /// # Errors
    ///
    /// See [`emit`](Self::emit).
    pub fn emit_to(&self, out: &mut impl Write) -> Result<Emitted, BuildError> {
        let root = self.resolve_root()?;
        let out_dir = match &self.out_dir {
            Some(dir) => dir.clone(),
            None => env_path("OUT_DIR")?,
        };
        let run = self.run(&root)?;
        for path in &run.watch {
            directive(out, "rerun-if-changed", &path.to_string_lossy())?;
        }
        let refused = self.strictness == Strictness::Strict;
        for d in &run.diagnostics {
            let kind = if d.is_error() || refused {
                "error"
            } else {
                "warning"
            };
            directive(out, kind, &finding(d))?;
        }
        let generated = run.generated?;
        let source = out_dir.join(SOURCE_FILE);
        let manifest = out_dir.join(MANIFEST_FILE);
        write_if_changed(&source, generated.source.as_bytes())?;
        write_if_changed(&manifest, generated.manifest_json.as_bytes())?;
        Ok(Emitted {
            source,
            manifest,
            warnings: run.diagnostics.iter().filter(|d| !d.is_error()).count(),
        })
    }

    /// Load, validate and generate, without printing or writing anything.
    ///
    /// # Errors
    ///
    /// See [`emit`](Self::emit).
    pub fn generate(&self) -> Result<Generated, BuildError> {
        let root = self.resolve_root()?;
        let run = self.run(&root)?;
        let mut generated = run.generated?;
        generated.diagnostics = run.diagnostics;
        generated.watch = run.watch;
        Ok(generated)
    }

    fn resolve_root(&self) -> Result<PathBuf, BuildError> {
        if self.dir != "agent" && self.dir != "agents" {
            return Err(BuildError::BadDir {
                dir: self.dir.clone(),
            });
        }
        let root = match &self.root {
            Some(root) => root.clone(),
            None => env_path("CARGO_MANIFEST_DIR")?,
        };
        std::path::absolute(&root).map_err(|e| BuildError::Load(Error::io(root, e)))
    }

    fn run(&self, root: &Path) -> Result<Run, BuildError> {
        let mut dir = Dir::new(root);
        if self.optional {
            dir = dir.optional();
        }
        let default_name = self
            .name
            .clone()
            .or_else(|| std::env::var("CARGO_PKG_NAME").ok());
        if let Some(name) = default_name {
            dir = dir.default_name(name);
        }
        let mut report = dir.load()?;
        let watch = watch_list(&root.join(&self.dir))?;
        self.check_layout(&mut report);
        check_resources(root, &report.package, &mut report.diagnostics)?;
        let diagnostics = report.diagnostics.clone();
        let generated = report
            .into_package(self.strictness)
            .map_err(BuildError::from)
            .and_then(|package| generate(&package, root, &self.crate_path));
        Ok(Run {
            diagnostics,
            watch,
            generated,
        })
    }

    /// `build("agent")` promises `agent/`; a package that has `agents/` is not what was asked.
    fn check_layout(&self, report: &mut Report) {
        let found = match report.package.layout {
            Layout::Absent => return,
            Layout::Single => "agent",
            Layout::Multi => "agents",
        };
        if found != self.dir {
            report.diagnostics.push(Diagnostic::error(
                found,
                None,
                format!(
                    "`build({:?})` expects `{}/`, but the package has `{found}/`",
                    self.dir, self.dir
                ),
            ));
        }
    }
}

/// What one run found and produced. `generated` is an error when the files are refused.
struct Run {
    diagnostics: Vec<Diagnostic>,
    watch: Vec<PathBuf>,
    generated: Result<Generated, BuildError>,
}

/// What [`Build::generate`] returns.
#[derive(Debug, Clone)]
pub struct Generated {
    /// The Rust source of `adam_agent.rs`.
    pub source: String,
    /// The normalised manifest and its digests as JSON: the content of `adam_manifest.json`.
    pub manifest_json: String,
    /// Every finding, warnings included.
    pub diagnostics: Vec<Diagnostic>,
    /// The paths cargo is told to watch: the directory and every file in it.
    pub watch: Vec<PathBuf>,
}

/// What [`Build::emit`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emitted {
    /// `OUT_DIR/adam_agent.rs`, the file `adam::include_agent!()` includes.
    pub source: PathBuf,
    /// `OUT_DIR/adam_manifest.json`, the normalised manifest and its digests, for people to read.
    pub manifest: PathBuf,
    /// How many warnings were printed.
    pub warnings: usize,
}

/// Why a build script step failed.
///
/// `Debug` prints the same text as `Display`, so `fn main() -> Result<(), BuildError>` gives cargo
/// one readable line instead of a struct dump.
#[derive(thiserror::Error)]
pub enum BuildError {
    /// A variable cargo sets for build scripts is missing: run this in a `build.rs`, or set the
    /// value on [`Build`].
    #[error(
        "the environment variable {name} is not set: run this in a build script, or set it on `Build`"
    )]
    Env {
        /// The variable.
        name: &'static str,
    },
    /// `build()` was given a directory that is not `agent` or `agents`.
    #[error("build({dir:?}): the agent directory is `agent` (one agent) or `agents` (several)")]
    BadDir {
        /// What was given.
        dir: String,
    },
    /// The directory could not be read, or the files are invalid.
    #[error(transparent)]
    Load(#[from] Error),
    /// A generated file, a directive or the output could not be written.
    #[error("cannot write {}", path.display())]
    Write {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A path is not valid UTF-8, so it cannot be written into Rust source.
    #[error("the path {} is not valid UTF-8", .0.display())]
    NonUtf8Path(PathBuf),
}

impl fmt::Debug for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl BuildError {
    /// The findings behind a refusal, when this is one.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        match self {
            Self::Load(Error::Invalid { diagnostics }) => diagnostics,
            _ => &[],
        }
    }
}

/// The generated source and manifest for a package that loaded.
fn generate(package: &Package, root: &Path, crate_path: &str) -> Result<Generated, BuildError> {
    let source = codegen::render(package, root, crate_path)?;
    let dir = Dir::new(root);
    let mut digests = BTreeMap::new();
    for agent in &package.agents {
        digests.insert(agent.name.as_str(), dir.digest(agent)?.to_string());
    }
    #[derive(Serialize)]
    struct File<'a> {
        digests: BTreeMap<&'a str, String>,
        package: &'a Package,
    }
    let mut manifest_json = serde_json::to_string_pretty(&File { digests, package })
        .map_err(|e| Error::codec("encode", "the package", e))?;
    manifest_json.push('\n');
    Ok(Generated {
        source,
        manifest_json,
        diagnostics: Vec::new(),
        watch: Vec::new(),
    })
}

/// One line for a diagnostic: `path:line: message`, the shape editors turn into a link.
fn finding(d: &Diagnostic) -> String {
    match d.line {
        Some(line) => format!("{}:{line}: {}", d.path.display(), d.message),
        None => format!("{}: {}", d.path.display(), d.message),
    }
}

/// `cargo::KEY=VALUE`, on one line.
fn directive(out: &mut impl Write, key: &str, value: &str) -> Result<(), BuildError> {
    let value = value.replace(['\n', '\r'], " ");
    writeln!(out, "cargo::{key}={value}").map_err(|e| BuildError::Write {
        path: PathBuf::from("standard output"),
        source: e,
    })
}

fn env_path(name: &'static str) -> Result<PathBuf, BuildError> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .ok_or(BuildError::Env { name })
}

/// The directory, then every entry under it, sorted; symbolic links are listed, not entered.
/// Empty when the directory does not exist (cargo would treat a missing path as always changed).
fn watch_list(dir: &Path) -> Result<Vec<PathBuf>, BuildError> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), BuildError> {
        let mut entries = fs::read_dir(dir)
            .map_err(|e| Error::io(dir, e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::io(dir, e))?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type().map_err(|e| Error::io(&path, e))?;
            if path.file_name().and_then(OsStr::to_str).is_none() {
                continue; // cargo needs UTF-8 paths, and the loader warned about the name
            }
            out.push(path.clone());
            if file_type.is_dir() {
                walk(&path, out)?;
            }
        }
        Ok(())
    }
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = vec![dir.to_path_buf()];
    walk(dir, &mut out)?;
    Ok(out)
}

/// A skill embeds its resources in the binary: over 1 MiB is an error.
fn check_resources(
    root: &Path,
    package: &Package,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), BuildError> {
    fn agent(
        root: &Path,
        a: &AgentManifest,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<(), BuildError> {
        for skill in &a.skills {
            let dir = skill.path.parent().unwrap_or(Path::new(""));
            let mut total = 0_u64;
            for resource in &skill.resources {
                let path = root.join(dir).join(resource);
                total += fs::metadata(&path).map_err(|e| Error::io(&path, e))?.len();
            }
            if total > RESOURCE_LIMIT {
                diagnostics.push(Diagnostic::error(
                    &skill.path,
                    None,
                    format!(
                        "the resources of skill `{}` are {total} bytes; at most {RESOURCE_LIMIT} \
                         (1 MiB) are embedded in the binary",
                        skill.name
                    ),
                ));
            }
        }
        for sub in &a.subagents {
            if let crate::Subagent::Local(local) = sub {
                agent(root, local, diagnostics)?;
            }
        }
        Ok(())
    }
    for a in &package.agents {
        agent(root, a, diagnostics)?;
    }
    Ok(())
}

/// Write `bytes` to `path` unless the file already holds them, so an unchanged agent does not
/// make rustc rebuild the crate.
fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<(), BuildError> {
    if fs::read(path).is_ok_and(|old| old == bytes) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| BuildError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(path, bytes).map_err(|source| BuildError::Write {
        path: path.to_path_buf(),
        source,
    })
}
