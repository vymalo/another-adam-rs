//! `DevContainer` against a stub Podman and a stub devcontainer CLI: what it calls, in which
//! order, with which arguments and environment; what it reports; what it refuses; how it ends.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;

use adam_devcontainer::Runtime;
use adam_error::{Classify, ErrorClass};
use adam_workspace::{
    EnvError, EnvKind, EnvSession, EnvStepState, Environment, ExecId, ExecSpec, SecretRef,
};
use common::{
    CONTAINER, DEFAULT_IMAGE, DEPLOYMENT, MODEL_KEY, RUN, Rig, Steps, devcontainer_file, flag,
    flags,
};
use serde_json::json;

const DEVBOX: &str = r#"// the devbox fixture
{
  "build": { "dockerfile": "Dockerfile" },
  "remoteUser": "vscode",
  "postCreateCommand": "devbox-tool --version > /tmp/devbox-ready",
}"#;

fn devbox_files() -> Vec<(&'static str, String)> {
    let mut files = devcontainer_file(DEVBOX).to_vec();
    files.push((".devcontainer/Dockerfile", "FROM scratch\n".to_owned()));
    files
}

fn as_refs<'a>(files: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    files.iter().map(|(n, c)| (*n, c.as_str())).collect()
}

/// A rig and a run whose first slot is the `devbox` repository, with the container's inspect ready.
async fn devbox() -> (Rig, adam_workspace::RunWorkspace, PathBuf) {
    let rig = Rig::new();
    let ws = rig.run(RUN);
    let slot = rig
        .repository(&ws, "devbox", &as_refs(&devbox_files()))
        .await;
    rig.inspect_for(&ws, true).await;
    (rig, ws, slot)
}

fn error_of(result: Result<Arc<dyn EnvSession>, EnvError>) -> EnvError {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(e) => e,
    }
}

// -------------------------------------------------------------------------------------- building

#[tokio::test]
async fn the_first_command_builds_the_environment_in_order_and_says_so() {
    let (rig, ws, slot) = devbox().await;
    let steps = Steps::default();
    let session = rig.env.ensure(&ws, &steps).await.unwrap();

    assert_eq!(
        rig.order(),
        [
            "cli read-configuration",
            "cli up",
            "podman inspect",
            "cli run-user-commands"
        ],
        "the file is read, the container made, looked at, and only then do the repository's own commands run"
    );
    let config = slot.join(".devcontainer/devcontainer.json");
    let override_file = rig
        .root
        .join("environments")
        .join(RUN)
        .join("devcontainer.json");
    for sub in ["read-configuration", "up", "run-user-commands"] {
        let call = &rig.cli(sub)[0];
        assert_eq!(flag(call, "--docker-path"), rig.settings.podman.to_str());
        assert_eq!(flag(call, "--workspace-folder"), slot.to_str());
        assert_eq!(flag(call, "--override-config"), override_file.to_str());
        assert_eq!(flag(call, "--config"), config.to_str());
        assert_eq!(
            flags(call, "--id-label"),
            [
                format!("adam.vymalo.com/run={RUN}"),
                format!("adam.vymalo.com/deployment={DEPLOYMENT}")
            ]
        );
        assert!(
            call.contains(&"--mount-workspace-git-root=false".to_owned()),
            "{call:?}"
        );
        assert_eq!(flag(call, "--log-format"), Some("json"), "{sub}");
    }
    let read = &rig.cli("read-configuration")[0];
    assert!(read.contains(&"--include-merged-configuration".to_owned()));
    let up = &rig.cli("up")[0];
    for wanted in ["--no-lockfile", "--skip-post-create"] {
        assert!(up.contains(&wanted.to_owned()), "{wanted}");
    }
    assert_eq!(flag(up, "--gpu-availability"), Some("none"));
    assert_eq!(
        flag(up, "--container-session-data-folder"),
        Some("/tmp/adam-devcontainer")
    );
    assert!(!up.contains(&"--remove-existing-container".to_owned()));
    assert_eq!(
        flag(
            &rig.cli("run-user-commands")[0],
            "--container-session-data-folder"
        ),
        Some("/tmp/adam-devcontainer")
    );
    assert_eq!(
        rig.podman("inspect")[0],
        ["inspect", "--type", "container", CONTAINER]
    );

    // One step, shown as it ran and ended.
    let latest = steps.latest();
    assert_eq!(latest.len(), 1, "{latest:?}");
    assert_eq!(
        latest[0].label,
        "Building the environment from .devcontainer/devcontainer.json (devbox)"
    );
    assert_eq!(latest[0].state, EnvStepState::Completed);
    assert!(
        latest[0]
            .detail
            .as_deref()
            .unwrap()
            .starts_with("ready in "),
        "{latest:?}"
    );
    assert_eq!(steps.all().first().unwrap().state, EnvStepState::Running);

    // The file the CLI was given, and what is kept.
    let made = rig.override_file(RUN);
    assert_eq!(made["workspaceFolder"], slot.to_str().unwrap());
    assert_eq!(
        made["workspaceMount"],
        format!(
            "type=bind,source={0},target={0}",
            rig.root.join("workspaces").join(RUN).display()
        )
    );
    assert_eq!(
        made["postCreateCommand"],
        "devbox-tool --version > /tmp/devbox-ready"
    );
    assert_eq!(made["runArgs"], json!(["--network=host"]));
    let state = rig.state(RUN);
    assert_eq!(state["phase"], "ready");
    assert_eq!(state["container_id"], CONTAINER);
    assert_eq!(state["image"], "localhost/vsc-slot-1a2b-uid:latest");
    assert_eq!(state["keep_id"], true);
    assert_eq!(state["config_source"], ".devcontainer/devcontainer.json");
    assert_eq!(state["slots"].as_array().unwrap().len(), 1);
    assert!(
        rig.root
            .join("environments")
            .join(RUN)
            .join("build.log")
            .is_file()
    );

    let described = session.describe();
    assert_eq!(
        described.kind,
        EnvKind::DevContainer {
            source: Some(config),
            image: "localhost/vsc-slot-1a2b-uid:latest".to_owned()
        }
    );

    // The next command reuses it: one look at the container, nothing else, no step.
    steps.clear();
    rig.env.ensure(&ws, &steps).await.unwrap();
    assert_eq!(rig.cli_calls().len(), 3);
    assert!(steps.all().is_empty());
}

