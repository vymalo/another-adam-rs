//! What a devcontainer may ask for. A repository's `devcontainer.json` is untrusted input: it can
//! run commands, mount directories, add capabilities and build images from anywhere under its
//! context. The same policy is checked three times, because each check sees something the others
//! cannot:
//!
//! 1. [`check_raw`], on the repository's own file, before anything is built;
//! 2. [`check_merged`], on the `mergedConfiguration` of `devcontainer read-configuration`, because
//!    features and the image's `devcontainer.metadata` label add `privileged`, `capAdd`, `securityOpt`
//!    and `mounts` to what the file says;
//! 3. [`check_inspect`], on `podman inspect` of the container that was created, which has the last
//!    word: whatever the first two missed, a container that is privileged, has a device, a bind
//!    mount that is not one of ours, or shares the process namespace is refused and removed.
//!
//! Every function returns [`EnvError::Refused`] naming the file and every key at fault, or
//! [`EnvError::Config`] for a file that does not say what to build.

use std::path::{Component, Path, PathBuf};

use adam_workspace::EnvError;
use serde_json::{Map, Value};

use crate::Network;

/// Where the tools of the coder are mounted in the container.
pub(crate) const TOOLS_TARGET: &str = "/opt/adam/bin";
/// Where the secrets of a run are mounted in the container.
pub(crate) const SECRETS_TARGET: &str = "/run/adam/secrets";
/// Targets a repository may not mount over: ours.
const RESERVED_TARGETS: [&str; 2] = ["/opt/adam", "/run/adam"];
/// The prefix of the labels this crate puts on its containers.
pub(crate) const LABEL_PREFIX: &str = "adam.vymalo.com/";

/// `runArgs` that take a value (the next argument, or after `=`).
const RUN_ARGS_WITH_VALUE: [&str; 15] = [
    "-e",
    "--env",
    "-l",
    "--label",
    "--hostname",
    "--add-host",
    "--shm-size",
    "--ulimit",
    "--tmpfs",
    "-m",
    "--memory",
    "--memory-swap",
    "--memory-reservation",
    "--cpus",
    "--pids-limit",
];
/// `runArgs` that are a switch.
const RUN_ARGS_SWITCH: [&str; 1] = ["--init"];

/// The keys of a mount that a volume or a tmpfs may carry besides its type, source and target.
const MOUNT_EXTRA_KEYS: [&str; 6] = [
    "external",
    "volume-nocopy",
    "subpath",
    "tmpfs-size",
    "tmpfs-mode",
    "readonly",
];

/// A bind mount: what this crate adds to a container, and what the checks of the merged
/// configuration and of the created container let through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bind {
    /// The directory or file on the volume the coder and the Podman service share.
    pub source: String,
    /// Where it is in the container.
    pub target: String,
    /// Whether the container may not write to it.
    pub readonly: bool,
}

impl Bind {
    /// A bind of `source` at `target`.
    pub(crate) fn new(
        source: impl Into<String>,
        target: impl Into<String>,
        readonly: bool,
    ) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            readonly,
        }
    }

    /// The mount as the string form of `devcontainer.json`'s `mounts` (and of `workspaceMount`).
    /// `None` when a path holds a character that the string form cannot carry.
    pub(crate) fn mount_string(&self) -> Option<String> {
        let bad = |s: &str| s.contains([',', '\n', '"']);
        if bad(&self.source) || bad(&self.target) {
            return None;
        }
        Some(format!(
            "type=bind,source={},target={}{}",
            self.source,
            self.target,
            if self.readonly { ",readonly" } else { "" }
        ))
    }
}

/// One entry of `mounts`, read from its string or object form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MountSpec {
    kind: String,
    source: String,
    target: String,
    /// Keys the entry carries besides type, source and target (lowercase).
    extra: Vec<String>,
}

impl MountSpec {
    /// Read `value`: `type=volume,source=x,target=/y` or `{"type": .., "source": .., "target": ..}`.
    pub(crate) fn parse(value: &Value) -> Result<Self, String> {
        let mut spec = Self::default();
        match value {
            Value::String(text) => {
                for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    let (key, val) = part.split_once('=').unwrap_or((part, ""));
                    spec.set(key, val);
                }
            }
            Value::Object(map) => {
                for (key, val) in map {
                    let text = match val {
                        Value::String(s) => s.clone(),
                        Value::Bool(b) => b.to_string(),
                        Value::Number(n) => n.to_string(),
                        _ => return Err(format!("`{key}` is not a string")),
                    };
                    spec.set(key, &text);
                }
            }
            _ => return Err("is neither a string nor an object".to_owned()),
        }
        Ok(spec)
    }

    fn set(&mut self, key: &str, value: &str) {
        match key.to_ascii_lowercase().as_str() {
            "type" => self.kind = value.to_ascii_lowercase(),
            "source" | "src" => self.source = value.to_owned(),
            "target" | "dst" | "destination" => self.target = value.to_owned(),
            "ro" => self.extra.push("readonly".to_owned()),
            other => self.extra.push(other.to_owned()),
        }
    }

    /// The type, `volume` when the entry says none (what `--mount` does).
    fn kind(&self) -> &str {
        if self.kind.is_empty() {
            "volume"
        } else {
            &self.kind
        }
    }

    fn is_bind_of(&self, binds: &[Bind]) -> bool {
        self.kind() == "bind"
            && binds
                .iter()
                .any(|b| b.source == self.source && b.target == self.target)
    }
}

