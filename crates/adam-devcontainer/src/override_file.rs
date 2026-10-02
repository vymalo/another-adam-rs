//! The file the devcontainer CLI is given instead of the repository's: its content, with the
//! changes this deployment makes.
//!
//! The CLI is run with `--override-config <run dir>/devcontainer.json`, and with `--config` naming
//! the repository's own file, so that the content is ours and the file's directory stays the
//! repository's (a relative `build.dockerfile`, a `context` and local features resolve against it).
//!
//! | Key | Rule |
//! |---|---|
//! | `workspaceFolder` | the first slot's own path: the same path in the coder and in the container |
//! | `workspaceMount` | a bind of the run's whole workspace at the same path, so every slot is in |
//! | `mounts` | the repository's own (volumes and tmpfs, checked by [`policy`](crate::policy)), then ours, all read-only: the mirror of each repository slot, each slot's `.git`, the tools, the secrets. The first slot is also bound where the repository says its workspace is (default `/workspaces/<dir>`), read-write |
//! | `runArgs` | the repository's own (checked), then `--network=host` or `--network=none` |
//! | `initializeCommand` | removed: the specification runs it on the host side, which here is the coder, next to its credentials |
//! | `appPort`, `forwardPorts`, `portsAttributes`, `otherPortsAttributes` | removed: nothing is published |
//!
//! Every change is listed in [`Override::changes`], which the step that builds the environment
//! shows.

use std::path::{Path, PathBuf};

use adam_workspace::EnvError;
use serde_json::{Map, Value, json};

use crate::Network;
use crate::policy::{Bind, SECRETS_TARGET, TOOLS_TARGET};

/// A slot of the run, as far as the file needs to know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlotInfo {
    /// The slot's directory name.
    pub dir: String,
    /// Its path.
    pub path: PathBuf,
    /// The bare mirror a repository slot's worktree is linked to; `None` for a scratch slot.
    pub mirror: Option<PathBuf>,
}

/// What the file is made from.
pub(crate) struct OverrideInput<'a> {
    /// The repository's file (already checked by [`policy::check_raw`](crate::policy::check_raw)).
    pub raw: &'a Value,
    /// `<root>/workspaces/<run>`.
    pub run_dir: &'a Path,
    /// The first slot, whose file this is.
    pub config_slot: &'a SlotInfo,
    /// Every slot of the run, in the order they joined.
    pub slots: &'a [SlotInfo],
    /// `<root>/environments/.tools/<version>`.
    pub tools_dir: &'a Path,
    /// `<root>/environments/<run>/secrets`, when there is a secret to give.
    pub secrets_dir: Option<&'a Path>,
    /// The network of the container.
    pub network: Network,
}

/// The file and what was changed to make it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Override {
    /// The content.
    pub value: Value,
    /// One line per change, for a person.
    pub changes: Vec<String>,
    /// The binds this crate added, which the later checks let through.
    pub binds: Vec<Bind>,
}

