//! The language server tools (ADR 0044 slice 17): `file_diagnostics`, `find_definition`,
//! `find_references`, over the scripted server `adam-lsp-script` (`src/bin/adam-lsp-script.rs`, a
//! second binary of the crate that no image builds).
//!
//! The servers are real processes of the test (`sh` starts the script, which may start a child and a
//! grandchild): every stop point is proved by pid, and a test that starts one stops it (the
//! registry kills what is left when it goes out of scope).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_coder::tools::lsp::{Language, ServerSpec, ServerTable};
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_coder::{AGENT_NAME, CoderAgent, ToolEnv, coder_tools};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{KeptStore, ToolCtx, ToolOutput};
use adam_model::{DynModel, MockModel};
use adam_runtime::{CollectingSink, Inbound, Runtime};
use common::{Fixture, GITHUB_TOKEN, call};
use serde_json::{Value, json};

/// The scripted server.
const SCRIPT_BIN: &str = env!("CARGO_BIN_EXE_adam-lsp-script");

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "))
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(30), "{what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The table that points every language at the scripted server, with `script` as its script.
fn table_for(script: &Path) -> ServerTable {
    let spec = ServerSpec {
        program: SCRIPT_BIN.to_owned(),
        launch: format!("exec '{SCRIPT_BIN}'"),
        env: vec![("ADAM_LSP_SCRIPT".to_owned(), script.display().to_string())],
        version_probe: false,
    };
    Language::ALL
        .into_iter()
        .fold(ServerTable::production(), |table, language| {
            table.with(language, spec.clone())
        })
}

struct Lsp {
    fx: Fixture,
    ctx: ToolCtx,
    dir: tempfile::TempDir,
}

impl Lsp {
    async fn new(script: Value) -> Self {
        Self::with(script, |_| {}).await
    }

    async fn with(script: Value, tweak: impl FnOnce(&mut adam_coder::CoderSettings)) -> Self {
        Self::over(Fixture::with(tweak).await, script).await
    }

    /// The fixture `fx`, its language table pointed at the scripted server, and the workspace made.
    async fn over(fx: Fixture, script: Value) -> Self {
        Self::build(fx, script, table_for).await
    }

    /// [`over`](Self::over) with the table `table` makes of the script's path.
    async fn build(
        mut fx: Fixture,
        script: Value,
        table: impl FnOnce(&Path) -> ServerTable,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("script.json");
        write_script(dir.path(), &script);
        Arc::get_mut(&mut fx.env)
            .expect("the tools are not shared yet")
            .language_table = table(&script_path);
        let ctx = ToolCtx::detached("tool", "call-1", Arc::new(CollectingSink::new()))
            .with_state(fx.env.clone())
            .with_state(Arc::new(KeptStore(fx.env.kept.clone())));
        let rig = Self { fx, ctx, dir };
        rig.prepare().await;
        rig
    }

    /// The person names the repository and the workspace is made.
    async fn prepare(&self) {
        let run = self.run();
        let mut notes = self.fx.env.notes.load(&run).await.unwrap();
        notes.name_repos(named_in(
            &without_untrusted(&self.fx.remote_url()),
            &self.fx.env.settings.default_repo_host,
        ));
        self.fx.env.notes.save(&run, &notes).await.unwrap();
        let out = self
            .call(
                "prepare_workspace",
                json!({"repo_url": self.fx.remote_url(), "base_branch": "main"}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
    }

    fn run(&self) -> String {
        self.ctx.run_id().to_string()
    }

    fn worktree(&self) -> PathBuf {
        common::slot_dir(&self.fx.root, &self.run())
    }

    /// A file in the worktree.
    fn put(&self, rel: &str, text: &str) {
        let path = self.worktree().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The script, written again: the next server started reads this one.
    fn script(&self, script: &Value) {
        write_script(self.dir.path(), script);
    }

    async fn call(&self, name: &str, args: Value) -> ToolOutput {
        let tools = coder_tools(&self.fx.env);
        let tool = tools.get(name).expect("the tool is in the set").clone();
        tool.call(&self.ctx, args)
            .await
            .expect("a result, not a failure")
    }

    async fn diagnostics(&self, args: Value) -> ToolOutput {
        self.call("file_diagnostics", args).await
    }

    /// What the server logged: one JSON value per message it received.
    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("log.jsonl"))
            .unwrap_or_default()
            .lines()
            // The script writes a line at once; a reader that polls may still meet the end of the
            // file between two writes of the OS, and a line that is not whole is not read yet.
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn logged(&self, method: &str) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|m| m["method"] == method)
            .collect()
    }

    fn servers(&self) -> Vec<adam_coder::tools::lsp::ServerInfo> {
        self.fx.env.commands.language_servers().running(&self.run())
    }

    /// The pid of the only server this run holds.
    fn pid(&self) -> u32 {
        let servers = self.servers();
        assert_eq!(servers.len(), 1, "{servers:?}");
        servers[0].pid.expect("a pid")
    }

    /// The pids the script wrote: the server, its child and its grandchild (the last two a moment
    /// after the server starts).
    async fn tree(&self) -> Vec<u32> {
        let dir = self.dir.path().to_path_buf();
        until("the pids of the tree", || {
            dir.join("pid.grandchild").exists()
        })
        .await;
        tree_of(&dir)
    }

    async fn tree_is_gone(&self, what: &str) {
        let pids = self.tree().await;
        until(what, || pids.iter().all(|p| !alive(*p))).await;
    }
}

/// The pids of `dir`'s script: the server, its child and its grandchild.
fn tree_of(dir: &Path) -> Vec<u32> {
    ["pid", "pid.child", "pid.grandchild"]
        .iter()
        .map(|n| {
            std::fs::read_to_string(dir.join(n))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        })
        .collect()
}

/// The script, with the log and the pid file this rig reads.
fn write_script(dir: &Path, script: &Value) {
    let mut script = script.clone();
    script["log"] = json!(dir.join("log.jsonl").display().to_string());
    script["pid_file"] = json!(dir.join("pid").display().to_string());
    std::fs::write(dir.join("script.json"), script.to_string()).unwrap();
}

/// A file with an error at its second line and a warning at its first.
const LIB: &str = "pub fn first() {}\npub fn second() -> u32 { \"x\" }\n";

fn diag(severity: u64, line: u64, character: u64, message: &str) -> Value {
    json!({"severity": severity, "line": line, "character": character, "message": message})
}

fn rust_project(lsp: &Lsp) {
    lsp.put("Cargo.toml", "[package]\nname = \"x\"\n");
    lsp.put("src/lib.rs", LIB);
}

fn dart_project(lsp: &Lsp) {
    lsp.put("pubspec.yaml", "name: x\nenvironment:\n  sdk: ^3.0.0\n");
    lsp.put(
        "lib/main.dart",
        "int add(int a, int b) => a + b;\nint broken() => add(1, 'two');\n",
    );
}

// ------------------------------------------------------------------------------- no server

#[tokio::test]
async fn no_server_on_path_starts_nothing_and_the_message_names_the_program() {
    let lsp = Lsp::build(Fixture::new().await, json!({}), |_| {
        ServerTable::production().with(
            Language::Rust,
            ServerSpec {
                program: "/nonexistent/adam-no-such-server".to_owned(),
                launch: "exec /nonexistent/adam-no-such-server".to_owned(),
                env: Vec::new(),
                version_probe: false,
            },
        )
    })
    .await;
    rust_project(&lsp);
    for _ in 0..2 {
        let out = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
        // A result, not an error: the model is told what to do instead.
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("No language server for Rust")
                && out.content.contains("adam-no-such-server")
                && out.content.contains("cargo check"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("/nonexistent"), "{}", out.content);
    }
    assert!(lsp.servers().is_empty());
}