/// Why `spec` may not be a mount of a repository's container, if it may not. A repository gets
/// volumes and tmpfs only, never a bind: a bind would show it the shared volume or the Podman
/// service's own files.
fn mount_problem(spec: &MountSpec) -> Option<String> {
    let kind = spec.kind();
    if kind != "volume" && kind != "tmpfs" {
        return Some(format!(
            "the {kind} mount of `{}` (only `volume` and `tmpfs` mounts are allowed)",
            spec.target
        ));
    }
    if let Some(extra) = spec
        .extra
        .iter()
        .find(|k| !MOUNT_EXTRA_KEYS.contains(&k.as_str()))
    {
        // `volume-opt` and `volume-driver` make a volume a bind of a host path in disguise.
        return Some(format!(
            "the mount of `{}` has the option `{extra}`, which is not allowed",
            spec.target
        ));
    }
    if !is_normal_absolute(&spec.target) {
        return Some(format!(
            "the mount target `{}` is not an absolute path",
            spec.target
        ));
    }
    if RESERVED_TARGETS
        .iter()
        .any(|r| Path::new(&spec.target).starts_with(r))
    {
        return Some(format!(
            "the mount target `{}` is one the coder uses",
            spec.target
        ));
    }
    if kind == "volume" && !spec.source.is_empty() && !is_volume_name(&spec.source) {
        return Some(format!(
            "the volume name `{}` is not a name (a path would be a bind mount)",
            spec.source
        ));
    }
    if kind == "tmpfs" && !spec.source.is_empty() {
        return Some("a tmpfs mount has no source".to_owned());
    }
    None
}

/// A volume name, where `${...}` expressions of the file count as one letter each (the CLI
/// substitutes them after this check, and a result that is a path is refused again by the check of
/// the created container).
fn is_volume_name(name: &str) -> bool {
    let mut flat = String::new();
    let mut rest = name;
    while let Some(start) = rest.find("${") {
        flat.push_str(&rest[..start]);
        flat.push('x');
        match rest[start..].find('}') {
            Some(end) => rest = &rest[start + end + 1..],
            None => return false,
        }
    }
    flat.push_str(rest);
    let mut chars = flat.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// An absolute path with no `.` or `..` component.
fn is_normal_absolute(path: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

/// `path` resolved lexically against `base`, or `None` when it climbs out of the root.
pub(crate) fn lexical_join(base: &Path, path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in base.join(path).components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    Some(out)
}

/// Where the config file is, for the checks that read paths in it.
pub(crate) struct RawContext<'a> {
    /// The file, relative to the slot, as the messages call it.
    pub file: &'a Path,
    /// The directory the file's relative paths are relative to (the file's own directory).
    pub config_dir: &'a Path,
    /// The first slot of the run: no path of the file may leave it.
    pub slot: &'a Path,
}

fn refused(file: &Path, problems: Vec<String>) -> Result<(), EnvError> {
    if problems.is_empty() {
        Ok(())
    } else {
        Err(EnvError::Refused(format!(
            "{}: {}",
            file.display(),
            problems.join("; ")
        )))
    }
}

/// Check the repository's own file (see the module documentation, check 1).
///
/// # Errors
///
/// [`EnvError::Config`] when it names neither an `image` nor a `build`; [`EnvError::Refused`] for
/// everything the policy refuses.
pub(crate) fn check_raw(config: &Value, at: &RawContext<'_>) -> Result<(), EnvError> {
    let file = at.file;
    let Some(map) = config.as_object() else {
        return Err(EnvError::Config {
            file: file.to_owned(),
            reason: "must hold one JSON object".to_owned(),
        });
    };
    let mut problems = Vec::new();
    if map.contains_key("dockerComposeFile") {
        problems.push(
            "`dockerComposeFile` is not supported: a devcontainer here is one container, built from \
             `image` or `build`"
                .to_owned(),
        );
    } else if !builds_something(map) {
        return Err(EnvError::Config {
            file: file.to_owned(),
            reason: "says neither `image` nor `build.dockerfile`: there is nothing to build"
                .to_owned(),
        });
    }
    privileges(map, &mut problems);
    mounts(map, &[], &mut problems);
    run_args(map, &mut problems);
    build(map, at, &mut problems);
    features(map, at, &mut problems);
    refused(file, problems)
}

/// Whether the file says what the container is made from.
fn builds_something(map: &Map<String, Value>) -> bool {
    let text = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    text(map.get("image"))
        || text(map.get("dockerFile"))
        || text(map.get("dockerfile"))
        || map
            .get("build")
            .and_then(Value::as_object)
            .is_some_and(|b| text(b.get("dockerfile")) || text(b.get("dockerFile")))
}

/// `privileged`, `capAdd` and `securityOpt`: the same keys in the file, the merged configuration
/// and (as `HostConfig`) the created container.
fn privileges(map: &Map<String, Value>, problems: &mut Vec<String>) {
    match map.get("privileged") {
        None | Some(Value::Null | Value::Bool(false)) => {}
        Some(_) => {
            problems.push("`privileged` is refused: a devcontainer runs unprivileged".to_owned())
        }
    }
    for cap in strings(map.get("capAdd")) {
        if !is_ptrace(&cap) {
            problems.push(format!(
                "`capAdd` {cap} is refused (only SYS_PTRACE, for debuggers)"
            ));
        }
    }
    for opt in strings(map.get("securityOpt")) {
        if !is_allowed_security_opt(&opt) {
            problems.push(format!(
                "`securityOpt` {opt} is refused (only `seccomp=unconfined` and `label=disable`)"
            ));
        }
    }
}

/// The strings of an array value; a value that is not an array of strings counts as one entry per
/// element that is not (so that it is refused, not skipped).
fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            .collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(other) => vec![other.to_string()],
    }
}

