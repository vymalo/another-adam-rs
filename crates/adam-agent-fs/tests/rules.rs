//! One fixture directory per rule. Each invalid one must produce exactly one diagnostic, of the
//! stated severity, and the valid one none.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::Path;

use adam_agent_fs::{
    Dir, Layout, ManifestSource, McpServer, Report, Severity, Strictness, Subagent,
};

const ROOT: &str = "---\nname: root\n---\nYou are root.\n";
const SUB: &str = "---\ndescription: Does one thing.\n---\nYou do one thing.\n";
const SKILL: &str = "---\nname: pdf\ndescription: Works with PDFs.\n---\nBody.\n";

fn write(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
    dir
}

fn load(dir: &Path) -> Report {
    Dir::new(dir).default_name("root").load().unwrap()
}

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// (rule, files, severity, a fragment of the message)
type Case = (
    &'static str,
    Vec<(&'static str, &'static str)>,
    Severity,
    &'static str,
);

fn with_root(
    rule: &'static str,
    files: Vec<(&'static str, &'static str)>,
    sev: Severity,
    needle: &'static str,
) -> Case {
    let mut all = vec![("agent/instructions.md", ROOT)];
    all.extend(files);
    (rule, all, sev, needle)
}

fn cases() -> Vec<Case> {
    use Severity::{Error, Warning};
    vec![
        // --- discovery
        (
            "both agent/ and agents/",
            vec![
                ("agent/instructions.md", ROOT),
                ("agents/a/instructions.md", ROOT),
            ],
            Error,
            "both `agent/` and `agents/`",
        ),
        (
            "neither agent/ nor agents/",
            vec![("README.md", "x")],
            Error,
            "neither `agent/` nor `agents/`",
        ),
        (
            "agent/ without instructions.md",
            vec![("agent/skills/README.md", "x")],
            Error,
            "no `instructions.md`",
        ),
        (
            "agents/<name> without instructions.md",
            vec![
                ("agents/a/instructions.md", "Hi.\n"),
                ("agents/b/notes.md", "x"),
            ],
            Warning,
            "no `instructions.md`",
        ),
        (
            "agents/ with no directory",
            vec![("agents/readme.txt", "x")],
            Error,
            "holds no agent directory",
        ),
        (
            "agents/<name> name mismatch",
            vec![("agents/a/instructions.md", "---\nname: b\n---\nHi.\n")],
            Error,
            "does not match the directory `a`",
        ),
        (
            "agents/<name> invalid directory name",
            vec![("agents/Bad Name/instructions.md", "Hi.\n")],
            Error,
            "not a valid agent name",
        ),
        with_root(
            "unknown entry in the agent directory",
            vec![("agent/tools/x.rs", "fn main() {}")],
            Warning,
            "`tools` is not part of an agent directory",
        ),
        with_root(
            "directory subagent without instructions.md",
            vec![("agent/subagents/x/notes.md", "n")],
            Error,
            "needs an `instructions.md`",
        ),
        with_root(
            "remote subagent as a directory",
            vec![(
                "agent/subagents/x/instructions.md",
                "---\ndescription: d\na2a: https://example.com/card\n---\n",
            )],
            Error,
            "which is a single file",
        ),
        with_root(
            "duplicate subagent (x.md and x.agent.md)",
            vec![
                ("agent/subagents/x.md", SUB),
                ("agent/subagents/x.agent.md", SUB),
            ],
            Error,
            "subagent `x` is already defined",
        ),
        with_root(
            "schedules in a subagent directory",
            vec![
                ("agent/subagents/x/instructions.md", SUB),
                (
                    "agent/subagents/x/schedules/a.md",
                    "---\ncron: \"* * * * *\"\n---\nGo.\n",
                ),
            ],
            Warning,
            "schedules belong to the root agent",
        ),
        // --- frontmatter of agents
        (
            "unterminated frontmatter",
            vec![("agent/instructions.md", "---\nname: root\nYou are root.\n")],
            Error,
            "never closed",
        ),
        (
            "invalid YAML",
            vec![("agent/instructions.md", "---\nname: [root\n---\nHi.\n")],
            Error,
            "invalid frontmatter",
        ),
        (
            "unknown key",
            vec![(
                "agent/instructions.md",
                "---\nname: root\nflavour: mint\n---\nHi.\n",
            )],
            Warning,
            "unknown key `flavour`",
        ),
        (
            "Claude/Copilot key adam ignores",
            vec![(
                "agent/instructions.md",
                "---\nname: root\ncolor: blue\n---\nHi.\n",
            )],
            Warning,
            "`color` is a Claude Code or Copilot key",
        ),
        (
            "secret key api_key",
            vec![(
                "agent/instructions.md",
                "---\nname: root\napi_key: abc\n---\nHi.\n",
            )],
            Error,
            "secrets and endpoints belong in the environment",
        ),
        (
            "secret key apiKey",
            vec![(
                "agent/instructions.md",
                "---\nname: root\napiKey: abc\n---\nHi.\n",
            )],
            Error,
            "secrets and endpoints belong in the environment",
        ),
        (
            "secret key token",
            vec![(
                "agent/instructions.md",
                "---\nname: root\ntoken: abc\n---\nHi.\n",
            )],
            Error,
            "secrets and endpoints belong in the environment",
        ),
        (
            "secret key secret",
            vec![(
                "agent/instructions.md",
                "---\nname: root\nsecret: abc\n---\nHi.\n",
            )],
            Error,
            "secrets and endpoints belong in the environment",
        ),
        (
            "secret key password",
            vec![(
                "agent/instructions.md",
                "---\nname: root\npassword: abc\n---\nHi.\n",
            )],
            Error,
            "secrets and endpoints belong in the environment",
        ),
        (
            "endpoint key base_url",
            vec![(
                "agent/instructions.md",
                "---\nname: root\nbase_url: http://x\n---\nHi.\n",
            )],
            Error,
            "secrets and endpoints belong in the environment",
        ),
        (
            "model is an endpoint",
            vec![(
                "agent/instructions.md",
                "---\nname: root\nmodel: https://api.example.com/v1\n---\nHi.\n",
            )],
            Error,
            "gateway alias, not an endpoint",
        ),
        (
            "mcpServers in the frontmatter",
            vec![(
                "agent/instructions.md",
                "---\nname: root\nmcpServers: {}\n---\nHi.\n",
            )],
            Warning,
            "put MCP servers in `mcp.json`",
        ),
        (
            "maxTurns and limits.max_turns disagree",
            vec![(
                "agent/instructions.md",
                "---\nname: root\nmaxTurns: 5\nlimits: { max_turns: 9 }\n---\nHi.\n",
            )],
            Warning,
            "disagree",
        ),
        (
            "unknown limits key",
            vec![(
                "agent/instructions.md",
                "---\nname: root\nlimits: { max_turn: 9 }\n---\nHi.\n",
            )],
            Warning,
            "`limits.max_turn`",
        ),
        (
            "unknown card key",
            vec![(
                "agent/instructions.md",
                "---\nname: root\ncard: { colour: red }\n---\nHi.\n",
            )],
            Warning,
            "`card.colour`",
        ),
        (
            "a2a on the root agent",
            vec![(
                "agent/instructions.md",
                "---\nname: root\na2a: https://a.example.com/card.json\n---\nHi.\n",
            )],
            Error,
            "cannot be used on an agent",
        ),
        (
            "vars key is not a placeholder name",
            vec![(
                "agent/instructions.md",
                "---\nname: root\nvars: { \"a b\": 1 }\n---\nHi.\n",
            )],
            Error,
            "cannot be a `{{placeholder}}`",
        ),
        (
            "vars value is not a scalar",
            vec![(
                "agent/instructions.md",
                "---\nname: root\nvars: { a: [1] }\n---\nHi.\n",
            )],
            Error,
            "invalid frontmatter",
        ),
        (
            "no instructions at all",
            vec![("agent/instructions.md", "---\nname: root\n---\n")],
            Error,
            "has no instructions",
        ),
        // --- subagents
        with_root(
            "subagent without a description",
            vec![("agent/subagents/x.md", "---\nmodel: inherit\n---\nBody.\n")],
            Error,
            "needs a `description`",
        ),
        with_root(
            "subagent with an empty body",
            vec![("agent/subagents/x.md", "---\ndescription: d\n---\n")],
            Error,
            "has no instructions",
        ),
        with_root(
            "subagent name in the frontmatter is invalid",
            vec![(
                "agent/subagents/x.md",
                "---\nname: My Agent\ndescription: d\n---\nBody.\n",
            )],
            Warning,
            "not a valid agent name",
        ),
        with_root(
            "subagent file name needs lower-casing",
            vec![("agent/subagents/Code-Reviewer.agent.md", SUB)],
            Warning,
            "using `code-reviewer`",
        ),
        with_root(
            "card on a subagent",
            vec![(
                "agent/subagents/x.md",
                "---\ndescription: d\ncard: { name: x }\n---\nBody.\n",
            )],
            Warning,
            "`card` is for the root agent",
        ),
        with_root(
            "tool name that adam cannot bind",
            vec![(
                "agent/subagents/x.md",
                "---\ndescription: d\ntools: Read\n---\nBody.\n",
            )],
            Warning,
            "`Read` is not an adam tool name",
        ),
        with_root(
            "remote subagent with a bad URL",
            vec![(
                "agent/subagents/x.md",
                "---\ndescription: d\na2a: ftp://example.com/card\n---\n",
            )],
            Error,
            "not an http(s) agent-card URL",
        ),
        with_root(
            "remote subagent with a bad auth",
            vec![(
                "agent/subagents/x.md",
                "---\ndescription: d\na2a: https://example.com/card\nauth: token-123\n---\n",
            )],
            Error,
            "`bearer:ENV_VAR`",
        ),
        with_root(
            "auth without a2a",
            vec![(
                "agent/subagents/x.md",
                "---\ndescription: d\nauth: bearer:TOKEN\n---\nBody.\n",
            )],
            Error,
            "`auth` needs `a2a`",
        ),
        with_root(
            "local-only key on a remote subagent",
            vec![(
                "agent/subagents/x.md",
                "---\ndescription: d\na2a: https://example.com/card\ntools: [a]\n---\n",
            )],
            Warning,
            "`tools` is ignored on a remote subagent",
        ),
        // --- skills
        with_root(
            "skill name differs from its directory",
            vec![(
                "agent/skills/pdf/SKILL.md",
                "---\nname: pdf-tools\ndescription: d\n---\nB\n",
            )],
            Warning,
            "does not match the directory `pdf`",
        ),
        with_root(
            "skill without a name",
            vec![("agent/skills/pdf/SKILL.md", "---\ndescription: d\n---\nB\n")],
            Warning,
            "no `name`",
        ),
        with_root(
            "skill name breaks the spec",
            vec![(
                "agent/skills/Pdf/SKILL.md",
                "---\nname: Pdf\ndescription: d\n---\nB\n",
            )],
            Warning,
            "breaks the Agent Skills name rule",
        ),
        with_root(
            "skill without a description",
            vec![("agent/skills/pdf/SKILL.md", "---\nname: pdf\n---\nB\n")],
            Error,
            "needs a `description`",
        ),
        with_root(
            "skill with an empty description",
            vec![(
                "agent/skills/pdf/SKILL.md",
                "---\nname: pdf\ndescription: \"  \"\n---\nB\n",
            )],
            Error,
            "needs a `description`",
        ),
        with_root(
            "skill without frontmatter (directory form)",
            vec![("agent/skills/pdf/SKILL.md", "Just text.\n")],
            Error,
            "needs a `description`",
        ),
        with_root(
            "skill with unparseable YAML",
            vec![(
                "agent/skills/pdf/SKILL.md",
                "---\nname: pdf\ndescription: [oops\n---\nB\n",
            )],
            Error,
            "invalid frontmatter",
        ),
        with_root(
            "skill with an unterminated frontmatter",
            vec![(
                "agent/skills/pdf/SKILL.md",
                "---\nname: pdf\ndescription: d\n",
            )],
            Error,
            "never closed",
        ),
        with_root(
            "flat skill without frontmatter",
            vec![("agent/skills/triage.md", "# Triage\n\nSort the issue.\n")],
            Warning,
            "taken from the first line",
        ),
        with_root(
            "empty flat skill",
            vec![("agent/skills/triage.md", "\n\n")],
            Error,
            "the skill is empty",
        ),
        with_root(
            "skill name in the same skills/ twice",
            vec![
                ("agent/skills/pdf/SKILL.md", SKILL),
                ("agent/skills/pdf.md", SKILL),
            ],
            Error,
            "skill `pdf` is already defined",
        ),
        // --- mcp.json
        with_root(
            "mcp.json is not JSON",
            vec![("agent/mcp.json", "{\"mcpServers\": ")],
            Error,
            "invalid JSON",
        ),
        with_root(
            "mcp.json unknown top-level key",
            vec![("agent/mcp.json", r#"{"servers":{}}"#)],
            Warning,
            "unknown top-level key `servers`",
        ),
        with_root(
            "mcp url without type",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a":{"url":"https://x.example.com"}}}"#,
            )],
            Error,
            "`url` but no `type`",
        ),
        with_root(
            "mcp unsupported type",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a":{"type":"ws","url":"wss://x.example.com"}}}"#,
            )],
            Error,
            "`type: ws` is not supported",
        ),
        with_root(
            "mcp server with nothing to run",
            vec![("agent/mcp.json", r#"{"mcpServers":{"a":{}}}"#)],
            Error,
            "needs a `command`",
        ),
        with_root(
            "mcp remote with a command",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a":{"type":"http","command":"x"}}}"#,
            )],
            Error,
            "remote (`type`) but has a `command`",
        ),
        with_root(
            "mcp remote url is not http",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a":{"type":"sse","url":"ftp://x"}}}"#,
            )],
            Error,
            "must start with `http://`",
        ),
        with_root(
            "mcp server name with a double underscore",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a__b":{"command":"x"}}}"#,
            )],
            Error,
            "server name `a__b`",
        ),
        with_root(
            "mcp server name ending in an underscore",
            vec![("agent/mcp.json", r#"{"mcpServers":{"a_":{"command":"x"}}}"#)],
            Error,
            "not ending in `_`",
        ),
        with_root(
            "mcp allow-listed tool name starting with an underscore",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a":{"command":"x","tools":["_x"]}}}"#,
            )],
            Error,
            "not starting with `_`",
        ),
        with_root(
            "mcp allow-listed tool name too long",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"srv":{"command":"x","tools":["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}}}"#,
            )],
            Error,
            "cannot be named",
        ),
        with_root(
            "mcp literal bearer token in a header",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a":{"type":"http","url":"https://x.example.com","headers":{"Authorization":"Bearer sk-live-123"}}}}"#,
            )],
            Warning,
            "literal credential",
        ),
        with_root(
            "mcp literal secret in a header by name",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a":{"type":"http","url":"https://x.example.com","headers":{"X-Api-Key":"abc123"}}}}"#,
            )],
            Warning,
            "literal credential",
        ),
        with_root(
            "mcp literal secret in the environment",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a":{"command":"x","env":{"GITHUB_TOKEN":"ghp_abc"}}}}"#,
            )],
            Warning,
            "literal credential",
        ),
        with_root(
            "mcp credential in the URL",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a":{"type":"http","url":"https://user:pw@x.example.com/mcp"}}}"#,
            )],
            Warning,
            "literal credential",
        ),
        with_root(
            "mcp credential in the URL query",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a":{"type":"http","url":"https://x.example.com/mcp?api_key=abc"}}}"#,
            )],
            Warning,
            "literal credential",
        ),
        with_root(
            "mcp secret key on a server",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a":{"command":"x","apiKey":"abc"}}}"#,
            )],
            Error,
            "secrets belong in the environment",
        ),
        with_root(
            "mcp unknown server key",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a":{"command":"x","timeout":5}}}"#,
            )],
            Warning,
            "unknown key `timeout`",
        ),
        with_root(
            "mcp malformed reference",
            vec![(
                "agent/mcp.json",
                r#"{"mcpServers":{"a":{"command":"x","args":["${unclosed"]}}}"#,
            )],
            Warning,
            "is not a `${VAR}`",
        ),
        // --- schedules
        with_root(
            "schedule without cron",
            vec![("agent/schedules/a.md", "---\ntimezone: UTC\n---\nGo.\n")],
            Error,
            "needs `cron`",
        ),
        with_root(
            "schedule with a four-field cron",
            vec![("agent/schedules/a.md", "---\ncron: \"0 9 * *\"\n---\nGo.\n")],
            Error,
            "has 4 fields",
        ),
        with_root(
            "schedule with a bad time zone",
            vec![(
                "agent/schedules/a.md",
                "---\ncron: \"0 9 * * *\"\ntimezone: Mars Base\n---\nGo.\n",
            )],
            Error,
            "not an IANA time zone",
        ),
        with_root(
            "schedule with an empty prompt",
            vec![("agent/schedules/a.md", "---\ncron: \"0 9 * * *\"\n---\n")],
            Error,
            "needs a body",
        ),
        with_root(
            "schedule for another agent",
            vec![(
                "agent/schedules/a.md",
                "---\ncron: \"0 9 * * *\"\nagent: other\n---\nGo.\n",
            )],
            Error,
            "does not match the agent `root`",
        ),
        with_root(
            "schedule with an unknown key",
            vec![(
                "agent/schedules/a.md",
                "---\ncron: \"0 9 * * *\"\nretries: 3\n---\nGo.\n",
            )],
            Warning,
            "unknown key `retries`",
        ),
    ]
}

