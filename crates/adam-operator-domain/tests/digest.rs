//! The digest: the same input always gives the same digest, a change of what a pod runs changes it,
//! and a change of what is only applied (scale, suspend, access, ownership, the deletion policy)
//! does not.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use adam_operator_domain::{ResolvedAgent, spec_digest};
use adam_operator_ports::{OwnerHandle, RuntimeSpec};
use common::{example, must_resolve, remove, set, typed};
use proptest::prelude::*;
use serde_json::{Value, json};

type Change = Box<dyn Fn(&mut Value, &mut Value)>;

/// The digests of the two examples. They change only when what the operator makes of the examples
/// changes, and a change is a rollout of every agent the operator runs, so it is made on purpose:
/// update these with the change that causes it, and say so in the commit.
const CODER_DIGEST: &str =
    "sha256:6518a3dd4c7b33c086cb44ef042a385fb378e56b75fb58b281124a9039695e95";

fn digest_of(name: &str, change: impl FnOnce(&mut Value, &mut Value)) -> String {
    must_resolve(name, change).digest
}

#[test]
fn the_digest_of_the_examples_is_pinned() {
    // The coder's digest is the one printed by `resolve` at the time this test was written.
    assert_eq!(digest_of("coder", |_, _| {}), CODER_DIGEST);
    let chat = digest_of("chat", |_, _| {});
    assert!(chat.starts_with("sha256:") && chat != CODER_DIGEST);
}

#[test]
fn resolving_twice_gives_the_same_everything() {
    for name in ["coder", "chat"] {
        let a = must_resolve(name, |_, _| {});
        let b = must_resolve(name, |_, _| {});
        assert_eq!(a, b);
        assert_eq!(a.runtime.digest, a.digest);
        assert_eq!(
            spec_digest(&a.runtime),
            a.digest,
            "the digest is of the spec it sits in"
        );
    }
}