#[tokio::test]
async fn a_file_no_server_serves_is_answered_with_what_they_serve() {
    let lsp = Lsp::new(json!({})).await;
    lsp.put("notes.txt", "hello\n");
    let out = lsp.diagnostics(json!({"path": "notes.txt"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains(".rs (Rust)")
            && out.content.contains(".go (Go)")
            && out.content.contains(".dart (Dart)")
            && out.content.contains(".toml (TOML)")
            && out.content.contains(".yaml .yml (YAML)")
            && out.content.contains("named Dockerfile or Containerfile"),
        "{}",
        out.content
    );
    assert!(lsp.servers().is_empty());
}

#[tokio::test]
async fn dart_files_start_a_dart_server_at_the_nearest_pubspec() {
    let lsp =
        Lsp::new(json!({"diagnostics": [diag(1, 1, 23, "argument_type_not_assignable")]})).await;
    dart_project(&lsp);
    lsp.put("apps/mobile/pubspec.yaml", "name: mobile\n");
    lsp.put("apps/mobile/lib/app.dart", "void main() {}\n");

    let out = lsp.diagnostics(json!({"path": "lib/main.dart"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content
            .contains("lib/main.dart:2:24: error: argument_type_not_assignable"),
        "{}",
        out.content
    );

    let servers = lsp.servers();
    assert_eq!(servers.len(), 1, "{servers:?}");
    assert_eq!(servers[0].language, Language::Dart);
    assert_eq!(servers[0].root, lsp.worktree());

    let sub = lsp
        .diagnostics(json!({"path": "apps/mobile/lib/app.dart"}))
        .await;
    assert!(!sub.is_error, "{}", sub.content);
    let servers = lsp.servers();
    assert_eq!(servers.len(), 2, "{servers:?}");
    assert!(servers.iter().all(|s| s.language == Language::Dart));
    assert!(
        servers
            .iter()
            .any(|s| s.root == lsp.worktree().join("apps/mobile"))
    );
}

// ------------------------------------------------------------------ the six of ADR 0089 (L1)

/// The production table with every program replaced by the scripted server and its arguments
/// kept: with `log_argv` the script logs what its launch line gave it.
fn production_launches(script: &Path) -> ServerTable {
    let production = ServerTable::production();
    Language::ALL
        .into_iter()
        .fold(ServerTable::production(), |table, language| {
            let spec = production.get(language);
            let args = spec
                .launch
                .strip_prefix(&format!("exec {}", spec.program))
                .expect("a launch line is `exec <program> ...`");
            let mut env = spec.env.clone();
            env.push(("ADAM_LSP_SCRIPT".to_owned(), script.display().to_string()));
            table.with(
                language,
                ServerSpec {
                    program: SCRIPT_BIN.to_owned(),
                    launch: format!("exec '{SCRIPT_BIN}'{args}"),
                    env,
                    version_probe: false,
                },
            )
        })
}

/// A rig whose servers are started with the production launch lines, room for `max` of them.
async fn six_rig(script: Value, max: usize) -> Lsp {
    Lsp::build(
        Fixture::with(|s| s.language_servers_max = max).await,
        script,
        production_launches,
    )
    .await
}

impl Lsp {
    /// The arguments of every server started so far, in the order they were started.
    fn argvs(&self) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter_map(|m| m.get("argv").cloned())
            .collect()
    }

    /// The answers of the client to the script's `server_requests`, by id, once there are `n`.
    async fn answers(&self, n: usize) -> Vec<Value> {
        until("the answers to be logged", || {
            self.log()
                .iter()
                .filter(|m| m.get("answer_to").is_some())
                .count()
                >= n
        })
        .await;
        self.log()
            .into_iter()
            .filter(|m| m.get("answer_to").is_some())
            .collect()
    }
}

#[tokio::test]
async fn each_of_the_six_languages_starts_its_own_server_with_its_launch_line_at_the_slot_root() {
    let lsp = six_rig(
        json!({"log_argv": true, "diagnostics": [diag(1, 0, 0, "seen")]}),
        8,
    )
    .await;
    let cases: [(&str, Language, &str, &[&str]); 6] = [
        ("config/app.toml", Language::Toml, "toml", &["lsp", "stdio"]),
        (
            ".github/workflows/ci.yml",
            Language::Yaml,
            "yaml",
            &["--stdio"],
        ),
        ("deploy/values.json", Language::Json, "json", &["--stdio"]),
        ("dev/e2e.sh", Language::Shell, "shellscript", &["start"]),
        (
            "docker/Dockerfile",
            Language::Dockerfile,
            "dockerfile",
            &["--stdio"],
        ),
        ("src/add.c", Language::C, "c", &["--background-index=false"]),
    ];
    for (n, (path, language, id, argv)) in cases.into_iter().enumerate() {
        lsp.put(path, "x\n");
        let out = lsp.diagnostics(json!({"path": path})).await;
        assert!(!out.is_error, "{path}: {}", out.content);
        assert!(
            out.content.contains(&format!("{path}:1:1: error: seen")),
            "{path}: {}",
            out.content
        );
        let servers = lsp.servers();
        assert_eq!(servers.len(), n + 1, "one server per language: {servers:?}");
        let server = servers
            .iter()
            .find(|s| s.language == language)
            .unwrap_or_else(|| panic!("a {language:?} server: {servers:?}"));
        // None of the six has a marker here: the project root is the slot's.
        assert_eq!(server.root, lsp.worktree(), "{path}");
        assert_eq!(lsp.argvs()[n], json!(argv), "{path}");
        let opened = lsp.logged("textDocument/didOpen");
        assert_eq!(
            opened[n]["params"]["textDocument"]["languageId"], id,
            "{path}"
        );
    }
}

#[tokio::test]
async fn a_dockerfile_is_matched_by_its_name_and_not_by_the_name_of_its_directory() {
    let lsp = six_rig(json!({"diagnostics": [diag(1, 0, 0, "seen")]}), 8).await;
    for path in ["Dockerfile", "Containerfile", "api.Dockerfile"] {
        lsp.put(path, "FROM alpine\n");
        let out = lsp.diagnostics(json!({"path": path})).await;
        assert!(
            out.content.contains(&format!("{path}:1:1: error: seen")),
            "{path}: {}",
            out.content
        );
    }
    let servers = lsp.servers();
    assert_eq!(servers.len(), 1, "one server for the three: {servers:?}");
    assert_eq!(servers[0].language, Language::Dockerfile);
    for path in [
        "Dockerfile.d/notes.txt",
        "images/Dockerfile/README.md",
        "Dockerfile.prod",
    ] {
        lsp.put(path, "FROM alpine\n");
        let out = lsp.diagnostics(json!({"path": path})).await;
        assert!(!out.is_error, "{path}: {}", out.content);
        assert!(
            out.content.contains("No language server serves")
                && out.content.contains("Dockerfile or Containerfile"),
            "{path}: {}",
            out.content
        );
    }
    assert_eq!(lsp.servers().len(), 1);
}

#[tokio::test]
async fn clangd_and_taplo_are_rooted_at_their_markers_and_the_rest_at_the_slot() {
    let lsp = six_rig(json!({"diagnostics": []}), 8).await;
    lsp.put("native/compile_flags.txt", "-std=c11\n");
    lsp.put(
        "native/src/add.c",
        "int add(int a, int b) { return a + b; }\n",
    );
    lsp.put("engine/compile_commands.json", "[]\n");
    lsp.put("engine/src/a.cpp", "int a() { return 1; }\n");
    lsp.put("tools/.clangd", "CompileFlags:\n  Add: [-std=c++20]\n");
    lsp.put("tools/b.hpp", "int b();\n");
    lsp.put("loose/x.h", "int x();\n");
    lsp.put("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    lsp.put("crates/b/taplo.toml", "include = [\"*.toml\"]\n");
    lsp.put("crates/b/Cargo.toml", "[package]\nname = \"b\"\n");
    let root = lsp.worktree();
    for (path, language, at) in [
        ("native/src/add.c", Language::C, root.join("native")),
        ("engine/src/a.cpp", Language::C, root.join("engine")),
        ("tools/b.hpp", Language::C, root.join("tools")),
        ("loose/x.h", Language::C, root.clone()),
        ("crates/a/Cargo.toml", Language::Toml, root.clone()),
        ("crates/b/Cargo.toml", Language::Toml, root.join("crates/b")),
    ] {
        let out = lsp.diagnostics(json!({"path": path})).await;
        assert!(!out.is_error, "{path}: {}", out.content);
        assert!(
            lsp.servers()
                .iter()
                .any(|s| s.language == language && s.root == at),
            "{path} at {}: {:?}",
            at.display(),
            lsp.servers()
        );
    }
    assert_eq!(lsp.servers().len(), 6, "{:?}", lsp.servers());
}

#[tokio::test]
async fn the_yaml_server_is_told_to_use_no_schema_store_and_no_kubernetes_catalog() {
    // What yaml-language-server 2.0.0 asks for when it is initialized.
    let lsp = six_rig(
        json!({"server_requests": [{"method": "workspace/configuration", "params": {"items": [
            {"section": "yaml"}, {"section": "http"}, {"section": "[yaml]"}, {"section": "editor"}
        ]}}]}),
        8,
    )
    .await;
    lsp.put("deploy/values.yaml", "replicas: 1\n");
    let out = lsp.diagnostics(json!({"path": "deploy/values.yaml"})).await;
    assert!(!out.is_error, "{}", out.content);
    let answers = lsp.answers(1).await;
    let yaml = &answers[0]["result"][0];
    assert_eq!(yaml["schemaStore"]["enable"], false, "{answers:?}");
    assert_eq!(yaml["kubernetesCRDStore"]["enable"], false, "{answers:?}");
    assert_eq!(yaml["validate"], true, "{answers:?}");
    assert!(yaml.get("schemas").is_none(), "no schema is named: {yaml}");
    for other in 1..4 {
        assert!(answers[0]["result"][other].is_null(), "{answers:?}");
    }
}

#[tokio::test]
async fn the_json_server_fetches_only_file_schemas_and_the_client_has_none_to_give() {
    // A `$schema` of the web is asked of the client (`vscode/content`), which has none.
    let lsp = six_rig(
        json!({"server_requests": [
            {"method": "vscode/content", "params": ["https://json.schemastore.org/package.json"]}
        ]}),
        8,
    )
    .await;
    lsp.put(
        "package.json",
        "{\"$schema\": \"https://json.schemastore.org/package.json\"}\n",
    );
    let out = lsp.diagnostics(json!({"path": "package.json"})).await;
    assert!(!out.is_error, "{}", out.content);
    let options = lsp.init_options(0);
    assert_eq!(
        options["handledSchemaProtocols"],
        json!(["file"]),
        "{options}"
    );
    let answers = lsp.answers(1).await;
    assert_eq!(answers[0]["error"]["code"], -32601, "{answers:?}");
}

#[tokio::test]
async fn taplo_is_told_to_validate_by_no_schema() {
    let lsp = six_rig(
        json!({"server_requests": [{"method": "workspace/configuration", "params": {"items": [
            {"section": "evenBetterToml"}
        ]}}]}),
        8,
    )
    .await;
    lsp.put("Cargo.toml", "[package]\nname = \"x\"\n");
    let out = lsp.diagnostics(json!({"path": "Cargo.toml"})).await;
    assert!(!out.is_error, "{}", out.content);
    let answers = lsp.answers(1).await;
    assert_eq!(
        answers[0]["result"][0]["schema"]["enabled"], false,
        "{answers:?}"
    );
}

#[tokio::test]
async fn a_new_language_with_no_server_is_answered_as_not_installed() {
    let lsp = Lsp::build(Fixture::new().await, json!({}), |_| {
        ServerTable::production().with(
            Language::Yaml,
            ServerSpec {
                program: "/nonexistent/yaml-language-server".to_owned(),
                launch: "exec /nonexistent/yaml-language-server --stdio".to_owned(),
                env: Vec::new(),
                version_probe: false,
            },
        )
    })
    .await;
    lsp.put(".github/workflows/ci.yml", "on: push\n");
    let out = lsp
        .diagnostics(json!({"path": ".github/workflows/ci.yml"}))
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("No language server for YAML")
            && out
                .content
                .contains("`yaml-language-server` is not on PATH")
            && out.content.contains("the tool that reads the file"),
        "{}",
        out.content
    );
    assert!(lsp.servers().is_empty());
}

// ------------------------------------------------------------------------------- diagnostics

#[tokio::test]
async fn diagnostics_are_errors_and_warnings_only_sorted_by_line() {
    let lsp = Lsp::new(json!({"diagnostics": [
        diag(1, 1, 24, "mismatched types"),
        diag(2, 0, 7, "unused function"),
        diag(3, 0, 0, "a hint that is not shown"),
        diag(4, 1, 0, "neither is this one"),
    ]}))
    .await;
    rust_project(&lsp);
    let out = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(!out.is_error, "{}", out.content);
    let lines: Vec<&str> = out.content.lines().collect();
    assert_eq!(
        lines[0], "src/lib.rs:1:8: warning: unused function",
        "{}",
        out.content
    );
    assert_eq!(
        lines[1], "src/lib.rs:2:25: error: mismatched types",
        "{}",
        out.content
    );
    assert!(
        lines[2].starts_with("[end: 2 diagnostics; 1 error, 1 warning of adam-lsp-script"),
        "{}",
        out.content
    );
    assert!(!out.content.contains("hint"), "{}", out.content);
    // The server is held, for this run, slot, language and project root.
    let servers = lsp.servers();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].language, Language::Rust);
    assert!(
        servers[0]
            .root
            .ends_with(lsp.worktree().file_name().unwrap()),
        "{servers:?}"
    );
}

#[tokio::test]
async fn a_server_that_offers_pull_diagnostics_is_asked_for_them() {
    let lsp = Lsp::new(json!({"pull": true, "diagnostics": [diag(1, 0, 0, "pulled")]})).await;
    rust_project(&lsp);
    let out = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(
        out.content.contains("src/lib.rs:1:1: error: pulled"),
        "{}",
        out.content
    );
    assert_eq!(lsp.logged("textDocument/diagnostic").len(), 1);
    // A second call is asked again: it is a pull, not a wait for a publication.
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert_eq!(lsp.logged("textDocument/diagnostic").len(), 2);
}

#[tokio::test]
async fn a_server_that_publishes_nothing_for_a_file_is_not_waited_for_the_whole_wait_and_is_not_taken_for_a_clean_file()
 {
    let lsp = Lsp::new(json!({"silent_when_clean": true})).await;
    rust_project(&lsp);
    let started = Instant::now();
    let out = lsp
        .diagnostics(json!({"path": "src/lib.rs", "wait": 60}))
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content
            .contains("No diagnostics received for `src/lib.rs` within 3 s")
            && !out.content.contains("(no errors or warnings)"),
        "{}",
        out.content
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_server_that_publishes_an_empty_set_says_the_file_is_clean() {
    let lsp = Lsp::new(json!({})).await;
    rust_project(&lsp);
    let out = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(
        out.content.contains("(no errors or warnings)")
            && !out.content.contains("No diagnostics received"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn diagnostics_published_under_another_spelling_of_the_address_are_found() {
    let lsp =
        Lsp::new(json!({"uri_spelling": "lower_hex", "diagnostics": [diag(1, 0, 0, "found it")]}))
            .await;
    rust_project(&lsp);
    let path = lsp.worktree().display().to_string();
    assert!(
        path.contains(['.', '-', '_']),
        "the test needs a path with a byte the spelling changes: {path}"
    );
    let out = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(out.content.contains("found it"), "{}", out.content);
}

#[tokio::test]
async fn the_columns_of_a_diagnostic_are_characters_whatever_the_server_counts() {
    // Line 1 holds an accent (1 UTF-16 unit, 2 bytes) and an emoji (2 units, 4 bytes): `bad` is the
    // fourth character, at column 4 (1-based), 4 UTF-16 units from the start of the line.
    let utf16 = Lsp::new(json!({"diagnostics": [diag(1, 0, 4, "here")]})).await;
    rust_project(&utf16);
    utf16.put("src/lib.rs", "é😀 bad\n");
    let out = utf16.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(
        out.content.contains("src/lib.rs:1:4: error: here"),
        "{}",
        out.content
    );
    // In bytes the same place is column 8 of the server's, which is the same character.
    let utf8 =
        Lsp::new(json!({"position_encoding": "utf-8", "diagnostics": [diag(1, 0, 7, "here")]}))
            .await;
    rust_project(&utf8);
    utf8.put("src/lib.rs", "é😀 bad\n");
    let out = utf8.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(
        out.content.contains("src/lib.rs:1:4: error: here"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn a_secret_in_a_diagnostic_is_scrubbed_before_it_is_cut_or_shown() {
    let lsp = Lsp::new(
        json!({"diagnostics": [diag(1, 0, 0, &format!("bad token {GITHUB_TOKEN} here"))]}),
    )
    .await;
    rust_project(&lsp);
    let out = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(!out.content.contains(GITHUB_TOKEN), "{}", out.content);
    assert!(
        out.content
            .contains("[redacted: secret removed, do not reuse]"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn the_paths_of_this_machine_are_taken_out_of_what_a_server_says() {
    let lsp = Lsp::new(json!({})).await;
    rust_project(&lsp);
    // The server says the slot's own path in its message.
    let slot = lsp.worktree().display().to_string();
    lsp.script(&json!({"diagnostics": [diag(1, 0, 0, &format!("see {slot}/src/lib.rs"))]}));
    lsp.fx
        .env
        .commands
        .language_servers()
        .stop_run(&lsp.run(), "the test starts again")
        .await;
    let out = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(out.content.contains("see src/lib.rs"), "{}", out.content);
    assert!(!out.content.contains(&slot), "{}", out.content);
}

#[tokio::test]
async fn diagnostics_are_paged() {
    let many: Vec<Value> = (0..230)
        .map(|i| diag(1, i, 0, &format!("problem {i}")))
        .collect();
    let lsp = Lsp::new(json!({ "diagnostics": many })).await;
    rust_project(&lsp);
    let text: String = (0..230).map(|i| format!("fn f{i}() {{}}\n")).collect();
    lsp.put("src/lib.rs", &text);
    let first = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(!first.is_error, "{}", first.content);
    assert_eq!(first.content.lines().count(), 101, "{}", first.content);
    assert!(
        first
            .content
            .ends_with("[more: 130 diagnostics left; next page: offset=101]"),
        "{}",
        first.content
    );
    let last = lsp
        .diagnostics(json!({"path": "src/lib.rs", "offset": 201}))
        .await;
    assert!(
        last.content.contains("src/lib.rs:230:1"),
        "{}",
        last.content
    );
    assert!(
        last.content.contains("[end: 230 diagnostics"),
        "{}",
        last.content
    );
    let past = lsp
        .diagnostics(json!({"path": "src/lib.rs", "offset": 999}))
        .await;
    assert!(
        past.is_error && past.content.contains("past the end"),
        "{}",
        past.content
    );
}

// --------------------------------------------------------------------------------- the server

#[tokio::test]
async fn a_server_is_kept_between_calls_and_one_exists_per_project_root() {
    let lsp = Lsp::new(json!({"diagnostics": [diag(2, 0, 0, "w")]})).await;
    rust_project(&lsp);
    lsp.put("src/other.rs", "pub fn other() {}\n");
    lsp.put("tools/Cargo.toml", "[package]\nname = \"tools\"\n");
    lsp.put("tools/src/main.rs", "fn main() {}\n");
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    let pid = lsp.pid();
    // Another file of the same project: the same process, no new spawn.
    lsp.diagnostics(json!({"path": "src/other.rs"})).await;
    assert_eq!(lsp.pid(), pid);
    assert_eq!(lsp.logged("initialize").len(), 1);
    // A file under another `Cargo.toml` is another project, so another server.
    lsp.diagnostics(json!({"path": "tools/src/main.rs"})).await;
    let servers = lsp.servers();
    assert_eq!(servers.len(), 2, "{servers:?}");
    assert!(servers.iter().any(|s| s.pid == Some(pid)));
    assert_eq!(lsp.logged("initialize").len(), 2);
}

#[tokio::test]
async fn indexing_past_the_wait_gives_a_partial_result_and_the_next_call_finds_the_server_warm() {
    let lsp = Lsp::new(
        json!({"progress_ms": 3500, "diagnostics": [diag(1, 0, 0, "found while indexing")]}),
    )
    .await;
    rust_project(&lsp);
    let started = Instant::now();
    let first = lsp
        .diagnostics(json!({"path": "src/lib.rs", "wait": 1}))
        .await;
    assert!(!first.is_error, "{}", first.content);
    assert!(
        first.content.starts_with("[still indexing after ")
            // The time spent, then the bound the call asked for (ANR-1185).
            && first.content.contains(" s of at most 1 s (server up "),
        "{}",
        first.content
    );
    assert!(
        first.content.contains("the server is kept"),
        "{}",
        first.content
    );
    assert!(
        first.content.contains("found while indexing"),
        "{}",
        first.content
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    let pid = lsp.pid();
    // Indexing ends; the second call answers from the same process, with no line about indexing.
    let second = lsp
        .diagnostics(json!({"path": "src/lib.rs", "wait": 30}))
        .await;
    assert!(
        !second.content.contains("still indexing"),
        "{}",
        second.content
    );
    assert!(
        second.content.contains("found while indexing"),
        "{}",
        second.content
    );
    assert_eq!(lsp.pid(), pid, "no new spawn");
    assert_eq!(lsp.logged("initialize").len(), 1);
}

#[tokio::test]
async fn a_server_that_reports_its_status_is_not_ready_until_it_says_it_is_quiescent() {
    // Not a `$/progress` in sight: only `experimental/serverStatus`, as rust-analyzer says it.
    let lsp = Lsp::new(json!({"status_ms": 3000, "diagnostics": [diag(1, 0, 0, "early")]})).await;
    rust_project(&lsp);
    let first = lsp
        .diagnostics(json!({"path": "src/lib.rs", "wait": 1}))
        .await;
    assert!(
        first.content.starts_with("[still indexing after ")
            // The time spent, then the bound the call asked for (ANR-1185).
            && first.content.contains(" s of at most 1 s (server up "),
        "{}",
        first.content
    );
    let second = lsp
        .diagnostics(json!({"path": "src/lib.rs", "wait": 30}))
        .await;
    assert!(
        !second.content.contains("still indexing"),
        "{}",
        second.content
    );
    assert!(second.content.contains("early"), "{}", second.content);
}

#[tokio::test]
async fn a_server_that_crashes_mid_request_is_reported_with_its_scrubbed_stderr_and_started_again()
{
    let lsp = Lsp::new(json!({
        "crash_on_request": 1,
        "crash_stderr": format!("panic: boom with {GITHUB_TOKEN}"),
        "results": {"textDocument/definition": null},
    }))
    .await;
    rust_project(&lsp);
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first"}),
        )
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("stopped"), "{}", out.content);
    assert!(out.content.contains("panic: boom"), "{}", out.content);
    assert!(!out.content.contains(GITHUB_TOKEN), "{}", out.content);
    assert!(
        out.content.contains("The next call starts it again"),
        "{}",
        out.content
    );
    until("the dead server to be forgotten", || {
        lsp.servers().is_empty()
    })
    .await;
    // The next call starts a new one (the script no longer crashes).
    lsp.script(&json!({"results": {"textDocument/definition": null}}));
    let again = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first"}),
        )
        .await;
    assert!(!again.is_error, "{}", again.content);
    assert!(
        again.content.contains("(no definition found)"),
        "{}",
        again.content
    );
    assert_eq!(lsp.servers().len(), 1);
    assert_eq!(lsp.logged("initialize").len(), 2);
}

#[tokio::test]
async fn only_the_last_16_kib_of_a_servers_stderr_are_kept_and_shown_when_it_dies() {
    let lsp = Lsp::new(json!({
        "crash_on_request": 1,
        "crash_stderr": format!("{}{}", "a".repeat(40_000), "THE-END-OF-STDERR"),
    }))
    .await;
    rust_project(&lsp);
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first"}),
        )
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("THE-END-OF-STDERR"), "{}", out.content);
    assert!(out.content.len() < 6000, "{} bytes", out.content.len());
}

#[tokio::test]
async fn a_server_that_never_answers_is_bounded_by_wait_and_stopped() {
    let lsp = Lsp::new(json!({"never_answer": ["textDocument/definition"]})).await;
    rust_project(&lsp);
    let started = Instant::now();
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first", "wait": 1}),
        )
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("did not answer"), "{}", out.content);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    // It is stopped: no process of it is left, and the registry has none.
    assert!(lsp.servers().is_empty());
    let pid: u32 = std::fs::read_to_string(lsp.dir.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    until("the hung server to be gone", || !alive(pid)).await;
}

#[tokio::test]
async fn framing_that_is_not_framing_stops_the_server_and_the_tool_says_so() {
    let lsp = Lsp::new(json!({"bad_framing_on_request": 1})).await;
    rust_project(&lsp);
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first"}),
        )
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("malformed framing"), "{}", out.content);
    assert!(lsp.servers().is_empty());
    let pid: u32 = std::fs::read_to_string(lsp.dir.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    until("the server to be gone", || !alive(pid)).await;
}

#[tokio::test]
async fn a_message_over_16_mib_stops_the_server_before_it_is_read() {
    let lsp = Lsp::new(json!({"oversize_on_request": 1})).await;
    rust_project(&lsp);
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first"}),
        )
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("99999999") && out.content.contains("16777216"),
        "{}",
        out.content
    );
    assert!(lsp.servers().is_empty());
}

#[tokio::test]
async fn a_server_that_does_not_answer_initialize_does_not_start_and_nothing_is_left() {
    // The tool gives `initialize` 60 s; this asks the registry with a short one.
    let lsp = Lsp::new(json!({"initialize_never": true, "spawn_tree": true})).await;
    rust_project(&lsp);
    let env = &lsp.fx.env;
    let session = adam_workspace::LocalSession::shared();
    let spec = env.language_table.get(Language::Rust);
    let started = Instant::now();
    let want = adam_coder::tools::lsp::server::Want {
        key: adam_coder::tools::lsp::server::Key {
            run: lsp.run(),
            slot: "remote".to_owned(),
            language: Language::Rust,
            root: lsp.worktree(),
            mode: adam_coder::tools::lsp::Mode::Full,
        },
        spec,
        session,
        mode: adam_coder::tools::lsp::Mode::Full,
        idle: Duration::from_secs(600),
        max: 4,
        kill_wait: Duration::from_secs(5),
        hide: Vec::new(),
        init_timeout: Duration::from_secs(1),
        redactor: adam_coder::Redactor::default(),
        governor: None,
    };
    let failed = env
        .commands
        .language_servers()
        .get_or_start(want)
        .await
        .err()
        .expect("it does not start");
    assert!(
        matches!(&failed, adam_coder::tools::lsp::server::StartError::Failed(why) if why.contains("did not answer `initialize`")),
        "{failed:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(lsp.servers().is_empty());
    lsp.tree_is_gone("what the unstarted server left").await;
}

// ------------------------------------------------------------------------- definition, references

#[tokio::test]
async fn a_definition_is_found_by_line_and_symbol_and_shown_with_its_source() {
    let lsp = Lsp::new(json!({"results": {"textDocument/definition": [
        {"uri": "$FILE", "range": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 12}}}
    ]}}))
    .await;
    rust_project(&lsp);
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 2, "symbol": "second"}),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("src/lib.rs:1:8: pub fn first() {}"),
        "{}",
        out.content
    );
    // The server was asked at line 2 (index 1), column 7 of `pub fn second`.
    let asked = lsp.logged("textDocument/definition");
    assert_eq!(
        asked[0]["params"]["position"],
        json!({"line": 1, "character": 7})
    );
}