#[test]
fn every_invalid_directory_produces_exactly_one_diagnostic() {
    let mut failures = Vec::new();
    for (rule, files, severity, needle) in cases() {
        let dir = write(&files);
        let report = load(dir.path());
        let ok = report.diagnostics.len() == 1
            && report.diagnostics[0].severity == severity
            && report.diagnostics[0].message.contains(needle);
        if !ok {
            failures.push(format!(
                "{rule}: expected one {severity} containing {needle:?}, got {:#?}",
                report.diagnostics
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} rule(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn the_valid_fixture_has_no_diagnostic_and_reads_completely() {
    let report = Dir::new(fixture("valid")).load().unwrap();
    assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
    let package = report.into_package(Strictness::Strict).unwrap();
    assert_eq!(package.layout, Layout::Single);
    let [coder] = package.agents.as_slice() else {
        panic!("one agent expected: {:?}", package.agents.len());
    };
    assert_eq!(coder.name, "coder");
    assert_eq!(
        coder.frontmatter.limits.as_ref().unwrap().max_turns,
        Some(200)
    );
    assert_eq!(coder.frontmatter.vars["max_check_cycles"], "3");
    assert_eq!(coder.frontmatter.vars["strict"], "true");
    assert_eq!(coder.frontmatter.metadata["version"], "1.2");
    assert_eq!(
        coder.frontmatter.card.as_ref().unwrap().skills[0].id,
        "coding-task"
    );
    assert!(
        coder
            .instructions
            .prompt()
            .ends_with("## Style\n\nKeep commits small.")
    );
    assert!(
        coder
            .instructions
            .body
            .starts_with("You are the coder agent.")
    );
    assert_eq!(coder.instructions.parts.len(), 1);

    let skills: Vec<_> = coder.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(skills, ["release-notes", "triage"]);
    assert_eq!(
        coder.skills[0].resources,
        ["references/style.md", "scripts/run.sh"]
    );
    assert_eq!(coder.skills[0].allowed_tools, ["Bash(git log:*)", "Read"]);
    assert_eq!(coder.skills[0].metadata["version"], "1.0");

    let subs: Vec<_> = coder.subagents.iter().map(Subagent::name).collect();
    assert_eq!(subs, ["billing", "legacy", "researcher", "reviewer"]);
    assert!(
        matches!(&coder.subagents[0], Subagent::Remote(r) if r.url.ends_with("agent-card.json"))
    );
    let Subagent::Local(researcher) = &coder.subagents[2] else {
        panic!("researcher is local");
    };
    assert_eq!(researcher.skills.len(), 1);
    assert_eq!(researcher.subagents[0].name(), "summarizer");
    assert!(researcher.mcp.is_some());

    let mcp = coder.mcp.as_ref().unwrap();
    assert!(matches!(mcp.servers["linear"], McpServer::Remote { .. }));
    let names: Vec<_> = mcp.env_references().into_iter().collect();
    assert_eq!(names, ["LINEAR_API_TOKEN", "MCP_LOG_LEVEL"]);

    let schedules: Vec<_> = coder.schedules.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(schedules, ["daily-digest", "team/weekly"]);
    assert_eq!(coder.schedules[0].timezone, "Europe/Berlin");
    assert_eq!(coder.schedules[1].timezone, "UTC");
}

#[test]
fn mcp_placeholders_are_left_unexpanded() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/mcp.json",
            r#"{"mcpServers":{"a":{"type":"http","url":"${MCP_URL}","headers":{"Authorization":"Bearer ${T:-dev}"}}}}"#,
        ),
    ]);
    let report = load(dir.path());
    assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
    let mcp = report.package.agents[0].mcp.as_ref().unwrap();
    let McpServer::Remote { url, headers, .. } = &mcp.servers["a"] else {
        panic!("remote");
    };
    assert_eq!(url, "${MCP_URL}");
    assert_eq!(headers["Authorization"], "Bearer ${T:-dev}");
}

#[test]
fn a_literal_secret_in_mcp_json_is_refused_by_a_strict_build_and_kept_by_a_lenient_one() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/mcp.json",
            r#"{"mcpServers":{"a":{"type":"http","url":"https://x.example.com","headers":{"Authorization":"Bearer sk-live-123"}}}}"#,
        ),
    ]);
    let lenient = load(dir.path()).into_package(Strictness::Lenient);
    assert!(lenient.is_ok());
    let strict = load(dir.path())
        .into_package(Strictness::Strict)
        .unwrap_err();
    assert!(
        strict.to_string().contains("literal credential"),
        "{strict}"
    );
}