#[test]
fn the_order_of_the_input_is_not_the_input() {
    // The examples written with their maps in another order are the same objects.
    let (service, config) = example("coder");
    let reorder = |v: &Value| -> Value {
        fn reverse(v: &Value) -> Value {
            match v {
                Value::Object(m) => {
                    // Rebuild from the text with the keys reversed.
                    let mut items: Vec<(String, Value)> =
                        m.iter().map(|(k, v)| (k.clone(), reverse(v))).collect();
                    items.reverse();
                    let text = format!(
                        "{{{}}}",
                        items
                            .iter()
                            .map(|(k, v)| format!("{}:{}", serde_json::to_string(k).unwrap(), v))
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                    serde_json::from_str(&text).unwrap()
                }
                Value::Array(a) => Value::Array(a.iter().map(reverse).collect()),
                other => other.clone(),
            }
        }
        reverse(v)
    };
    let (s, c) = typed(&reorder(&service), &reorder(&config));
    let r = adam_operator_domain::resolve(&s, &c, OwnerHandle::new("owner-token")).unwrap();
    assert_eq!(r.digest, CODER_DIGEST);
}

#[test]
fn what_is_only_applied_does_not_roll_the_pods() {
    let base = digest_of("coder", |_, _| {});
    let same: Vec<(&str, Change)> = vec![
        (
            "suspend",
            Box::new(|s, _| set(s, "/spec/suspend", json!(true))),
        ),
        (
            "deletion policy",
            Box::new(|s, _| set(s, "/spec/deletionPolicy", json!("Delete"))),
        ),
        (
            "description",
            Box::new(|s, _| set(s, "/spec/description", json!("something else"))),
        ),
        (
            "registry",
            Box::new(|s, _| set(s, "/spec/registry", json!({"title": "New", "tags": ["x"]}))),
        ),
        (
            "who may reach it",
            Box::new(|s, _| remove(s, "/spec/access/allowFrom")),
        ),
        (
            "who may reach it, differently",
            Box::new(|s, _| {
                set(
                    s,
                    "/spec/access/allowFrom",
                    json!([{"ipBlock": {"cidr": "10.0.0.0/8"}}]),
                )
            }),
        ),
    ];
    for (what, change) in same {
        assert_eq!(
            digest_of("coder", change),
            base,
            "{what} must not change the digest"
        );
    }
    // Scale: the same pods, more of them.
    let r = must_resolve("coder", |s, c| {
        set(s, "/spec/scaling/workers", json!(5));
        set(
            c,
            "/spec/harness/adam/coder/workspacePlacement",
            json!("isolated"),
        );
    });
    let r2 = must_resolve("coder", |s, c| {
        set(s, "/spec/scaling/workers", json!(2));
        set(
            c,
            "/spec/harness/adam/coder/workspacePlacement",
            json!("isolated"),
        );
    });
    assert_eq!(
        r.digest, r2.digest,
        "scaling.workers must not roll the running pods"
    );
    assert_ne!(r.digest, base, "a placement does");
}

#[test]
fn the_owner_is_not_part_of_the_digest() {
    let (s, c) = example("coder");
    let (s, c) = typed(&s, &c);
    let a = adam_operator_domain::resolve(&s, &c, OwnerHandle::new("uid-1")).unwrap();
    let b = adam_operator_domain::resolve(&s, &c, OwnerHandle::new("uid-2")).unwrap();
    assert_eq!(a.digest, b.digest);
    assert_ne!(a.runtime.owner, b.runtime.owner);
}

#[test]
fn what_a_pod_runs_rolls_the_pods() {
    let base = digest_of("coder", |_, _| {});
    let different: Vec<(&str, Change)> = vec![
        (
            "image",
            Box::new(|_, c| {
                set(
                    c,
                    "/spec/environment/image/ref",
                    json!("ghcr.io/vymalo/another-adam-rs/coder:sha-1111111"),
                )
            }),
        ),
        (
            "a variable",
            Box::new(|_, c| set(c, "/spec/harness/adam/coder/maxCheckCycles", json!(4))),
        ),
        (
            "extraEnv",
            Box::new(|_, c| set(c, "/spec/extraEnv/NEW", json!("1"))),
        ),
        (
            "the model",
            Box::new(|_, c| set(c, "/spec/model/model", json!("other"))),
        ),
        (
            "a context window",
            Box::new(|_, c| set(c, "/spec/model/contextWindow", json!(1_000_000))),
        ),
        (
            "another context window",
            Box::new(|_, c| set(c, "/spec/model/contextWindow", json!(131_072))),
        ),
        (
            "an MCP server's URL",
            Box::new(|_, c| {
                set(
                    c,
                    "/spec/tools/mcpServers/context7/url",
                    json!("https://elsewhere.example.com/mcp"),
                )
            }),
        ),
        (
            "a header's secret",
            Box::new(|_, c| {
                set(
                    c,
                    "/spec/tools/mcpServers/context7/headers/Authorization/secretRef/name",
                    json!("other-secrets"),
                )
            }),
        ),
        (
            "the sidecar's port",
            Box::new(|_, c| set(c, "/spec/tools/githubMcp/port", json!(9091))),
        ),
        (
            "a volume's size",
            Box::new(|_, c| {
                set(
                    c,
                    "/spec/environment/volumes/0/source/persistent/size",
                    json!("30Gi"),
                )
            }),
        ),
        (
            "the security context",
            Box::new(|_, c| set(c, "/spec/security/fsGroup", json!(10002))),
        ),
        (
            "the grace period",
            Box::new(|_, c| {
                set(
                    c,
                    "/spec/environment/terminationGracePeriodSeconds",
                    json!(60),
                )
            }),
        ),
        (
            "the public URL",
            Box::new(|s, _| {
                set(
                    s,
                    "/spec/interfaces/a2a/publicUrl",
                    json!("https://coder.example.com/"),
                )
            }),
        ),
        (
            "the database",
            Box::new(|s, _| {
                set(
                    s,
                    "/spec/store/postgres/secretRef/name",
                    json!("another-db"),
                )
            }),
        ),
        (
            "resources",
            Box::new(|_, c| set(c, "/spec/environment/resources/limits/memory", json!("7Gi"))),
        ),
        (
            "the topology",
            Box::new(|s, _| set(s, "/spec/scaling/topology", json!("split"))),
        ),
    ];
    let mut seen = std::collections::BTreeSet::from([base]);
    for (what, change) in different {
        let d = digest_of("coder", change);
        assert!(
            seen.insert(d),
            "{what} must change the digest, and to a digest no other change gave"
        );
    }
}

#[test]
fn a_folders_content_rolls_the_pods() {
    let one = digest_of("chat", |_, c| {
        set(
            c,
            "/spec/harness/adam/agent/folder/files/instructions.md",
            json!("Your name is A.\n"),
        )
    });
    let two = digest_of("chat", |_, c| {
        set(
            c,
            "/spec/harness/adam/agent/folder/files/instructions.md",
            json!("Your name is B.\n"),
        )
    });
    let extra = digest_of("chat", |_, c| {
        set(
            c,
            "/spec/harness/adam/agent/folder/files/skills~1x~1SKILL.md",
            json!("# x"),
        )
    });
    assert_ne!(one, two);
    assert_ne!(one, extra);
}

#[test]
fn a_referenced_config_map_is_tracked_by_its_name_only() {
    // The operator cannot read it (it is pure, and has no Secret or ConfigMap rights), so a change
    // of its content is not a rollout: a rename is. The limit is in the README.
    let named = |n: &str| {
        digest_of("chat", |_, c| {
            remove(c, "/spec/harness/adam/agent/folder/files");
            set(
                c,
                "/spec/harness/adam/agent/folder/configMapRef",
                json!({"name": n}),
            );
        })
    };
    assert_eq!(named("chat-agent"), named("chat-agent"));
    assert_ne!(named("chat-agent"), named("chat-agent-2"));
}

// ------------------------------------------------------------ properties

fn small() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9]{1,10}"
}

