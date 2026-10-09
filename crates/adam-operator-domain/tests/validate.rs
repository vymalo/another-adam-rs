//! The rules of `validate`: each one refused with the field it is about, on the examples with one
//! thing changed; and the examples themselves are accepted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use adam_operator_domain::{ConfigIssue, validate};
use common::{
    assert_issue, documents, example, issues, must_resolve, remove, repo_root, set, typed,
};
use serde_json::{Value, json};

type Change = fn(&mut Value, &mut Value);

const CODER: &str = "/spec/harness/adam/coder";

#[test]
fn the_examples_are_valid() {
    for name in ["coder", "chat"] {
        let (s, c) = example(name);
        let (s, c) = typed(&s, &c);
        validate(&s, &c).unwrap_or_else(|i| panic!("{name}: {i:?}"));
    }
}

/// Each case: a name, a change to the coder example, the field the issue is about, and a word it
/// says.
#[test]
fn the_coder_rules() {
    let cases: Vec<(&str, Change, &str, &str)> = vec![
        // the chart's placements
        (
            "a2a-only",
            |_, c| set(c, &format!("{CODER}/workspacePlacement"), json!("a2a-only")),
            "workspacePlacement",
            "a2a-only is refused",
        ),
        (
            "nonsense",
            |_, c| set(c, &format!("{CODER}/workspacePlacement"), json!("sideways")),
            "workspacePlacement",
            "one of shared, affinity or isolated",
        ),
        (
            "many workers without placement",
            |s, _| set(s, "/spec/scaling/workers", json!(2)),
            "spec.scaling.workers",
            "need AgentConfig spec.harness.adam.coder.workspacePlacement",
        ),
        (
            "shared needs a shared claim",
            |s, c| {
                set(s, "/spec/scaling/workers", json!(2));
                set(c, &format!("{CODER}/workspacePlacement"), json!("shared"));
            },
            "volumes[work]",
            "perReplica: false",
        ),
        (
            "affinity needs a shared claim",
            |_, c| set(c, &format!("{CODER}/workspacePlacement"), json!("affinity")),
            "volumes[work]",
            "perReplica: false",
        ),
        (
            "isolated needs a claim per worker",
            |_, c| {
                set(c, &format!("{CODER}/workspacePlacement"), json!("isolated"));
                set(
                    c,
                    "/spec/environment/volumes/0/source/persistent/perReplica",
                    json!(false),
                );
            },
            "volumes[work]",
            "perReplica: true",
        ),
        (
            "isolated needs the work volume",
            |_, c| {
                set(c, &format!("{CODER}/workspacePlacement"), json!("isolated"));
                set(c, "/spec/environment/volumes", json!([]));
            },
            "spec.environment.volumes",
            "needs a volume named `work`",
        ),
        // githubMcp
        (
            "port 0",
            |_, c| set(c, "/spec/tools/githubMcp/port", json!(0)),
            "githubMcp.port",
            "1 to 65535",
        ),
        (
            "port too large",
            |_, c| set(c, "/spec/tools/githubMcp/port", json!(65536)),
            "githubMcp.port",
            "1 to 65535",
        ),
        // MCP servers
        (
            "url with a user name",
            |_, c| {
                set(
                    c,
                    "/spec/tools/mcpServers/context7/url",
                    json!("https://user:pw@mcp.example.com/"),
                )
            },
            "mcpServers[context7].url",
            "user name or password",
        ),
        (
            "url with a variable",
            |_, c| {
                set(
                    c,
                    "/spec/tools/mcpServers/context7/url",
                    json!("https://mcp.example.com/${KEY}"),
                )
            },
            "mcpServers[context7].url",
            "${VAR}",
        ),
        (
            "url that is not http",
            |_, c| {
                set(
                    c,
                    "/spec/tools/mcpServers/context7/url",
                    json!("ftp://mcp.example.com/"),
                )
            },
            "mcpServers[context7].url",
            "not an http or https URL",
        ),
        (
            "plain http to another machine",
            |_, c| set(c, "/spec/tools/allowInsecureHttp", json!(false)),
            "mcpServers[websearch].url",
            "allowInsecureHttp",
        ),
        (
            "header name",
            |_, c| {
                set(
                    c,
                    "/spec/tools/mcpServers/context7/headers/Bad Header",
                    json!({"secretRef": {"name": "s", "key": "K"}}),
                )
            },
            "headers[Bad Header]",
            "not an HTTP header name",
        ),
        (
            "prefix with a variable",
            |_, c| {
                set(
                    c,
                    "/spec/tools/mcpServers/context7/headers/Authorization/prefix",
                    json!("Bearer ${X}"),
                )
            },
            "prefix",
            "plain text",
        ),
        (
            "server name",
            |_, c| {
                set(
                    c,
                    "/spec/tools/mcpServers/bad name",
                    json!({"url": "https://x.example.com/"}),
                )
            },
            "mcpServers[bad name]",
            "letters, digits",
        ),
        (
            "header key that is not a variable name",
            |_, c| {
                set(
                    c,
                    "/spec/tools/mcpServers/context7/headers/Authorization/secretRef/key",
                    json!("api-key.txt"),
                )
            },
            "header Secret key",
            "must be a variable name",
        ),
        (
            "header key the operator sets",
            |_, c| {
                set(
                    c,
                    "/spec/tools/mcpServers/context7/headers/Authorization/secretRef/key",
                    json!("MODEL_API_KEY"),
                )
            },
            "header Secret key",
            "operator sets",
        ),
        (
            "one variable, two secrets",
            |_, c| {
                set(
                    c,
                    "/spec/tools/mcpServers/context7/headers/Authorization/secretRef",
                    json!({"name": "other", "key": "SEARCH_MCP_TOKEN"}),
                )
            },
            "header Secret key",
            "different Secrets",
        ),
        // extraBody
        (
            "extraBody sets a member the runtime owns",
            |_, c| set(c, "/spec/model/extraBody", json!({"stream": false})),
            "extraBody.stream",
            "runtime owns",
        ),
        (
            "extraBody sets another member the runtime owns",
            |_, c| set(c, "/spec/model/extraBody", json!({"tools": []})),
            "extraBody.tools",
            "runtime owns",
        ),
        // contextWindow: the schema's bounds, again
        (
            "a context window of 0",
            |_, c| set(c, "/spec/model/contextWindow", json!(0)),
            "spec.model.contextWindow",
            "from 1 to 9007199254740991",
        ),
        (
            "a context window over 2^53 - 1",
            |_, c| {
                set(
                    c,
                    "/spec/model/contextWindow",
                    json!(9_007_199_254_740_992_u64),
                )
            },
            "spec.model.contextWindow",
            "from 1 to 9007199254740991",
        ),
        // extraEnv
        (
            "extraEnv names an operator variable",
            |_, c| set(c, "/spec/extraEnv/MODEL", json!("x")),
            "extraEnv.MODEL",
            "operator sets",
        ),
        (
            "extraEnv names a variable the operator sets only sometimes",
            |_, c| set(c, "/spec/extraEnv/GITHUB_TOKEN", json!("x")),
            "extraEnv.GITHUB_TOKEN",
            "operator sets",
        ),
        (
            "extraEnv names the context window, which model.contextWindow sets",
            |_, c| set(c, "/spec/extraEnv/MODEL_CONTEXT_WINDOW", json!("1000000")),
            "extraEnv.MODEL_CONTEXT_WINDOW",
            "operator sets",
        ),
        (
            "extraEnv names a header variable",
            |_, c| set(c, "/spec/extraEnv/SEARCH_MCP_TOKEN", json!("x")),
            "extraEnv.SEARCH_MCP_TOKEN",
            "operator sets",
        ),
        (
            "extraEnv names a non-variable",
            |_, c| set(c, "/spec/extraEnv/not a name", json!("x")),
            "extraEnv.not a name",
            "not a variable name",
        ),
        // GitHub
        (
            "installation id 0",
            |_, c| {
                let app = format!("{CODER}/github/app");
                remove(c, &format!("{app}/owners"));
                set(c, &format!("{app}/installationId"), json!(0));
            },
            "installationId",
            "positive",
        ),
        (
            "owner `*` beside another",
            |_, c| {
                set(
                    c,
                    &format!("{CODER}/github/app/owners"),
                    json!(["*", "vymalo"]),
                )
            },
            "github.app.owners",
            "`*`",
        ),
        (
            "owner with a comma",
            |_, c| set(c, &format!("{CODER}/github/app/owners"), json!(["a,b"])),
            "github.app.owners",
            "no comma or space",
        ),
        (
            "no owners",
            |_, c| set(c, &format!("{CODER}/github/app/owners"), json!([])),
            "github.app.owners",
            "at least one",
        ),
        (
            "api url",
            |_, c| set(c, &format!("{CODER}/githubApiUrl"), json!("api.github.com")),
            "githubApiUrl",
            "not an http or https URL",
        ),
        (
            "a host with a comma",
            |_, c| {
                set(
                    c,
                    &format!("{CODER}/allowedRepoHosts"),
                    json!(["a.com,b.com"]),
                )
            },
            "allowedRepoHosts[0]",
            "no comma or space",
        ),
        (
            "author without email",
            |_, c| {
                set(
                    c,
                    &format!("{CODER}/gitAuthor"),
                    json!({"name": "x", "email": ""}),
                )
            },
            "gitAuthor",
            "both needed",
        ),
        // the model
        (
            "model url with a password",
            |_, c| {
                set(
                    c,
                    "/spec/model/baseUrl",
                    json!({"value": "https://u:p@gw.example.com/v1"}),
                )
            },
            "model.baseUrl.value",
            "user name or password",
        ),
        (
            "empty model",
            |_, c| set(c, "/spec/model/model", json!(" ")),
            "model.model",
            "empty",
        ),
        (
            "empty secret key",
            |_, c| set(c, "/spec/model/apiKeySecretRef/key", json!("")),
            "apiKeySecretRef",
            "name and a key",
        ),
        // the environment
        (
            "image with a space",
            |_, c| set(c, "/spec/environment/image/ref", json!("a b")),
            "image.ref",
            "no space",
        ),
        (
            "bad quantity",
            |_, c| set(c, "/spec/environment/resources/requests/cpu", json!("lots")),
            "resources.requests.cpu",
            "not a quantity",
        ),
        (
            "volume name",
            |_, c| set(c, "/spec/environment/volumes/0/name", json!("Work_Dir")),
            "volumes[Work_Dir]",
            "DNS label",
        ),
        (
            "reserved volume name",
            |_, c| {
                set(
                    c,
                    "/spec/environment/volumes/1",
                    json!({"name": "extra-mcp", "scope": "agent", "mountPath": "/x", "source": {"persistent": {"size": "1Gi"}}}),
                )
            },
            "volumes[extra-mcp]",
            "uses this volume name itself",
        ),
        (
            "duplicate volume name",
            |_, c| {
                set(
                    c,
                    "/spec/environment/volumes/1",
                    json!({"name": "work", "scope": "agent", "mountPath": "/other", "source": {"persistent": {"size": "1Gi"}}}),
                )
            },
            "volumes[work]",
            "two volumes",
        ),
        (
            "volume scope",
            |_, c| set(c, "/spec/environment/volumes/0/scope", json!("project")),
            ".scope",
            "v0 uses `agent`",
        ),
        (
            "relative mount path",
            |_, c| set(c, "/spec/environment/volumes/0/mountPath", json!("work")),
            "mountPath",
            "absolute path",
        ),
        (
            "mount over the operator's",
            |_, c| {
                set(
                    c,
                    "/spec/environment/volumes/0/mountPath",
                    json!("/etc/adam"),
                )
            },
            "mountPath",
            "overlaps /etc/adam/agent",
        ),
        (
            "mounts that overlap",
            |_, c| {
                set(
                    c,
                    "/spec/environment/volumes/1",
                    json!({"name": "more", "scope": "agent", "mountPath": "/work/more", "source": {"persistent": {"size": "1Gi"}}}),
                )
            },
            "volumes[more].mountPath",
            "overlaps the mount /work",
        ),
        (
            "volume size",
            |_, c| {
                set(
                    c,
                    "/spec/environment/volumes/0/source/persistent/size",
                    json!("big"),
                )
            },
            "persistent.size",
            "not a size",
        ),
        // the service
        (
            "config name",
            |s, _| set(s, "/spec/configRef/name", json!("other")),
            "configRef.name",
            "\"other\"",
        ),
        (
            "service name",
            |s, _| set(s, "/metadata/name", json!("Coder_1")),
            "metadata.name",
            "DNS label",
        ),
        (
            "service name too long",
            |s, _| set(s, "/metadata/name", json!("x".repeat(53))),
            "metadata.name",
            "52",
        ),
        (
            "a2a off",
            |s, _| set(s, "/spec/interfaces/a2a/enabled", json!(false)),
            "a2a.enabled",
            "fail closed",
        ),
        (
            "no token",
            |s, _| remove(s, "/spec/interfaces/a2a/bearerTokensSecretRef"),
            "bearerTokensSecretRef",
            "no token, no server",
        ),
        (
            "public url",
            |s, _| {
                set(
                    s,
                    "/spec/interfaces/a2a/publicUrl",
                    json!("coder.example.com"),
                )
            },
            "publicUrl",
            "not an http or https URL",
        ),
        (
            "a surface v0 does not serve",
            |s, _| set(s, "/spec/interfaces/mcp/enabled", json!(true)),
            "spec.interfaces",
            "A2A only",
        ),
        (
            "front without split",
            |s, _| set(s, "/spec/scaling/front", json!({"replicas": 2})),
            "scaling.front",
            "split",
        ),
        (
            "cnpg size",
            |s, _| {
                remove(s, "/spec/store/postgres/secretRef");
                set(
                    s,
                    "/spec/store/postgres/cnpg",
                    json!({"instances": 1, "storage": {"size": "x"}}),
                );
            },
            "cnpg.storage.size",
            "not a size",
        ),
        (
            "allowFrom with an empty peer",
            |s, _| set(s, "/spec/access/allowFrom", json!([{}])),
            "allowFrom[0]",
            "needs a namespaceSelector",
        ),
        (
            "allowFrom with a range and a selector",
            |s, _| {
                set(
                    s,
                    "/spec/access/allowFrom",
                    json!([{"ipBlock": {"cidr": "10.0.0.0/8"}, "podSelector": {}}]),
                )
            },
            "allowFrom[0]",
            "cannot be combined",
        ),
        (
            "allowFrom with a bad operator",
            |s, _| {
                set(
                    s,
                    "/spec/access/allowFrom",
                    json!([{"podSelector": {"matchExpressions": [{"key": "a", "operator": "Maybe"}]}}]),
                )
            },
            "operator",
            "not In, NotIn",
        ),
        (
            "allowFrom In without values",
            |s, _| {
                set(
                    s,
                    "/spec/access/allowFrom",
                    json!([{"podSelector": {"matchExpressions": [{"key": "a", "operator": "In"}]}}]),
                )
            },
            "values",
            "at least one value",
        ),
    ];
    for (what, change, field, message) in cases {
        println!("case: {what}");
        let got = issues("coder", change);
        assert_issue(&got, field, message);
    }
}

