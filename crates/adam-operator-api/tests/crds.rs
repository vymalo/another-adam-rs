//! The generated CRDs: identity, and every CEL rule of the design.

#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may

use adam_operator_api::{AgentConfig, AgentService, GROUP, VERSION, crds};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::core::CustomResourceExt;
use serde_json::Value;

fn schema_of(crd: &CustomResourceDefinition) -> Value {
    let schema = crd.spec.versions[0]
        .schema
        .as_ref()
        .and_then(|s| s.open_api_v3_schema.as_ref())
        .expect("a derived CRD has a schema");
    serde_json::to_value(schema).expect("a schema serialises")
}

/// Every `(rule, message)` of `x-kubernetes-validations` in a schema, at any depth.
fn validations(v: &Value, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(map) => {
            if let Some(Value::Array(rules)) = map.get("x-kubernetes-validations") {
                for r in rules {
                    out.push((
                        r["rule"].as_str().unwrap_or_default().to_owned(),
                        r["message"].as_str().unwrap_or_default().to_owned(),
                    ));
                }
            }
            map.values().for_each(|c| validations(c, out));
        }
        Value::Array(items) => items.iter().for_each(|c| validations(c, out)),
        _ => {}
    }
}

fn all_rules() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for crd in crds() {
        validations(&schema_of(&crd), &mut out);
    }
    out
}

#[test]
fn both_kinds_are_namespaced_v1alpha1_of_the_platform_group() {
    let all = crds();
    let names: Vec<_> = all
        .iter()
        .filter_map(|c| c.metadata.name.as_deref())
        .collect();
    assert_eq!(
        names,
        [
            "agentconfigs.agents.vymalo.com",
            "agentservices.agents.vymalo.com"
        ]
    );
    for crd in &all {
        assert_eq!(crd.spec.group, GROUP);
        assert_eq!(crd.spec.group, "agents.vymalo.com");
        assert_eq!(crd.spec.scope, "Namespaced");
        assert_eq!(crd.spec.versions.len(), 1);
        let v = &crd.spec.versions[0];
        assert_eq!(v.name, VERSION);
        assert_eq!(v.name, "v1alpha1");
        assert!(v.served && v.storage);
        assert!(
            v.subresources.as_ref().is_some_and(|s| s.status.is_some()),
            "status subresource"
        );
    }
    assert_eq!(AgentService::crd().spec.names.kind, "AgentService");
    assert_eq!(AgentConfig::crd().spec.names.kind, "AgentConfig");
}

/// The CEL rules of the CRD. `(rule, message)`.
const SPEC_RULES: &[(&str, &str)] = &[
    // interfaces.responses.enabled and interfaces.mcp.enabled must be false (one field type, used twice)
    (
        "self == false",
        "v0 serves A2A only: this interface cannot be enabled",
    ),
    // interfaces.a2a is on unless said otherwise, and then it needs its token (no token, no server)
    (
        "(has(self.enabled) && !self.enabled) || has(self.bearerTokensSecretRef)",
        "interfaces.a2a needs bearerTokensSecretRef: no token, no server",
    ),
    // exactly one of store.postgres.secretRef and store.postgres.cnpg
    (
        "has(self.secretRef) != has(self.cnpg)",
        "exactly one of store.postgres.secretRef and store.postgres.cnpg",
    ),
    // exactly one of github.app and github.token
    (
        "has(self.app) != has(self.token)",
        "exactly one of github.app and github.token",
    ),
    // exactly one of app.installationId and app.owners
    (
        "has(self.installationId) != has(self.owners)",
        "exactly one of github.app.installationId and github.app.owners",
    ),
    // exactly one of agent.folder.files and agent.folder.configMapRef
    (
        "has(self.files) != has(self.configMapRef)",
        "exactly one of agent.folder.files and agent.folder.configMapRef",
    ),
    // exactly one of folder and embedded
    (
        "has(self.folder) != has(self.embedded)",
        "exactly one of agent.folder and agent.embedded",
    ),
    // binary: adam-coder needs embedded and the coder block
    (
        "self.binary != 'adam-coder' || (has(self.agent.embedded) && has(self.coder))",
        "binary adam-coder needs agent.embedded and the coder block",
    ),
    // binary: adam-agent needs folder and no coder block
    (
        "self.binary != 'adam-agent' || (has(self.agent.folder) && !has(self.coder))",
        "binary adam-agent needs agent.folder and no coder block",
    ),
    // scaling.front only with topology: split
    (
        "!has(self.front) || self.topology == 'split'",
        "scaling.front is only allowed with topology: split",
    ),
    // beyond the list, same shape: a model endpoint is a value or a Secret key
    (
        "has(self.value) != has(self.secretRef)",
        "exactly one of model.baseUrl.value and model.baseUrl.secretRef",
    ),
];

#[test]
fn the_generated_crds_contain_every_cel_rule_of_the_spec() {
    let found = all_rules();
    for (rule, message) in SPEC_RULES {
        assert!(
            found.iter().any(|(r, m)| r == rule && m == message),
            "missing rule {rule:?} with message {message:?}; the CRDs have {found:#?}"
        );
    }
}

#[test]
fn the_crds_contain_no_cel_rule_the_tests_do_not_know() {
    let known: Vec<_> = SPEC_RULES.iter().map(|(r, _)| *r).collect();
    for (rule, _) in all_rules() {
        assert!(
            known.contains(&rule.as_str()),
            "a CEL rule with no test: {rule:?}"
        );
    }
}

#[test]
fn a_secret_is_never_a_value() {
    // No string field of either schema is named like a secret value: a secret is a `secretRef`
    // object (a name and a key), or the object that holds one (`github.token`).
    fn leaks(v: &Value, path: &str, out: &mut Vec<String>) {
        match v {
            Value::Object(map) => {
                if let Some(Value::Object(props)) = map.get("properties") {
                    for (name, schema) in props {
                        let l = name.to_lowercase();
                        let secretish = ["token", "password", "apikey", "privatekey", "bearer"]
                            .iter()
                            .any(|w| l.contains(w));
                        let is_string =
                            schema.get("type").and_then(Value::as_str) == Some("string");
                        if secretish && is_string {
                            out.push(format!("{path}.{name}"));
                        }
                    }
                }
                map.iter()
                    .for_each(|(k, c)| leaks(c, &format!("{path}.{k}"), out));
            }
            Value::Array(items) => items.iter().for_each(|c| leaks(c, path, out)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    for crd in crds() {
        leaks(&schema_of(&crd), "", &mut found);
    }
    assert!(
        found.is_empty(),
        "string fields that look like secret values: {found:?}"
    );
}

#[test]
fn a2a_is_on_unless_said_otherwise() {
    // The schema's default, which the API server applies, and the type's serde default agree: true.
    let service = schema_of(&AgentService::crd());
    let interfaces = &service["properties"]["spec"]["properties"]["interfaces"];
    assert_eq!(
        interfaces["default"]["a2a"]["enabled"], true,
        "{interfaces}"
    );
    assert_eq!(
        interfaces["properties"]["a2a"]["properties"]["enabled"]["default"], true,
        "{interfaces}"
    );
    let spec: adam_operator_api::AgentServiceSpec = serde_json::from_value(serde_json::json!({
        "configRef": {"name": "c"},
        "store": {"postgres": {"secretRef": {"name": "db", "key": "uri"}}}
    }))
    .expect("a spec with no interfaces");
    assert!(spec.interfaces.a2a.enabled);
    assert!(spec.interfaces.a2a.bearer_tokens_secret_ref.is_none());
    assert!(!spec.interfaces.mcp.enabled && !spec.interfaces.responses.enabled);
}
