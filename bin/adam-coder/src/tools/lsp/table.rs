//! The closed table of language servers: which program serves which files, how it is started and
//! what it is told. It is code, never a setting: a deployment cannot name a program to run here.
//! A test points [`ServerTable`] at the scripted server of `src/bin/adam-lsp-script.rs`.
//!
//! The names of the servers' own settings below are **unverified** (written from memory of each
//! server's documentation, 2026-11-17). A name a server does not know is ignored by it, so a wrong
//! one costs a weaker result and nothing else. `tests/real_language_servers.rs` is a gated suite for the
//! real servers (`ADAM_TEST_LANGUAGE_SERVERS=1`): `rust-analyzer` and `pyright` were run on
//! 2026-11-17 and reported an error at its line, a definition and references; the other two were not
//! installed there. `dart language-server` takes `--protocol=lsp` (*verified* 2026-11-18 in
//! dart-lang/sdk at tag 3.12.2, `pkg/dartdev/lib/src/commands/language_server.dart`); the Dart
//! server is told no setting, and its run of the suite is the pull request author's report
//! (*unverified* here).
//!
//! The six of ADR 0091 (TOML, YAML, JSON, shell, Dockerfile, C and C++) were read, not run
//! (*verified* 2026-11-28 in the packages on npm, not executed): `yaml-language-server` 2.0.0 pulls
//! the section `yaml` when it is initialized (`out/src/languageserver/handlers/settingsHandlers.js`:
//! `schemaStore.enable`, `kubernetesCRDStore.enable`); `vscode-json-language-server` of
//! vscode-langservers-extracted 4.10.0 takes `handledSchemaProtocols` in its initialization options
//! and asks the client (`vscode/content`) for a schema of any other scheme
//! (`lib/json-language-server/jsonServer.js`), and reads its settings only when it is pushed them,
//! so `json.schemaDownload.enable`, a setting of VS Code's own client, is not one of its names;
//! `bash-language-server` 5.8.1 is started with `start` and has no network endpoint unless it is
//! given one (`explainshellEndpoint`, `''` by default); `docker-langserver` of
//! dockerfile-language-server-nodejs 0.15.0 takes `--stdio`; `taplo` (the strings of @taplo/cli
//! 0.7.0) names its section `evenBetterToml`, with `schema.enabled` and the Schema Store catalog as
//! its default. That taplo asks for its section (rather than waiting to be pushed it), the
//! `languageId`s of shell and C, and everything about `clangd` are *unverified*: the gated suite
//! (`ADAM_TEST_LANGUAGE_SERVERS=1`) is how they are checked once the image has them (ADR 0091 L2).

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
    /// Dart: `dart language-server`.
    Dart,
    /// TOML: `taplo lsp stdio`.
    Toml,
    /// YAML: `yaml-language-server`.
    Yaml,
    /// JSON, and JSON with comments: `vscode-json-language-server`.
    Json,
    /// Shell scripts: `bash-language-server`.
    Shell,
    /// Dockerfiles and Containerfiles, matched by the file's name: `docker-langserver`.
    Dockerfile,
    /// C and C++, their headers included: `clangd`.
    C,
}

/// The `tsserver.js` of the image's own TypeScript: Node from nodejs.org is extracted under
/// `/usr/local`, and `npm install -g typescript` puts the package there (the workspace image's
/// `toolchains/install.sh`). `typescript-language-server` 6.1.0 has no `--tsserver-path` flag; it
/// takes the path as the `tsserver.path` initialization option, validates it, and falls back to the
/// workspace's TypeScript and then its own when an environment has no such file.
const IMAGE_TSSERVER_PATH: &str = "/usr/local/lib/node_modules/typescript/lib/tsserver.js";

impl Language {
    /// Every language, in the order the table lists them.
    pub const ALL: [Self; 11] = [
        Self::Rust,
        Self::TypeScript,
        Self::Python,
        Self::Go,
        Self::Dart,
        Self::Toml,
        Self::Yaml,
        Self::Json,
        Self::Shell,
        Self::Dockerfile,
        Self::C,
    ];

