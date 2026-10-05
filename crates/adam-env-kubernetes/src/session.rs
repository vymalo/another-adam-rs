//! The session of a run whose environment is a pod: how a command is made for it.
//!
//! A command is the program `adam-kube-exec` ([`Invocation`](crate::Invocation)), which the caller
//! spawns in its own process group like any other, with the pod, the working directory, the
//! variables the caller set and `adam-exec`'s words. The client runs `adam-exec run|shell` in the run
//! container (`pods/exec`) and ends with the command's exit code. Killing the client does not stop
//! the command in the pod; [`kill`](adam_workspace::EnvSession::kill) does, by id, through
//! `adam-exec kill`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use adam_workspace::{
    EnvDescription, EnvError, EnvKind, EnvSession, ExecId, ExecSpec, PreparedCommand, Program,
    SecretRef,
};
use async_trait::async_trait;

use crate::environment::Inner;
use crate::exec::ExitStatus;
use crate::invocation::{Invocation, Mode, is_env_name};

/// The name the model key has for [`EnvSession::secret_ref`].
const MODEL_KEY: &str = "model-key";

/// The variable the template sets from the deployment's Secret, and a process reads the model key
/// from.
const MODEL_KEY_VARIABLE: &str = "MODEL_API_KEY";

/// The program the coder brings along (the name [`EnvSession::tool_path`] is asked for).
const OPENCODE: &str = "opencode";

/// A run's pod.
pub(crate) struct KubeSession {
    pub(crate) inner: Arc<Inner>,
    pub(crate) run: String,
    pub(crate) pod: String,
    /// `<root>/workspaces/<run>`: where every cwd is.
    pub(crate) run_dir: PathBuf,
}

impl KubeSession {
    fn exec_id(&self) -> ExecId {
        let n = self.inner.counter.fetch_add(1, Ordering::Relaxed);
        ExecId::new(format!("kp-{}-{n}", self.inner.epoch))
    }
}

#[async_trait]
impl EnvSession for KubeSession {
    fn describe(&self) -> EnvDescription {
        let image = self.inner.template.image().map(str::to_owned);
        EnvDescription {
            summary: match &image {
                Some(image) => format!("the run's own pod {}, image {image}", self.pod),
                None => format!("the run's own pod {}", self.pod),
            },
            kind: EnvKind::Kubernetes {
                pod: self.pod.clone(),
                image,
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
        let (mode, words) = match &spec.program {
            Program::Shell(line) => (Mode::Shell, vec![format!(":{line}")]),
            Program::Argv(argv) => {
                if argv.is_empty() {
                    return Err(EnvError::Refused("there is no program to run".to_owned()));
                }
                let mut words = Vec::with_capacity(argv.len());
                for word in argv {
                    let Some(word) = word.to_str() else {
                        return Err(EnvError::Refused(
                            "a command in a pod takes only UTF-8 arguments".to_owned(),
                        ));
                    };
                    words.push(format!(":{word}"));
                }
                (Mode::Run, words)
            }
        };
        if words.iter().any(|w| w.contains('\0')) {
            return Err(EnvError::Refused(
                "a word of the command has a NUL byte".to_owned(),
            ));
        }

        // What the caller set (minus what it said to hide), then what git inside needs: the tree
        // may belong to another user than the pod's.
        let mut env: BTreeMap<String, String> = spec
            .env
            .iter()
            .filter(|(name, _)| !spec.hide.contains(name))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        env.insert("GIT_CONFIG_COUNT".to_owned(), "1".to_owned());
        env.insert("GIT_CONFIG_KEY_0".to_owned(), "safe.directory".to_owned());
        env.insert("GIT_CONFIG_VALUE_0".to_owned(), "*".to_owned());
        for (name, value) in &env {
            if !is_env_name(name) || value.contains('\0') {
                return Err(EnvError::Refused(format!(
                    "{name:?} cannot be a variable of a command in a pod"
                )));
            }
        }
        // The pod has variables of its own (the model key, from the deployment's Secret): a name
        // the caller hid is removed from the command's environment.
        let unset: Vec<String> = spec
            .hide
            .iter()
            .filter(|name| is_env_name(name))
            .cloned()
            .collect();

        let id = self.exec_id();
        let settings = &self.inner.settings;
        let invocation = Invocation {
            namespace: settings.namespace.clone(),
            pod: self.pod.clone(),
            container: Some(settings.container.clone()),
            adam_exec: settings.adam_exec.clone(),
            env: env.into_iter().collect(),
            unset,
            mode,
            id: id.as_str().to_owned(),
            cwd: cwd.to_owned(),
            words,
        };
        self.inner.touch(&self.run);
        Ok(PreparedCommand {
            program: settings.exec_client.clone(),
            args: invocation
                .to_args()
                .into_iter()
                .map(OsString::from)
                .collect(),
            cwd: spec.cwd.clone(),
            env: settings
                .client_env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
            // The client holds the coder's access to the cluster and nothing else of the coder's.
            env_clear: true,
            env_remove: Vec::new(),
            exec: id,
        })
    }

    async fn kill(&self, exec: &ExecId) {
        self.inner.touch(&self.run);
        let settings = &self.inner.settings;
        let argv = vec![
            settings.adam_exec.clone(),
            "kill".to_owned(),
            exec.as_str().to_owned(),
        ];
        match self
            .inner
            .exec
            .capture(&self.pod, argv, settings.exec_timeout)
            .await
        {
            Ok(done) if matches!(done.status, ExitStatus::Code(0)) => {}
            Ok(done) => tracing::warn!(
                exec = %exec,
                status = ?done.status,
                "could not stop what the command left in the pod"
            ),
            Err(e) => tracing::warn!(
                exec = %exec,
                error = %e,
                "could not stop what the command left in the pod"
            ),
        }
    }

    fn tool_path(&self, name: &str) -> Option<PathBuf> {
        let settings = &self.inner.settings;
        // The coder's own OpenCode, which the template's init container put in the tools directory.
        (name == OPENCODE && settings.opencode)
            .then(|| PathBuf::from(format!("{}/{OPENCODE}", settings.tools_dir)))
    }

    fn secret_ref(&self, name: &str) -> Option<SecretRef> {
        (name == MODEL_KEY && self.inner.settings.model_key)
            .then(|| SecretRef::Env(MODEL_KEY_VARIABLE.to_owned()))
    }
}