#[test]
fn the_folder_rules() {
    let cases: Vec<(Change, &str, &str)> = vec![
        (
            |_, c| set(c, "/spec/harness/adam/agent/folder/files/..~1x", json!("a")),
            "files[../x]",
            "relative path",
        ),
        (
            |_, c| set(c, "/spec/harness/adam/agent/folder/files/~1abs", json!("a")),
            "files[",
            "relative path",
        ),
        (
            |_, c| {
                set(
                    c,
                    "/spec/harness/adam/agent/folder/files/skills~1..~1x",
                    json!("a"),
                )
            },
            "files[skills/../x]",
            "relative path",
        ),
        (
            |_, c| {
                set(
                    c,
                    "/spec/harness/adam/agent/folder/files",
                    json!({"README.md": "x"}),
                )
            },
            "folder.files",
            "no instructions.md",
        ),
        (
            |_, c| {
                set(
                    c,
                    "/spec/harness/adam/agent/folder/files/big.md",
                    json!("x".repeat(1024 * 1024)),
                )
            },
            "folder.files",
            "1 MiB",
        ),
        (
            |_, c| {
                remove(c, "/spec/harness/adam/agent/folder/files");
                set(
                    c,
                    "/spec/harness/adam/agent/folder/configMapRef",
                    json!({"name": " "}),
                );
            },
            "configMapRef.name",
            "empty",
        ),
    ];
    for (change, field, message) in cases {
        assert_issue(&issues("chat", change), field, message);
    }
}