    /// The language of a file, and the `languageId` its text document is opened with.
    ///
    /// A file is matched by its whole name first (`Dockerfile`, `Containerfile`, written as they
    /// are), then by its extension in any case (`api.Dockerfile`, `x.RS`). Only the last component
    /// of the path counts: a directory's name gives no file a language.
    pub fn of_path(path: &str) -> Option<(Self, &'static str)> {
        let name = path.rsplit('/').next().unwrap_or(path);
        if let "Dockerfile" | "Containerfile" = name {
            return Some((Self::Dockerfile, "dockerfile"));
        }
        let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => (Self::Rust, "rust"),
            "ts" | "mts" | "cts" => (Self::TypeScript, "typescript"),
            "tsx" => (Self::TypeScript, "typescriptreact"),
            "js" | "mjs" | "cjs" => (Self::TypeScript, "javascript"),
            "jsx" => (Self::TypeScript, "javascriptreact"),
            "py" | "pyi" => (Self::Python, "python"),
            "go" => (Self::Go, "go"),
            "dart" => (Self::Dart, "dart"),
            "toml" => (Self::Toml, "toml"),
            "yaml" | "yml" => (Self::Yaml, "yaml"),
            "json" => (Self::Json, "json"),
            "jsonc" => (Self::Json, "jsonc"),
            // VS Code's name for a shell script; `bash-language-server` 5.8.1 does not read it.
            "sh" | "bash" => (Self::Shell, "shellscript"),
            "dockerfile" => (Self::Dockerfile, "dockerfile"),
            // A header is C++ as VS Code names it (*unverified* that clangd reads the name: it
            // takes the language from the compile command and the extension).
            "c" => (Self::C, "c"),
            "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" => (Self::C, "cpp"),
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
            Self::Dart => "Dart",
            Self::Toml => "TOML",
            Self::Yaml => "YAML",
            Self::Json => "JSON",
            Self::Shell => "shell",
            Self::Dockerfile => "Dockerfile",
            Self::C => "C and C++",
        }
    }

    /// The extensions the server's watched files are looked for by.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["rs"],
            Self::TypeScript => &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
            Self::Python => &["py", "pyi"],
            Self::Go => &["go"],
            Self::Dart => &["dart"],
            Self::Toml => &["toml"],
            Self::Yaml => &["yaml", "yml"],
            Self::Json => &["json", "jsonc"],
            Self::Shell => &["sh", "bash"],
            // None: the server reads the file it is given and no other, so nothing is watched.
            Self::Dockerfile => &[],
            Self::C => &["c", "h", "cc", "cpp", "cxx", "hpp", "hh"],
        }
    }

    /// The files that mark the root of a project of this language, nearest first. None: the root is
    /// the slot's.
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
            Self::Dart => &["pubspec.yaml"],
            Self::Toml => &[".taplo.toml", "taplo.toml"],
            Self::Yaml | Self::Json | Self::Shell | Self::Dockerfile => &[],
            Self::C => &["compile_commands.json", "compile_flags.txt", ".clangd"],
        }
    }

    /// The sections of the server's settings that a `workspace/configuration` request asks for, and
    /// the options it is started with (`initializationOptions`). A [`Mode::Safe`] server is read only:
    /// no build script, no procedural macro, no check on save, and a rust-analyzer builds in `scratch`
    /// (a directory of its own, removed when the server stops); a [`Mode::Full`] one has what the
    /// project's own build does.
    ///
    /// No server fetches a schema from the network on its own (ADR 0091): taplo validates by no
    /// schema (its default catalog is the Schema Store's), the YAML server uses neither the Schema
    /// Store nor the Kubernetes catalog, and the JSON server handles only `file:` schemas itself and
    /// asks the client for the rest, which answers that it has none. A schema a YAML file names in a
    /// `# yaml-language-server: $schema=` comment is still fetched by that server: the deployment's
    /// network decides. The six of ADR 0091 run no project code, so their two modes are the same.
    pub fn settings(self, mode: Mode, scratch: &str) -> Settings {
        let reference = mode == Mode::Safe;
        match self {
            Self::Rust => {
                let mut options = json!({"cargo": {"targetDir": true}});
                if reference {
                    options = json!({
                        "cargo": {
                            "buildScripts": {"enable": false},
                            "targetDir": scratch,
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
                options: json!({
                    "disableAutomaticTypingAcquisition": true,
                    "tsserver": {"path": IMAGE_TSSERVER_PATH},
                }),
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
            Self::Toml => Settings {
                sections: json!({"evenBetterToml": {"schema": {"enabled": false}}}),
                options: json!({}),
            },
            Self::Yaml => Settings {
                sections: json!({"yaml": {
                    "validate": true,
                    "schemaStore": {"enable": false},
                    "kubernetesCRDStore": {"enable": false},
                }}),
                options: json!({}),
            },
            Self::Json => Settings {
                sections: json!({}),
                options: json!({"handledSchemaProtocols": ["file"]}),
            },
            // Told nothing. clangd's background index is off in its launch line
            // (`ServerTable::production`), in both modes.
            Self::Dart | Self::Shell | Self::Dockerfile | Self::C => Settings {
                sections: json!({}),
                options: json!({}),
            },
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
    entries: [ServerSpec; Language::ALL.len()],
}

impl ServerTable {
    /// The servers of the images (`rust-analyzer`, `typescript-language-server --stdio`,
    /// `pyright-langserver --stdio`, `gopls serve`, `dart language-server --protocol=lsp`), and the
    /// six of ADR 0091, which the workspace image does not have yet (its L2): `taplo lsp stdio`,
    /// `yaml-language-server --stdio`, `vscode-json-language-server --stdio`,
    /// `bash-language-server start`, `docker-langserver --stdio`, `clangd --background-index=false`.
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
                // project chose): named by `tsserver.path` in the initialization options
                // (`Language::settings`), because 6.1.0 has no `--tsserver-path` flag.
                spec(
                    "typescript-language-server",
                    "exec typescript-language-server --stdio",
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
                spec(
                    "dart",
                    "exec dart language-server --protocol=lsp",
                    false,
                    &[],
                ),
                spec("taplo", "exec taplo lsp stdio", false, &[]),
                spec(
                    "yaml-language-server",
                    "exec yaml-language-server --stdio",
                    false,
                    &[],
                ),
                spec(
                    "vscode-json-language-server",
                    "exec vscode-json-language-server --stdio",
                    false,
                    &[],
                ),
                spec(
                    "bash-language-server",
                    "exec bash-language-server start",
                    false,
                    &[],
                ),
                spec(
                    "docker-langserver",
                    "exec docker-langserver --stdio",
                    false,
                    &[],
                ),
                // No background index in either mode: clangd writes it under the project root
                // (`.cache/clangd/index`, *unverified*), which is the slot. References are then
                // those of the files it has parsed, the open file and what it includes.
                spec("clangd", "exec clangd --background-index=false", false, &[]),
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
            Language::Dart => 4,
            Language::Toml => 5,
            Language::Yaml => 6,
            Language::Json => 7,
            Language::Shell => 8,
            Language::Dockerfile => 9,
            Language::C => 10,
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
        assert_eq!(id("lib/main.dart"), Some((Language::Dart, "dart")));
        assert_eq!(id("Cargo.toml"), Some((Language::Toml, "toml")));
        assert_eq!(id(".github/ci.yml"), Some((Language::Yaml, "yaml")));
        assert_eq!(id("deploy/values.YAML"), Some((Language::Yaml, "yaml")));
        assert_eq!(id("package.json"), Some((Language::Json, "json")));
        assert_eq!(
            id(".vscode/settings.jsonc"),
            Some((Language::Json, "jsonc"))
        );
        assert_eq!(id("dev/e2e.sh"), Some((Language::Shell, "shellscript")));
        assert_eq!(id("x.bash"), Some((Language::Shell, "shellscript")));
        assert_eq!(id("src/main.c"), Some((Language::C, "c")));
        for cpp in ["a.h", "a.cc", "a.cpp", "a.cxx", "a.hpp", "a.hh"] {
            assert_eq!(id(cpp), Some((Language::C, "cpp")), "{cpp}");
        }
        // A directory called `x.rs` is not the extension of the file in it.
        for none in ["README.md", "Makefile", "dir.rs/file", "noext", "x.zsh"] {
            assert_eq!(id(none), None, "{none}");
        }
    }

    /// A Dockerfile is found by its name: the whole last component, or its extension.
    #[test]
    fn a_dockerfile_has_a_language_by_its_name() {
        let id = Language::of_path;
        for name in [
            "Dockerfile",
            "docker/coder/Dockerfile",
            "Containerfile",
            "api.Dockerfile",
            "deploy/web.dockerfile",
        ] {
            assert_eq!(
                id(name),
                Some((Language::Dockerfile, "dockerfile")),
                "{name}"
            );
        }
        // The name of a directory gives the file in it nothing; a name is matched as it is
        // written, and only the names of the table are matched.
        for none in [
            "Dockerfile/README.md",
            "Dockerfile.d/notes.txt",
            "Dockerfile.prod",
            "dockerfile",
            "MyDockerfile",
            "Containerfile.txt",
        ] {
            assert_eq!(id(none), None, "{none}");
        }
        assert_eq!(
            id("Dockerfile.d/run.sh"),
            Some((Language::Shell, "shellscript"))
        );
    }

    /// Every language has a name, at least one way to be matched, and its own server; the
    /// languages with no marker have the slot's root.
    #[test]
    fn every_language_of_the_table_is_whole() {
        let table = ServerTable::production();
        for language in Language::ALL {
            assert!(!language.name().is_empty());
            let spec = table.get(language);
            assert!(
                spec.launch.starts_with(&format!("exec {}", spec.program)),
                "{language:?}: {}",
                spec.launch
            );
            for ext in language.extensions() {
                assert_eq!(
                    Language::of_path(&format!("x.{ext}")).map(|(l, _)| l),
                    Some(language),
                    "{ext}"
                );
            }
        }
        for none in [
            Language::Yaml,
            Language::Json,
            Language::Shell,
            Language::Dockerfile,
        ] {
            assert!(none.markers().is_empty(), "{none:?}");
        }
        assert_eq!(
            Language::C.markers(),
            ["compile_commands.json", "compile_flags.txt", ".clangd"]
        );
        assert_eq!(Language::Toml.markers(), [".taplo.toml", "taplo.toml"]);
    }

    /// ADR 0091: no server of the six fetches a schema on its own, in either mode.
    #[test]
    fn the_yaml_json_and_toml_servers_fetch_no_schema_from_the_network() {
        for mode in [Mode::Safe, Mode::Full] {
            let yaml = Language::Yaml.settings(mode, "");
            assert_eq!(yaml.section("yaml")["schemaStore"]["enable"], false);
            assert_eq!(yaml.section("yaml")["kubernetesCRDStore"]["enable"], false);
            assert!(yaml.section("yaml").get("schemas").is_none());
            let json = Language::Json.settings(mode, "");
            assert_eq!(json.options["handledSchemaProtocols"], json!(["file"]));
            let toml = Language::Toml.settings(mode, "");
            assert_eq!(toml.section("evenBetterToml")["schema"]["enabled"], false);
        }
    }

    /// clangd writes its background index under the project root: it is off in its launch line,
    /// which is the same in both modes.
    #[test]
    fn clangd_does_not_index_in_the_background() {
        let table = ServerTable::production();
        assert_eq!(
            table.get(Language::C).launch,
            "exec clangd --background-index=false"
        );
    }

    #[test]
    fn a_section_is_found_by_its_name_or_its_path() {
        let settings = Language::Python.settings(Mode::Full, "");
        assert_eq!(
            settings.section("python.analysis")["diagnosticMode"],
            "openFilesOnly"
        );
        assert!(settings.section("python.nothing").is_null());
        let rust = Language::Rust.settings(Mode::Full, "");
        assert_eq!(rust.section("rust-analyzer")["cargo"]["targetDir"], true);
    }

    #[test]
    fn a_reference_has_no_build_script_no_macro_and_no_check_on_save() {
        let read = Language::Rust
            .settings(Mode::Safe, "/tmp/adam-rust-analyzer/x")
            .options;
        assert_eq!(read["cargo"]["buildScripts"]["enable"], false);
        assert_eq!(read["procMacro"]["enable"], false);
        assert_eq!(read["checkOnSave"], false);
        assert_eq!(
            read["cargo"]["targetDir"], "/tmp/adam-rust-analyzer/x",
            "the directory the coder made, outside the slot"
        );
        let write = Language::Rust.settings(Mode::Full, "").options;
        assert!(write["cargo"].get("buildScripts").is_none());
        assert!(write.get("procMacro").is_none());
    }

    /// `typescript-language-server` 6.1.0 (the image's) has no `--tsserver-path`: it exits with
    /// "unknown option '--tsserver-path'" and the tools answer nothing (the first run of
    /// `real_language_servers` in the built image found it). The image's own TypeScript is named by
    /// the `tsserver.path` initialization option, which the server validates and falls back from;
    /// the launch is the plain `--stdio` the image's own check runs.
    #[test]
    fn the_typescript_server_names_the_images_typescript_by_an_option_not_a_flag() {
        let table = ServerTable::production();
        let spec = table.get(Language::TypeScript);
        assert!(
            !spec.launch.contains("--tsserver-path"),
            "typescript-language-server 6.1.0 has no --tsserver-path: {}",
            spec.launch
        );
        assert_eq!(spec.launch, "exec typescript-language-server --stdio");
        let options = Language::TypeScript.settings(Mode::Full, "").options;
        assert_eq!(
            options["tsserver"]["path"], "/usr/local/lib/node_modules/typescript/lib/tsserver.js",
            "the image's own TypeScript, never the project's node_modules"
        );
    }

    #[test]
    fn the_table_names_eleven_servers_and_a_test_can_replace_one() {
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
                "gopls",
                "dart",
                "taplo",
                "yaml-language-server",
                "vscode-json-language-server",
                "bash-language-server",
                "docker-langserver",
                "clangd",
            ]
        );
        let launches: Vec<&str> = Language::ALL[5..]
            .iter()
            .map(|l| table.get(*l).launch.as_str())
            .collect();
        assert_eq!(
            launches,
            [
                "exec taplo lsp stdio",
                "exec yaml-language-server --stdio",
                "exec vscode-json-language-server --stdio",
                "exec bash-language-server start",
                "exec docker-langserver --stdio",
                "exec clangd --background-index=false",
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
