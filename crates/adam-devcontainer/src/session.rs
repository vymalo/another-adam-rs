//! The session of a run whose environment is a devcontainer: how a command is made for it.
//!
//! A command is `devcontainer exec` with the same arguments as `up` had (it finds the container by
//! the run's id labels), the variables the caller set as `--remote-env`, and `adam-exec` as the
//! command, which enters the working directory, records the process so that it can be stopped,
//! and becomes the command. The coder spawns that in a process group of its own; the process in the
//! container is stopped through [`kill`](adam_workspace::EnvSession::kill).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use adam_workspace::{
    EnvDescription, EnvError, EnvKind, EnvSession, ExecId, ExecSpec, PreparedCommand, Program,
    SecretRef,
};
use async_trait::async_trait;

use crate::environment::Inner;
use crate::podman::{DEPLOYMENT_LABEL, RUN_LABEL};
use crate::policy::{SECRETS_TARGET, TOOLS_TARGET};

/// The name the model key has for [`EnvSession::secret_ref`].
const MODEL_KEY: &str = "model-key";

/// Where the CLI keeps its per-container cache inside the container.
pub(crate) const CONTAINER_SESSION_DATA: &str = "/tmp/adam-devcontainer";

/// The flags every call of the CLI about a run's container has: where the configuration is, how to
/// find the container, which Podman to talk to.
pub(crate) fn common_args(
    inner: &Inner,
    run: &str,
    config_slot: &Path,
    config_file: Option<&Path>,
    override_file: &Path,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--docker-path".into(),
        inner.settings.podman.clone().into_os_string(),
        "--workspace-folder".into(),
        config_slot.as_os_str().to_owned(),
        "--override-config".into(),
        override_file.as_os_str().to_owned(),
    ];
    if let Some(file) = config_file {
        args.push("--config".into());
        args.push(file.as_os_str().to_owned());
    }
    args.extend([
        "--id-label".into(),
        format!("{RUN_LABEL}={run}").into(),
        "--id-label".into(),
        format!("{DEPLOYMENT_LABEL}={}", inner.settings.deployment).into(),
        "--mount-workspace-git-root=false".into(),
    ]);
    args
}

/// A run's devcontainer.
pub(crate) struct DevSession {
    pub(crate) inner: Arc<Inner>,
    pub(crate) run: String,
    /// `<root>/workspaces/<run>`: where every cwd is.
    pub(crate) run_dir: PathBuf,
    /// The first slot.
    pub(crate) config_slot: PathBuf,
    /// The repository's `devcontainer.json`, as the CLI's `--config`.
    pub(crate) config_file: Option<PathBuf>,
    pub(crate) override_file: PathBuf,
    pub(crate) container_id: String,
    pub(crate) image: String,
    /// A line for a person: whose environment this is.
    pub(crate) label: String,
    pub(crate) has_model_key: bool,
}

impl DevSession {
    fn exec_id(&self) -> ExecId {
        let n = self.inner.counter.fetch_add(1, Ordering::Relaxed);
        ExecId::new(format!("dc-{}-{n}", self.inner.epoch))
    }
}

/// A variable name the CLI and a shell can both take.
fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[async_trait]
impl EnvSession for DevSession {
    fn describe(&self) -> EnvDescription {
        let source = self.config_file.clone();
        EnvDescription {
            summary: match &source {
                Some(file) => format!(
                    "the devcontainer of {} ({}), image {}",
                    self.label,
                    file.file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                    self.image
                ),
                None => format!("the default devcontainer, image {}", self.image),
            },
            kind: EnvKind::DevContainer {
                source,
                image: self.image.clone(),
            },
        }
    }

    fn prepare(&self, spec: &ExecSpec) -> Result<PreparedCommand, EnvError> {
        if !spec.cwd.is_absolute()
            || spec
                .cwd
                .components()
                .any(|c| matches!(c, Component::ParentDir))
            || !spec.cwd.starts_with(&self.run_dir)
        {
            return Err(EnvError::Refused(format!(
                "{} is not inside this run's workspace",
                spec.cwd.display()
            )));
        }
        let Some(cwd) = spec.cwd.to_str().filter(|c| !c.contains('\n')) else {
            return Err(EnvError::Refused(
                "the working directory is not plain text".to_owned(),
            ));
        };

        // What the caller set (minus what it said to hide), then what git inside needs: the tree is
        // owned by the coder's user, whoever the remote user is.
        let mut remote: BTreeMap<String, String> = spec
            .env
            .iter()
            .filter(|(name, _)| !spec.hide.contains(name))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        remote.insert("GIT_CONFIG_COUNT".to_owned(), "1".to_owned());
        remote.insert("GIT_CONFIG_KEY_0".to_owned(), "safe.directory".to_owned());
        remote.insert("GIT_CONFIG_VALUE_0".to_owned(), "*".to_owned());

        let id = self.exec_id();
        let mut args = vec![OsString::from("exec")];
        args.extend(common_args(
            &self.inner,
            &self.run,
            &self.config_slot,
            self.config_file.as_deref(),
            &self.override_file,
        ));
        // The exec client must write plain text: with a JSON log it takes a terminal and swallows the
        // output (*verified* 2026-10-01).
        args.extend(["--log-format".into(), "text".into()]);
        for (name, value) in &remote {
            if !is_env_name(name) || value.contains('\0') {
                return Err(EnvError::Refused(format!(
                    "{name:?} cannot be a remote variable"
                )));
            }
            args.push("--remote-env".into());
            args.push(format!("{name}={value}").into());
        }
        args.push(format!("{TOOLS_TARGET}/adam-exec").into());
        // Every word after the working directory has a ":" in front of it, which adam-exec removes:
        // the CLI would take a word such as "--version" for one of its own options.
        let marked = |word: &std::ffi::OsStr| {
            let mut marked = OsString::from(":");
            marked.push(word);
            marked
        };
        match &spec.program {
            Program::Shell(line) => {
                args.extend([
                    "shell".into(),
                    id.as_str().into(),
                    cwd.into(),
                    marked(line.as_ref()),
                ]);
            }
            Program::Argv(argv) => {
                if argv.is_empty() {
                    return Err(EnvError::Refused("there is no program to run".to_owned()));
                }
                args.extend(["run".into(), id.as_str().into(), cwd.into()]);
                args.extend(argv.iter().map(|word| marked(word)));
            }
        }
        Ok(PreparedCommand {
            program: self.inner.settings.cli.clone(),
            args,
            cwd: spec.cwd.clone(),
            env: self
                .inner
                .env
                .vars()
                .into_iter()
                .map(|(name, value)| (OsString::from(name), value))
                .collect(),
            env_clear: true,
            env_remove: Vec::new(),
            exec: id,
        })
    }

    async fn kill(&self, exec: &ExecId) {
        if let Err(e) = self
            .inner
            .podman
            .exec_kill(&self.container_id, exec.as_str())
            .await
        {
            tracing::warn!(error = %e, exec = %exec, "could not stop what the command left in the container");
        }
    }

    fn secret_ref(&self, name: &str) -> Option<SecretRef> {
        (name == MODEL_KEY && self.has_model_key)
            .then(|| SecretRef::File(PathBuf::from(format!("{SECRETS_TARGET}/{MODEL_KEY}"))))
    }
}