#[tokio::test]
async fn the_column_sent_is_counted_in_utf16_by_default_and_in_bytes_when_the_server_chose_utf8() {
    let line = "é😀 value\n";
    let utf16 = Lsp::new(json!({})).await;
    rust_project(&utf16);
    utf16.put("src/lib.rs", line);
    utf16
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "value"}),
        )
        .await;
    // é is 1 unit, the emoji 2, the space 1: 4.
    assert_eq!(
        utf16.logged("textDocument/definition")[0]["params"]["position"],
        json!({"line": 0, "character": 4})
    );
    let utf8 = Lsp::new(json!({"position_encoding": "utf-8"})).await;
    rust_project(&utf8);
    utf8.put("src/lib.rs", line);
    utf8.call(
        "find_definition",
        json!({"path": "src/lib.rs", "line": 1, "symbol": "value"}),
    )
    .await;
    // é is 2 bytes, the emoji 4, the space 1: 7.
    assert_eq!(
        utf8.logged("textDocument/definition")[0]["params"]["position"],
        json!({"line": 0, "character": 7})
    );
    // The client offered both encodings, utf-8 first.
    assert_eq!(
        utf8.logged("initialize")[0]["params"]["capabilities"]["general"]["positionEncodings"],
        json!(["utf-8", "utf-16"])
    );
}