#[tokio::test]
async fn a_repository_without_a_devcontainer_gets_the_default_image_and_says_so() {
    let rig = Rig::new();
    let ws = rig.run(RUN);
    rig.repository(&ws, "sandbox", &[]).await;
    rig.inspect_for(&ws, true).await;
    let steps = Steps::default();
    rig.env.ensure(&ws, &steps).await.unwrap();

    let latest = steps.latest();
    assert_eq!(
        latest[0].label,
        format!("Using the default environment ({DEFAULT_IMAGE})")
    );
    assert_eq!(latest[0].state, EnvStepState::Completed);
    assert_eq!(rig.override_file(RUN)["image"], DEFAULT_IMAGE);
    for sub in ["read-configuration", "up", "run-user-commands"] {
        assert_eq!(
            flag(&rig.cli(sub)[0], "--config"),
            None,
            "there is no file of the repository to point at"
        );
    }
    assert_eq!(rig.state(RUN)["config_source"], serde_json::Value::Null);
}

#[tokio::test]
async fn the_first_slot_decides_and_a_scratch_slot_counts() {
    // A scratch project first: the default image, whatever a later repository says.
    let rig = Rig::new();
    let ws = rig.run(RUN);
    rig.scratch(&ws, "notes", &[]).await;
    rig.repository(&ws, "devbox", &as_refs(&devbox_files()))
        .await;
    rig.inspect_for(&ws, true).await;
    let steps = Steps::default();
    rig.env.ensure(&ws, &steps).await.unwrap();
    assert_eq!(rig.override_file(RUN)["image"], DEFAULT_IMAGE);
    let latest = steps.latest();
    assert!(
        latest
            .iter()
            .any(|s| s.label.starts_with("Using the default environment")),
        "{latest:?}"
    );
    let ignored = latest
        .iter()
        .find(|s| s.id == "environment-ignored")
        .expect("a step says which is used");
    assert!(
        ignored.detail.as_deref().unwrap().contains("notes"),
        "{ignored:?}"
    );
    assert!(
        ignored.detail.as_deref().unwrap().contains("devbox"),
        "{ignored:?}"
    );

    // A repository first: its file, and a scratch project after it is mounted in.
    let rig = Rig::new();
    let ws = rig.run(RUN);
    rig.repository(&ws, "devbox", &as_refs(&devbox_files()))
        .await;
    rig.scratch(
        &ws,
        "notes",
        &[(".devcontainer/devcontainer.json", r#"{"image":"ignored"}"#)],
    )
    .await;
    rig.inspect_for(&ws, true).await;
    let steps = Steps::default();
    rig.env.ensure(&ws, &steps).await.unwrap();
    assert_eq!(rig.override_file(RUN)["build"]["dockerfile"], "Dockerfile");
    let mounts = rig.override_file(RUN)["mounts"].to_string();
    assert!(mounts.contains("/notes/.git"), "{mounts}");
    assert!(steps.latest().iter().any(|s| s.id == "environment-ignored"));
}

#[tokio::test]
async fn several_devcontainer_folders_use_the_first_and_a_step_says_so() {
    let rig = Rig::new();
    let ws = rig.run(RUN);
    rig.scratch(
        &ws,
        "multi",
        &[
            (
                ".devcontainer/rust/devcontainer.json",
                r#"{"image":"rust"}"#,
            ),
            (".devcontainer/go/devcontainer.json", r#"{"image":"go"}"#),
        ],
    )
    .await;
    rig.inspect_for(&ws, true).await;
    let steps = Steps::default();
    rig.env.ensure(&ws, &steps).await.unwrap();
    assert_eq!(rig.override_file(RUN)["image"], "go");
    let several = steps
        .latest()
        .into_iter()
        .find(|s| s.id == "environment-several")
        .unwrap();
    assert!(
        several
            .detail
            .unwrap()
            .contains("also found .devcontainer/rust/devcontainer.json")
    );
}

#[tokio::test]
async fn the_cli_and_podman_start_from_nothing_and_get_the_allow_list() {
    let (rig, ws, _) = devbox().await;
    rig.env.ensure(&ws, &Steps::default()).await.unwrap();
    let allowed: BTreeSet<&str> = [
        "PATH",
        "HOME",
        "CONTAINER_HOST",
        "LANG",
        "TMPDIR",
        "PWD",
        "SHLVL",
        "_",
        "OLDPWD",
    ]
    .into();
    for sub in ["read-configuration", "up", "run-user-commands"] {
        let text = std::fs::read_to_string(rig.bin.join(format!("cli.env.{sub}.1"))).unwrap();
        let names: BTreeSet<&str> = text
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, _)| k))
            .collect();
        assert!(
            names.is_subset(&allowed),
            "{sub}: {:?}",
            names.difference(&allowed).collect::<Vec<_>>()
        );
        for needed in ["PATH", "HOME", "CONTAINER_HOST"] {
            assert!(names.contains(needed), "{sub}: {needed}");
        }
        assert!(text.contains(&format!(
            "HOME={}",
            rig.root.join("environments/.cli-home").display()
        )));
        assert!(text.contains("CONTAINER_HOST=unix:///run/podman/podman.sock"));
        assert!(!text.contains(MODEL_KEY));
    }
}