fn is_ptrace(cap: &str) -> bool {
    let upper = cap.trim().to_ascii_uppercase();
    upper.strip_prefix("CAP_").unwrap_or(&upper) == "SYS_PTRACE"
}

fn is_allowed_security_opt(opt: &str) -> bool {
    matches!(
        opt.trim().to_ascii_lowercase().as_str(),
        "seccomp=unconfined" | "label=disable" | "label:disable"
    )
}

/// `mounts`: volumes and tmpfs, plus the binds in `binds` (ours, in the merged configuration).
fn mounts(map: &Map<String, Value>, binds: &[Bind], problems: &mut Vec<String>) {
    let Some(value) = map.get("mounts") else {
        return;
    };
    let Some(items) = value.as_array() else {
        if !value.is_null() {
            problems.push("`mounts` is not an array".to_owned());
        }
        return;
    };
    for item in items {
        match MountSpec::parse(item) {
            Ok(spec) if spec.is_bind_of(binds) => {}
            Ok(spec) => problems.extend(mount_problem(&spec).map(|p| format!("`mounts`: {p}"))),
            Err(why) => problems.push(format!("`mounts` has an entry that {why}")),
        }
    }
}

/// `runArgs`: a short list of flags.
fn run_args(map: &Map<String, Value>, problems: &mut Vec<String>) {
    let Some(value) = map.get("runArgs") else {
        return;
    };
    let Some(items) = value.as_array() else {
        if !value.is_null() {
            problems.push("`runArgs` is not an array".to_owned());
        }
        return;
    };
    let mut args = items.iter();
    while let Some(arg) = args.next() {
        let Some(arg) = arg.as_str() else {
            problems.push("`runArgs` has an entry that is not a string".to_owned());
            continue;
        };
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if arg.starts_with("--") => (flag, Some(value.to_owned())),
            _ => (arg, None),
        };
        if RUN_ARGS_SWITCH.contains(&flag) {
            continue;
        }
        let mut is_label = matches!(flag, "-l" | "--label");
        let value = if RUN_ARGS_WITH_VALUE.contains(&flag) {
            match inline {
                Some(v) => v,
                None => match args.next().and_then(Value::as_str) {
                    Some(v) => v.to_owned(),
                    None => {
                        problems.push(format!("`runArgs` {flag} has no value"));
                        continue;
                    }
                },
            }
        } else if let Some(short) = ["-e", "-l", "-m"]
            .iter()
            .find(|s| arg.starts_with(**s) && arg.len() > s.len() && !arg.starts_with("--"))
        {
            // `-eNAME=value`, `-lkey=value`, `-m512m`.
            is_label = *short == "-l";
            arg[short.len()..].to_owned()
        } else {
            problems.push(format!(
                "`runArgs` {arg} is refused (allowed: -e/--env, -l/--label, --hostname, --add-host, \
                 --shm-size, --ulimit, --init, --tmpfs, --memory*, --cpus, --pids-limit)"
            ));
            continue;
        };
        if is_label && value.starts_with(LABEL_PREFIX) {
            problems.push(format!(
                "`runArgs` {flag} {value}: the label prefix `{LABEL_PREFIX}` is the coder's"
            ));
        }
    }
}