#[tokio::test]
async fn a_symbol_that_is_not_on_the_line_is_answered_with_the_line_as_it_is() {
    let lsp = Lsp::new(json!({})).await;
    rust_project(&lsp);
    let missing = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "second"}),
        )
        .await;
    assert!(missing.is_error, "{}", missing.content);
    assert!(
        missing.content.contains("`second` is not on line 1")
            && missing.content.contains("pub fn first() {}"),
        "{}",
        missing.content
    );
    let fewer = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 2, "symbol": "second", "occurrence": 2}),
        )
        .await;
    assert!(
        fewer.is_error && fewer.content.contains("1 time, not 2"),
        "{}",
        fewer.content
    );
    let past = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 99, "symbol": "second"}),
        )
        .await;
    assert!(
        past.is_error && past.content.contains("past the end"),
        "{}",
        past.content
    );
    // None of these asked the server anything.
    assert!(lsp.logged("textDocument/definition").is_empty());
}

#[tokio::test]
async fn a_location_outside_the_slot_is_labelled_and_no_absolute_path_is_shown() {
    let lsp = Lsp::new(json!({"results": {"textDocument/definition": [
        {"uri": "file:///usr/lib/rustlib/src/rust/library/core/src/option.rs",
         "range": {"start": {"line": 41, "character": 0}, "end": {"line": 41, "character": 4}}},
        {"uri": "$FILE", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}},
        {"uri": "https://example.com/x.rs",
         "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}}
    ]}}))
    .await;
    rust_project(&lsp);
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first"}),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content
            .contains("(outside the workspace) core/src/option.rs:42"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("src/lib.rs:1:1: pub fn first() {}"),
        "{}",
        out.content
    );
    for forbidden in ["/usr/lib", &lsp.fx.root.display().to_string(), "file://"] {
        assert!(
            !out.content.contains(forbidden),
            "{forbidden}: {}",
            out.content
        );
    }
}

#[tokio::test]
async fn a_location_in_another_slot_of_the_workspace_is_named_by_its_slot() {
    let fx = Fixture::new().await;
    let other = fx.extra_remote(
        "other",
        &[
            ("Cargo.toml", "[package]\n"),
            ("src/lib.rs", "pub fn there() {}\n"),
        ],
    );
    let lsp = Lsp::over(fx, json!({})).await;
    rust_project(&lsp);
    let url = other.to_string_lossy().into_owned();
    let run = lsp.run();
    let mut notes = lsp.fx.env.notes.load(&run).await.unwrap();
    notes.name_repos(named_in(
        &without_untrusted(&url),
        &lsp.fx.env.settings.default_repo_host,
    ));
    lsp.fx.env.notes.save(&run, &notes).await.unwrap();
    let made = lsp
        .call(
            "prepare_workspace",
            json!({"repo_url": url, "base_branch": "main"}),
        )
        .await;
    assert!(!made.is_error, "{}", made.content);
    let there = common::slot_dir(&lsp.fx.root, &run)
        .parent()
        .unwrap()
        .join("other")
        .join("src/lib.rs");
    assert!(there.is_file(), "{there:?}");
    lsp.script(&json!({"results": {"textDocument/definition": [
        {"uri": format!("file://{}", there.display()),
         "range": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 12}}}
    ]}}));
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first", "repo": "remote"}),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content
            .contains("other/src/lib.rs:1:8: pub fn there() {}"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn a_hundred_thousand_references_are_cut_at_2000_and_paged_under_the_page_size() {
    let lsp =
        Lsp::new(json!({"references_count": 100_000, "references_uri": "file:///a/b.rs"})).await;
    rust_project(&lsp);
    let first = lsp
        .call(
            "find_references",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first"}),
        )
        .await;
    assert!(!first.is_error, "{}", first.content);
    assert_eq!(first.content.lines().count(), 101, "{}", first.content);
    assert!(
        first
            .content
            .ends_with("[more: 1900 references left; next page: offset=101]"),
        "{}",
        first.content.lines().last().unwrap()
    );
    assert!(first.content.len() < 50 * 1024, "{}", first.content.len());
    let last = lsp
        .call(
            "find_references",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first", "offset": 1901}),
        )
        .await;
    assert!(
        last.content.contains(
            "[end: 2000 references; the server returned 100000; only the first 2000 are listed]"
        ),
        "{}",
        last.content
    );
}

#[tokio::test]
async fn references_are_sorted_and_the_declaration_is_asked_for_when_wanted() {
    let lsp = Lsp::new(json!({"references_count": 3})).await;
    rust_project(&lsp);
    let out = lsp
        .call(
            "find_references",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first", "include_declaration": false}),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        lsp.logged("textDocument/references")[0]["params"]["context"],
        json!({"includeDeclaration": false})
    );
    let lines: Vec<&str> = out.content.lines().collect();
    assert!(
        lines[0].starts_with("src/lib.rs:1:1: pub fn first"),
        "{}",
        out.content
    );
    assert!(lines[1].starts_with("src/lib.rs:2:1: "), "{}", out.content);
    assert!(
        lines[2].starts_with("src/lib.rs:3:1: ") || lines[2].starts_with("[end"),
        "{}",
        out.content
    );
}

// ------------------------------------------------------------------------------------- sync

#[tokio::test]
async fn edit_file_then_diagnostics_shows_that_the_server_holds_the_new_text() {
    let lsp = Lsp::new(json!({"echo": true, "diagnostics": [diag(1, 0, 0, "seen")]})).await;
    lsp.put("Cargo.toml", "[package]\n");
    lsp.put("src/lib.rs", "fn old_text() {}\n");
    let before = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(
        before.content.contains("held: fn old_text() {}"),
        "{}",
        before.content
    );
    let edited = lsp
        .call(
            "edit_file",
            json!({"path": "src/lib.rs", "old": "old_text", "new": "new_text"}),
        )
        .await;
    assert!(!edited.is_error, "{}", edited.content);
    let after = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(
        after.content.contains("held: fn new_text() {}"),
        "{}",
        after.content
    );
    assert!(!after.content.contains("old_text"), "{}", after.content);
    // One `didOpen`, then a `didChange` with the whole text and the next version.
    assert_eq!(lsp.logged("textDocument/didOpen").len(), 1);
    let changes = lsp.logged("textDocument/didChange");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["params"]["textDocument"]["version"], 2);
    assert_eq!(
        changes[0]["params"]["contentChanges"][0]["text"],
        "fn new_text() {}\n"
    );
}

#[tokio::test]
async fn files_changed_beside_the_write_tools_reach_the_server_as_watched_files() {
    let lsp = Lsp::new(json!({})).await;
    rust_project(&lsp);
    lsp.put("src/kept.rs", "pub fn kept() {}\n");
    lsp.put("src/gone.rs", "pub fn gone() {}\n");
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(
        lsp.logged("workspace/didChangeWatchedFiles").is_empty(),
        "the first call is the baseline"
    );
    // What `run`, a build or the shell does: no write tool is told. The walk of the root is made at
    // most once per `WATCHED_TTL`, so the change is reported by the call after the interval.
    lsp.put("src/new.rs", "pub fn new() {}\n");
    std::fs::remove_file(lsp.worktree().join("src/gone.rs")).unwrap();
    lsp.put("src/kept.rs", "pub fn kept() { /* a longer body */ }\n");
    tokio::time::sleep(adam_coder::tools::lsp::server::WATCHED_TTL + Duration::from_millis(200))
        .await;
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    // The notification is written before the call answers; the script logs it when it reads it.
    until("the server to read the notification", || {
        !lsp.logged("workspace/didChangeWatchedFiles").is_empty()
    })
    .await;
    let watched = lsp.logged("workspace/didChangeWatchedFiles");
    assert_eq!(watched.len(), 1);
    let changes = watched[0]["params"]["changes"].as_array().unwrap();
    let kind = |name: &str| {
        changes
            .iter()
            .find(|c| c["uri"].as_str().unwrap().ends_with(name))
            .map(|c| c["type"].as_u64().unwrap())
    };
    assert_eq!(kind("src/new.rs"), Some(1));
    assert_eq!(kind("src/kept.rs"), Some(2));
    assert_eq!(kind("src/gone.rs"), Some(3));
    assert_eq!(changes.len(), 3, "{changes:?}");
}

