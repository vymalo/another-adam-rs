//! The binary's contract: `crdgen` prints exactly the checked-in CRDs, and `run` says what it needs.

#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may

use std::path::Path;
use std::process::Command;

fn operator() -> Command {
    Command::new(env!("CARGO_BIN_EXE_adam-operator"))
}

/// `deploy/operator-crds` reads its own copy (a Helm chart reads only its directory); it is the checked-in file, byte for byte
/// (render-check.sh checks it too, with helm).
#[test]
fn the_crds_chart_holds_the_checked_in_crds() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let checked_in =
        std::fs::read_to_string(root.join("deploy/crds/agents.vymalo.com.yaml")).unwrap();
    let copy =
        std::fs::read_to_string(root.join("deploy/operator-crds/files/agents.vymalo.com.yaml"))
            .unwrap_or_default();
    assert!(
        copy == checked_in,
        "deploy/operator-crds/files/agents.vymalo.com.yaml differs from deploy/crds/agents.vymalo.com.yaml. Copy it:\n  cp deploy/crds/agents.vymalo.com.yaml deploy/operator-crds/files/"
    );
}

#[test]
fn crdgen_matches_the_checked_in_crds() {
    let out = operator()
        .arg("crdgen")
        .output()
        .expect("runs the operator");
    assert!(
        out.status.success(),
        "crdgen failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let generated = String::from_utf8(out.stdout).expect("utf-8");

    let golden =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/crds/agents.vymalo.com.yaml");
    let checked_in = std::fs::read_to_string(&golden).unwrap_or_default();
    assert!(
        generated == checked_in,
        "{} differs from the output of `crdgen`. Regenerate it:\n  cargo run -q -p adam-operator -- crdgen > deploy/crds/agents.vymalo.com.yaml",
        golden.display()
    );
}

#[test]
fn crdgen_prints_one_document_per_kind() {
    let out = operator()
        .arg("crdgen")
        .output()
        .expect("runs the operator");
    let text = String::from_utf8(out.stdout).expect("utf-8");
    assert_eq!(
        text.matches("\n---\n").count(),
        2,
        "one `---` before each of the two CRDs"
    );
    assert!(text.contains("name: agentconfigs.agents.vymalo.com"));
    assert!(text.contains("name: agentservices.agents.vymalo.com"));
}

#[cfg(not(feature = "runtime-kubernetes"))]
#[test]
fn run_without_a_runtime_provider_says_what_to_rebuild_with() {
    let out = operator().arg("run").output().expect("runs the operator");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no runtime provider"), "{stderr}");
}

#[cfg(feature = "runtime-kubernetes")]
#[test]
fn run_without_a_cluster_fails_and_says_where_it_looked() {
    // No in-cluster environment, and a kubeconfig that is not there: nothing to connect to.
    let out = operator()
        .arg("run")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env("KUBECONFIG", "/nonexistent/kubeconfig")
        .env("HOME", "/nonexistent")
        .env("HEALTH_ADDR", "127.0.0.1:0")
        .env("METRICS_ADDR", "127.0.0.1:0")
        .output()
        .expect("runs the operator");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("connecting to the cluster"), "{stderr}");
}

#[test]
fn run_lists_its_settings_and_the_environment_variables_that_set_them() {
    let out = operator()
        .args(["run", "--help"])
        .output()
        .expect("runs the operator");
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    for variable in [
        "WATCH_NAMESPACE",
        "HEALTH_ADDR",
        "METRICS_ADDR",
        "POD_NAME",
        "ADAM_OPERATOR_CONCURRENCY",
        "ADAM_OPERATOR_RESYNC_SECS",
        "ADAM_OPERATOR_RESYNC_PENDING_SECS",
    ] {
        assert!(help.contains(variable), "{variable} is not in:\n{help}");
    }
    assert!(
        help.contains("0.0.0.0:8081") && help.contains("0.0.0.0:9090"),
        "{help}"
    );
}