#[test]
fn a_bad_server_is_left_out_and_the_others_stay() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/mcp.json",
            r#"{"mcpServers":{"bad":{"url":"https://x.example.com"},"good":{"command":"x"}}}"#,
        ),
    ]);
    let report = load(dir.path());
    assert_eq!(report.diagnostics.len(), 1);
    let servers: Vec<_> = report.package.agents[0]
        .mcp
        .as_ref()
        .unwrap()
        .servers
        .keys()
        .cloned()
        .collect();
    assert_eq!(servers, ["good"]);
}

#[test]
fn optional_source_accepts_a_directory_without_agents() {
    let dir = write(&[("README.md", "x")]);
    let report = Dir::new(dir.path()).optional().load().unwrap();
    assert!(report.diagnostics.is_empty());
    assert_eq!(report.package.layout, Layout::Absent);
    assert!(report.package.agents.is_empty());
}

#[test]
fn several_agents_are_named_by_their_directories() {
    let dir = write(&[
        (
            "agents/writer/instructions.md",
            "---\ndescription: Writes.\n---\nWrite.\n",
        ),
        (
            "agents/editor/instructions.md",
            "---\nname: editor\n---\nEdit.\n",
        ),
        ("agents/.hidden/instructions.md", "x"),
    ]);
    let report = load(dir.path());
    assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
    assert_eq!(report.package.layout, Layout::Multi);
    let names: Vec<_> = report
        .package
        .agents
        .iter()
        .map(|a| a.name.as_str())
        .collect();
    assert_eq!(names, ["editor", "writer"]);
}