/// Make the file.
///
/// # Errors
///
/// [`EnvError::Refused`] for a path that the string form of a mount cannot carry.
pub(crate) fn build(input: &OverrideInput<'_>) -> Result<Override, EnvError> {
    let mut map: Map<String, Value> = input.raw.as_object().cloned().unwrap_or_default();
    let mut changes = Vec::new();
    let slot_path = input.config_slot.path.display().to_string();
    let run_dir = input.run_dir.display().to_string();

    let declared = declared_folder(&map, &input.config_slot.dir, &slot_path);
    match map.get("workspaceFolder").and_then(Value::as_str) {
        Some(was) if was == slot_path => {}
        Some(was) => changes.push(format!(
            "workspaceFolder {was} is {slot_path}: the path is the same in the coder and in the container"
        )),
        None => {}
    }
    if map.contains_key("workspaceMount") {
        changes.push(format!(
            "workspaceMount is a bind of {run_dir}: every slot of the run is in the container"
        ));
    }
    map.insert("workspaceFolder".to_owned(), json!(slot_path));

    let mut binds = vec![Bind::new(&run_dir, &run_dir, false)];
    let mut readonly = Vec::new();
    for slot in input.slots {
        if let Some(mirror) = &slot.mirror {
            readonly.push(Bind::new(
                mirror.display().to_string(),
                mirror.display().to_string(),
                true,
            ));
        }
        let git = slot.path.join(".git").display().to_string();
        readonly.push(Bind::new(&git, &git, true));
    }
    let tools = input.tools_dir.display().to_string();
    readonly.push(Bind::new(tools, TOOLS_TARGET, true));
    if let Some(secrets) = input.secrets_dir {
        readonly.push(Bind::new(
            secrets.display().to_string(),
            SECRETS_TARGET,
            true,
        ));
    }
    binds.extend(readonly.iter().cloned());
    changes.push(format!(
        "{} read-only mounts added: each repository's mirror and each slot's .git (so that git can read \
         inside), the tools at {TOOLS_TARGET}{}",
        readonly.len(),
        if input.secrets_dir.is_some() {
            format!(", the secrets at {SECRETS_TARGET}")
        } else {
            String::new()
        }
    ));
    match declared {
        Some(folder) if folder == slot_path => {}
        Some(folder) if !overlaps_ours(&folder, &binds) => {
            changes.push(format!(
                "{slot_path} is also bound at {folder}, where the repository says its workspace is"
            ));
            binds.push(Bind::new(&slot_path, folder, false));
        }
        Some(folder) => changes.push(format!(
            "the workspace folder {folder} is not bound: it overlaps a path of the coder"
        )),
        None => changes.push(
            "the workspace folder uses a variable that only the devcontainer CLI resolves: not bound"
                .to_owned(),
        ),
    }

    let mut mounts: Vec<Value> = map
        .get("mounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let workspace_mount = binds[0].mount_string();
    for bind in binds.iter().skip(1) {
        let text = bind.mount_string().ok_or_else(|| unsupported_path(bind))?;
        mounts.push(Value::String(text));
    }
    map.insert(
        "workspaceMount".to_owned(),
        Value::String(workspace_mount.ok_or_else(|| unsupported_path(&binds[0]))?),
    );
    map.insert("mounts".to_owned(), Value::Array(mounts));

    let mut run_args: Vec<Value> = map
        .get("runArgs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let network = match input.network {
        Network::Inherit => "--network=host",
        Network::None => "--network=none",
    };
    run_args.push(Value::String(network.to_owned()));
    map.insert("runArgs".to_owned(), Value::Array(run_args));
    changes.push(format!(
        "{network}{}",
        match input.network {
            Network::Inherit => ": the network of the Podman service, which the deployment limits",
            Network::None => ": no network",
        }
    ));

    if map.remove("initializeCommand").is_some() {
        changes.push(
            "initializeCommand removed: it would run in the coder's own container, next to its credentials"
                .to_owned(),
        );
    }
    let ports: Vec<&str> = [
        "appPort",
        "forwardPorts",
        "portsAttributes",
        "otherPortsAttributes",
    ]
    .into_iter()
    .filter(|key| map.remove(*key).is_some())
    .collect();
    if !ports.is_empty() {
        changes.push(format!(
            "{} ignored: nothing is published",
            ports.join(", ")
        ));
    }
    Ok(Override {
        value: Value::Object(map),
        changes,
        binds,
    })
}

fn unsupported_path(bind: &Bind) -> EnvError {
    EnvError::Refused(format!(
        "the path {} or {} holds a character that a mount cannot carry (a comma, a quote or a line break)",
        bind.source, bind.target
    ))
}

/// Where the repository says its workspace is: `workspaceFolder` (with the two variables a file may
/// use for it substituted), else `/workspaces/<slot dir>`. `None` when the file uses another
/// variable, which only the CLI could resolve.
fn declared_folder(map: &Map<String, Value>, dir: &str, slot_path: &str) -> Option<String> {
    let Some(text) = map.get("workspaceFolder").and_then(Value::as_str) else {
        return Some(format!("/workspaces/{dir}"));
    };
    let text = text
        .replace("${localWorkspaceFolderBasename}", dir)
        .replace("${localWorkspaceFolder}", slot_path);
    (!text.contains("${")).then_some(text)
}

/// Whether binding the slot at `folder` would cover, or be covered by, something else this crate
/// binds, or is not a place a workspace can be.
fn overlaps_ours(folder: &str, binds: &[Bind]) -> bool {
    let path = Path::new(folder);
    let plain = path.is_absolute()
        && path.components().count() >= 3
        && path.components().all(|c| {
            matches!(
                c,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        });
    !plain
        || binds.iter().any(|b| {
            let target = Path::new(&b.target);
            target.starts_with(path) || path.starts_with(target)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(dir: &str, mirror: bool) -> SlotInfo {
        SlotInfo {
            dir: dir.to_owned(),
            path: PathBuf::from(format!("/work/workspaces/r1/{dir}")),
            mirror: mirror.then(|| PathBuf::from(format!("/work/git/h/o/{dir}.git"))),
        }
    }

    fn make(raw: &Value, slots: &[SlotInfo], secrets: bool, network: Network) -> Override {
        build(&OverrideInput {
            raw,
            run_dir: Path::new("/work/workspaces/r1"),
            config_slot: &slots[0],
            slots,
            tools_dir: Path::new("/work/environments/.tools/ab12"),
            secrets_dir: secrets.then_some(Path::new("/work/environments/r1/secrets")),
            network,
        })
        .unwrap()
    }

    fn mounts_of(o: &Override) -> Vec<&str> {
        o.value["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect()
    }

    #[test]
    fn the_workspace_is_the_same_path_in_the_container_and_holds_every_slot() {
        let o = make(
            &json!({"image": "x", "workspaceFolder": "/workspaces/devbox", "workspaceMount": "type=bind,source=/etc,target=/w"}),
            &[slot("devbox", true)],
            false,
            Network::Inherit,
        );
        assert_eq!(o.value["workspaceFolder"], "/work/workspaces/r1/devbox");
        assert_eq!(
            o.value["workspaceMount"],
            "type=bind,source=/work/workspaces/r1,target=/work/workspaces/r1"
        );
        assert!(o.changes.iter().any(|c| {
            c.starts_with("workspaceFolder /workspaces/devbox is /work/workspaces/r1/devbox")
        }));
        assert!(
            o.changes
                .iter()
                .any(|c| c.starts_with("workspaceMount is a bind of /work/workspaces/r1"))
        );
    }

    #[test]
    fn the_first_slot_is_also_bound_where_the_repository_says_its_workspace_is() {
        let o = make(
            &json!({"image": "x"}),
            &[slot("devbox", true)],
            false,
            Network::Inherit,
        );
        let mounts = mounts_of(&o);
        assert!(
            mounts
                .contains(&"type=bind,source=/work/workspaces/r1/devbox,target=/workspaces/devbox"),
            "{mounts:?}"
        );
        assert!(o.binds.contains(&Bind::new(
            "/work/workspaces/r1/devbox",
            "/workspaces/devbox",
            false
        )));
        // A file that names a folder with the CLI's variables: the same.
        let o = make(
            &json!({"image": "x", "workspaceFolder": "/src/${localWorkspaceFolderBasename}"}),
            &[slot("devbox", false)],
            false,
            Network::Inherit,
        );
        assert!(
            mounts_of(&o)
                .contains(&"type=bind,source=/work/workspaces/r1/devbox,target=/src/devbox")
        );
        // Not bound when it would cover the coder's own paths, or is no place at all.
        for folder in [
            "/",
            "/work",
            "/opt/adam/bin",
            "/work/workspaces/r1/devbox/sub",
            "relative",
            "/a/${localEnv:X}",
        ] {
            let o = make(
                &json!({"image": "x", "workspaceFolder": folder}),
                &[slot("devbox", false)],
                false,
                Network::Inherit,
            );
            assert!(
                !o.binds
                    .iter()
                    .any(|b| b.source == "/work/workspaces/r1/devbox"),
                "{folder}: the slot is not bound there"
            );
            assert_eq!(o.value["workspaceFolder"], "/work/workspaces/r1/devbox");
        }
    }

    #[test]
    fn every_mirror_and_every_dot_git_is_read_only_and_so_are_the_tools_and_the_secrets() {
        let slots = [
            slot("devbox", true),
            slot("lib", true),
            slot("notes", false),
        ];
        let o = make(&json!({"image": "x"}), &slots, true, Network::Inherit);
        let mounts = mounts_of(&o);
        for want in [
            "type=bind,source=/work/git/h/o/devbox.git,target=/work/git/h/o/devbox.git,readonly",
            "type=bind,source=/work/git/h/o/lib.git,target=/work/git/h/o/lib.git,readonly",
            "type=bind,source=/work/workspaces/r1/devbox/.git,target=/work/workspaces/r1/devbox/.git,readonly",
            "type=bind,source=/work/workspaces/r1/lib/.git,target=/work/workspaces/r1/lib/.git,readonly",
            "type=bind,source=/work/workspaces/r1/notes/.git,target=/work/workspaces/r1/notes/.git,readonly",
            "type=bind,source=/work/environments/.tools/ab12,target=/opt/adam/bin,readonly",
            "type=bind,source=/work/environments/r1/secrets,target=/run/adam/secrets,readonly",
        ] {
            assert!(mounts.contains(&want), "{want} in {mounts:?}");
        }
        assert!(
            !mounts.iter().any(|m| m.contains("notes.git")),
            "a scratch slot has no mirror"
        );
        // Without a secret there is no secrets mount.
        let o = make(&json!({"image": "x"}), &slots, false, Network::Inherit);
        assert!(
            !mounts_of(&o)
                .iter()
                .any(|m| m.contains("/run/adam/secrets"))
        );
    }

    #[test]
    fn the_repositorys_own_mounts_come_first_and_stay() {
        let o = make(
            &json!({"image": "x", "mounts": ["type=volume,source=cache,target=/c", {"type": "tmpfs", "target": "/t"}]}),
            &[slot("devbox", true)],
            false,
            Network::Inherit,
        );
        let all = o.value["mounts"].as_array().unwrap();
        assert_eq!(all[0], "type=volume,source=cache,target=/c");
        assert_eq!(all[1], json!({"type": "tmpfs", "target": "/t"}));
        assert!(all[2].as_str().unwrap().starts_with("type=bind,"));
    }

    #[test]
    fn the_network_arg_is_appended_after_the_repositorys_run_args() {
        let o = make(
            &json!({"image": "x", "runArgs": ["--init", "-e", "A=b"]}),
            &[slot("d", false)],
            false,
            Network::Inherit,
        );
        assert_eq!(
            o.value["runArgs"],
            json!(["--init", "-e", "A=b", "--network=host"])
        );
        let o = make(
            &json!({"image": "x"}),
            &[slot("d", false)],
            false,
            Network::None,
        );
        assert_eq!(o.value["runArgs"], json!(["--network=none"]));
        assert!(
            o.changes
                .iter()
                .any(|c| c.starts_with("--network=none: no network"))
        );
    }

    #[test]
    fn initialize_command_and_every_port_key_are_removed_and_the_changes_say_so() {
        let o = make(
            &json!({"image": "x", "initializeCommand": "touch /work/INIT-RAN", "appPort": [3000],
                    "forwardPorts": [3000], "portsAttributes": {"3000": {}}, "postCreateCommand": "true"}),
            &[slot("d", false)],
            false,
            Network::Inherit,
        );
        let map = o.value.as_object().unwrap();
        for key in [
            "initializeCommand",
            "appPort",
            "forwardPorts",
            "portsAttributes",
        ] {
            assert!(!map.contains_key(key), "{key}");
        }
        assert_eq!(map["postCreateCommand"], "true", "what runs inside stays");
        assert!(
            o.changes
                .iter()
                .any(|c| c.starts_with("initializeCommand removed"))
        );
        assert!(
            o.changes.iter().any(
                |c| c == "appPort, forwardPorts, portsAttributes ignored: nothing is published"
            )
        );
    }

    #[test]
    fn the_binds_are_exactly_the_mounts_and_the_workspace() {
        let o = make(
            &json!({"image": "x"}),
            &[slot("devbox", true)],
            true,
            Network::Inherit,
        );
        let mounts: Vec<String> = mounts_of(&o).iter().map(|m| (*m).to_owned()).collect();
        for bind in o.binds.iter().skip(1) {
            assert!(mounts.contains(&bind.mount_string().unwrap()), "{bind:?}");
        }
        assert_eq!(
            o.binds[0],
            Bind::new("/work/workspaces/r1", "/work/workspaces/r1", false)
        );
    }

    #[test]
    fn a_path_a_mount_cannot_carry_is_refused() {
        let slots = [SlotInfo {
            dir: "a,b".into(),
            path: PathBuf::from("/work/workspaces/r1/a,b"),
            mirror: None,
        }];
        let err = build(&OverrideInput {
            raw: &json!({"image": "x"}),
            run_dir: Path::new("/work/workspaces/r1"),
            config_slot: &slots[0],
            slots: &slots,
            tools_dir: Path::new("/t"),
            secrets_dir: None,
            network: Network::Inherit,
        })
        .unwrap_err();
        assert!(matches!(err, EnvError::Refused(_)), "{err:?}");
    }
}