#[test]
fn a_folder_may_hold_its_agent_in_agent_or_agents() {
    for path in [
        "agent/instructions.md",
        "agents/reviewer/instructions.md",
        "instructions.md",
    ] {
        must_resolve("chat", |_, c| {
            set(
                c,
                "/spec/harness/adam/agent/folder/files",
                json!({ path: "x" }),
            );
        });
    }
}

#[test]
fn every_problem_is_reported_not_only_the_first() {
    let got: Vec<ConfigIssue> = issues("coder", |s, c| {
        set(s, "/spec/scaling/workers", json!(2));
        set(c, "/spec/tools/githubMcp/port", json!(0));
        set(c, "/spec/extraEnv/MODEL", json!("x"));
    });
    assert!(got.len() >= 3, "{got:?}");
}

#[test]
fn issues_are_in_a_fixed_order_and_say_which_object() {
    let change: Change = |s, c| {
        set(c, "/spec/model/model", json!(""));
        set(s, "/spec/interfaces/a2a/publicUrl", json!("nope"));
    };
    let first = issues("coder", change);
    assert_eq!(first, issues("coder", change));
    assert!(
        first[0].field.starts_with("AgentService"),
        "the service first: {first:?}"
    );
    assert!(first.last().unwrap().field.starts_with("AgentConfig"));
    assert!(first[0].to_string().contains(": "));
}