#[tokio::test]
async fn the_root_is_walked_once_per_interval_and_a_change_is_reported_on_the_next_walk() {
    let lsp = Lsp::new(json!({})).await;
    rust_project(&lsp);
    // The first call walks the root: it is the baseline the later ones are compared with.
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(
        lsp.logged("workspace/didChangeWatchedFiles").is_empty(),
        "the first call is the baseline"
    );
    lsp.put("src/later.rs", "pub fn later() {}\n");
    for _ in 0..4 {
        lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    }
    assert!(
        lsp.logged("workspace/didChangeWatchedFiles").is_empty(),
        "four calls inside the interval reuse the walk"
    );
    // Past the interval the next call walks: the file that appeared meanwhile is reported once.
    tokio::time::sleep(adam_coder::tools::lsp::server::WATCHED_TTL + Duration::from_millis(200))
        .await;
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    until("the walk to report the file", || {
        !lsp.logged("workspace/didChangeWatchedFiles").is_empty()
    })
    .await;
    let watched = lsp.logged("workspace/didChangeWatchedFiles");
    assert_eq!(watched.len(), 1);
    let changes = watched[0]["params"]["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0]["type"], 1, "created");
    assert!(
        changes[0]["uri"]
            .as_str()
            .unwrap()
            .ends_with("src/later.rs")
    );
}

#[tokio::test]
async fn a_write_of_a_file_tool_is_walked_at_the_next_call_inside_the_interval() {
    let lsp = Lsp::new(json!({})).await;
    rust_project(&lsp);
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(lsp.logged("workspace/didChangeWatchedFiles").is_empty());
    // The coder's own tool wrote a file the server does not hold open: the next call must walk,
    // not wait for the interval.
    let written = lsp
        .call(
            "write_file",
            json!({"path": "src/other.rs", "content": "pub fn other() {}\n"}),
        )
        .await;
    assert!(!written.is_error, "{}", written.content);
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    until("the write to be reported", || {
        !lsp.logged("workspace/didChangeWatchedFiles").is_empty()
    })
    .await;
    let watched = lsp.logged("workspace/didChangeWatchedFiles");
    assert_eq!(watched.len(), 1);
    let changes = watched[0]["params"]["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0]["type"], 1);
    assert!(
        changes[0]["uri"]
            .as_str()
            .unwrap()
            .ends_with("src/other.rs")
    );
}

// ------------------------------------------------------------------------------- never writes

#[tokio::test]
async fn a_server_that_asks_to_apply_an_edit_is_refused_and_the_file_is_unchanged() {
    let lsp = Lsp::new(json!({
        "diagnostics": [diag(1, 0, 0, "x")],
        "server_requests": [
            {"method": "workspace/applyEdit", "params": {"edit": {"changes": {}}}},
            {"method": "workspace/configuration", "params": {"items": [{"section": "rust-analyzer"}, {"section": "nothing"}]}},
            {"method": "no/such/method", "params": {}}
        ],
    }))
    .await;
    rust_project(&lsp);
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    until("the answers to be logged", || {
        lsp.log()
            .iter()
            .filter(|m| m.get("answer_to").is_some())
            .count()
            == 3
    })
    .await;
    let answers: Vec<Value> = lsp
        .log()
        .into_iter()
        .filter(|m| m.get("answer_to").is_some())
        .collect();
    let by_id = |id: u64| {
        answers
            .iter()
            .find(|a| a["answer_to"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by_id(9001)["result"]["applied"], false, "{answers:?}");
    let configuration = &by_id(9002)["result"];
    assert_eq!(configuration[0]["cargo"]["targetDir"], true, "{answers:?}");
    assert!(configuration[1].is_null());
    assert_eq!(by_id(9003)["error"]["code"], -32601);
    // The capability is never declared, and the file is as it was.
    let caps = &lsp.logged("initialize")[0]["params"]["capabilities"];
    assert!(caps["workspace"].get("applyEdit").is_none(), "{caps}");
    assert_eq!(
        std::fs::read_to_string(lsp.worktree().join("src/lib.rs")).unwrap(),
        LIB
    );
}

// ------------------------------------------------------------------------------- references

mod references {
    use super::*;

    fn def(name: &str, url: &str) -> adam::agent_fs::ReferenceDef {
        adam::agent_fs::ReferenceDef {
            name: name.into(),
            url: url.into(),
            branch: None,
            description: "a library".into(),
        }
    }

    /// A rig whose deployment declares the reference `lib` (a Rust project), made by a first read.
    pub(super) async fn with_reference(script: Value) -> Lsp {
        let fx = Fixture::new().await;
        let remote = fx.extra_remote(
            "lib",
            &[
                ("Cargo.toml", "[package]\nname = \"lib\"\n"),
                ("src/lib.rs", LIB),
            ],
        );
        let fx = fx.referencing(vec![def("lib", &remote.to_string_lossy())]);
        let lsp = Lsp::over(fx, script).await;
        let first = lsp.call("list_files", json!({"repo": "lib"})).await;
        assert!(!first.is_error, "{}", first.content);
        lsp
    }

    #[tokio::test]
    async fn a_reference_is_served_with_settings_that_build_and_write_nothing() {
        let lsp = with_reference(json!({"diagnostics": [diag(1, 1, 0, "in the reference")]})).await;
        let out = lsp
            .diagnostics(json!({"path": "src/lib.rs", "repo": "lib"}))
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("in the reference"), "{}", out.content);
        let options = &lsp.logged("initialize")[0]["params"]["initializationOptions"];
        assert_eq!(
            options["cargo"]["buildScripts"]["enable"], false,
            "{options}"
        );
        assert_eq!(options["procMacro"]["enable"], false);
        assert_eq!(options["checkOnSave"], false);
        // A writable slot's server keeps them on (it has no such options).
        let writable = Lsp::new(json!({})).await;
        rust_project(&writable);
        writable.diagnostics(json!({"path": "src/lib.rs"})).await;
        let options = &writable.logged("initialize")[0]["params"]["initializationOptions"];
        assert!(options["cargo"].get("buildScripts").is_none(), "{options}");
    }

    #[tokio::test]
    async fn a_server_that_writes_into_a_reference_is_detected_restored_and_stopped() {
        let lsp = with_reference(json!({
            "write_on_request": {"n": 1, "path": "src/generated.rs", "content": "// by the server\n"},
            "results": {"textDocument/definition": null},
        }))
        .await;
        let out = lsp
            .call(
                "find_definition",
                json!({"path": "src/lib.rs", "line": 1, "symbol": "first", "repo": "lib"}),
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("is a reference") && out.content.contains("as they were"),
            "{}",
            out.content
        );
        let slot = lsp
            .fx
            .root
            .join("workspaces")
            .join(lsp.run())
            .join("lib")
            .join("src/generated.rs");
        assert!(!slot.exists(), "the file the server wrote is gone");
        assert!(lsp.servers().is_empty(), "the server is stopped");
    }
}

// ------------------------------------------------------------------------------ stop points

fn script_with_tree() -> Value {
    json!({"spawn_tree": true, "diagnostics": [diag(1, 0, 0, "x")]})
}

#[tokio::test]
async fn an_idle_server_stops_and_leaves_no_process() {
    let lsp = Lsp::with(script_with_tree(), |s| {
        s.language_server_idle = Duration::from_secs(1);
    })
    .await;
    rust_project(&lsp);
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    let pid = lsp.pid();
    until("the idle server to be stopped", || lsp.servers().is_empty()).await;
    lsp.tree_is_gone("what the idle server left").await;
    assert!(!alive(pid));
}

#[tokio::test]
async fn stopping_a_run_leaves_no_process_of_its_servers_not_even_a_grandchild() {
    let lsp = Lsp::new(script_with_tree()).await;
    rust_project(&lsp);
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    // The grandchild is written by the child a moment after the server starts.
    let pids = lsp.tree().await;
    assert!(pids.iter().all(|p| alive(*p)), "{pids:?}");
    lsp.fx
        .env
        .commands
        .stop_run(&lsp.run(), "the run parked")
        .await;
    assert!(lsp.servers().is_empty());
    lsp.tree_is_gone("what the stop left").await;
}

/// The run's image changing stops what runs in the environment it leaves: the tool stops the
/// run's commands (and so its language servers) before the pod is made again (ADR 0052).
#[tokio::test]
async fn changing_the_runs_image_stops_the_servers_of_the_run() {
    use adam_coder::tools::environment::use_repository_environment;
    // A fake environment that, like a run pod, has an image of the run's to set (`off` clears it).
    let fake = common::FakeEnvironment::in_run_pods(None, &["ghcr.io/acme/"]);
    let lsp = Lsp::over(Fixture::new().await.using(fake), script_with_tree()).await;
    rust_project(&lsp);
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    let pids = lsp.tree().await;
    assert!(pids.iter().all(|p| alive(*p)), "{pids:?}");
    let out = use_repository_environment(&lsp.fx.env, &lsp.ctx, None, None, true)
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(lsp.servers().is_empty());
    lsp.tree_is_gone("what the image change left").await;
    // The next call asks for the program again, and starts a new server.
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert_eq!(lsp.servers().len(), 1);
}

#[tokio::test]
async fn the_worker_stopping_stops_every_server() {
    let lsp = Lsp::new(script_with_tree()).await;
    rust_project(&lsp);
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    lsp.tree().await;
    lsp.fx.env.commands.stop_all().await;
    assert!(lsp.servers().is_empty());
    lsp.tree_is_gone("what the worker's stop left").await;
}

#[tokio::test]
async fn dropping_the_registry_kills_what_it_holds() {
    let lsp = Lsp::new(script_with_tree()).await;
    rust_project(&lsp);
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    let pids = lsp.tree().await;
    let dir = lsp.dir.path().to_path_buf();
    drop(lsp);
    until("the dropped registry's servers", || {
        pids.iter().all(|p| !alive(*p))
    })
    .await;
    drop(dir);
}

/// A coder on `fx` over a store the test holds, with `script` for the model.
struct Driven {
    rt: Runtime,
    store: DynStore,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<tokio::task::JoinHandle<Result<(), adam_runtime::RuntimeError>>>,
}

impl Driven {
    fn new(env: Arc<ToolEnv>, script: impl FnOnce(&MockModel)) -> Self {
        let mock = Arc::new(MockModel::new());
        script(&mock);
        let model: DynModel = mock;
        let agent = CoderAgent::new(model, "test-model", env);
        let store: DynStore = Arc::new(MemoryStore::new());
        let rt = Runtime::builder(store.clone())
            .agent(agent)
            .poll_interval(Duration::from_millis(20))
            .build();
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        let worker = {
            let rt = rt.clone();
            tokio::spawn(async move {
                rt.run_worker(async {
                    let _ = rx.await;
                })
                .await
            })
        };
        Self {
            rt,
            store,
            stop: Some(stop),
            worker: Some(worker),
        }
    }

    async fn start(&self, text: &str) -> RunId {
        self.rt
            .start(
                AGENT_NAME,
                Inbound::new("message", json!({ "text": text })),
                None,
            )
            .await
            .unwrap()
    }

    async fn settle(&self, run: RunId) -> RunStatus {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let view = self.rt.view(run).await.unwrap().expect("run exists");
            match view.status {
                RunStatus::Done | RunStatus::Failed => return view.status,
                RunStatus::Parked if view.waiting => return view.status,
                _ => {}
            }
            assert!(Instant::now() < deadline, "timed out: {view:#?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
    }
}

/// The model's script: prepare (the file is on `main`), ask the server about it.
fn script_calls(url: &str, m: &MockModel) {
    m.push_tool_calls(vec![call(
        "c1",
        "prepare_workspace",
        json!({"repo_url": url, "base_branch": "main"}),
    )]);
    m.push_tool_calls(vec![call(
        "c3",
        "file_diagnostics",
        json!({"path": "src/lib.rs"}),
    )]);
}

async fn driven_rig(script: Value) -> (Fixture, PathBuf, tempfile::TempDir) {
    let mut fx = Fixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    write_script(dir.path(), &script);
    Arc::get_mut(&mut fx.env).unwrap().language_table = table_for(&dir.path().join("script.json"));
    fx.commit_to_main("src/lib.rs", LIB);
    let pid = dir.path().join("pid");
    (fx, pid, dir)
}

#[tokio::test]
async fn a_finished_root_run_stops_its_servers() {
    let (fx, _pid, dir) = driven_rig(script_with_tree()).await;
    let url = fx.remote_url();
    let driven = Driven::new(fx.env.clone(), |m| {
        script_calls(&url, m);
        m.push_text("Looked at it.");
    });
    let run = driven.start(&format!("look at {url}")).await;
    assert_eq!(driven.settle(run).await, RunStatus::Done);
    assert!(
        fx.env
            .commands
            .language_servers()
            .running(&run.to_string())
            .is_empty(),
        "the run is over"
    );
    until("the pids", || dir.path().join("pid.grandchild").exists()).await;
    let pids = tree_of(dir.path());
    until("what the finished run left", || {
        pids.iter().all(|p| !alive(*p))
    })
    .await;
    driven.finish().await;
}

#[tokio::test]
async fn a_park_on_the_person_stops_the_servers() {
    let (fx, _pid, dir) = driven_rig(script_with_tree()).await;
    let url = fx.remote_url();
    let driven = Driven::new(fx.env.clone(), |m| {
        script_calls(&url, m);
        m.push_tool_calls(vec![call(
            "c4",
            "ask_user",
            json!({"question": "Shall I go on?"}),
        )]);
    });
    let run = driven.start(&format!("look at {url}")).await;
    assert_eq!(driven.settle(run).await, RunStatus::Parked);
    assert!(
        fx.env
            .commands
            .language_servers()
            .running(&run.to_string())
            .is_empty()
    );
    let pids = tree_of(dir.path());
    until("what the park left", || pids.iter().all(|p| !alive(*p))).await;
    driven.finish().await;
}

#[tokio::test]
async fn a_cancelled_run_is_reached_by_the_sweep() {
    let (fx, _pid, dir) = driven_rig(script_with_tree()).await;
    let url = fx.remote_url();
    let driven = Driven::new(fx.env.clone(), |m| {
        script_calls(&url, m);
        // The run is busy with a long call when it is cancelled.
        m.push_tool_calls(vec![call(
            "c4",
            "run_command",
            json!({"command": "sleep 30"}),
        )]);
    });
    let run = driven.start(&format!("look at {url}")).await;
    let run_id = run.to_string();
    until("the server to run", || {
        !fx.env
            .commands
            .language_servers()
            .running(&run_id)
            .is_empty()
    })
    .await;
    driven
        .rt
        .cancel(run, "cancelled by the test")
        .await
        .unwrap();
    assert_eq!(driven.settle(run).await, RunStatus::Failed);
    // The agent's step was never called: the sweep reads the run's standing and stops the server.
    until("the pids", || dir.path().join("pid.grandchild").exists()).await;
    let pids = tree_of(dir.path());
    assert!(
        pids.iter().all(|p| alive(*p)),
        "nothing has looked at the run yet"
    );
    let stopped = fx.env.commands.sweep(driven.store.as_ref()).await;
    assert_eq!(stopped, 1);
    until("what the cancelled run left", || {
        pids.iter().all(|p| !alive(*p))
    })
    .await;
    driven.finish().await;
}

// ------------------------------------------------------------------------------- the bound

#[tokio::test]
async fn at_the_bound_the_least_recently_used_idle_server_is_stopped_for_the_next() {
    let lsp = Lsp::with(json!({}), |s| s.language_servers_max = 1).await;
    rust_project(&lsp);
    lsp.put("tools/Cargo.toml", "[package]\nname = \"tools\"\n");
    lsp.put("tools/src/main.rs", "fn main() {}\n");
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    let first = lsp.pid();
    lsp.diagnostics(json!({"path": "tools/src/main.rs"})).await;
    let second = lsp.pid();
    assert_ne!(first, second);
    until("the first server to be gone", || !alive(first)).await;
    assert_eq!(lsp.servers().len(), 1);
}

#[tokio::test]
async fn at_the_bound_with_every_server_working_the_tool_says_so() {
    let lsp = Arc::new(
        Lsp::with(json!({"never_answer": ["textDocument/definition"]}), |s| {
            s.language_servers_max = 1
        })
        .await,
    );
    rust_project(&lsp);
    lsp.put("tools/Cargo.toml", "[package]\nname = \"tools\"\n");
    lsp.put("tools/src/main.rs", "fn main() {}\n");
    // The first call holds its server for the 4 s it waits for an answer that never comes.
    let working = {
        let lsp = lsp.clone();
        tokio::spawn(async move {
            lsp.call(
                "find_definition",
                json!({"path": "src/lib.rs", "line": 1, "symbol": "first", "wait": 4}),
            )
            .await
        })
    };
    until("the server to be working", || !lsp.servers().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let refused = lsp.diagnostics(json!({"path": "tools/src/main.rs"})).await;
    assert!(refused.is_error, "{}", refused.content);
    assert!(
        refused.content.contains("holds 1 language servers")
            && refused.content.contains("every one is working"),
        "{}",
        refused.content
    );
    let first = working.await.unwrap();
    assert!(first.is_error, "{}", first.content);
}

// ------------------------------------------------------------------------------ the settings

#[tokio::test]
async fn with_the_servers_off_the_three_tools_are_not_in_the_set() {
    let lsp = Lsp::with(json!({}), |s| s.language_servers = false).await;
    rust_project(&lsp);
    let tools = coder_tools(&lsp.fx.env);
    assert!(tools.get("file_diagnostics").is_none());
    assert!(tools.get("find_definition").is_none());
    assert!(tools.get("find_references").is_none());
}

// ====================================================================== the review round (2026-11-17)

use adam_a2a::AccessTier;
use adam_coder::tools::lsp::Mode;
use adam_coder::tools::lsp::server::{Key, Probe, StartError, Want};
use adam_workspace::{EnvDescription, EnvError, EnvSession, ExecId, ExecSpec, PreparedCommand};

impl Lsp {
    /// The access tier the run's notes say.
    async fn set_tier(&self, tier: AccessTier) {
        let run = self.run();
        let mut notes = self.fx.env.notes.load(&run).await.unwrap();
        notes.tier = Some(tier);
        self.fx.env.notes.save(&run, &notes).await.unwrap();
    }

    fn init_options(&self, n: usize) -> Value {
        self.logged("initialize")[n]["params"]["initializationOptions"].clone()
    }
}

/// A session that sees what is started and killed through it.
struct Recording {
    inner: Arc<dyn EnvSession>,
    kill_delay: Duration,
    summary: Option<&'static str>,
    events: std::sync::Mutex<Vec<&'static str>>,
    kills: std::sync::Mutex<Vec<ExecId>>,
}

impl Recording {
    fn new(kill_delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            inner: adam_workspace::LocalSession::shared(),
            kill_delay,
            summary: None,
            events: std::sync::Mutex::default(),
            kills: std::sync::Mutex::default(),
        })
    }

    /// A session that says it is `summary`: another environment than the plain one.
    fn described_as(summary: &'static str) -> Arc<Self> {
        Arc::new(Self {
            inner: adam_workspace::LocalSession::shared(),
            kill_delay: Duration::ZERO,
            summary: Some(summary),
            events: std::sync::Mutex::default(),
            kills: std::sync::Mutex::default(),
        })
    }

    fn events(&self) -> Vec<&'static str> {
        self.events.lock().unwrap().clone()
    }

    fn kills(&self) -> Vec<ExecId> {
        self.kills.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl EnvSession for Recording {
    fn describe(&self) -> EnvDescription {
        let mut described = self.inner.describe();
        if let Some(summary) = self.summary {
            summary.clone_into(&mut described.summary);
        }
        described
    }

    fn prepare(&self, spec: &ExecSpec) -> Result<PreparedCommand, EnvError> {
        self.events.lock().unwrap().push("prepare");
        self.inner.prepare(spec)
    }

    async fn kill(&self, exec: &ExecId) {
        tokio::time::sleep(self.kill_delay).await;
        self.kills.lock().unwrap().push(exec.clone());
        self.events.lock().unwrap().push("kill-done");
        self.inner.kill(exec).await;
    }
}

/// What the registry is asked for a server of the rig's script at `root`.
fn want<'a>(
    lsp: &'a Lsp,
    session: Arc<dyn EnvSession>,
    root: PathBuf,
    max: usize,
    init_timeout: Duration,
) -> Want<'a> {
    Want {
        key: Key {
            run: lsp.run(),
            slot: "w".to_owned(),
            language: Language::Rust,
            root,
            mode: Mode::Full,
        },
        spec: lsp.fx.env.language_table.get(Language::Rust),
        session,
        mode: Mode::Full,
        idle: Duration::from_secs(600),
        max,
        kill_wait: Duration::from_secs(5),
        hide: Vec::new(),
        init_timeout,
        redactor: adam_coder::Redactor::default(),
        governor: None,
    }
}

// ----------------------------------------------------------------------- 1: the modes

#[tokio::test]
async fn at_the_plan_tier_a_server_that_writes_into_a_writable_slot_is_restored_and_stopped() {
    let lsp = Lsp::new(json!({
        "write_on_request": {"n": 1, "path": "src/generated.rs", "content": "// by the server\n"},
        "results": {"textDocument/definition": null},
    }))
    .await;
    rust_project(&lsp);
    lsp.set_tier(AccessTier::Plan).await;
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first"}),
        )
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("`plan` tier") && out.content.contains("as they were"),
        "{}",
        out.content
    );
    assert!(
        !lsp.worktree().join("src/generated.rs").exists(),
        "the file the server wrote is gone"
    );
    assert!(lsp.servers().is_empty(), "the server is stopped");
}

