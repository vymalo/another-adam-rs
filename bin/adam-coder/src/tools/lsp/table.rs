//! The closed table of language servers: which program serves which files, how it is started and
//! what it is told. It is code, never a setting: a deployment cannot name a program to run here.
//! A test points [`ServerTable`] at the scripted server of `src/bin/adam-lsp-script.rs`.
//!
//! The names of the servers' own settings below are **unverified** (written from memory of each
//! server's documentation, 2026-11-17; no real server was started while writing them). A name a
//! server does not know is ignored by it, so a wrong one costs a weaker result and nothing else.
//! `tests/language_servers.rs` has a gated suite for the real servers (`ADAM_TEST_LANGUAGE_SERVERS=1`).

use serde_json::{Value, json};

/// How much a server may do in the environment: what it runs and what it may write.
///
/// A run at the access tier `plan`, and any run in a reference, gets a **safe** server: no build
/// script, no procedural macro, no check on save, the image's TypeScript with no plugin and no type
/// acquisition, and the slot is compared before and after each call and put back when it changed. A
/// run at a higher tier in a slot it may write gets a **full** server (as `run_checks` there runs
/// the same code) and no comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Mode {
    /// Read only: the reference settings, and the slot held to what it was.
    Safe,
    /// What the project's own build does is allowed.
    Full,
}

/// A language the tools know. Closed on purpose: a new variant must fail to compile everywhere a
/// language is matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    /// Rust: `rust-analyzer`.
    Rust,
    /// TypeScript and JavaScript: `typescript-language-server`.
    TypeScript,
    /// Python: `pyright-langserver`, on Node (it sees no interpreter and no installed package).
    Python,
    /// Go: `gopls`.
    Go,
}

impl Language {
    /// Every language, in the order the table lists them.
    pub const ALL: [Self; 4] = [Self::Rust, Self::TypeScript, Self::Python, Self::Go];

    /// The language of a file, and the `languageId` its text document is opened with.
    pub fn of_path(path: &str) -> Option<(Self, &'static str)> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => (Self::Rust, "rust"),
            "ts" | "mts" | "cts" => (Self::TypeScript, "typescript"),
            "tsx" => (Self::TypeScript, "typescriptreact"),
            "js" | "mjs" | "cjs" => (Self::TypeScript, "javascript"),
            "jsx" => (Self::TypeScript, "javascriptreact"),
            "py" | "pyi" => (Self::Python, "python"),
            "go" => (Self::Go, "go"),
            _ => return None,
        })
    }

    /// The name the model is told.
    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::TypeScript => "TypeScript",
            Self::Python => "Python",
            Self::Go => "Go",
        }
    }

    /// The extensions the server's watched files are looked for by.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["rs"],
            Self::TypeScript => &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
            Self::Python => &["py", "pyi"],
            Self::Go => &["go"],
        }
    }

    /// The files that mark the root of a project of this language, nearest first.
    pub fn markers(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["Cargo.toml"],
            Self::TypeScript => &["tsconfig.json", "jsconfig.json", "package.json"],
            Self::Python => &[
                "pyrightconfig.json",
                "pyproject.toml",
                "setup.cfg",
                "setup.py",
            ],
            Self::Go => &["go.work", "go.mod"],
        }
    }

    /// The sections of the server's settings that a `workspace/configuration` request asks for, and
    /// the options it is started with (`initializationOptions`), for a slot that is written or a
    /// safe server (read only: no build script, no procedural macro, no check on save, nothing that
    /// writes beside the files).
    pub fn settings(self, mode: Mode) -> Settings {
        let reference = mode == Mode::Safe;
        match self {
            Self::Rust => {
                let mut options = json!({"cargo": {"targetDir": true}});
                if reference {
                    options = json!({
                        "cargo": {
                            "buildScripts": {"enable": false},
                            "targetDir": "/tmp/adam-rust-analyzer-target",
                        },
                        "procMacro": {"enable": false},
                        "checkOnSave": false,
                    });
                }
                Settings {
                    sections: json!({"rust-analyzer": options.clone()}),
                    options,
                }
            }
            // The image's own TypeScript is the one used (see `ServerTable::production`), and no
            // package is fetched to type a project's imports.
            Self::TypeScript => Settings {
                sections: json!({"typescript": {"disableAutomaticTypeAcquisition": true}}),
                options: json!({"disableAutomaticTypingAcquisition": true}),
            },
            Self::Python => Settings {
                sections: json!({
                    "python": {"analysis": {"autoSearchPaths": true, "diagnosticMode": "openFilesOnly"}}
                }),
                options: json!({}),
            },
            Self::Go => {
                let options = json!({"buildFlags": ["-mod=readonly"]});
                Settings {
                    sections: json!({"gopls": options.clone()}),
                    options,
                }
            }
        }
    }
}

/// What a server is told about itself.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Answers to `workspace/configuration`, by section (`rust-analyzer`, `python.analysis`).
    pub sections: Value,
    /// `initializationOptions`.
    pub options: Value,
}

impl Settings {
    /// The value of `section` (a dotted name), or `null` when there is none.
    pub fn section(&self, section: &str) -> Value {
        let mut at = &self.sections;
        // `python.analysis` is found as `{"python": {"analysis": ..}}`, and a section that is a
        // key of its own (`rust-analyzer`) as that key.
        if let Some(whole) = at.get(section) {
            return whole.clone();
        }
        for part in section.split('.') {
            match at.get(part) {
                Some(next) => at = next,
                None => return Value::Null,
            }
        }
        at.clone()
    }
}