#[test]
fn ignored_files_are_not_read() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        ("agent/.secret.md", "---\nunterminated"),
        ("agent/skills/.git/SKILL.md", "---\nunterminated"),
        ("agent/skills/x.test.md", "---\nunterminated"),
        ("agent/skills/__tests__/SKILL.md", "---\nunterminated"),
        ("agent/skills/README.md", "---\nunterminated"),
        ("agent/subagents/y.test.md", "---\nunterminated"),
        ("agent/subagents/README.md", "---\nunterminated"),
        ("agent/subagents/notes.txt", "not markdown"),
        ("agent/instructions/.draft.md", "draft"),
        ("agent/instructions/a.test.md", "test"),
        ("agent/instructions/notes.txt", "text"),
    ]);
    let report = load(dir.path());
    assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
    assert!(report.package.agents[0].instructions.parts.is_empty());
    assert!(report.package.agents[0].skills.is_empty());
}

#[test]
fn instruction_parts_follow_filename_order() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        ("agent/instructions/20-b.md", "Second.\n"),
        ("agent/instructions/10-a.md", "\r\nFirst.\r\n"),
    ]);
    let report = load(dir.path());
    assert!(report.diagnostics.is_empty());
    assert_eq!(
        report.package.agents[0].instructions.prompt(),
        "You are root.\n\nFirst.\n\nSecond."
    );
}