#[tokio::test]
async fn the_mode_of_a_server_follows_the_tier_safe_at_plan_and_full_above_it() {
    let lsp = Lsp::new(json!({
        "write_on_request": {"n": 1, "path": "src/generated.rs", "content": "// by the server\n"},
        "results": {"textDocument/definition": null},
    }))
    .await;
    rust_project(&lsp);
    // At plan the slot is read only for it: the reference settings.
    lsp.set_tier(AccessTier::Plan).await;
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    let safe = lsp.init_options(0);
    assert_eq!(safe["cargo"]["buildScripts"]["enable"], false, "{safe}");
    assert_eq!(safe["procMacro"]["enable"], false);
    // Above it, in a slot the run may write, the project's own build runs, as run_checks runs it,
    // and the slot is not held to what it was.
    lsp.set_tier(AccessTier::Edits).await;
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first"}),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        lsp.logged("initialize").len(),
        2,
        "a server of its own mode"
    );
    let full = lsp.init_options(1);
    assert!(full["cargo"].get("buildScripts").is_none(), "{full}");
    assert!(lsp.worktree().join("src/generated.rs").exists());
}

// ------------------------------------------------------------- 2: a dropped call, a whole frame

#[tokio::test]
async fn a_call_dropped_in_the_middle_of_a_large_file_leaves_the_server_usable() {
    let lsp = Lsp::new(json!({
        "stall_after_initialized_ms": 4000,
        "diagnostics": [diag(1, 0, 0, "still here")],
    }))
    .await;
    rust_project(&lsp);
    lsp.put(
        "src/big.rs",
        &"// filler of a large file, line by line\n".repeat(95_000),
    );
    // The server reads nothing for 4 s: the `didOpen` of 3.6 MiB is stuck in the pipe when the call
    // is dropped (a time limit, a cancel).
    let dropped = tokio::time::timeout(
        Duration::from_millis(1500),
        lsp.diagnostics(json!({"path": "src/big.rs", "wait": 5})),
    )
    .await;
    assert!(dropped.is_err(), "the call was still writing");
    let pid = lsp.pid();
    let out = lsp
        .diagnostics(json!({"path": "src/lib.rs", "wait": 20}))
        .await;
    assert!(
        out.content.contains("src/lib.rs:1:1: error: still here"),
        "{}",
        out.content
    );
    assert_eq!(lsp.pid(), pid, "the same server answered");
}

// ------------------------------------------------------------------- 3: the gate and `busy`