fn env_name() -> impl Strategy<Value = String> {
    "[A-Z][A-Z0-9_]{2,10}".prop_filter("not a name the operator sets", |n| {
        !adam_operator_domain::contract::env::OPERATOR_SET.contains(&n.as_str())
    })
}

fn resolve_with(
    extra: &[(String, String)],
    files: &[(String, String)],
    workers: u32,
    owner: &str,
) -> ResolvedAgent {
    let (mut s, mut c) = example("chat");
    let env: serde_json::Map<String, Value> =
        extra.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    set(&mut c, "/spec/extraEnv", Value::Object(env));
    for (name, content) in files {
        set(
            &mut c,
            &format!("/spec/harness/adam/agent/folder/files/skills~1{name}~1SKILL.md"),
            json!(content),
        );
    }
    set(&mut s, "/spec/scaling/workers", json!(workers));
    let (s, c) = typed(&s, &c);
    adam_operator_domain::resolve(&s, &c, OwnerHandle::new(owner)).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The same input, the same digest, whatever the input is, and the digest is of the spec.
    #[test]
    fn the_same_input_gives_the_same_digest(
        extra in proptest::collection::vec((env_name(), small()), 0..6),
        files in proptest::collection::vec((small(), small()), 0..4),
        workers in 1u32..8,
    ) {
        let a = resolve_with(&extra, &files, workers, "x");
        let b = resolve_with(&extra, &files, workers, "x");
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(spec_digest(&a.runtime), a.digest.clone());
        prop_assert!(a.digest.starts_with("sha256:"));
    }

    /// The order the entries are given in is not part of the input.
    #[test]
    fn the_order_of_extra_env_is_irrelevant(
        extra in proptest::collection::vec((env_name(), small()), 0..6),
    ) {
        let mut reversed = extra.clone();
        reversed.reverse();
        // A repeated name keeps its last value: make the two lists agree on that.
        let mut dedup = std::collections::BTreeMap::new();
        for (k, v) in &extra { dedup.insert(k.clone(), v.clone()); }
        let forward: Vec<_> = dedup.clone().into_iter().collect();
        let backward: Vec<_> = dedup.into_iter().rev().collect();
        let a = resolve_with(&forward, &[], 1, "x");
        let b = resolve_with(&backward, &[], 1, "x");
        prop_assert_eq!(a.digest, b.digest);
    }

    /// Scale and ownership never move the digest.
    #[test]
    fn scale_and_owner_do_not_move_it(
        extra in proptest::collection::vec((env_name(), small()), 0..4),
        w1 in 1u32..10, w2 in 1u32..10, o1 in small(), o2 in small(),
    ) {
        let a = resolve_with(&extra, &[], w1, &o1);
        let b = resolve_with(&extra, &[], w2, &o2);
        prop_assert_eq!(a.digest, b.digest);
    }

    /// A different environment is a different digest.
    #[test]
    fn a_different_variable_is_a_different_digest(name in env_name(), v1 in small(), v2 in small()) {
        prop_assume!(v1 != v2);
        let a = resolve_with(&[(name.clone(), v1)], &[], 1, "x");
        let b = resolve_with(&[(name, v2)], &[], 1, "x");
        prop_assert_ne!(a.digest, b.digest);
    }

    /// A different folder is a different digest, and a different ConfigMap name.
    #[test]
    fn a_different_folder_is_a_different_digest(name in small(), c1 in small(), c2 in small()) {
        prop_assume!(c1 != c2);
        let a = resolve_with(&[], &[(name.clone(), c1)], 1, "x");
        let b = resolve_with(&[], &[(name, c2)], 1, "x");
        prop_assert_ne!(a.digest, b.digest);
        prop_assert_ne!(&a.runtime.file_sets[0].name, &b.runtime.file_sets[0].name);
    }
}

#[test]
fn a_spec_that_is_not_ours_still_has_a_digest() {
    // `spec_digest` is a function of the spec alone: a provider or a test can recompute it.
    let r = must_resolve("coder", |_, _| {});
    let mut spec: RuntimeSpec = r.runtime.clone();
    spec.digest.clear();
    spec.suspend = true;
    spec.workloads[0].replicas = 9;
    assert_eq!(spec_digest(&spec), r.digest);
}