#[test]
fn a_service_and_a_config_of_different_namespaces_are_refused() {
    let got = issues("coder", |_, c| {
        set(c, "/metadata/namespace", json!("elsewhere"))
    });
    assert_issue(&got, "configRef", "same namespace");
}

#[test]
fn an_object_without_a_namespace_is_refused() {
    assert_issue(
        &issues("coder", |s, _| remove(s, "/metadata/namespace")),
        "metadata.namespace",
        "no namespace",
    );
}

#[test]
fn placements_are_accepted_when_the_volumes_fit() {
    for (placement, per_replica, workers) in [
        ("", true, 1),
        ("shared", false, 2),
        ("affinity", false, 3),
        ("isolated", true, 2),
        ("ISOLATED ", true, 2),
    ] {
        must_resolve("coder", |s, c| {
            set(s, "/spec/scaling/workers", json!(workers));
            set(c, &format!("{CODER}/workspacePlacement"), json!(placement));
            set(
                c,
                "/spec/environment/volumes/0/source/persistent/perReplica",
                json!(per_replica),
            );
        });
    }
}

#[test]
fn a_folder_agent_with_many_workers_needs_no_placement() {
    // adam-agent runs are not pinned: any worker steps any run (adam-agent README, "Roles").
    must_resolve("chat", |s, _| set(s, "/spec/scaling/workers", json!(3)));
}