#[test]
fn a_skill_with_an_error_is_left_out_and_its_siblings_stay() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/skills/good/SKILL.md",
            "---\nname: good\ndescription: d\n---\nB\n",
        ),
        ("agent/skills/bad/SKILL.md", "---\nname: bad\n---\nB\n"),
    ]);
    let report = load(dir.path());
    assert_eq!(report.errors().count(), 1);
    let skills: Vec<_> = report.package.agents[0]
        .skills
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(skills, ["good"]);
    assert!(!report.is_ok());
    assert!(report.into_package(Strictness::Lenient).is_err());
}

#[test]
fn diagnostics_carry_the_file_and_the_line() {
    let dir = write(&[(
        "agent/instructions.md",
        "---\nname: root\nmodel: alias\nflavour: mint\n---\nHi.\n",
    )]);
    let report = load(dir.path());
    let d = &report.diagnostics[0];
    assert_eq!(d.path, Path::new("agent/instructions.md"));
    assert_eq!(d.line, Some(4));
    assert_eq!(
        d.to_string(),
        "agent/instructions.md:4: warning: unknown key `flavour` is ignored"
    );

    let dir = write(&[(
        "agent/instructions.md",
        "---\nname: root\nmodel: [a\n---\nHi.\n",
    )]);
    let report = load(dir.path());
    assert!(
        report.diagnostics[0].line.is_some_and(|l| l >= 3),
        "{:?}",
        report.diagnostics
    );
}

