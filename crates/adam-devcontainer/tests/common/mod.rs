//! A rig for `DevContainer` with no Podman and no devcontainer CLI: two shell stubs that record
//! what they were called with and answer from files, over a real `Workspaces` with real git.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_devcontainer::{DevContainer, Network, Runtime, Settings};
use adam_workspace::{
    EnvProgress, EnvStep, GitIdentity, RepoRef, RunWorkspace, StaticToken, Workspaces,
};
use secrecy::SecretString;
use serde_json::{Value, json};
use tempfile::TempDir;

pub const RUN: &str = "run-0001";
pub const DEPLOYMENT: &str = "coder-test";
pub const CONTAINER: &str = "0123456789abcdef0123456789abcdef";
pub const MODEL_KEY: &str = "sk-model-key-0123456789";
pub const DEFAULT_IMAGE: &str = "registry.example/base:1@sha256:aaaa";

const PODMAN: &str = r#"#!/bin/sh
d=$(dirname "$0")
printf '%s\037' "$@" >> "$d/podman.argv"; printf '\n' >> "$d/podman.argv"
sub=$1
echo "podman $sub" >> "$d/order.log"
n=$(cat "$d/count.podman.$sub" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$d/count.podman.$sub"
pick() { if [ -f "$d/$1.$n" ]; then echo "$d/$1.$n"; else echo "$d/$1"; fi; }
if [ "$sub" = rm ] && [ ! -f "$d/rm.fails" ]; then echo '[]' > "$d/ps.json"; fi
[ -f "$d/podman.$sub.sleep" ] && sleep "$(cat "$d/podman.$sub.sleep")"
case $sub in
  ps) cat "$d/ps.json" ;;
  inspect) cat "$d/inspect.json" ;;
  images) cat "$d/images.json" 2>/dev/null || echo '[]' ;;
esac
f=$(pick "podman.$sub.out"); [ -f "$f" ] && cat "$f"
e=$(pick "podman.$sub.err"); [ -f "$e" ] && cat "$e" >&2
x=$(pick "podman.$sub.exit"); [ -f "$x" ] && exit "$(cat "$x")"
exit 0
"#;