#[tokio::test]
async fn a_call_dropped_while_waiting_for_the_gate_does_not_keep_the_server_busy() {
    let lsp = Lsp::new(json!({})).await;
    rust_project(&lsp);
    lsp.put("tools/Cargo.toml", "[package]\nname = \"tools\"\n");
    let servers = lsp.fx.env.commands.language_servers();
    let session = adam_workspace::LocalSession::shared();
    let first = servers
        .get_or_start(want(
            &lsp,
            session.clone(),
            lsp.worktree(),
            1,
            Duration::from_secs(30),
        ))
        .await
        .unwrap();
    let held = first.lock(Duration::from_secs(5)).await.expect("the gate");
    // A second call waits for the gate and is dropped (its time limit, a cancel).
    let waiting = tokio::time::timeout(
        Duration::from_millis(300),
        first.lock(Duration::from_secs(30)),
    )
    .await;
    assert!(waiting.is_err());
    drop(held);
    // Nothing works now: at a bound of 1 the next server's room is made by stopping this one.
    let next = servers
        .get_or_start(want(
            &lsp,
            session,
            lsp.worktree().join("tools"),
            1,
            Duration::from_secs(30),
        ))
        .await;
    assert!(next.is_ok(), "{:?}", next.err());
}

// ----------------------------------------------------------------------- 4: no process id

#[tokio::test]
async fn initialize_names_no_process_because_the_workers_pid_means_nothing_elsewhere() {
    let lsp = Lsp::new(json!({"diagnostics": [diag(1, 0, 0, "x")]})).await;
    rust_project(&lsp);
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    let init = &lsp.logged("initialize")[0]["params"];
    assert!(init["processId"].is_null(), "{}", init["processId"]);
}

// --------------------------------------------------------------- 6: the bound under concurrency