#[test]
fn a_missing_root_is_an_io_error_and_classifies_as_not_found() {
    use adam_error::{Classify, ErrorClass};
    let err = Dir::new("/nonexistent/adam-agent-fs").load().unwrap_err();
    assert_eq!(err.class(), ErrorClass::NotFound);
}

#[test]
fn invalid_reports_classify_as_invalid() {
    use adam_error::{Classify, ErrorClass};
    let dir = write(&[("README.md", "x")]);
    let err = load(dir.path())
        .into_package(Strictness::Lenient)
        .unwrap_err();
    assert_eq!(err.class(), ErrorClass::Invalid);
    assert!(
        err.to_string().starts_with("1 error(s) in the agent files"),
        "{err}"
    );
}

#[test]
fn a_non_utf8_file_is_an_error_not_a_crash() {
    let dir = write(&[("agent/instructions.md", ROOT)]);
    std::fs::write(dir.path().join("agent/skills.md"), b"x").unwrap();
    std::fs::create_dir_all(dir.path().join("agent/skills/x")).unwrap();
    std::fs::write(
        dir.path().join("agent/skills/x/SKILL.md"),
        [0xff, 0xfe, 0x00],
    )
    .unwrap();
    let report = load(dir.path());
    assert!(
        report.errors().any(|d| d.message.contains("UTF-8")),
        "{:#?}",
        report.diagnostics
    );
}