/// How one language's server is started.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    /// The program the probe looks for on `PATH` (`command -v`), and that the message names.
    pub program: String,
    /// The shell command that starts it, in the project root, in the environment's login shell. It
    /// ends in `exec <server>`, so that the server is the process the coder holds.
    pub launch: String,
    /// Variables to set for it (never a secret).
    pub env: Vec<(String, String)>,
    /// Ask `<program> --version` too, in the project root: a toolchain file that pins a version
    /// without the component makes the program exist and fail.
    pub version_probe: bool,
}

/// The servers, by language.
#[derive(Debug, Clone)]
pub struct ServerTable {
    entries: [ServerSpec; 4],
}

impl ServerTable {
    /// The servers of the images (`rust-analyzer`, `typescript-language-server --stdio`,
    /// `pyright-langserver --stdio`, `gopls serve`).
    pub fn production() -> Self {
        let spec =
            |program: &str, launch: &str, version_probe: bool, env: &[(&str, &str)]| ServerSpec {
                program: program.to_owned(),
                launch: launch.to_owned(),
                env: env
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
                version_probe,
            };
        Self {
            entries: [
                spec("rust-analyzer", "exec rust-analyzer", true, &[]),
                // The image's own TypeScript, never the project's `node_modules` (which is code the
                // project chose): found next to the global packages, else the server's own.
                spec(
                    "typescript-language-server",
                    "tsserver=\"$(npm root -g 2>/dev/null)/typescript/lib/tsserver.js\"; \
                     if [ -f \"$tsserver\" ]; then exec typescript-language-server --stdio \
                     --tsserver-path \"$tsserver\"; fi; exec typescript-language-server --stdio",
                    false,
                    &[],
                ),
                spec(
                    "pyright-langserver",
                    "exec pyright-langserver --stdio",
                    false,
                    &[],
                ),
                spec(
                    "gopls",
                    "exec gopls serve",
                    false,
                    &[("GOFLAGS", "-mod=readonly"), ("GOTOOLCHAIN", "local")],
                ),
            ],
        }
    }

    /// The same table with `spec` for `language` (a test points a language at the scripted server).
    #[must_use]
    pub fn with(mut self, language: Language, spec: ServerSpec) -> Self {
        self.entries[Self::at(language)] = spec;
        self
    }

    /// The server of `language`.
    pub fn get(&self, language: Language) -> &ServerSpec {
        &self.entries[Self::at(language)]
    }

    fn at(language: Language) -> usize {
        match language {
            Language::Rust => 0,
            Language::TypeScript => 1,
            Language::Python => 2,
            Language::Go => 3,
        }
    }
}

impl Default for ServerTable {
    fn default() -> Self {
        Self::production()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_has_a_language_by_its_extension() {
        let id = Language::of_path;
        assert_eq!(id("src/main.rs"), Some((Language::Rust, "rust")));
        assert_eq!(
            id("a/b.TSX"),
            Some((Language::TypeScript, "typescriptreact"))
        );
        assert_eq!(id("x.mjs"), Some((Language::TypeScript, "javascript")));
        assert_eq!(id("m.pyi"), Some((Language::Python, "python")));
        assert_eq!(id("cmd/x.go"), Some((Language::Go, "go")));
        // A directory called `x.rs` is not the extension of the file in it.
        for none in ["README.md", "Makefile", "dir.rs/file", "noext"] {
            assert_eq!(id(none), None, "{none}");
        }
    }

    #[test]
    fn a_section_is_found_by_its_name_or_its_path() {
        let settings = Language::Python.settings(Mode::Full);
        assert_eq!(
            settings.section("python.analysis")["diagnosticMode"],
            "openFilesOnly"
        );
        assert!(settings.section("python.nothing").is_null());
        let rust = Language::Rust.settings(Mode::Full);
        assert_eq!(rust.section("rust-analyzer")["cargo"]["targetDir"], true);
    }

    #[test]
    fn a_reference_has_no_build_script_no_macro_and_no_check_on_save() {
        let read = Language::Rust.settings(Mode::Safe).options;
        assert_eq!(read["cargo"]["buildScripts"]["enable"], false);
        assert_eq!(read["procMacro"]["enable"], false);
        assert_eq!(read["checkOnSave"], false);
        assert_ne!(read["cargo"]["targetDir"], true, "a path outside the slot");
        let write = Language::Rust.settings(Mode::Full).options;
        assert!(write["cargo"].get("buildScripts").is_none());
        assert!(write.get("procMacro").is_none());
    }

    #[test]
    fn the_table_names_four_servers_and_a_test_can_replace_one() {
        let table = ServerTable::production();
        let programs: Vec<&str> = Language::ALL
            .iter()
            .map(|l| table.get(*l).program.as_str())
            .collect();
        assert_eq!(
            programs,
            [
                "rust-analyzer",
                "typescript-language-server",
                "pyright-langserver",
                "gopls"
            ]
        );
        assert!(
            table
                .get(Language::Go)
                .env
                .iter()
                .any(|(k, v)| k == "GOFLAGS" && v == "-mod=readonly")
        );
        let spec = ServerSpec {
            program: "x".into(),
            launch: "exec x".into(),
            env: Vec::new(),
            version_probe: false,
        };
        let table = table.with(Language::Python, spec);
        assert_eq!(table.get(Language::Python).program, "x");
        assert_eq!(table.get(Language::Rust).program, "rust-analyzer");
    }
}