/// `build`: no extra options for the builder, and nothing outside the repository in its paths.
fn build(map: &Map<String, Value>, at: &RawContext<'_>, problems: &mut Vec<String>) {
    let mut paths = Vec::new();
    for key in ["dockerFile", "dockerfile", "context"] {
        if let Some(path) = map.get(key).and_then(Value::as_str) {
            paths.push((key.to_owned(), path.to_owned()));
        }
    }
    if let Some(build) = map.get("build").and_then(Value::as_object) {
        for key in ["dockerfile", "dockerFile", "context"] {
            if let Some(path) = build.get(key).and_then(Value::as_str) {
                paths.push((format!("build.{key}"), path.to_owned()));
            }
        }
        if build
            .get("options")
            .and_then(Value::as_array)
            .is_some_and(|o| !o.is_empty())
        {
            problems.push(
                "`build.options` is refused: extra options of the builder can read files outside \
                 the repository (--secret, --ssh)"
                    .to_owned(),
            );
        }
    }
    for (key, path) in paths {
        if !inside(at, &path) {
            problems.push(format!(
                "`{key}` {path} leaves the repository: a build reads only what is in it"
            ));
        }
    }
}

/// Local features (a path) must be inside the repository too.
fn features(map: &Map<String, Value>, at: &RawContext<'_>, problems: &mut Vec<String>) {
    let Some(features) = map.get("features").and_then(Value::as_object) else {
        return;
    };
    for id in features.keys() {
        if (id.starts_with('.') || id.starts_with('/')) && !inside(at, id) {
            problems.push(format!(
                "feature {id} leaves the repository: a local feature must be inside it"
            ));
        }
    }
}

/// Whether `path` (relative to the config's directory) stays inside the slot.
fn inside(at: &RawContext<'_>, path: &str) -> bool {
    lexical_join(at.config_dir, path).is_some_and(|p| p.starts_with(at.slot))
}

/// Check what the CLI says the configuration is once features and the image's metadata are
/// merged in (see the module documentation, check 2). `binds` are the mounts this crate added.
///
/// # Errors
///
/// [`EnvError::Refused`] for a privilege, a capability, a security option or a mount the policy
/// refuses; [`EnvError::Build`] when the CLI gave no merged configuration (it cannot be checked).
pub(crate) fn check_merged(
    merged: Option<&Value>,
    binds: &[Bind],
    file: &Path,
) -> Result<(), EnvError> {
    let Some(map) = merged.and_then(Value::as_object) else {
        return Err(EnvError::Build {
            reason: "the devcontainer CLI gave no merged configuration to check".to_owned(),
            log_tail: String::new(),
        });
    };
    let mut problems = Vec::new();
    privileges(map, &mut problems);
    mounts(map, binds, &mut problems);
    refused(
        file,
        problems
            .into_iter()
            .map(|p| format!("{p} (added by a feature or the image)"))
            .collect(),
    )
}

/// What a created container must look like.
pub(crate) struct Expect<'a> {
    /// The binds this crate added (every other bind is refused).
    pub binds: &'a [Bind],
    /// The network mode it asked for.
    pub network: Network,
}

/// What `podman inspect` says about a container that passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Inspected {
    /// The id.
    pub id: String,
    /// The image it runs.
    pub image: String,
    /// Whether it is running.
    pub running: bool,
}