#[cfg(unix)]
#[test]
fn symbolic_links_are_followed_one_step_and_cannot_loop() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        ("real/pdf/SKILL.md", SKILL),
        ("real/pdf/refs/a.md", "a"),
    ]);
    std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("agent/skills")).unwrap();
    // A link back to the skill directory from inside it must not make the walk loop.
    std::os::unix::fs::symlink(
        dir.path().join("real/pdf"),
        dir.path().join("real/pdf/refs/loop"),
    )
    .unwrap();
    let report = load(dir.path());
    assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
    let skill = &report.package.agents[0].skills[0];
    assert_eq!(skill.resources, ["refs/a.md"]);
}

#[test]
fn the_root_name_comes_from_the_frontmatter_then_from_the_default() {
    let dir = write(&[("agent/instructions.md", "Hi.\n")]);
    let none = Dir::new(dir.path()).load().unwrap();
    assert_eq!(none.diagnostics.len(), 1);
    assert!(
        none.diagnostics[0].message.contains("no name"),
        "{:?}",
        none.diagnostics
    );
    let with_default = Dir::new(dir.path()).default_name("my-pkg").load().unwrap();
    assert!(with_default.diagnostics.is_empty());
    assert_eq!(with_default.package.agents[0].name, "my-pkg");

    let dir = write(&[("agent/instructions.md", "---\nname: given\n---\nHi.\n")]);
    let report = Dir::new(dir.path()).default_name("my-pkg").load().unwrap();
    assert_eq!(report.package.agents[0].name, "given");
}