#[tokio::test]
async fn two_starts_for_different_roots_at_a_bound_of_one_leave_one_server() {
    let lsp = Lsp::new(json!({})).await;
    rust_project(&lsp);
    lsp.put("tools/Cargo.toml", "[package]\nname = \"tools\"\n");
    let servers = lsp.fx.env.commands.language_servers();
    let session = adam_workspace::LocalSession::shared();
    let (a, b) = tokio::join!(
        servers.get_or_start(want(
            &lsp,
            session.clone(),
            lsp.worktree(),
            1,
            Duration::from_secs(30)
        )),
        servers.get_or_start(want(
            &lsp,
            session.clone(),
            lsp.worktree().join("tools"),
            1,
            Duration::from_secs(30)
        )),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(servers.count(), 1, "the bound holds under concurrency");
    let pids = [a.pid().unwrap(), b.pid().unwrap()];
    until("only one of the two processes to be left", || {
        pids.iter().filter(|p| alive(**p)).count() == 1
    })
    .await;
}

#[tokio::test]
async fn a_server_that_is_being_stopped_still_counts_until_its_process_is_gone() {
    let lsp = Lsp::new(json!({})).await;
    rust_project(&lsp);
    lsp.put("tools/Cargo.toml", "[package]\nname = \"tools\"\n");
    let servers = lsp.fx.env.commands.language_servers();
    // The environment takes 700 ms to kill what it holds.
    let session = Recording::new(Duration::from_millis(700));
    let first = servers
        .get_or_start(want(
            &lsp,
            session.clone(),
            lsp.worktree(),
            1,
            Duration::from_secs(30),
        ))
        .await
        .unwrap();
    first.request_stop("the test");
    let second = servers
        .get_or_start(want(
            &lsp,
            session.clone(),
            lsp.worktree().join("tools"),
            1,
            Duration::from_secs(30),
        ))
        .await;
    assert!(second.is_ok(), "{:?}", second.err());
    // The second was not started before the first was gone.
    assert_eq!(session.events(), ["prepare", "kill-done", "prepare"]);
}

// ------------------------------------------------------------------------- 7: stop points

#[tokio::test]
async fn a_start_that_is_dropped_asks_the_environment_to_kill_what_it_started() {
    let lsp = Lsp::new(json!({"initialize_never": true})).await;
    rust_project(&lsp);
    let servers = lsp.fx.env.commands.language_servers();
    let session = Recording::new(Duration::ZERO);
    let started = tokio::time::timeout(
        Duration::from_millis(1500),
        servers.get_or_start(want(
            &lsp,
            session.clone(),
            lsp.worktree(),
            4,
            Duration::from_secs(60),
        )),
    )
    .await;
    assert!(
        started.is_err(),
        "the start was still waiting for `initialize`"
    );
    until("the environment to be asked to kill the exec", || {
        !session.kills().is_empty()
    })
    .await;
}

#[tokio::test]
async fn a_start_that_finishes_after_its_run_was_stopped_leaves_no_server() {
    let lsp = Lsp::new(json!({"initialize_delay_ms": 1500})).await;
    rust_project(&lsp);
    let servers = lsp.fx.env.commands.language_servers();
    let run = lsp.run();
    let session = adam_workspace::LocalSession::shared();
    let (started, ()) = tokio::join!(
        servers.get_or_start(want(
            &lsp,
            session,
            lsp.worktree(),
            4,
            Duration::from_secs(30)
        )),
        async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            servers.stop_run(&run, "the run parked").await;
        }
    );
    assert!(
        matches!(&started, Err(StartError::Stopped)),
        "{:?}",
        started.as_ref().err()
    );
    assert_eq!(servers.count(), 0);
    let pid: u32 = std::fs::read_to_string(lsp.dir.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    until("the late server's process to be gone", || !alive(pid)).await;
}

// -------------------------------------------------------------------- 8: a closed client

#[tokio::test]
async fn a_server_whose_output_stops_being_messages_is_stopped_at_once_with_what_it_started() {
    let lsp = Lsp::new(json!({
        "garbage_after_ms": 3000, "spawn_tree": true, "diagnostics": [diag(1, 0, 0, "x")],
    }))
    .await;
    rust_project(&lsp);
    lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    let pids = lsp.tree().await;
    // No call is in flight when the garbage comes: nothing else would notice.
    until("the process tree to be stopped", || {
        pids.iter().all(|p| !alive(*p))
    })
    .await;
}

// ------------------------------------------------------------------ 9: the scratch directory

#[tokio::test]
async fn the_safe_rust_analyzer_target_directory_is_private_and_removed_when_the_server_stops() {
    let a = references::with_reference(json!({})).await;
    let b = references::with_reference(json!({})).await;
    let mut targets = Vec::new();
    for lsp in [&a, &b] {
        let out = lsp
            .diagnostics(json!({"path": "src/lib.rs", "repo": "lib"}))
            .await;
        assert!(!out.is_error, "{}", out.content);
        let options = lsp.init_options(0);
        let target = options["cargo"]["targetDir"].as_str().unwrap().to_owned();
        assert!(target.starts_with("/tmp/"), "{target}");
        targets.push(target);
    }
    assert_ne!(targets[0], targets[1], "one per run and slot");
    for (lsp, target) in [&a, &b].into_iter().zip(&targets) {
        // What rust-analyzer would have built there.
        std::fs::create_dir_all(format!("{target}/debug")).unwrap();
        std::fs::write(format!("{target}/debug/artifact"), "x").unwrap();
        lsp.fx.env.commands.stop_run(&lsp.run(), "the test").await;
        assert!(
            !Path::new(target).exists(),
            "{target} is removed when the server stops"
        );
    }
}

// ----------------------------------------------------------------------- 11: what is shown

#[tokio::test]
async fn what_a_server_says_loses_its_direction_controls_and_the_paths_of_other_places() {
    let message = "reordered \u{202e}txet\u{2066}x\u{2028}y\u{2029}z, from \
                   /usr/local/go/src/fmt/print.go:12:3 and (/home/ana/.cargo/registry/src/x/y.rs) \
                   and /etc/passwd";
    let lsp = Lsp::new(json!({"diagnostics": [diag(1, 0, 0, message)]})).await;
    rust_project(&lsp);
    let out = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    for bad in ['\u{202e}', '\u{2066}', '\u{2028}', '\u{2029}'] {
        assert!(
            !out.content.contains(bad),
            "U+{:04X}: {}",
            u32::from(bad),
            out.content
        );
    }
    assert!(out.content.contains("\\u{202e}"), "{}", out.content);
    assert!(
        !out.content.contains("/usr/local") && !out.content.contains("/home/ana"),
        "{}",
        out.content
    );
    assert!(
        out.content
            .contains("(outside the workspace) src/fmt/print.go:12:3")
            && out.content.contains("(outside the workspace) src/x/y.rs")
            && out.content.contains("(outside the workspace) etc/passwd"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn the_name_of_a_file_and_the_source_line_of_a_location_lose_their_direction_controls() {
    let lsp = Lsp::new(json!({"results": {"textDocument/definition": [
        {"uri": "$FILE", "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 4}}}
    ]}}))
    .await;
    rust_project(&lsp);
    let name = "src/evil\u{202e}gnp.rs";
    lsp.put(
        name,
        "// \u{202e}hidden \u{2028}text\u{2029}more\npub fn x() {}\n",
    );
    let out = lsp
        .call(
            "find_definition",
            json!({"path": name, "line": 2, "symbol": "x"}),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    for bad in ['\u{202e}', '\u{2028}', '\u{2029}'] {
        assert!(
            !out.content.contains(bad),
            "U+{:04X}: {}",
            u32::from(bad),
            out.content
        );
    }
    assert!(out.content.contains("\\u{202e}"), "{}", out.content);
}

// ------------------------------------------------------------------------ 12: positions

#[tokio::test]
async fn a_lone_carriage_return_is_a_line_break_as_the_protocol_counts() {
    let lsp = Lsp::new(json!({"results": {"textDocument/definition": null}})).await;
    rust_project(&lsp);
    lsp.put("src/cr.rs", "pub fn a() {}\rpub fn target_fn() {}\r");
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/cr.rs", "line": 2, "symbol": "target_fn"}),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    let asked = &lsp.logged("textDocument/definition")[0]["params"]["position"];
    assert_eq!(asked["line"], 1, "{asked}");
    assert_eq!(asked["character"], 7, "{asked}");
}

#[tokio::test]
async fn a_symbol_that_is_only_inside_other_words_is_not_on_the_line_as_a_word() {
    let lsp = Lsp::new(json!({"results": {"textDocument/definition": null}})).await;
    rust_project(&lsp);
    lsp.put("src/v.rs", "let valid = 1;\n");
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/v.rs", "line": 1, "symbol": "id"}),
        )
        .await;
    assert!(
        out.is_error
            && out
                .content
                .contains("not on line 1 of `src/v.rs` as a word")
            && out.content.contains("let valid = 1;"),
        "{}",
        out.content
    );
    assert!(lsp.logged("textDocument/definition").is_empty());
}

#[tokio::test]
async fn a_pull_that_is_cancelled_while_the_server_works_is_still_indexing_not_a_failure() {
    let lsp =
        Lsp::new(json!({"pull": true, "error_on": {"textDocument/diagnostic": -32802}})).await;
    rust_project(&lsp);
    let out = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("still indexing"), "{}", out.content);
}

// ------------------------------------------------------------------------- the mutation pass

#[tokio::test]
async fn a_start_that_finishes_after_the_worker_stopped_leaves_no_server() {
    let lsp = Lsp::new(json!({"initialize_delay_ms": 1500})).await;
    rust_project(&lsp);
    let servers = lsp.fx.env.commands.language_servers();
    let session = adam_workspace::LocalSession::shared();
    let (started, ()) = tokio::join!(
        servers.get_or_start(want(
            &lsp,
            session,
            lsp.worktree(),
            4,
            Duration::from_secs(30)
        )),
        async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            servers.stop_all().await;
        }
    );
    assert!(
        matches!(&started, Err(StartError::Stopped)),
        "{:?}",
        started.as_ref().err()
    );
    assert_eq!(servers.count(), 0);
    let pid: u32 = std::fs::read_to_string(lsp.dir.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    until("the late server's process to be gone", || !alive(pid)).await;
}

#[tokio::test]
async fn a_server_in_a_call_longer_than_its_idle_time_is_not_stopped() {
    let lsp = Lsp::new(json!({})).await;
    rust_project(&lsp);
    let servers = lsp.fx.env.commands.language_servers();
    let mut asked = want(
        &lsp,
        adam_workspace::LocalSession::shared(),
        lsp.worktree(),
        4,
        Duration::from_secs(30),
    );
    asked.idle = Duration::from_secs(1);
    let server = servers.get_or_start(asked).await.unwrap();
    let held = server.lock(Duration::from_secs(5)).await.expect("the gate");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(server.is_alive(), "it was working the whole time");
    assert_eq!(servers.count(), 1);
    drop(held);
    until("the server to be stopped once it is idle", || {
        !server.is_alive()
    })
    .await;
}

#[tokio::test]
async fn a_start_that_fails_initialize_asks_the_environment_to_kill_its_exec() {
    let lsp = Lsp::new(json!({"initialize_never": true})).await;
    rust_project(&lsp);
    let servers = lsp.fx.env.commands.language_servers();
    let session = Recording::new(Duration::ZERO);
    let started = servers
        .get_or_start(want(
            &lsp,
            session.clone(),
            lsp.worktree(),
            4,
            Duration::from_secs(1),
        ))
        .await;
    assert!(
        matches!(&started, Err(StartError::Failed(_))),
        "{:?}",
        started.as_ref().err()
    );
    assert!(
        !session.kills().is_empty(),
        "the environment was asked to kill what it started"
    );
    assert_eq!(servers.count(), 0);
}

#[tokio::test]
async fn a_server_of_an_environment_described_differently_is_replaced() {
    let lsp = Lsp::new(json!({})).await;
    rust_project(&lsp);
    let servers = lsp.fx.env.commands.language_servers();
    let first = servers
        .get_or_start(want(
            &lsp,
            Recording::described_as("a container"),
            lsp.worktree(),
            4,
            Duration::from_secs(30),
        ))
        .await
        .unwrap();
    let again = servers
        .get_or_start(want(
            &lsp,
            Recording::described_as("a container"),
            lsp.worktree(),
            4,
            Duration::from_secs(30),
        ))
        .await
        .unwrap();
    assert_eq!(again.pid(), first.pid(), "the same environment keeps it");
    let (old, replaced) = (first.pid().unwrap(), {
        servers
            .get_or_start(want(
                &lsp,
                Recording::described_as("another container"),
                lsp.worktree(),
                4,
                Duration::from_secs(30),
            ))
            .await
            .unwrap()
    });
    assert_ne!(replaced.pid().unwrap(), old);
    until("the old server's process to be gone", || !alive(old)).await;
    assert_eq!(servers.count(), 1);
}

#[tokio::test]
async fn a_write_into_a_reference_between_calls_is_caught_at_the_next() {
    let lsp = references::with_reference(json!({"diagnostics": [diag(1, 0, 0, "x")]})).await;
    let out = lsp
        .diagnostics(json!({"path": "src/lib.rs", "repo": "lib"}))
        .await;
    assert!(!out.is_error, "{}", out.content);
    let written = lsp
        .fx
        .root
        .join("workspaces")
        .join(lsp.run())
        .join("lib")
        .join("src/generated.rs");
    std::fs::write(&written, "// between the calls\n").unwrap();
    let out = lsp
        .diagnostics(json!({"path": "src/lib.rs", "repo": "lib"}))
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("is a reference") && out.content.contains("as they were"),
        "{}",
        out.content
    );
    assert!(!written.exists(), "the file is removed");
    assert!(lsp.servers().is_empty(), "the server is stopped");
}

#[tokio::test]
async fn a_run_without_notes_gets_a_safe_server() {
    let lsp = Lsp::new(json!({"diagnostics": [diag(1, 0, 0, "x")]})).await;
    rust_project(&lsp);
    let notes = lsp
        .fx
        .root
        .join("coder")
        .join(format!("{}.json", lsp.run()));
    std::fs::remove_file(&notes).unwrap();
    let out = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(!out.is_error, "{}", out.content);
    let options = lsp.init_options(0);
    assert_eq!(
        options["cargo"]["buildScripts"]["enable"], false,
        "{options}"
    );
}

#[tokio::test]
async fn at_a_bound_of_two_the_least_recently_used_is_stopped() {
    let lsp = Lsp::new(json!({})).await;
    rust_project(&lsp);
    for dir in ["b", "c"] {
        lsp.put(&format!("{dir}/Cargo.toml"), "[package]\nname = \"p\"\n");
    }
    let servers = lsp.fx.env.commands.language_servers();
    let session = adam_workspace::LocalSession::shared();
    let start = |root: PathBuf| {
        let asked = want(&lsp, session.clone(), root, 2, Duration::from_secs(30));
        async { servers.get_or_start(asked).await.unwrap() }
    };
    let a = start(lsp.worktree()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let b = start(lsp.worktree().join("b")).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // A is used again, so B is the one least recently used.
    let again = start(lsp.worktree()).await;
    assert_eq!(again.pid(), a.pid());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _c = start(lsp.worktree().join("c")).await;
    let (a, b) = (a.pid().unwrap(), b.pid().unwrap());
    until("the least recently used to be gone", || !alive(b)).await;
    assert!(alive(a), "the one used last is kept");
    assert_eq!(servers.count(), 2);
}

#[tokio::test]
async fn a_probe_is_asked_again_when_the_environment_is_described_differently() {
    let lsp = Lsp::new(json!({})).await;
    let servers = lsp.fx.env.commands.language_servers();
    let run = lsp.run();
    servers.remember_probe(&run, "w", Language::Rust, "a container", Probe::Found);
    assert_eq!(
        servers.probed(&run, "w", Language::Rust, "a container"),
        Some(Probe::Found)
    );
    assert_eq!(
        servers.probed(&run, "w", Language::Rust, "another container"),
        None
    );
}

#[tokio::test]
async fn a_second_call_on_a_file_never_published_is_still_not_clean() {
    let lsp = Lsp::new(json!({"silent_when_clean": true})).await;
    rust_project(&lsp);
    for call in 1..=2 {
        let out = lsp
            .diagnostics(json!({"path": "src/lib.rs", "wait": 5}))
            .await;
        assert!(
            out.content.contains("No diagnostics received")
                && !out.content.contains("(no errors or warnings)"),
            "call {call}: {}",
            out.content
        );
    }
}

#[tokio::test]
async fn a_pull_answered_content_modified_is_still_indexing_not_a_failure() {
    let lsp =
        Lsp::new(json!({"pull": true, "error_on": {"textDocument/diagnostic": -32801}})).await;
    rust_project(&lsp);
    let out = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("still indexing"), "{}", out.content);
}

#[tokio::test]
async fn a_secret_across_the_1000_character_cut_is_not_shown_in_part() {
    // The token starts 15 characters before the cut: cut first, its start would not be known.
    let message = format!("{}{GITHUB_TOKEN} and more", "x".repeat(985));
    let lsp = Lsp::new(json!({"diagnostics": [diag(1, 0, 0, &message)]})).await;
    rust_project(&lsp);
    let out = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    assert!(!out.content.contains("ghp_FAKE"), "{}", out.content);
    // The mark starts 15 characters before the cut, so the cut falls inside it: its front is what
    // is shown, and the secret's tail is not.
    assert!(out.content.contains("[redacted: secr…"), "{}", out.content);
}

/// A rig whose server says `symbol` of `src/{name}.rs` (`text`, one line) is defined at `character`
/// of that line, and what `find_definition` of it says.
async fn definition_in(name: &str, text: &str, symbol: &str, character: u64) -> (Lsp, String) {
    let lsp = Lsp::new(json!({"results": {"textDocument/definition": [
        {"uri": "$FILE", "range": {"start": {"line": 0, "character": character},
                                   "end": {"line": 0, "character": character + 1}}}
    ]}}))
    .await;
    rust_project(&lsp);
    let rel = format!("src/{name}.rs");
    lsp.put(&rel, text);
    let out = lsp
        .call(
            "find_definition",
            json!({"path": rel, "line": 1, "symbol": symbol}),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    (lsp, out.content)
}

#[tokio::test]
async fn a_location_climbing_out_with_dotdot_is_outside_the_workspace() {
    let lsp = Lsp::new(json!({"results": {"textDocument/definition": [
        {"uri": "$ROOT/../../etc/passwd",
         "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}
    ]}}))
    .await;
    rust_project(&lsp);
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first"}),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("(outside the workspace) ") && !out.content.contains(".."),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn long_source_lines_and_messages_are_cut() {
    let (_lsp, said) =
        definition_in("long", &format!("let y = {};\n", "y".repeat(5000)), "y", 4).await;
    let line = said
        .lines()
        .find(|l| l.starts_with("src/long.rs:1:5"))
        .unwrap();
    assert!(line.ends_with('…'), "{line}");
    assert!(
        line.chars().count() <= "src/long.rs:1:5: ".len() + 201,
        "{}",
        line.chars().count()
    );

    let lsp = Lsp::new(json!({"diagnostics": [diag(1, 0, 0, &"m".repeat(5000))]})).await;
    rust_project(&lsp);
    let out = lsp.diagnostics(json!({"path": "src/lib.rs"})).await;
    let line = out.content.lines().find(|l| l.contains("error: ")).unwrap();
    assert!(line.ends_with('…'), "{} chars", line.chars().count());
    assert!(
        line.chars().count() <= "src/lib.rs:1:1: error: ".len() + 1001,
        "{}",
        line.chars().count()
    );
}

#[tokio::test]
async fn the_column_of_a_location_is_characters() {
    // The server counts UTF-16: `bad` is at unit 4 of `é😀 bad`, the fourth character.
    let (_lsp, said) = definition_in("u", "é😀 bad\n", "bad", 4).await;
    assert!(said.contains("src/u.rs:1:4: é😀 bad"), "{said}");
}

#[tokio::test]
async fn a_definition_asked_while_indexing_keeps_the_server() {
    let lsp = Lsp::new(json!({
        "progress_ms": 60000, "never_answer": ["textDocument/definition"],
    }))
    .await;
    rust_project(&lsp);
    let out = lsp
        .call(
            "find_definition",
            json!({"path": "src/lib.rs", "line": 1, "symbol": "first", "wait": 1}),
        )
        .await;
    assert!(out.content.contains("still indexing"), "{}", out.content);
    assert_eq!(lsp.servers().len(), 1, "the server is kept");
    assert!(alive(lsp.pid()));
}

#[cfg(unix)]
#[tokio::test]
async fn a_server_counts_inside_the_process_pool() {
    let lsp = Lsp::new(json!({})).await;
    rust_project(&lsp);
    let budget = adam_budget::Budget::builder().processes(1).build();
    let governor = adam_code_tools::governor::Governor::new(
        Arc::new(adam_admission_local::LocalAdmission::new(budget)),
        Arc::new(adam_confine::Rlimits::new()),
        adam_workspace::ProcessLimits::new(),
    )
    .with_wait(Duration::from_millis(100));
    // The only seat is somebody else's: a server has to wait for one, and is told to try again.
    let _taken = governor
        .seat(adam_budget::ConversationId::new("another-run"))
        .await
        .expect("the pool is free");
    let mut asked = want(
        &lsp,
        adam_workspace::LocalSession::shared(),
        lsp.worktree(),
        4,
        Duration::from_secs(5),
    );
    asked.governor = Some(governor);
    let failed = match lsp
        .fx
        .env
        .commands
        .language_servers()
        .get_or_start(asked)
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("no seat is free"),
    };
    assert!(matches!(failed, StartError::Seat(_)), "{failed:?}");
    assert!(lsp.servers().is_empty());
}