/// Check the created container (see the module documentation, check 3). `inspect` is one element
/// of the array `podman inspect` prints.
///
/// # Errors
///
/// [`EnvError::Refused`] naming every field at fault.
pub(crate) fn check_inspect(
    inspect: &Value,
    expect: &Expect<'_>,
    file: &Path,
) -> Result<Inspected, EnvError> {
    let host = inspect.get("HostConfig").unwrap_or(&Value::Null);
    let mut problems = Vec::new();
    if host.get("Privileged").and_then(Value::as_bool) != Some(false) {
        problems.push("the container is privileged".to_owned());
    }
    for cap in strings(host.get("CapAdd")) {
        if !is_ptrace(&cap) {
            problems.push(format!("the container has the capability {cap}"));
        }
    }
    for opt in strings(host.get("SecurityOpt")) {
        if !is_allowed_security_opt(&opt) {
            problems.push(format!("the container has the security option {opt}"));
        }
    }
    if host
        .get("Devices")
        .and_then(Value::as_array)
        .is_some_and(|d| !d.is_empty())
    {
        problems.push("the container has a device".to_owned());
    }
    let mode = |key: &str| {
        host.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let network = mode("NetworkMode");
    let wanted = match expect.network {
        Network::Inherit => "host",
        Network::None => "none",
    };
    if network != wanted {
        problems.push(format!("the network mode is `{network}`, not `{wanted}`"));
    }
    let pid = mode("PidMode");
    if !(pid.is_empty() || pid == "private") {
        problems.push(format!("the process namespace is `{pid}`, not its own"));
    }
    // `host` is the Podman service's own namespace (its containers.conf sets ipcns="host"): every
    // devcontainer shares it, and no one else's is reachable. Joining another container's or a path is not.
    let ipc = mode("IpcMode");
    if ipc.starts_with("container:") || ipc.starts_with("ns:") {
        problems.push(format!("the IPC namespace is `{ipc}`"));
    }
    for mount in inspect
        .get("Mounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let kind = mount.get("Type").and_then(Value::as_str).unwrap_or("");
        let source = mount.get("Source").and_then(Value::as_str).unwrap_or("");
        let target = mount
            .get("Destination")
            .and_then(Value::as_str)
            .unwrap_or("");
        match kind {
            "volume" | "tmpfs" => {}
            "bind" => match expect
                .binds
                .iter()
                .find(|b| b.source == source && b.target == target)
            {
                None => problems.push(format!(
                    "the container has a bind mount of {source} at {target}"
                )),
                Some(bind)
                    if bind.readonly && mount.get("RW").and_then(Value::as_bool) != Some(false) =>
                {
                    problems.push(format!("{target} is writable, and must be read-only"));
                }
                Some(_) => {}
            },
            other => problems.push(format!(
                "the container has a mount of type {other} at {target}"
            )),
        }
    }
    refused(
        file,
        problems
            .into_iter()
            .map(|p| format!("{p} (the created container)"))
            .collect(),
    )?;
    let image = inspect
        .get("ImageName")
        .or_else(|| inspect.pointer("/Config/Image"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    Ok(Inspected {
        id: inspect
            .get("Id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        image,
        running: inspect
            .pointer("/State/Running")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SLOT: &str = "/work/workspaces/r1/devbox";

    fn at() -> RawContext<'static> {
        RawContext {
            file: Path::new(".devcontainer/devcontainer.json"),
            config_dir: Path::new("/work/workspaces/r1/devbox/.devcontainer"),
            slot: Path::new(SLOT),
        }
    }

    fn refused_with(config: &Value) -> String {
        match check_raw(config, &at()) {
            Err(EnvError::Refused(why)) => why,
            other => panic!("expected a refusal, got {other:?} for {config}"),
        }
    }

    #[test]
    fn an_ordinary_file_passes() {
        let config = json!({
            "build": {"dockerfile": "Dockerfile", "context": ".."},
            "remoteUser": "vscode",
            "containerEnv": {"A": "b"},
            "mounts": [
                "type=volume,source=cargo-cache,target=/usr/local/cargo/registry",
                {"type": "tmpfs", "target": "/tmp/fast"},
            ],
            "runArgs": ["--init", "-e", "FOO=bar", "--label=team=x", "--memory", "2g", "--cpus=2", "-eX=1"],
            "capAdd": ["SYS_PTRACE"],
            "securityOpt": ["seccomp=unconfined"],
            "privileged": false,
            "features": {"ghcr.io/devcontainers/features/node:1": {}, "./local": {}},
            "postCreateCommand": "npm ci",
        });
        check_raw(&config, &at()).unwrap();
        check_raw(&json!({"image": "x"}), &at()).unwrap();
        check_raw(&json!({"dockerFile": "Dockerfile"}), &at()).unwrap();
    }

    #[test]
    fn a_file_that_builds_nothing_is_a_config_error_not_a_refusal() {
        for config in [
            json!({}),
            json!({"image": ""}),
            json!({"build": {}}),
            json!({"remoteUser": "x"}),
        ] {
            let err = check_raw(&config, &at()).unwrap_err();
            let EnvError::Config { file, reason } = &err else {
                panic!("{err:?}")
            };
            assert_eq!(file, Path::new(".devcontainer/devcontainer.json"));
            assert!(reason.contains("nothing to build"), "{reason}");
        }
    }

    #[test]
    fn compose_is_refused_whatever_else_the_file_says() {
        let why = refused_with(
            &json!({"dockerComposeFile": "docker-compose.yml", "service": "app", "image": "x"}),
        );
        assert!(why.contains("dockerComposeFile"), "{why}");
        assert!(
            why.starts_with(".devcontainer/devcontainer.json: "),
            "{why}"
        );
    }

    #[test]
    fn privileged_is_refused_in_any_form_but_false() {
        for value in [json!(true), json!("true"), json!(1)] {
            let why = refused_with(&json!({"image": "x", "privileged": value}));
            assert!(why.contains("`privileged` is refused"), "{why}");
        }
    }

    #[test]
    fn only_sys_ptrace_may_be_added() {
        check_raw(
            &json!({"image": "x", "capAdd": ["sys_ptrace", "CAP_SYS_PTRACE"]}),
            &at(),
        )
        .unwrap();
        let why = refused_with(
            &json!({"image": "x", "capAdd": ["SYS_PTRACE", "NET_ADMIN", "SYS_ADMIN"]}),
        );
        assert!(
            why.contains("NET_ADMIN")
                && why.contains("SYS_ADMIN")
                && !why.contains("capAdd SYS_PTRACE"),
            "{why}"
        );
    }

    #[test]
    fn only_two_security_options_pass() {
        check_raw(
            &json!({"image": "x", "securityOpt": ["seccomp=unconfined", "label=disable"]}),
            &at(),
        )
        .unwrap();
        for opt in [
            "apparmor=unconfined",
            "seccomp=/etc/evil.json",
            "no-new-privileges=false",
            "systempaths=unconfined",
        ] {
            let why = refused_with(&json!({"image": "x", "securityOpt": [opt]}));
            assert!(why.contains(opt), "{why}");
        }
    }

    #[test]
    fn a_bind_mount_is_refused_in_every_spelling() {
        for mount in [
            json!("type=bind,source=/var/run/docker.sock,target=/var/run/docker.sock"),
            json!("source=/etc,target=/host-etc,type=bind"),
            json!({"type": "bind", "source": "/work", "target": "/w"}),
            json!("TYPE=BIND,src=/etc,dst=/e"),
            // No type is a volume, and a volume named like a path is no volume.
            json!("source=/etc,target=/e"),
            json!("type=volume,source=../../etc,target=/e"),
            json!("type=devpts,target=/dev/pts"),
            json!("type=volume,source=x,target=/e,volume-opt=type=none"),
            json!("type=volume,source=x,target=/e,volume-driver=local"),
        ] {
            let why = refused_with(&json!({"image": "x", "mounts": [mount.clone()]}));
            assert!(why.contains("`mounts`"), "{mount}: {why}");
        }
    }

    #[test]
    fn volumes_and_tmpfs_pass_with_their_own_options_and_a_name_may_use_variables() {
        check_raw(
            &json!({"image": "x", "mounts": [
                "type=volume,source=${localWorkspaceFolderBasename}-node_modules,target=/w/node_modules",
                "type=volume,target=/anonymous",
                "type=tmpfs,target=/t,tmpfs-size=64m",
                "type=volume,source=cache,target=/c,readonly",
            ]}),
            &at(),
        )
        .unwrap();
    }

    #[test]
    fn a_mount_over_what_the_coder_uses_or_with_a_relative_target_is_refused() {
        for target in [
            "/opt/adam/bin",
            "/run/adam/secrets",
            "/opt/adam",
            "relative",
            "/a/../b",
        ] {
            let why = refused_with(
                &json!({"image": "x", "mounts": [format!("type=volume,source=v,target={target}")]}),
            );
            assert!(why.contains(target), "{target}: {why}");
        }
    }

    #[test]
    fn run_args_outside_the_list_are_refused_with_the_argument_named() {
        for arg in [
            "--privileged",
            "--cap-add=ALL",
            "--device",
            "--net=host",
            "--network=host",
            "--pid=host",
            "--ipc=host",
            "--userns=host",
            "-v",
            "--volume=/:/host",
            "--mount",
            "--env-file",
            "--security-opt",
            "--user=root",
            "-p",
            "--publish=80:80",
            "--entrypoint",
            "--rm",
            "--pull=always",
        ] {
            let why = refused_with(&json!({"image": "x", "runArgs": [arg]}));
            assert!(why.contains(arg), "{arg}: {why}");
        }
    }

    #[test]
    fn a_run_arg_that_needs_a_value_and_has_none_is_refused_and_labels_of_the_coder_are_reserved() {
        let why = refused_with(&json!({"image": "x", "runArgs": ["--memory"]}));
        assert!(why.contains("no value"), "{why}");
        let why = refused_with(
            &json!({"image": "x", "runArgs": ["-l", "adam.vymalo.com/run=someone-else"]}),
        );
        assert!(why.contains("the coder's"), "{why}");
        let why = refused_with(&json!({"image": "x", "runArgs": [1]}));
        assert!(why.contains("not a string"), "{why}");
    }

    #[test]
    fn a_build_may_not_leave_the_repository_or_pass_options_to_the_builder() {
        for config in [
            json!({"build": {"dockerfile": "Dockerfile", "context": "../.."}}),
            json!({"build": {"dockerfile": "../../../etc/Dockerfile"}}),
            json!({"build": {"dockerfile": "/etc/Dockerfile"}}),
            json!({"dockerFile": "../../x", "context": ".."}),
        ] {
            let why = refused_with(&config);
            assert!(why.contains("leaves the repository"), "{config}: {why}");
        }
        // The slot's own root is inside.
        check_raw(
            &json!({"build": {"dockerfile": "Dockerfile", "context": ".."}}),
            &at(),
        )
        .unwrap();
        let why = refused_with(
            &json!({"build": {"dockerfile": "Dockerfile", "options": ["--secret", "id=x,src=/etc/passwd"]}}),
        );
        assert!(why.contains("build.options"), "{why}");
        check_raw(
            &json!({"build": {"dockerfile": "Dockerfile", "options": []}}),
            &at(),
        )
        .unwrap();
        let why = refused_with(&json!({"image": "x", "features": {"../../../evil": {}}}));
        assert!(why.contains("feature ../../../evil"), "{why}");
    }

    #[test]
    fn every_problem_is_named_in_one_refusal() {
        let why = refused_with(&json!({
            "image": "x", "privileged": true, "capAdd": ["NET_ADMIN"], "runArgs": ["--net=host"],
            "mounts": ["type=bind,source=/,target=/h"],
        }));
        for part in ["privileged", "NET_ADMIN", "--net=host", "bind"] {
            assert!(why.contains(part), "{part}: {why}");
        }
    }

    #[test]
    fn a_merged_configuration_with_a_privilege_from_a_feature_is_refused() {
        let ours = [Bind::new(
            "/work/environments/.tools/ab",
            TOOLS_TARGET,
            true,
        )];
        let file = Path::new(".devcontainer/devcontainer.json");
        check_merged(
            Some(&json!({"capAdd": ["SYS_PTRACE"], "securityOpt": ["seccomp=unconfined"], "privileged": false,
                "mounts": [{"type": "bind", "source": "/work/environments/.tools/ab", "target": TOOLS_TARGET},
                           "type=volume,source=dind-var-lib-docker,target=/var/lib/docker"]})),
            &ours,
            file,
        )
        .unwrap();
        for merged in [
            json!({"privileged": true}),
            json!({"capAdd": ["SYS_ADMIN"]}),
            json!({"securityOpt": ["apparmor=unconfined"]}),
            json!({"mounts": [{"type": "bind", "source": "/var/run/docker.sock", "target": "/var/run/docker.sock"}]}),
            // Ours is allowed only as it is: the same source and target.
            json!({"mounts": [{"type": "bind", "source": "/work", "target": TOOLS_TARGET}]}),
        ] {
            let err = check_merged(Some(&merged), &ours, file).unwrap_err();
            let EnvError::Refused(why) = &err else {
                panic!("{err:?}")
            };
            assert!(why.contains("added by a feature or the image"), "{why}");
        }
        let err = check_merged(None, &ours, file).unwrap_err();
        assert!(
            matches!(err, EnvError::Build { .. }),
            "no merged configuration cannot be checked: {err:?}"
        );
    }

    /// `podman inspect` of a container the way this crate makes one (the shape is Podman's documented
    /// inspect output; recorded by hand, see the README: *unverified* against a live Podman here).
    fn inspect_ok() -> Value {
        json!({
            "Id": "0123456789abcdef",
            "ImageName": "localhost/vsc-devbox-1a2b-uid:latest",
            "State": {"Running": true, "Status": "running"},
            "HostConfig": {
                "Privileged": false, "CapAdd": [], "SecurityOpt": ["label=disable"], "Devices": [],
                "NetworkMode": "host", "PidMode": "private", "IpcMode": "host", "UsernsMode": "private"
            },
            "Mounts": [
                {"Type": "bind", "Source": "/work/workspaces/r1", "Destination": "/work/workspaces/r1", "RW": true},
                {"Type": "bind", "Source": "/work/git/h/o/r.git", "Destination": "/work/git/h/o/r.git", "RW": false},
                {"Type": "volume", "Name": "anon", "Source": "/home/agent/.local/share/containers/volumes/anon/_data", "Destination": "/data", "RW": true}
            ]
        })
    }

    fn binds() -> Vec<Bind> {
        vec![
            Bind::new("/work/workspaces/r1", "/work/workspaces/r1", false),
            Bind::new("/work/git/h/o/r.git", "/work/git/h/o/r.git", true),
        ]
    }

    fn inspect_refused(inspect: &Value, network: Network) -> String {
        let binds = binds();
        match check_inspect(
            inspect,
            &Expect {
                binds: &binds,
                network,
            },
            Path::new("f.json"),
        ) {
            Err(EnvError::Refused(why)) => why,
            other => panic!("expected a refusal: {other:?}"),
        }
    }

    #[test]
    fn a_container_as_asked_passes_and_says_what_it_is() {
        let binds = binds();
        let got = check_inspect(
            &inspect_ok(),
            &Expect {
                binds: &binds,
                network: Network::Inherit,
            },
            Path::new("f.json"),
        )
        .unwrap();
        assert_eq!(
            got,
            Inspected {
                id: "0123456789abcdef".into(),
                image: "localhost/vsc-devbox-1a2b-uid:latest".into(),
                running: true,
            }
        );
        let mut none = inspect_ok();
        none["HostConfig"]["NetworkMode"] = json!("none");
        check_inspect(
            &none,
            &Expect {
                binds: &binds,
                network: Network::None,
            },
            Path::new("f.json"),
        )
        .unwrap();
    }

    #[test]
    fn the_created_container_is_refused_for_each_thing_the_first_two_checks_could_miss() {
        type Change = Box<dyn Fn(&mut Value)>;
        let cases: [(&str, Change); 10] = [
            (
                "privileged",
                Box::new(|v| v["HostConfig"]["Privileged"] = json!(true)),
            ),
            (
                "privileged",
                Box::new(|v| {
                    v["HostConfig"]
                        .as_object_mut()
                        .unwrap()
                        .remove("Privileged");
                }),
            ),
            (
                "capability CAP_SYS_ADMIN",
                Box::new(|v| v["HostConfig"]["CapAdd"] = json!(["CAP_SYS_ADMIN"])),
            ),
            (
                "security option apparmor=unconfined",
                Box::new(|v| v["HostConfig"]["SecurityOpt"] = json!(["apparmor=unconfined"])),
            ),
            (
                "a device",
                Box::new(|v| v["HostConfig"]["Devices"] = json!([{"PathOnHost": "/dev/fuse"}])),
            ),
            (
                "network mode is `bridge`",
                Box::new(|v| v["HostConfig"]["NetworkMode"] = json!("bridge")),
            ),
            (
                "process namespace is `host`",
                Box::new(|v| v["HostConfig"]["PidMode"] = json!("host")),
            ),
            (
                "IPC namespace is `container:abc`",
                Box::new(|v| v["HostConfig"]["IpcMode"] = json!("container:abc")),
            ),
            (
                "bind mount of /var/run/docker.sock",
                Box::new(|v| {
                    v["Mounts"].as_array_mut().unwrap().push(json!({"Type": "bind", "Source": "/var/run/docker.sock", "Destination": "/var/run/docker.sock", "RW": true}))
                }),
            ),
            (
                "/work/git/h/o/r.git is writable",
                Box::new(|v| v["Mounts"][1]["RW"] = json!(true)),
            ),
        ];
        for (needle, change) in cases {
            let mut inspect = inspect_ok();
            change(&mut inspect);
            let why = inspect_refused(&inspect, Network::Inherit);
            assert!(
                why.contains(needle) && why.ends_with("(the created container)"),
                "{needle}: {why}"
            );
        }
        // The mount of another run's workspace is a bind that is not ours, even at the same target.
        let mut other = inspect_ok();
        other["Mounts"][0]["Source"] = json!("/work/workspaces/r2");
        assert!(inspect_refused(&other, Network::Inherit).contains("/work/workspaces/r2"));
        // The network is what was asked: `host` is refused when `none` was asked for.
        let why = inspect_refused(&inspect_ok(), Network::None);
        assert!(why.contains("not `none`"), "{why}");
    }

    #[test]
    fn a_bind_mount_string_carries_the_flags_and_refuses_a_path_it_cannot_carry() {
        assert_eq!(
            Bind::new("/a/b", "/a/b", true).mount_string().unwrap(),
            "type=bind,source=/a/b,target=/a/b,readonly"
        );
        assert_eq!(
            Bind::new("/a", "/b", false).mount_string().unwrap(),
            "type=bind,source=/a,target=/b"
        );
        assert_eq!(Bind::new("/a,b", "/b", false).mount_string(), None);
    }

    #[test]
    fn paths_are_joined_lexically() {
        let base = Path::new("/w/r/s/.devcontainer");
        assert_eq!(lexical_join(base, ".."), Some(PathBuf::from("/w/r/s")));
        assert_eq!(
            lexical_join(base, "./a/../b"),
            Some(PathBuf::from("/w/r/s/.devcontainer/b"))
        );
        assert_eq!(lexical_join(base, "/etc"), Some(PathBuf::from("/etc")));
        assert_eq!(lexical_join(Path::new("/a"), "../../.."), None);
    }
}
