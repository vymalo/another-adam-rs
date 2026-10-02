//! `DevContainer` against a real rootless Podman service and the real devcontainer CLI: the
//! `devbox` fixture (a Dockerfile that adds a tool only its devcontainer has, and a lifecycle
//! command that uses it) built, used, killed into and released.
//!
//! **Gated**: it runs only with `ADAM_TEST_DEVCONTAINER=1`, and then needs
//!
//! * `CONTAINER_HOST`: the service's socket (`unix:///run/podman/podman.sock`);
//! * `ADAM_TEST_DEVCONTAINER_ROOT`: a directory the service sees at **the same path**, writable by
//!   the user that runs this test, which must be the user the service runs as (uid 10001 in the
//!   compose stack), so that files made inside a container are this user's outside;
//! * the devcontainer CLI and Podman's client on `PATH` (`DEVCONTAINER_CLI`, `DEVCONTAINER_PODMAN`
//!   name them otherwise);
//! * optionally `ADAM_TEST_DEVCONTAINER_BASE` (the image the fixture builds on, default the pinned
//!   `mcr.microsoft.com/devcontainers/base`) and `ADAM_TEST_DEVCONTAINER_DNS_NAME` (a name that the
//!   service's network resolves, such as a compose service, to check from inside).
//!
//! `ADAM_TEST_REQUIRE_DEVCONTAINER=1` makes a run that would skip fail instead (CI sets it). The
//! stack and the command are in `dev/podman/README.md`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::Arc;
use std::time::Duration;

use adam_devcontainer::{DevContainer, Network, Runtime, Settings};
use adam_workspace::{
    EnvKind, EnvProgress, EnvSession, EnvStep, Environment, ExecSpec, GitCredentials, RepoRef,
    StaticToken, Workspaces,
};
use tokio::io::AsyncWriteExt;

// devcontainers/base 2.2.1-trixie, by digest only: the devcontainer CLI (0.89.0) cannot parse a reference
// with both a tag and a digest, and then skips the image's metadata (its `remoteUser`).
const BASE: &str = "mcr.microsoft.com/devcontainers/base@sha256:1f851004adcd3dff3776b4a1da86727cf52280b0fdc9d5b0568c9c7d74274286";

#[derive(Default)]
struct Steps(std::sync::Mutex<Vec<EnvStep>>);