#[test]
fn the_shape_rules_of_the_crd_hold_without_an_api_server() {
    // The invalid examples are refused by the CEL rules of the CRD; `validate` repeats the shape
    // rules `resolve` relies on, so an object that never met an API server is refused too. The
    // example is paired with the valid object of the other kind.
    let (coder_service, coder_config) = example("coder");
    let mut covered = 0;
    let mut dir: Vec<_> = std::fs::read_dir(repo_root().join("deploy/operator/examples/invalid"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    dir.sort();
    for path in dir {
        let doc = documents(&path).remove(0);
        let (mut s, mut c) = (coder_service.clone(), coder_config.clone());
        match doc["kind"].as_str().unwrap() {
            "AgentService" => {
                s = doc.clone();
                set(&mut s, "/metadata/name", json!("coder"));
                set(
                    &mut s,
                    "/metadata/namespace",
                    json!("another-agentic-system"),
                );
                set(&mut s, "/spec/configRef/name", json!("coder"));
            }
            _ => {
                c = doc.clone();
                set(&mut c, "/metadata/name", json!("coder"));
                set(
                    &mut c,
                    "/metadata/namespace",
                    json!("another-agentic-system"),
                );
            }
        }
        let (s, c) = typed(&s, &c);
        let expect = std::fs::read_to_string(&path).unwrap();
        assert!(
            validate(&s, &c).is_err(),
            "{} was accepted by validate:\n{expect}",
            path.display()
        );
        covered += 1;
    }
    assert!(
        covered >= 19,
        "the invalid examples are all there: {covered}"
    );
}

#[test]
fn a_config_is_checked_by_itself() {
    let (s, c) = example("coder");
    let (_, config) = typed(&s, &c);
    adam_operator_domain::validate_config(&config).unwrap();

    // A rule of the config alone is found without a service...
    let (s, mut c) = example("coder");
    set(
        &mut c,
        &format!("{CODER}/workspacePlacement"),
        json!("sideways"),
    );
    let (_, config) = typed(&s, &c);
    let found = adam_operator_domain::validate_config(&config).unwrap_err();
    assert!(
        found.iter().any(|i| i.field.contains("workspacePlacement")),
        "{found:?}"
    );

    // ... and one of the service's (the name of the config it references) is not its business.
    let (mut s, c) = example("coder");
    set(&mut s, "/spec/configRef/name", json!("another"));
    let (service, config) = typed(&s, &c);
    adam_operator_domain::validate_config(&config).unwrap();
    assert!(validate(&service, &config).is_err());
}