#[test]
fn an_invalid_root_name_falls_back_to_the_default_with_a_warning() {
    let dir = write(&[("agent/instructions.md", "---\nname: Not Valid\n---\nHi.\n")]);
    let report = load(dir.path());
    assert_eq!(report.diagnostics.len(), 1);
    assert_eq!(report.diagnostics[0].severity, Severity::Warning);
    assert_eq!(report.package.agents[0].name, "root");
    let report = Dir::new(dir.path()).load().unwrap();
    assert_eq!(report.diagnostics.len(), 1);
    assert_eq!(report.diagnostics[0].severity, Severity::Error);
    assert!(report.package.agents.is_empty());
}

#[test]
fn optional_is_a_boolean_that_defaults_to_false() {
    use adam_agent_fs::parse_mcp;
    use std::path::Path;

    let mut diagnostics = Vec::new();
    let config = parse_mcp(
        Path::new("mcp.json"),
        r#"{"mcpServers":{
            "a":{"type":"http","url":"https://a.example.com","optional":true},
            "b":{"type":"http","url":"https://b.example.com","optional":false},
            "c":{"command":"x","optional":true},
            "d":{"command":"y"}}}"#,
        &mut diagnostics,
    )
    .unwrap();
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    let optional: Vec<(&str, bool)> = config
        .servers
        .iter()
        .map(|(name, server)| (name.as_str(), server.is_optional()))
        .collect();
    assert_eq!(
        optional,
        [("a", true), ("b", false), ("c", true), ("d", false)]
    );

    // Not a boolean: the file is refused as a whole, with the line.
    let mut diagnostics = Vec::new();
    let config = parse_mcp(
        Path::new("mcp.json"),
        r#"{"mcpServers":{"a":{"type":"http","url":"https://a.example.com","optional":"yes"}}}"#,
        &mut diagnostics,
    );
    assert!(config.is_none());
    assert!(
        diagnostics[0].to_string().contains("invalid JSON"),
        "{diagnostics:#?}"
    );
}

#[test]
fn files_is_a_boolean_that_defaults_to_false_and_is_no_unknown_key() {
    use adam_agent_fs::parse_mcp;
    use std::path::Path;

    let mut diagnostics = Vec::new();
    let config = parse_mcp(
        Path::new("mcp.json"),
        r#"{"mcpServers":{
            "browser":{"type":"http","url":"http://127.0.0.1:9222/mcp","files":true},
            "search":{"type":"http","url":"https://s.example.com","files":false},
            "local":{"command":"x","files":true},
            "plain":{"command":"y"}}}"#,
        &mut diagnostics,
    )
    .unwrap();
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    let files: Vec<(&str, bool)> = config
        .servers
        .iter()
        .map(|(name, server)| (name.as_str(), server.shares_files()))
        .collect();
    assert_eq!(
        files,
        [
            ("browser", true),
            ("local", true),
            ("plain", false),
            ("search", false)
        ]
    );

    let mut diagnostics = Vec::new();
    let config = parse_mcp(
        Path::new("mcp.json"),
        r#"{"mcpServers":{"a":{"type":"http","url":"https://a.example.com","files":"yes"}}}"#,
        &mut diagnostics,
    );
    assert!(config.is_none(), "not a boolean: the file is refused");
    assert!(
        diagnostics[0].to_string().contains("invalid JSON"),
        "{diagnostics:#?}"
    );
}

#[test]
fn merging_adds_servers_and_refuses_a_name_both_have() {
    use adam_agent_fs::parse_mcp;
    use std::path::Path;

    let read = |text: &str| {
        let mut diagnostics = Vec::new();
        let config = parse_mcp(Path::new("mcp.json"), text, &mut diagnostics).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        config
    };
    let own = read(r#"{"mcpServers":{"github":{"type":"http","url":"http://127.0.0.1:8082/"}}}"#);
    let extra = read(
        r#"{"mcpServers":{"websearch":{"type":"http","url":"https://s.example.com","optional":true}}}"#,
    );
    let merged = own.clone().merged_with(extra.clone()).unwrap();
    let names: Vec<&str> = merged.servers.keys().map(String::as_str).collect();
    assert_eq!(names, ["github", "websearch"]);
    assert!(merged.servers["websearch"].is_optional());
    assert!(!merged.servers["github"].is_optional());

    let clash = read(
        r#"{"mcpServers":{"websearch":{"command":"a"},"github":{"command":"b"},"zed":{"command":"c"}}}"#,
    );
    assert_eq!(
        own.merged_with(clash).unwrap_err(),
        ["github"],
        "the clashing names, and nothing merged"
    );
}