const CLI: &str = r#"#!/bin/sh
d=$(dirname "$0")
sub=$1
echo "cli $sub" >> "$d/order.log"
printf '%s\037' "$@" >> "$d/cli.argv"; printf '\n' >> "$d/cli.argv"
n=$(cat "$d/count.cli.$sub" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$d/count.cli.$sub"
env | sort > "$d/cli.env.$sub.$n"
pick() { if [ -f "$d/$1.$n" ]; then echo "$d/$1.$n"; else echo "$d/$1"; fi; }
[ -f "$d/cli.$sub.sleep" ] && sleep "$(cat "$d/cli.$sub.sleep")"
if [ "$sub" = up ] && [ -f "$d/ps.after-up.json" ] && [ ! -f "$d/up.no-container" ]; then cp "$d/ps.after-up.json" "$d/ps.json"; fi
f=$(pick "cli.$sub.stdout"); [ -f "$f" ] && cat "$f"
e=$(pick "cli.$sub.stderr"); [ -f "$e" ] && cat "$e" >&2
x=$(pick "cli.$sub.exit"); [ -f "$x" ] && exit "$(cat "$x")"
exit 0
"#;

/// The steps an `ensure` reported, in order.
#[derive(Default)]
pub struct Steps(Mutex<Vec<EnvStep>>);

impl EnvProgress for Steps {
    fn step(&self, step: EnvStep) {
        self.0.lock().unwrap().push(step);
    }
}

impl Steps {
    pub fn all(&self) -> Vec<EnvStep> {
        self.0.lock().unwrap().clone()
    }

    /// The last report of each step id, in the order the ids first appeared.
    pub fn latest(&self) -> Vec<EnvStep> {
        let all = self.all();
        let mut ids: Vec<&str> = Vec::new();
        for s in &all {
            if !ids.contains(&s.id.as_str()) {
                ids.push(&s.id);
            }
        }
        ids.iter()
            .map(|id| all.iter().rev().find(|s| s.id == *id).unwrap().clone())
            .collect()
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

pub struct Rig {
    pub tmp: TempDir,
    pub root: PathBuf,
    pub bin: PathBuf,
    pub workspaces: Workspaces,
    pub env: DevContainer,
    pub settings: Settings,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Seed")
        .env("GIT_AUTHOR_EMAIL", "seed@example.com")
        .env("GIT_COMMITTER_NAME", "Seed")
        .env("GIT_COMMITTER_EMAIL", "seed@example.com")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl Rig {
    /// A rig whose stubs answer as a healthy Podman and CLI would.
    pub fn new() -> Self {
        Self::with(|_| {})
    }

    pub fn with(tweak: impl FnOnce(&mut Settings)) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("work");
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        for (name, body) in [("podman", PODMAN), ("devcontainer", CLI)] {
            let path = bin.join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut settings = Settings::new(&root);
        settings.runtime = Runtime::Podman;
        settings.cli = bin.join("devcontainer");
        settings.podman = bin.join("podman");
        settings.container_host = "unix:///run/podman/podman.sock".to_owned();
        settings.default_image = DEFAULT_IMAGE.to_owned();
        settings.network = Network::Inherit;
        settings.deployment = DEPLOYMENT.to_owned();
        settings.model_key = Some(SecretString::from(MODEL_KEY));
        settings.up_timeout = Duration::from_secs(20);
        settings.setup_timeout = Duration::from_secs(20);
        settings.read_timeout = Duration::from_secs(20);
        settings.probe_timeout = Duration::from_secs(5);
        settings.release_timeout = Duration::from_secs(20);
        tweak(&mut settings);
        let rig = Self {
            workspaces: Workspaces::new(
                root.clone(),
                Arc::new(StaticToken::new("tok-never-in-a-container")),
            ),
            env: DevContainer::new(settings.clone()),
            settings,
            tmp,
            root,
            bin,
        };
        rig.healthy();
        rig
    }

    /// The same stubs and root, with other settings (a coder that restarted).
    pub fn restarted(&self) -> DevContainer {
        DevContainer::new(self.settings.clone())
    }

    /// What a healthy service and CLI say.
    pub fn healthy(&self) {
        self.put("podman.info.out", "{}");
        self.put("ps.json", "[]");
        self.put(
            "ps.after-up.json",
            &json!([{"Id": CONTAINER, "State": "running",
                     "Labels": {"adam.vymalo.com/run": RUN, "adam.vymalo.com/deployment": DEPLOYMENT}}])
            .to_string(),
        );
        self.put(
            "cli.read-configuration.stdout",
            r#"{"configuration":{},"mergedConfiguration":{"privileged":false}}"#,
        );
        self.put(
            "cli.up.stdout",
            &format!(r#"{{"outcome":"success","containerId":"{CONTAINER}","remoteUser":"vscode","remoteWorkspaceFolder":"/x"}}"#),
        );
        self.put(
            "cli.run-user-commands.stdout",
            r#"{"outcome":"success","result":"success"}"#,
        );
    }

    pub fn put(&self, name: &str, content: &str) {
        std::fs::write(self.bin.join(name), content).unwrap();
    }

    pub fn remove(&self, name: &str) {
        let _ = std::fs::remove_file(self.bin.join(name));
    }

    pub fn run(&self, run: &str) -> RunWorkspace {
        self.workspaces.run(run).unwrap()
    }

    /// A scratch slot, with these files in it (untracked).
    pub async fn scratch(&self, ws: &RunWorkspace, dir: &str, files: &[(&str, &str)]) -> PathBuf {
        let slot = ws
            .add_scratch(dir, &GitIdentity::new("Adam", "adam@example.com"))
            .await
            .unwrap();
        for (name, content) in files {
            let path = slot.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        slot.path().to_owned()
    }

    /// A repository on a local bare remote with these files, added to the run as a slot.
    pub async fn repository(
        &self,
        ws: &RunWorkspace,
        name: &str,
        files: &[(&str, &str)],
    ) -> PathBuf {
        let remote = self.tmp.path().join(format!("{name}.git"));
        let seed = self.tmp.path().join(format!("{name}-seed"));
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&seed).unwrap();
        git(
            &remote,
            &["init", "--bare", "--quiet", "--initial-branch=main"],
        );
        git(&seed, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(seed.join("README.md"), "hi\n").unwrap();
        for (file, content) in files {
            let path = seed.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "seed"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "--quiet", "origin", "main"]);
        let slot = ws
            .add_repository(&RepoRef::new(remote.to_str().unwrap(), "main"))
            .await
            .unwrap();
        slot.path().to_owned()
    }

    /// `podman inspect` of a container as the stub's `up` made it: the binds are what the override
    /// asked for, recomputed here from the slots (an oracle independent of the code under test).
    pub async fn inspect_for(&self, ws: &RunWorkspace, secrets: bool) {
        let slots = ws.slots_in_join_order().await.unwrap();
        let run_dir = self.root.join("workspaces").join(ws.run());
        let tools = self.env.install_tools().await.unwrap();
        let mut mounts = vec![bind(&run_dir, &run_dir, true)];
        for slot in &slots {
            if let Some(wt) = slot.worktree() {
                mounts.push(bind(wt.mirror(), wt.mirror(), false));
            }
            let git = slot.path().join(".git");
            mounts.push(bind(&git, &git, false));
        }
        mounts.push(bind(&tools, Path::new("/opt/adam/bin"), false));
        if secrets {
            let dir = self
                .root
                .join("environments")
                .join(ws.run())
                .join("secrets");
            mounts.push(bind(&dir, Path::new("/run/adam/secrets"), false));
        }
        mounts.push(bind(
            slots[0].path(),
            &PathBuf::from(format!("/workspaces/{}", slots[0].dir())),
            true,
        ));
        self.put(
            "inspect.json",
            &inspect_json(&mounts, "host", "keep-id").to_string(),
        );
    }

    /// The CLI's subcommands and Podman's `inspect`, `rm`, `exec`, `images` and `rmi`, in the order they were called.
    pub fn order(&self) -> Vec<String> {
        std::fs::read_to_string(self.bin.join("order.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !matches!(*l, "podman info" | "podman ps"))
            .map(str::to_owned)
            .collect()
    }

    /// A worktree of the older layout (`<root>/worktrees/<run>`), made by `Workspaces::prepare`.
    pub async fn legacy_worktree(&self, run: &str, name: &str) -> PathBuf {
        let remote = self.tmp.path().join(format!("{name}.git"));
        let seed = self.tmp.path().join(format!("{name}-seed"));
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&seed).unwrap();
        git(
            &remote,
            &["init", "--bare", "--quiet", "--initial-branch=main"],
        );
        git(&seed, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(seed.join("README.md"), "hi\n").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "seed"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "--quiet", "origin", "main"]);
        let wt = self
            .workspaces
            .prepare(&RepoRef::new(remote.to_str().unwrap(), "main"), run)
            .await
            .unwrap();
        wt.path().to_owned()
    }

    pub fn cli_calls(&self) -> Vec<Vec<String>> {
        calls(&self.bin.join("cli.argv"))
    }

    pub fn podman_calls(&self) -> Vec<Vec<String>> {
        calls(&self.bin.join("podman.argv"))
    }

    /// The calls of the CLI to subcommand `sub`.
    pub fn cli(&self, sub: &str) -> Vec<Vec<String>> {
        self.cli_calls()
            .into_iter()
            .filter(|c| c[0] == sub)
            .collect()
    }

    pub fn podman(&self, sub: &str) -> Vec<Vec<String>> {
        self.podman_calls()
            .into_iter()
            .filter(|c| c[0] == sub)
            .collect()
    }

    pub fn state(&self, run: &str) -> Value {
        let text =
            std::fs::read_to_string(self.root.join("environments").join(run).join("state.json"))
                .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    pub fn override_file(&self, run: &str) -> Value {
        let text = std::fs::read_to_string(
            self.root
                .join("environments")
                .join(run)
                .join("devcontainer.json"),
        )
        .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// Every file under the rig's temp directory that holds `needle`.
    pub fn files_containing(&self, needle: &str) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![self.tmp.path().to_owned()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                let Ok(meta) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if meta.is_dir() {
                    if path.file_name().is_some_and(|n| n == ".git") {
                        continue;
                    }
                    stack.push(path);
                } else if meta.is_file()
                    && std::fs::read(&path)
                        .is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
                {
                    found.push(path);
                }
            }
        }
        found
    }
}

fn bind(source: &Path, target: &Path, rw: bool) -> Value {
    json!({"Type": "bind", "Source": source, "Destination": target, "RW": rw})
}

/// `podman inspect` output (an array of one container) in the shape Podman documents.
pub fn inspect_json(mounts: &[Value], network: &str, userns: &str) -> Value {
    json!([{
        "Id": CONTAINER,
        "ImageName": "localhost/vsc-slot-1a2b-uid:latest",
        "State": {"Running": true, "Status": "running"},
        "HostConfig": {
            "Privileged": false, "CapAdd": [], "SecurityOpt": ["label=disable"], "Devices": [],
            "NetworkMode": network, "PidMode": "private", "IpcMode": "host", "UsernsMode": userns
        },
        "Mounts": mounts,
    }])
}

fn calls(path: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| {
            l.split('\u{1f}')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .collect()
}

/// The value after the flag `name` in `args`.
pub fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// Every value after a repeated flag.
pub fn flags<'a>(args: &'a [String], name: &str) -> Vec<&'a str> {
    args.windows(2)
        .filter(|w| w[0] == name)
        .map(|w| w[1].as_str())
        .collect()
}

pub fn devcontainer_file(body: &str) -> [(&'static str, String); 1] {
    [(".devcontainer/devcontainer.json", body.to_owned())]
}
