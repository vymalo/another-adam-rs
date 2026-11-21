//! ADR 0052's cutover, as tests: the Podman devcontainer path is gone, not dormant.
//!
//! Each test here failed on the code before the cutover (slice 3): the coder accepted
//! `DEVCONTAINER_RUNTIME=podman`, the removed variables were not listed, the crate and the two
//! image tools were there, and `rebuild_environment` was offered.
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests assert by unwrapping

use std::collections::HashMap;
use std::path::Path;

use adam_coder::Config;

/// A configuration of the control plane: the role that needs the fewest variables. The removed
/// variables are read (and warned about) by every role, and `DEVCONTAINER_RUNTIME=podman` is
/// refused before anything connects.
fn control_plane(extra: &[(&str, &str)]) -> Result<Config, adam_coder::ConfigError> {
    let mut vars: HashMap<String, String> = [
        ("ROLE", "control-plane"),
        ("DATABASE_URL", "postgres://u:p@127.0.0.1:1/x"),
        ("A2A_BEARER_TOKENS", "t"),
        ("PUBLIC_URL", "http://coder.test/"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    for (name, value) in extra {
        vars.insert((*name).to_owned(), (*value).to_owned());
    }
    Config::from_lookup(|name| vars.get(name).cloned())
}

/// The eleven variables of the Podman path (ADR 0052's breaking-changes table) and Podman's own
/// `CONTAINER_HOST`: each one that is set is ignored and named, so the process warns about it.
#[test]
fn every_removed_devcontainer_variable_is_ignored_and_named() {
    let removed = [
        "DEVCONTAINER_DEFAULT_IMAGE",
        "DEVCONTAINER_NETWORK",
        "DEVCONTAINER_DEPLOYMENT_ID",
        "DEVCONTAINER_UP_TIMEOUT_SECS",
        "DEVCONTAINER_SETUP_TIMEOUT_SECS",
        "DEVCONTAINER_PREPULL",
        "DEVCONTAINER_CLI",
        "DEVCONTAINER_PODMAN",
        "DEVCONTAINER_CACHE_DIR",
        "CONTAINER_HOST",
    ];
    assert!(control_plane(&[]).unwrap().ignored.is_empty());
    for name in removed {
        let config = control_plane(&[(name, "anything")])
            .unwrap_or_else(|e| panic!("{name} is ignored, not refused: {e}"));
        assert!(
            config.ignored.contains(&name),
            "{name} is not in {:?}",
            config.ignored
        );
    }
    // `DEVCONTAINER_RUNTIME` is the exception: a value that asks for the runtime is refused, every
    // other one is ignored like the rest.
    let config = control_plane(&[("DEVCONTAINER_RUNTIME", "off")]).unwrap();
    assert!(config.ignored.contains(&"DEVCONTAINER_RUNTIME"));
}

/// A deployment that asked for Podman does not start and silently run its commands in the coder:
/// exit 78, in words that name the variable and ADR 0052. Every role refuses, because the
/// deployment is the same for all of them.
#[test]
fn asking_for_podman_is_refused_naming_adr_0052() {
    for role in ["control-plane", "worker", "all"] {
        let error = control_plane(&[("ROLE", role), ("DEVCONTAINER_RUNTIME", " Podman ")])
            .expect_err("a deployment that asked for Podman does not start");
        let all = error.problems.join("\n");
        let refusal = error
            .problems
            .iter()
            .find(|p| p.contains("ADR 0052"))
            .unwrap_or_else(|| panic!("{role}: no refusal naming ADR 0052: {all}"));
        assert!(
            refusal.contains("DEVCONTAINER_RUNTIME"),
            "{role}: {refusal}"
        );
        assert!(
            !refusal.contains("  "),
            "{role}: a message has no runs of spaces: {refusal}"
        );
        assert_eq!(
            adam_coder::exit_code(&anyhow::Error::from(error)),
            78,
            "{role}: a configuration error is EX_CONFIG"
        );
    }
}

/// The crate, its types and the tool that made an environment again are gone, not dormant: a
/// source scan is the check a compile cannot make (the absence of a name).
#[test]
fn the_devcontainer_code_is_gone_from_the_tree() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    assert!(
        !root.join("crates/adam-devcontainer").exists(),
        "the crate is deleted"
    );
    for (file, needle) in [
        ("crates/adam-workspace/src/environment.rs", "DevContainer"),
        ("crates/adam-workspace/src/environment.rs", "fn rebuild"),
        ("crates/adam-runtime/src/usage.rs", "Devcontainer"),
        ("crates/adam-code-tools/src/sandbox.rs", "Devcontainer"),
        (
            "bin/adam-coder/agent/instructions.md",
            "rebuild_environment",
        ),
    ] {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        assert!(!text.contains(needle), "{file} still names {needle:?}");
    }
    assert!(
        !root
            .join("bin/adam-coder/tests/fixtures/tool-specs/rebuild_environment.json")
            .exists(),
        "the tool's fixture is deleted with it"
    );
}

/// The image carries neither the devcontainer CLI nor Podman's client, and no variable of theirs:
/// the smoke test in CI proves the image still builds and starts.
#[test]
fn the_coder_image_carries_neither_tool() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dockerfile = std::fs::read_to_string(root.join("docker/coder/Dockerfile")).unwrap();
    let lower = dockerfile.to_lowercase();
    assert!(!lower.contains("devcontainer"), "the CLI is removed");
    assert!(!lower.contains("podman"), "Podman's client is removed");
    assert!(
        !dockerfile.contains("DEVCONTAINER_"),
        "no setting of the removed path is baked in"
    );
}

/// `adam-exec` is a run pod's script now: one source, in the crate that runs it, copied by both
/// images (the coder's and the run-pod test image).
#[test]
fn adam_exec_belongs_to_adam_env_kubernetes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = root.join("crates/adam-env-kubernetes/src/adam-exec.sh");
    assert!(script.is_file(), "the script lives in its new crate");
    for file in [
        "docker/coder/Dockerfile",
        "docker/coder/test/run-pod.Dockerfile",
    ] {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        assert!(
            text.contains("crates/adam-env-kubernetes/src/adam-exec.sh"),
            "{file} copies the script from its crate"
        );
    }
    let ignore =
        std::fs::read_to_string(root.join("docker/coder/test/run-pod.Dockerfile.dockerignore"))
            .unwrap();
    assert!(
        ignore.contains("crates/adam-env-kubernetes/src/adam-exec.sh"),
        "the build context carries the script"
    );
}