// ------------------------------------------------------------------------------------ commands

#[tokio::test]
async fn a_command_is_devcontainer_exec_with_adam_exec_and_nothing_secret_on_the_command_line() {
    let (rig, ws, slot) = devbox().await;
    let session = rig.env.ensure(&ws, &Steps::default()).await.unwrap();

    let spec = ExecSpec::shell("devbox-tool --version", &slot)
        .env("CI", "1")
        .env("FROM_HIDE", "visible?")
        .hide(["FROM_HIDE", "GITHUB_TOKEN"]);
    let prepared = session.prepare(&spec).unwrap();
    assert_eq!(prepared.program, rig.settings.cli);
    assert_eq!(prepared.cwd, slot);
    assert!(prepared.env_clear && prepared.env_remove.is_empty());
    let names: BTreeSet<String> = prepared
        .env
        .keys()
        .map(|k| k.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        ["CONTAINER_HOST", "HOME", "LANG", "PATH", "TMPDIR"]
            .map(str::to_owned)
            .into()
    );
    let args: Vec<String> = prepared
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args[0], "exec");
    assert_eq!(flag(&args, "--workspace-folder"), slot.to_str());
    assert_eq!(
        flag(&args, "--log-format"),
        Some("text"),
        "a JSON log would take a terminal and swallow the output"
    );
    let remote = flags(&args, "--remote-env");
    for wanted in [
        "CI=1",
        "GIT_CONFIG_COUNT=1",
        "GIT_CONFIG_KEY_0=safe.directory",
        "GIT_CONFIG_VALUE_0=*",
    ] {
        assert!(remote.contains(&wanted), "{wanted} in {remote:?}");
    }
    assert!(
        !args.iter().any(|a| a.contains("FROM_HIDE")),
        "hiding wins over setting: {args:?}"
    );
    let tail = &args[args.len() - 5..];
    assert_eq!(tail[0], "/opt/adam/bin/adam-exec");
    assert_eq!(tail[1], "shell");
    assert_eq!(tail[2], prepared.exec.as_str());
    assert_eq!(tail[3], slot.to_str().unwrap());
    assert_eq!(tail[4], ":devbox-tool --version");

    // A program and its arguments: a word that looks like an option of the CLI is not one.
    let argv = session
        .prepare(&ExecSpec::argv(
            ["opencode", "acp", "--version", "-c", ""],
            &slot,
        ))
        .unwrap();
    let args: Vec<String> = argv
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args[args.len() - 8], "run");
    assert_eq!(
        &args[args.len() - 5..],
        [":opencode", ":acp", ":--version", ":-c", ":"].map(str::to_owned)
    );
    assert_ne!(
        argv.exec, prepared.exec,
        "every command has an id of its own"
    );
    assert!(prepared.exec.as_str().starts_with("dc-"));

    // The model key is a file, and nowhere else: not an argument, not a variable, not in a log.
    assert_eq!(
        session.secret_ref("model-key"),
        Some(SecretRef::File(PathBuf::from(
            "/run/adam/secrets/model-key"
        )))
    );
    assert_eq!(session.secret_ref("anything-else"), None);
    let key_file = rig
        .root
        .join("environments")
        .join(RUN)
        .join("secrets/model-key");
    assert_eq!(std::fs::read_to_string(&key_file).unwrap(), MODEL_KEY);
    assert_eq!(
        std::fs::metadata(&key_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(key_file.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        rig.files_containing(MODEL_KEY),
        [key_file],
        "the key is in the one file that is mounted read-only"
    );
    assert!(
        !args
            .iter()
            .chain(
                &prepared
                    .args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            )
            .any(|a| a.contains(MODEL_KEY))
    );
    let mounts = rig.override_file(RUN)["mounts"].to_string();
    assert!(
        mounts.contains("target=/run/adam/secrets,readonly"),
        "{mounts}"
    );
}

#[tokio::test]
async fn without_a_model_key_there_is_no_secret_to_refer_to() {
    let rig = Rig::with(|s| s.model_key = None);
    let ws = rig.run(RUN);
    let slot = rig
        .repository(&ws, "devbox", &as_refs(&devbox_files()))
        .await;
    rig.inspect_for(&ws, false).await;
    let session = rig.env.ensure(&ws, &Steps::default()).await.unwrap();
    assert_eq!(session.secret_ref("model-key"), None);
    assert!(
        !rig.root
            .join("environments")
            .join(RUN)
            .join("secrets/model-key")
            .exists()
    );
    assert!(
        !rig.override_file(RUN)["mounts"]
            .to_string()
            .contains("/run/adam/secrets")
    );
    session.prepare(&ExecSpec::shell("true", &slot)).unwrap();
}

#[tokio::test]
async fn a_command_that_cannot_be_prepared_is_refused() {
    let (rig, ws, slot) = devbox().await;
    let session = rig.env.ensure(&ws, &Steps::default()).await.unwrap();
    for cwd in [
        PathBuf::from("/etc"),
        PathBuf::from("relative"),
        slot.join("../../other-run"),
        rig.root.join("workspaces/other-run/x"),
    ] {
        let err = session.prepare(&ExecSpec::shell("true", &cwd)).unwrap_err();
        assert!(matches!(err, EnvError::Refused(_)), "{cwd:?}: {err:?}");
        assert_eq!(err.class(), ErrorClass::Invalid);
    }
    let err = session
        .prepare(&ExecSpec::argv(Vec::<OsString>::new(), &slot))
        .unwrap_err();
    assert!(matches!(err, EnvError::Refused(_)));
    for name in ["A-B", "1X", "", "A B", "X=Y"] {
        let err = session
            .prepare(&ExecSpec::shell("true", &slot).env(name, "v"))
            .unwrap_err();
        assert!(matches!(err, EnvError::Refused(_)), "{name:?}: {err:?}");
    }
}

#[tokio::test]
async fn kill_stops_what_the_command_left_in_the_container_and_never_fails() {
    let (rig, ws, slot) = devbox().await;
    let session = rig.env.ensure(&ws, &Steps::default()).await.unwrap();
    let id = session
        .prepare(&ExecSpec::shell("sleep 300", &slot))
        .unwrap()
        .exec;
    session.kill(&id).await;
    assert_eq!(
        rig.podman("exec")[0],
        [
            "exec",
            "--user",
            "0",
            CONTAINER,
            "/opt/adam/bin/adam-exec",
            "kill",
            id.as_str()
        ]
    );
    // Podman failing is logged, not raised.
    rig.put("podman.exec.exit", "1");
    session.kill(&ExecId::new("dc-1-9")).await;
    assert_eq!(rig.podman("exec").len(), 2);
}

// -------------------------------------------------------------------------------------- refusals

#[tokio::test]
async fn a_refused_file_is_a_failed_step_that_names_the_file_and_the_key_and_stays_that_way_until_it_changes()
 {
    let rig = Rig::new();
    let ws = rig.run(RUN);
    let slot = rig
        .repository(
            &ws,
            "devbox-broken",
            &[(
                ".devcontainer/devcontainer.json",
                r#"{"image":"x","privileged":true,"initializeCommand":"touch /work/INIT-RAN","containerEnv":{"LEAK":"${localEnv:GITHUB_TOKEN}"}}"#,
            )],
        )
        .await;
    rig.inspect_for(&ws, true).await;
    let steps = Steps::default();
    let err = error_of(rig.env.ensure(&ws, &steps).await);
    let EnvError::Refused(why) = &err else {
        panic!("{err:?}")
    };
    assert!(
        why.contains(".devcontainer/devcontainer.json") && why.contains("privileged"),
        "{why}"
    );
    assert_eq!(err.class(), ErrorClass::Invalid);
    assert!(
        rig.cli_calls().is_empty(),
        "nothing was built: the file is refused before the CLI runs"
    );
    let last = steps.latest().pop().unwrap();
    assert_eq!(last.state, EnvStepState::Failed);
    assert!(
        last.label
            .starts_with("Building the environment from .devcontainer/devcontainer.json")
    );
    assert!(last.detail.unwrap().contains("privileged"));
    assert_eq!(rig.state(RUN)["phase"], "broken");
    assert!(!slot.join("INIT-RAN").exists() && !rig.root.join("INIT-RAN").exists());

    // The same file, the same answer, at once, and the step says it again.
    steps.clear();
    let again = error_of(rig.env.ensure(&ws, &steps).await);
    assert_eq!(again.to_string(), err.to_string());
    assert_eq!(steps.latest().pop().unwrap().state, EnvStepState::Failed);
    assert!(rig.cli_calls().is_empty());

    // The file is fixed: it is built.
    std::fs::write(
        slot.join(".devcontainer/devcontainer.json"),
        r#"{"image":"x"}"#,
    )
    .unwrap();
    rig.env.ensure(&ws, &Steps::default()).await.unwrap();
    assert_eq!(rig.cli("up").len(), 1);
    assert_eq!(rig.state(RUN)["phase"], "ready");
}

#[tokio::test]
async fn rebuild_with_use_default_goes_on_in_the_default_image_and_keeps_the_file_out_of_it() {
    let rig = Rig::new();
    let ws = rig.run(RUN);
    rig.repository(
        &ws,
        "devbox-broken",
        &[(
            ".devcontainer/devcontainer.json",
            r#"{"image":"x","privileged":true}"#,
        )],
    )
    .await;
    rig.inspect_for(&ws, true).await;
    error_of(rig.env.ensure(&ws, &Steps::default()).await);
    rig.env.rebuild(RUN, true).await.unwrap();
    let steps = Steps::default();
    rig.env.ensure(&ws, &steps).await.unwrap();
    assert_eq!(rig.override_file(RUN)["image"], DEFAULT_IMAGE);
    assert!(
        steps
            .latest()
            .iter()
            .any(|s| s.label.starts_with("Using the default environment"))
    );
    assert_eq!(rig.state(RUN)["use_default"], true);
    // A plain rebuild of a working environment makes it again from the file.
    rig.env.rebuild(RUN, false).await.unwrap();
    assert_eq!(
        rig.state(RUN)["use_default"],
        true,
        "the choice stays for the run"
    );
}

#[tokio::test]
async fn what_features_and_the_image_add_is_checked_too() {
    let (rig, ws, _) = devbox().await;
    rig.put(
        "cli.read-configuration.stdout",
        r#"{"configuration":{},"mergedConfiguration":{"privileged":true,"capAdd":["SYS_ADMIN"]}}"#,
    );
    let err = error_of(rig.env.ensure(&ws, &Steps::default()).await);
    let EnvError::Refused(why) = &err else {
        panic!("{err:?}")
    };
    assert!(
        why.contains("privileged")
            && why.contains("SYS_ADMIN")
            && why.contains("added by a feature or the image"),
        "{why}"
    );
    assert!(rig.cli("up").is_empty(), "no container is made");
    assert_eq!(rig.state(RUN)["phase"], "broken");

    // A CLI that gives no merged configuration cannot be checked: failed closed.
    rig.env.rebuild(RUN, false).await.unwrap();
    rig.put("cli.read-configuration.stdout", r#"{"configuration":{}}"#);
    let err = error_of(rig.env.ensure(&ws, &Steps::default()).await);
    assert!(matches!(err, EnvError::Build { .. }), "{err:?}");
    assert!(rig.cli("up").is_empty());
}

#[tokio::test]
async fn the_created_container_has_the_last_word_and_is_removed_when_it_is_refused() {
    let (rig, ws, _) = devbox().await;
    let mut inspect: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(rig.bin.join("inspect.json")).unwrap())
            .unwrap();
    inspect[0]["Mounts"].as_array_mut().unwrap().push(json!({"Type": "bind", "Source": "/var/run/docker.sock", "Destination": "/var/run/docker.sock", "RW": true}));
    inspect[0]["HostConfig"]["Privileged"] = json!(true);
    rig.put("inspect.json", &inspect.to_string());
    let steps = Steps::default();
    let err = error_of(rig.env.ensure(&ws, &steps).await);
    let EnvError::Refused(why) = &err else {
        panic!("{err:?}")
    };
    assert!(
        why.contains("privileged")
            && why.contains("/var/run/docker.sock")
            && why.contains("the created container"),
        "{why}"
    );
    assert_eq!(
        rig.podman("rm")[0],
        ["rm", "-f", "--time", "5", CONTAINER],
        "the container is removed"
    );
    assert!(
        rig.cli("run-user-commands").is_empty(),
        "none of the repository's commands ran"
    );
    assert_eq!(rig.state(RUN)["phase"], "broken");
    assert_eq!(steps.latest().pop().unwrap().state, EnvStepState::Failed);
}

#[tokio::test]
async fn a_build_that_fails_says_what_it_said_with_the_secrets_gone() {
    let (rig, ws, _) = devbox().await;
    rig.put("cli.up.exit", "1");
    rig.put("cli.up.stdout", r#"{"outcome":"error","message":"Command failed","description":"An error occurred setting up the container."}"#);
    let mut log = String::new();
    for n in 0..60 {
        log.push_str(&format!(
            "{{\"type\":\"text\",\"level\":2,\"timestamp\":{n},\"text\":\"step {n}\"}}\n"
        ));
    }
    log.push_str(&format!("{{\"type\":\"text\",\"level\":2,\"timestamp\":99,\"text\":\"ENV GITHUB_TOKEN=ghp_abcdef0123 key {MODEL_KEY}\"}}\n"));
    log.push_str("{\"type\":\"text\",\"level\":3,\"timestamp\":100,\"text\":\"ERROR: failed to solve: dockerfile parse error\"}\n");
    rig.put("cli.up.stderr", &log);
    let steps = Steps::default();
    let err = error_of(rig.env.ensure(&ws, &steps).await);
    let EnvError::Build { reason, log_tail } = &err else {
        panic!("{err:?}")
    };
    assert!(
        reason.contains(".devcontainer/devcontainer.json")
            && reason.contains("An error occurred setting up the container."),
        "{reason}"
    );
    assert!(
        log_tail.contains("failed to solve: dockerfile parse error"),
        "{log_tail}"
    );
    assert!(
        log_tail.lines().count() <= 40,
        "{}",
        log_tail.lines().count()
    );
    assert!(
        !log_tail.contains("step 0\n") && log_tail.contains("step 59"),
        "the end, not the start"
    );
    assert!(
        !log_tail.contains("ghp_abcdef0123") && !log_tail.contains(MODEL_KEY),
        "{log_tail}"
    );
    assert!(
        !steps
            .latest()
            .pop()
            .unwrap()
            .detail
            .unwrap()
            .contains(MODEL_KEY)
    );
    let build_log =
        std::fs::read_to_string(rig.root.join("environments").join(RUN).join("build.log")).unwrap();
    assert!(!build_log.contains(MODEL_KEY) && !build_log.contains("ghp_abcdef0123"));
    assert_eq!(rig.state(RUN)["phase"], "broken");
    assert!(
        rig.state(RUN)["error"]
            .to_string()
            .contains("failed to solve")
    );
}

#[tokio::test]
async fn a_lifecycle_command_that_fails_breaks_the_environment_and_removes_the_container() {
    let (rig, ws, _) = devbox().await;
    rig.put("cli.run-user-commands.exit", "1");
    rig.put("cli.run-user-commands.stdout", r#"{"outcome":"error","message":"x","description":"postCreateCommand from devcontainer.json failed with exit code 127."}"#);
    let err = error_of(rig.env.ensure(&ws, &Steps::default()).await);
    let EnvError::Build { reason, .. } = &err else {
        panic!("{err:?}")
    };
    assert!(
        reason.contains("postCreateCommand") && reason.contains("127"),
        "{reason}"
    );
    assert_eq!(rig.podman("rm").len(), 1);
    assert_eq!(rig.state(RUN)["phase"], "broken");
}

#[tokio::test]
async fn a_configuration_the_cli_cannot_read_is_a_config_error_naming_the_file() {
    let (rig, ws, _) = devbox().await;
    rig.put("cli.read-configuration.exit", "1");
    rig.put(
        "cli.read-configuration.stderr",
        "Dev container config (/x/.devcontainer/devcontainer.json) not found.\n",
    );
    let err = error_of(rig.env.ensure(&ws, &Steps::default()).await);
    let EnvError::Config { file, reason } = &err else {
        panic!("{err:?}")
    };
    assert_eq!(
        file,
        std::path::Path::new(".devcontainer/devcontainer.json")
    );
    assert!(reason.contains("not found"), "{reason}");
}

#[tokio::test]
async fn a_slow_phase_is_a_timeout_and_the_partial_container_is_removed_by_label() {
    let rig = Rig::with(|s| s.up_timeout = std::time::Duration::from_secs(1));
    let ws = rig.run(RUN);
    rig.repository(&ws, "devbox", &as_refs(&devbox_files()))
        .await;
    rig.inspect_for(&ws, true).await;
    rig.put("cli.up.sleep", "5");
    rig.put("up.no-container", "");
    // A container the CLI made before it hung.
    rig.put(
        "ps.json",
        &json!([{"Id": "partial1", "State": "created", "Labels": {"adam.vymalo.com/run": RUN}}])
            .to_string(),
    );
    let started = std::time::Instant::now();
    let err = error_of(rig.env.ensure(&ws, &Steps::default()).await);
    assert!(
        matches!(
            err,
            EnvError::Timeout {
                phase: "build",
                secs: 1
            }
        ),
        "{err:?}"
    );
    assert_eq!(err.class(), ErrorClass::Transient);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "the CLI was killed, not waited for"
    );
    assert_eq!(rig.podman("rm")[0], ["rm", "-f", "--time", "5", "partial1"]);
    assert_eq!(rig.state(RUN)["phase"], "broken");
}

#[tokio::test]
async fn a_cli_that_is_not_there_is_unavailable_and_is_tried_again() {
    let rig = Rig::with(|s| s.cli = PathBuf::from("/no/such/devcontainer"));
    let ws = rig.run(RUN);
    rig.repository(&ws, "devbox", &as_refs(&devbox_files()))
        .await;
    rig.inspect_for(&ws, true).await;
    let err = error_of(rig.env.ensure(&ws, &Steps::default()).await);
    assert!(matches!(err, EnvError::Unavailable(_)), "{err:?}");
    assert!(err.to_string().contains("/no/such/devcontainer"));
    assert_eq!(err.class(), ErrorClass::Transient);
    // Not kept as broken: the next command tries again.
    let state_phase = rig.state(RUN)["phase"].clone();
    assert_eq!(state_phase, "building");
    let again = error_of(rig.env.ensure(&ws, &Steps::default()).await);
    assert!(matches!(again, EnvError::Unavailable(_)));
}

// ---------------------------------------------------------------------------------- no runtime

#[tokio::test]
async fn with_the_runtime_off_the_run_is_local_and_a_step_says_so_once() {
    let rig = Rig::with(|s| s.runtime = Runtime::Off);
    let ws = rig.run(RUN);
    rig.repository(&ws, "devbox", &as_refs(&devbox_files()))
        .await;
    let steps = Steps::default();
    let session = rig.env.ensure(&ws, &steps).await.unwrap();
    assert_eq!(session.describe().kind, EnvKind::Local);
    let all = steps.all();
    assert_eq!(all.len(), 1);
    assert!(
        all[0].label.starts_with("This repository has a devcontainer, but this deployment runs without a container runtime"),
        "{}",
        all[0].label
    );
    steps.clear();
    rig.env.ensure(&ws, &steps).await.unwrap();
    assert!(steps.all().is_empty(), "one step per run");
    assert!(rig.cli_calls().is_empty() && rig.podman_calls().is_empty());
    assert!(
        !rig.root.join("environments").exists(),
        "nothing is written"
    );

    // A repository with no devcontainer has nothing to say.
    let other = rig.run("run-0002");
    rig.repository(&other, "sandbox", &[]).await;
    let steps = Steps::default();
    rig.env.ensure(&other, &steps).await.unwrap();
    assert!(steps.all().is_empty());
}

#[tokio::test]
async fn an_unreachable_runtime_is_local_with_a_scrubbed_reason_and_the_probe_is_not_repeated_at_once()
 {
    let (rig, ws, _) = devbox().await;
    rig.put("podman.info.exit", "125");
    rig.put(
        "podman.info.err",
        "Cannot connect to Podman. Is the service running? unix:///run/podman/podman.sock: connect: no such file token=abc123456789\n",
    );
    let steps = Steps::default();
    let session = rig.env.ensure(&ws, &steps).await.unwrap();
    assert_eq!(session.describe().kind, EnvKind::Local);
    let step = steps.latest().pop().unwrap();
    assert!(
        step.label
            .starts_with("The container runtime is not reachable"),
        "{}",
        step.label
    );
    assert_eq!(step.state, EnvStepState::Completed);
    let detail = step.detail.unwrap();
    assert!(
        detail.contains("Cannot connect to Podman") && !detail.contains("abc123456789"),
        "{detail}"
    );
    assert_eq!(rig.state(RUN)["phase"], "local");
    assert!(rig.cli_calls().is_empty());

    // The run stays local, without another step; another run within the interval reuses the probe.
    steps.clear();
    rig.env.ensure(&ws, &steps).await.unwrap();
    assert!(steps.all().is_empty());
    let other = rig.run("run-0002");
    rig.scratch(&other, "notes", &[]).await;
    let steps = Steps::default();
    rig.env.ensure(&other, &steps).await.unwrap();
    assert_eq!(steps.all().len(), 1);
    assert_eq!(
        std::fs::read_to_string(rig.bin.join("count.podman.info"))
            .unwrap()
            .trim(),
        "1"
    );
}

#[tokio::test]
async fn a_service_that_answers_is_probed_once_for_every_run_in_the_interval() {
    let rig = Rig::new();
    for (run, name) in [("run-0001", "a"), ("run-0002", "b")] {
        let ws = rig.run(run);
        rig.scratch(&ws, name, &[]).await;
        rig.put(
            "ps.after-up.json",
            &json!([{"Id": CONTAINER, "State": "running", "Labels": {"adam.vymalo.com/run": run}}])
                .to_string(),
        );
        rig.inspect_for(&ws, true).await;
        rig.env.ensure(&ws, &Steps::default()).await.unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(rig.bin.join("count.podman.info"))
            .unwrap()
            .trim(),
        "1"
    );
}

#[tokio::test]
async fn a_workspace_of_the_older_layout_stays_local_with_a_step() {
    let rig = Rig::new();
    rig.legacy_worktree(RUN, "devbox").await;
    let ws = rig.run(RUN);
    let steps = Steps::default();
    let session = rig.env.ensure(&ws, &steps).await.unwrap();
    assert_eq!(session.describe().kind, EnvKind::Local);
    assert_eq!(steps.all().len(), 1);
    assert!(rig.cli_calls().is_empty());
}

#[tokio::test]
async fn a_workspace_with_nothing_in_it_has_no_environment_to_make() {
    let rig = Rig::new();
    let err = error_of(rig.env.ensure(&rig.run(RUN), &Steps::default()).await);
    assert!(matches!(err, EnvError::Refused(_)), "{err:?}");
}

// ------------------------------------------------------------------ the lifecycle after Ready

#[tokio::test]
async fn a_repository_that_joins_restarts_the_environment_with_its_mirror_mounted() {
    let (rig, ws, _) = devbox().await;
    rig.env.ensure(&ws, &Steps::default()).await.unwrap();
    rig.repository(&ws, "library", &[]).await;
    rig.inspect_for(&ws, true).await;
    // The restart finds the container listed the same way.
    let steps = Steps::default();
    rig.env.ensure(&ws, &steps).await.unwrap();
    let latest = steps.latest();
    assert_eq!(
        latest[0].label,
        "Restarting the environment: library joined the workspace"
    );
    assert_eq!(latest[0].state, EnvStepState::Completed);
    assert_eq!(rig.cli("up").len(), 2);
    assert!(rig.cli("up")[1].contains(&"--remove-existing-container".to_owned()));
    let mounts = rig.override_file(RUN)["mounts"].to_string();
    assert!(
        mounts.contains("library.git") && mounts.contains("/library/.git"),
        "{mounts}"
    );
    assert_eq!(rig.state(RUN)["slots"].as_array().unwrap().len(), 2);
    assert_eq!(rig.state(RUN)["phase"], "ready");
}

#[tokio::test]
async fn a_container_that_is_gone_is_made_again_and_a_step_says_so() {
    let (rig, ws, _) = devbox().await;
    rig.env.ensure(&ws, &Steps::default()).await.unwrap();
    rig.put("ps.json", "[]");
    let steps = Steps::default();
    rig.env.ensure(&ws, &steps).await.unwrap();
    assert_eq!(
        steps.latest()[0].label,
        "The environment was lost; rebuilding it"
    );
    assert!(rig.cli("up")[1].contains(&"--remove-existing-container".to_owned()));
    // A container that is there but stopped is gone as well.
    rig.put(
        "ps.json",
        &json!([{"Id": CONTAINER, "State": "exited", "Labels": {"adam.vymalo.com/run": RUN}}])
            .to_string(),
    );
    let steps = Steps::default();
    rig.env.ensure(&ws, &steps).await.unwrap();
    assert_eq!(
        steps.latest()[0].label,
        "The environment was lost; rebuilding it"
    );
}

#[tokio::test]
async fn a_service_that_stops_answering_is_an_error_and_not_a_silent_local() {
    let (rig, ws, _) = devbox().await;
    rig.env.ensure(&ws, &Steps::default()).await.unwrap();
    rig.put("podman.ps.exit", "125");
    let err = error_of(rig.env.ensure(&ws, &Steps::default()).await);
    assert!(matches!(err, EnvError::Unavailable(_)), "{err:?}");
    assert_eq!(rig.state(RUN)["phase"], "ready", "nothing was decided");
}

#[tokio::test]
async fn a_coder_that_restarts_finds_its_environment_again() {
    let (rig, ws, slot) = devbox().await;
    rig.env.ensure(&ws, &Steps::default()).await.unwrap();
    let again = rig.restarted();
    let steps = Steps::default();
    let session = again.ensure(&ws, &steps).await.unwrap();
    assert!(steps.all().is_empty());
    assert_eq!(rig.cli_calls().len(), 3, "nothing was built again");
    session.prepare(&ExecSpec::shell("true", &slot)).unwrap();
    assert_eq!(
        session.describe().kind,
        EnvKind::DevContainer {
            source: Some(slot.join(".devcontainer/devcontainer.json")),
            image: "localhost/vsc-slot-1a2b-uid:latest".to_owned(),
        }
    );
}

#[tokio::test]
async fn two_commands_that_need_the_environment_at_once_make_it_once() {
    let (rig, ws, _) = devbox().await;
    rig.put("cli.up.sleep", "1");
    let (first, second) = (Steps::default(), Steps::default());
    let (a, b) = tokio::join!(rig.env.ensure(&ws, &first), rig.env.ensure(&ws, &second));
    a.unwrap();
    b.unwrap();
    assert_eq!(rig.cli("up").len(), 1);
    assert_eq!(rig.cli("read-configuration").len(), 1);
}

// --------------------------------------------------------------------------------- release, sweep

const IMAGES: &str = r#"[
  {"Id":"1","Names":["localhost/vsc-slot-1a2b-uid:latest"]},
  {"Id":"2","Names":["localhost/vsc-slot-1a2b:latest"]},
  {"Id":"3","Names":["localhost/vsc-other-9z-uid:latest"]},
  {"Id":"4","Names":["registry.example/base:1"]}
]"#;

#[tokio::test]
async fn release_gives_the_files_back_removes_the_container_and_its_images_and_the_directory() {
    let (rig, ws, _) = devbox().await;
    rig.env.ensure(&ws, &Steps::default()).await.unwrap();
    rig.put("images.json", IMAGES);
    assert_eq!(rig.env.held_runs().await.unwrap(), [RUN]);

    rig.env.release(RUN).await.unwrap();

    let work = rig.root.join("workspaces").join(RUN);
    let meta = std::fs::metadata(&work).unwrap();
    let (uid, gid) = (meta.uid().to_string(), meta.gid().to_string());
    assert_eq!(
        rig.podman("exec")[0],
        [
            "exec",
            "--user",
            "0",
            CONTAINER,
            "/opt/adam/bin/adam-exec",
            "chown",
            uid.as_str(),
            gid.as_str(),
            work.to_str().unwrap()
        ],
        "the coder's own ids, as the host sees them: the script maps them through the container's id map"
    );
    assert_eq!(rig.podman("rm")[0], ["rm", "-f", "--time", "5", CONTAINER]);
    let order: Vec<_> = rig
        .order()
        .into_iter()
        .filter(|l| l.starts_with("podman"))
        .collect();
    let at = |what: &str| order.iter().position(|l| l == what).unwrap_or(usize::MAX);
    assert!(
        at("podman exec") < at("podman rm")
            && at("podman rm") < at("podman images")
            && at("podman images") < at("podman rmi"),
        "{order:?}"
    );
    let removed: BTreeSet<String> = rig.podman("rmi").iter().map(|c| c[1].clone()).collect();
    assert_eq!(
        removed,
        [
            "localhost/vsc-slot-1a2b-uid:latest",
            "localhost/vsc-slot-1a2b:latest"
        ]
        .map(str::to_owned)
        .into(),
        "the run's own images, and neither another run's nor the pulled base"
    );
    assert!(!rig.root.join("environments").join(RUN).exists());
    assert!(rig.env.held_runs().await.unwrap().is_empty());

    // Idempotent, and a run that holds nothing costs no call.
    let calls = rig.podman_calls().len();
    rig.env.release(RUN).await.unwrap();
    rig.env.release("never-held").await.unwrap();
    assert_eq!(rig.podman_calls().len(), calls);
}

#[tokio::test]
async fn a_container_that_will_not_go_stays_held_and_the_files_stay() {
    let (rig, ws, _) = devbox().await;
    rig.env.ensure(&ws, &Steps::default()).await.unwrap();
    rig.put("rm.fails", "");
    rig.put("podman.rm.exit", "125");
    let err = rig.env.release(RUN).await.unwrap_err();
    assert!(matches!(err, EnvError::Unavailable(_)), "{err:?}");
    assert_eq!(rig.podman("rm").len(), 2, "removed again once");
    assert!(
        rig.root
            .join("environments")
            .join(RUN)
            .join("state.json")
            .exists()
    );
    assert_eq!(rig.env.held_runs().await.unwrap(), [RUN]);
    // The exit code of removing is not trusted: a removal that reports an error and worked is fine.
    rig.remove("rm.fails");
    rig.env.release(RUN).await.unwrap();
}

#[tokio::test]
async fn release_with_the_service_down_stays_held_and_a_bad_run_id_is_refused() {
    let (rig, ws, _) = devbox().await;
    rig.env.ensure(&ws, &Steps::default()).await.unwrap();
    rig.put("podman.ps.exit", "125");
    assert!(matches!(
        rig.env.release(RUN).await,
        Err(EnvError::Unavailable(_))
    ));
    assert!(rig.root.join("environments").join(RUN).exists());
    for bad in ["../x", "a/b", "", ".hidden"] {
        assert!(
            matches!(rig.env.release(bad).await, Err(EnvError::Refused(_))),
            "{bad:?}"
        );
    }
}

#[tokio::test]
async fn the_sweep_finds_what_a_crash_left_by_state_and_by_label_for_this_deployment_only() {
    let rig = Rig::new();
    // A run with a state directory, one with only a container, one of another deployment.
    for (run, deployment) in [("run-state", DEPLOYMENT), ("run-foreign", "another-coder")] {
        let dir = rig.root.join("environments").join(run);
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = json!({"version": 1, "run": run, "deployment": deployment, "phase": "broken", "config_digest": "",
            "slots": [], "container_id": null, "image": null, "tools": null, "local_reason": null, "error": null});
        state["use_default"] = json!(false);
        std::fs::write(dir.join("state.json"), state.to_string()).unwrap();
    }
    std::fs::create_dir_all(rig.root.join("environments/.tools/abc")).unwrap();
    std::fs::create_dir_all(rig.root.join("environments/.cli-home")).unwrap();
    rig.put(
        "ps.json",
        &json!([
            {"Id": "c1", "State": "running", "Labels": {"adam.vymalo.com/run": "run-orphan", "adam.vymalo.com/deployment": DEPLOYMENT}},
            {"Id": "c2", "State": "exited", "Labels": {"adam.vymalo.com/run": "run-state", "adam.vymalo.com/deployment": DEPLOYMENT}},
        ])
        .to_string(),
    );
    assert_eq!(
        rig.env.held_runs().await.unwrap(),
        ["run-orphan", "run-state"]
    );
    let ps = &rig.podman("ps")[0];
    assert!(
        ps.contains(&format!("label=adam.vymalo.com/deployment={DEPLOYMENT}")),
        "{ps:?}"
    );
    // The sweep can release the orphan: nothing is known of it but its label.
    rig.put(
        "ps.json",
        &json!([{"Id": "c1", "State": "running", "Labels": {"adam.vymalo.com/run": "run-orphan"}}])
            .to_string(),
    );
    std::fs::create_dir_all(rig.root.join("environments/run-orphan")).unwrap();
    rig.env.release("run-orphan").await.unwrap();
    assert_eq!(rig.podman("rm")[0][4], "c1");
}

// ------------------------------------------------------------------------------------- tools

#[tokio::test]
async fn the_tools_directory_is_written_once_for_every_run() {
    let rig = Rig::new();
    let dir = rig.env.install_tools().await.unwrap();
    assert_eq!(dir.parent().unwrap(), rig.root.join("environments/.tools"));
    assert!(dir.join("adam-exec").is_file());
    assert_eq!(rig.env.install_tools().await.unwrap(), dir);
    let binary = rig.tmp.path().join("opencode");
    std::fs::write(&binary, b"\x7fELF").unwrap();
    let with = adam_devcontainer::DevContainer::new({
        let mut s = rig.settings.clone();
        s.opencode = Some(binary);
        s
    });
    let dir_with = with.install_tools().await.unwrap();
    assert_ne!(dir_with, dir);
    assert!(dir_with.join("opencode").is_file());
}