impl EnvProgress for Steps {
    fn step(&self, step: EnvStep) {
        // As they come, so that a build that hangs says in CI's log where it was.
        eprintln!("step: {step:?}");
        self.0.lock().unwrap().push(step);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = StdCommand::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Seed")
        .env("GIT_AUTHOR_EMAIL", "seed@example.com")
        .env("GIT_COMMITTER_NAME", "Seed")
        .env("GIT_COMMITTER_EMAIL", "seed@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn podman(args: &[&str]) -> String {
    let program =
        std::env::var("DEVCONTAINER_PODMAN").unwrap_or_else(|_| "podman-remote".to_owned());
    let out = StdCommand::new(program).args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The `devbox` fixture: a repository whose devcontainer has a tool only it has.
fn seed_devbox(dir: &Path, base: &str) {
    std::fs::create_dir_all(dir.join(".devcontainer")).unwrap();
    std::fs::write(
        dir.join(".devcontainer/devcontainer.json"),
        // JSONC, with a comment and a trailing comma, as people write it.
        r#"// The devbox fixture: its environment has `devbox-tool`, and nothing else has.
{
  "build": { "dockerfile": "Dockerfile" },
  "remoteUser": "vscode",
  "postCreateCommand": "devbox-tool --version > /tmp/devbox-ready",
  "containerEnv": { "WORKSPACE_SEEN": "${containerWorkspaceFolder}" },
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join(".devcontainer/Dockerfile"),
        format!("FROM {base}\nCOPY devbox-tool /usr/local/bin/devbox-tool\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join(".devcontainer/devbox-tool"),
        "#!/bin/sh\necho 'devbox-tool 1.0 (from the devcontainer)'\n",
    )
    .unwrap();
    // Executable in the repository: the COPY keeps the mode.
    StdCommand::new("chmod")
        .arg("755")
        .arg(dir.join(".devcontainer/devbox-tool"))
        .status()
        .unwrap();
    std::fs::write(dir.join("README.md"), "devbox\n").unwrap();
}

struct Inside<'a>(&'a dyn EnvSession, &'a Path);

impl Inside<'_> {
    /// Run a command line in the container: `(exit code, stdout)`.
    async fn run(&self, line: &str, stdin: Option<&str>) -> (i32, String) {
        let prepared = self.0.prepare(&ExecSpec::shell(line, self.1)).unwrap();
        let mut cmd = prepared.command();
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        if let Some(text) = stdin {
            input.write_all(text.as_bytes()).await.unwrap();
        }
        drop(input);
        let out = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
            .await
            .expect("the command finished")
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }
}

#[tokio::test]
async fn the_devbox_fixture_runs_in_its_devcontainer_on_rootless_podman() {
    // The crate's warnings (a kill or a removal that failed) go to the test's output.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::DEBUG)
        .try_init();
    let (enabled, required) = (
        std::env::var("ADAM_TEST_DEVCONTAINER").as_deref() == Ok("1"),
        std::env::var("ADAM_TEST_REQUIRE_DEVCONTAINER").as_deref() == Ok("1"),
    );
    let (Ok(host), Ok(root)) = (
        std::env::var("CONTAINER_HOST"),
        std::env::var("ADAM_TEST_DEVCONTAINER_ROOT"),
    ) else {
        assert!(
            !required,
            "ADAM_TEST_REQUIRE_DEVCONTAINER is set, but CONTAINER_HOST or ADAM_TEST_DEVCONTAINER_ROOT is not"
        );
        eprintln!(
            "skipped: set ADAM_TEST_DEVCONTAINER=1, CONTAINER_HOST and ADAM_TEST_DEVCONTAINER_ROOT (see the module documentation)"
        );
        return;
    };
    if !enabled {
        assert!(
            !required,
            "ADAM_TEST_REQUIRE_DEVCONTAINER is set, but ADAM_TEST_DEVCONTAINER is not 1"
        );
        eprintln!("skipped: set ADAM_TEST_DEVCONTAINER=1");
        return;
    }
    let root = PathBuf::from(root);
    let scratch = tempfile::tempdir_in(&root).unwrap();
    let run = format!(
        "it-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let base = std::env::var("ADAM_TEST_DEVCONTAINER_BASE").unwrap_or_else(|_| BASE.to_owned());

    // The repository, on a local remote, as the coder gets it.
    let remote = scratch.path().join("devbox.git");
    let seed = scratch.path().join("devbox-seed");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&seed).unwrap();
    git(
        &remote,
        &["init", "--bare", "--quiet", "--initial-branch=main"],
    );
    git(&seed, &["init", "--quiet", "--initial-branch=main"]);
    seed_devbox(&seed, &base);
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "--quiet", "-m", "seed"]);
    git(
        &seed,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&seed, &["push", "--quiet", "origin", "main"]);

    let credentials: Arc<dyn GitCredentials> =
        Arc::new(StaticToken::new("unused-for-a-local-remote"));
    let workspaces = Workspaces::new(root.join("workspaces-root"), credentials);
    let workspaces_root = workspaces.root().to_owned();
    std::fs::create_dir_all(&workspaces_root).unwrap();
    let ws = workspaces.run(&run).unwrap();
    let slot = ws
        .add_repository(&RepoRef::new(remote.to_str().unwrap(), "main"))
        .await
        .unwrap();
    let slot_path = slot.path().to_owned();

    let mut settings = Settings::new(&workspaces_root);
    settings.runtime = Runtime::Podman;
    settings.cli = std::env::var("DEVCONTAINER_CLI")
        .unwrap_or_else(|_| "devcontainer".to_owned())
        .into();
    settings.podman = std::env::var("DEVCONTAINER_PODMAN")
        .unwrap_or_else(|_| "podman-remote".to_owned())
        .into();
    settings.container_host = host;
    settings.default_image = base.clone();
    settings.network = Network::Inherit;
    settings.deployment = "adam-test".to_owned();
    settings.model_key = Some("sk-integration-test-key".to_owned().into());
    if let Some(secs) = std::env::var("ADAM_TEST_DEVCONTAINER_UP_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        settings.up_timeout = Duration::from_secs(secs);
    }
    let environment = DevContainer::new(settings);

    // Build it.
    let steps = Steps::default();
    let session = environment
        .ensure(&ws, &steps)
        .await
        .expect("the devbox devcontainer is built");
    let reported = steps.0.lock().unwrap().clone();
    assert!(
        reported
            .iter()
            .any(|s| s.label
                == "Building the environment from .devcontainer/devcontainer.json (devbox)"),
        "{reported:?}"
    );
    assert!(matches!(
        session.describe().kind,
        EnvKind::DevContainer {
            source: Some(_),
            ..
        }
    ));
    let inside = Inside(session.as_ref(), &slot_path);

    // A command: stdin, stdout, the exit code, and the tool only the devcontainer has.
    let (code, out) = inside.run("cat", Some("through stdin\n")).await;
    assert_eq!((code, out.as_str()), (0, "through stdin\n"));
    let (code, out) = inside.run("devbox-tool", None).await;
    assert_eq!(
        (code, out.trim()),
        (0, "devbox-tool 1.0 (from the devcontainer)")
    );
    let (code, _) = inside.run("exit 7", None).await;
    assert_eq!(code, 7);
    let (_, out) = inside.run("cat /tmp/devbox-ready", None).await;
    assert!(
        out.contains("devbox-tool 1.0"),
        "the postCreateCommand ran with the tool: {out:?}"
    );
    // The workspace folder is the slot's own path, the same in the coder and in the container.
    let (_, out) = inside
        .run("printf %s \"$WORKSPACE_SEEN\"; echo; pwd -P", None)
        .await;
    assert_eq!(
        out.lines().next().unwrap(),
        slot_path.to_str().unwrap(),
        "${{containerWorkspaceFolder}}: {out:?}"
    );

    // What the coder must never be in the container: its credentials.
    let (_, env) = inside.run("env", None).await;
    for secret in [
        "GITHUB_TOKEN",
        "DATABASE_URL",
        "A2A_BEARER_TOKENS",
        "MODEL_API_KEY",
        "sk-integration-test-key",
    ] {
        assert!(
            !env.contains(secret),
            "{secret} is in the container's environment"
        );
    }
    let (code, key) = inside.run("cat /run/adam/secrets/model-key", None).await;
    assert_eq!((code, key.as_str()), (0, "sk-integration-test-key"));
    let (code, _) = inside
        .run("echo x > /run/adam/secrets/model-key", None)
        .await;
    assert_ne!(code, 0, "the secrets are read-only");

    // A file made inside is the coder's outside, and git reads the read-only mirror.
    let (code, _) = inside
        .run(
            "touch made-inside && mkdir -p deep/er && touch deep/er/file",
            None,
        )
        .await;
    assert_eq!(code, 0);
    let me = std::fs::metadata(&slot_path).unwrap().uid();
    assert_eq!(
        std::fs::metadata(slot_path.join("made-inside"))
            .unwrap()
            .uid(),
        me
    );
    let (code, out) = inside
        .run("git status --short && git log --oneline -1", None)
        .await;
    assert_eq!(
        code, 0,
        "git works in the slot against the read-only mirror: {out:?}"
    );
    assert!(
        out.contains("made-inside") && out.contains("seed"),
        "{out:?}"
    );
    let (code, _) = inside
        .run(&format!("touch {}/.git/x", slot_path.display()), None)
        .await;
    assert_ne!(code, 0, "the slot's .git is read-only");

    // A name of the service's network resolves from inside.
    if let Ok(name) = std::env::var("ADAM_TEST_DEVCONTAINER_DNS_NAME") {
        let (code, out) = inside.run(&format!("getent hosts {name}"), None).await;
        assert_eq!(code, 0, "{name} resolves from the devcontainer: {out:?}");
    }

    // kill stops what a command left running in the container.
    let prepared = session
        .prepare(&ExecSpec::shell("exec sleep 3137", &slot_path))
        .unwrap();
    let mut running = prepared.command();
    running
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut client = running.spawn().unwrap();
    let alive = |text: &str| text.lines().any(|l| l.trim() == "yes");
    let probe = "for p in /proc/[0-9]*; do tr '\\0' ' ' < $p/cmdline 2>/dev/null; echo; done | grep -q 'sleep 3137' && echo yes || echo no";
    let mut seen = false;
    for _ in 0..30 {
        if alive(&inside.run(probe, None).await.1) {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(seen, "the sleep is running in the container");
    // Killing the client does not stop it (verified 2026-10-01); kill does.
    client.kill().await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        alive(&inside.run(probe, None).await.1),
        "the process outlives its client"
    );
    session.kill(&prepared.exec).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    if alive(&inside.run(probe, None).await.1) {
        // Say what the kill had to go on, as the user the command ran as.
        let id = prepared.exec.as_str();
        let (_, seen) = inside
            .run(
                &format!(
                    "id; ls -la /tmp/adam-exec; cat /tmp/adam-exec/{id}.pid; \
                     for p in /proc/[0-9]*; do c=$(tr '\\0' ' ' < $p/cmdline 2>/dev/null); \
                     case $c in *'sleep 3137'*) echo \"$p: $c\"; cut -d' ' -f1-8,22 $p/stat;; esac; done; \
                     /opt/adam/bin/adam-exec kill {id}; echo \"adam-exec kill as this user: $?\""
                ),
                None,
            )
            .await;
        panic!("kill did not stop the command; inside the container:\n{seen}");
    }

    // Reused: the next command does not build again.
    let again = Steps::default();
    environment.ensure(&ws, &again).await.unwrap();
    assert!(again.0.lock().unwrap().is_empty());
    assert_eq!(
        environment.held_runs().await.unwrap(),
        std::slice::from_ref(&run)
    );

    // Teardown: no container with the run's label, no image it built, no file the coder cannot delete.
    environment.release(&run).await.expect("released");
    let label = format!("label=adam.vymalo.com/run={run}");
    assert_eq!(
        podman(&["ps", "-a", "-q", "--filter", &label]).trim(),
        "",
        "no container is left"
    );
    assert!(
        !podman(&["images", "--format", "{{.Repository}}"]).contains("vsc-slot"),
        "no image of the run is left"
    );
    assert!(!workspaces_root.join("environments").join(&run).exists());
    assert!(environment.held_runs().await.unwrap().is_empty());
    ws.remove().await.expect(
        "the workspace, and every file a process in the container made, is the coder's to delete",
    );
    assert!(!slot_path.exists());
}
