//! Running the `git` CLI: environment hygiene, per-invocation credentials,
//! error classification and secret scrubbing.
//!
//! # Credentials
//!
//! A token is handed to `git` only through the environment of that one child
//! process:
//!
//! ```text
//! GIT_CONFIG_COUNT=1
//! GIT_CONFIG_KEY_0=http.<scheme>://<host>/.extraHeader
//! GIT_CONFIG_VALUE_0=Authorization: Basic base64("x-access-token:<token>")
//! ```
//!
//! For http(s) remotes the header is scoped to the remote's origin, so it is
//! never sent to a different host (redirects). For local remotes it is
//! unscoped and simply ignored. The token is never in a URL, in
//! `.git/config`, in the arguments (which `ps` shows), or in logs; everything
//! `git` writes to stderr is scrubbed of the token and of its base64 form
//! before it can become an error message.
//!
//! The one place the token is visible is the child's environment
//! (`/proc/<pid>/environ`, readable by the same uid only) for the duration of
//! the command.
//!
//! # Where a command runs
//!
//! `git` runs what a repository's configuration names (a `filter.*.clean`, a `textconv`), and a
//! repository can be one a run wrote. So the commands that **carry no credential** run in the git
//! sandbox ([`GitSandbox`]) when the deployment has one: another container, with no secret, that
//! executes only `git`. The commands that talk to a remote (`fetch`, `push`, `ls-remote`, a
//! first `init` of a mirror) and everything that carries a token always run in this process's
//! container, on the bare mirror, after its configuration has been put back to what the workspace
//! writes (`mirror_config`). The choice is made in [`GitCmd::route`] and nowhere
//! else; a command with a credential is never sent to the sandbox, and the sandbox drops the
//! variables a credential travels in even if one were.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use adam_git_sandbox::{Client, ClientError, Request, environment};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use secrecy::{ExposeSecret, SecretString};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::safe_fs::{self, SlotDir};

/// Commands that talk to a remote may take long (first clone of a big repo).
const NETWORK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Purely local commands should be quick.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Longest stderr excerpt kept in an error.
const MAX_STDERR: usize = 4000;

/// Environment variables that could redirect git to another repository or make
/// it log request headers (and thereby the token).
const SCRUBBED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_TRACE",
    "GIT_TRACE2",
    "GIT_TRACE2_EVENT",
    "GIT_TRACE2_PERF",
    "GIT_TRACE_CURL",
    "GIT_TRACE_CURL_NO_DATA",
    "GIT_TRACE_PACKET",
    "GIT_TRACE_PACK_ACCESS",
    "GIT_TRACE_PERFORMANCE",
    "GIT_TRACE_SETUP",
    "GIT_TRACE_SHALLOW",
    "GIT_CURL_VERBOSE",
];

/// A token plus the URL scope its header is limited to.
pub(crate) struct Auth {
    token: SecretString,
    /// `scheme://host[:port]/` for http(s) remotes.
    scope: Option<String>,
}

impl Auth {
    pub(crate) fn new(token: SecretString, scope: Option<&str>) -> Self {
        Self {
            token,
            scope: scope.map(str::to_owned),
        }
    }

    fn basic(&self) -> String {
        STANDARD.encode(format!("x-access-token:{}", self.token.expose_secret()))
    }

    fn config_key(&self) -> String {
        match &self.scope {
            Some(scope) => format!("http.{scope}.extraHeader"),
            None => "http.extraHeader".to_owned(),
        }
    }

    fn config_value(&self) -> String {
        format!("Authorization: Basic {}", self.basic())
    }

    /// Remove the token, its base64 form (padded or not) and the header from
    /// `text`.
    pub(crate) fn scrub(&self, text: &str) -> String {
        let secret = self.token.expose_secret();
        if secret.is_empty() {
            return text.to_owned();
        }
        let b64 = self.basic();
        let b64_bare = b64.trim_end_matches('=');
        text.replace(&b64, "[REDACTED]")
            .replace(b64_bare, "[REDACTED]")
            .replace(secret, "[REDACTED]")
    }
}

/// Captured result of a finished command.
pub(crate) struct GitOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) success: bool,
    pub(crate) code: Option<i32>,
}

impl GitOutput {
    pub(crate) fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }
}

/// The directories a `git` that works on a slot is **pinned** to: `GIT_DIR`, `GIT_COMMON_DIR` and
/// `GIT_WORK_TREE`, all from the coder's own metadata and never from a file the run's commands can
/// write.
///
/// Without the pin git finds its way from the slot's `.git` file, then the worktree's directory in
/// the mirror (`commondir`, `gitdir`) and reads the configuration of wherever that leads. A command
/// that rewrites any of them points the coder's own `git add -A` at a repository whose
/// configuration it wrote (`filter.*.clean`, `core.fsmonitor`), which runs in the coder's container.
/// With the pin, none of those files steers anything.
///
/// A worktree's pin is `GIT_DIR=<mirror>/worktrees/<name>`, `GIT_COMMON_DIR=<mirror>`; a scratch
/// project's is its own `.git` directory for both (it must be a real directory, not a file or a
/// link that would send git elsewhere).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPin {
    git_dir: PathBuf,
    common_dir: PathBuf,
    work_tree: PathBuf,
    /// The git directory is inside the work tree, so it has to be checked to be a directory there.
    inside: bool,
    /// The work tree's directory as the coder opened it: `git` can only be given its name, and this
    /// says whether the name still leads to it ([`SlotDir::is_still_here`]).
    root: Arc<SlotDir>,
    /// The git sandbox of the deployment, if it has one: where the questions asked of this
    /// repository are run ([`LocalGit`]).
    sandbox: Option<GitSandbox>,
}

impl GitPin {
    /// The pin of a linked worktree: `admin` is its directory in `mirror`'s `worktrees/`.
    pub(crate) fn worktree(
        admin: &Path,
        mirror: &Path,
        work_tree: &Path,
        root: Arc<SlotDir>,
    ) -> Self {
        Self {
            git_dir: admin.to_owned(),
            common_dir: mirror.to_owned(),
            work_tree: work_tree.to_owned(),
            inside: false,
            root,
            sandbox: None,
        }
    }

    /// The pin of a repository that lives in its own work tree (a scratch project).
    pub(crate) fn own_repository(work_tree: &Path, root: Arc<SlotDir>) -> Self {
        let git_dir = work_tree.join(".git");
        Self {
            common_dir: git_dir.clone(),
            git_dir,
            work_tree: work_tree.to_owned(),
            inside: true,
            root,
            sandbox: None,
        }
    }

    /// This pin, for a deployment that has a git sandbox.
    pub(crate) fn with_sandbox(mut self, sandbox: Option<GitSandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// The git sandbox the questions asked of this repository are run in (see [`LocalGit`]), `None`
    /// when git runs in the coder's own container.
    pub fn sandbox(&self) -> Option<&GitSandbox> {
        self.sandbox.as_ref()
    }

    /// The three variables as pairs.
    pub(crate) fn pairs(&self) -> [(OsString, OsString); 3] {
        [
            ("GIT_DIR".into(), self.git_dir.clone().into()),
            ("GIT_COMMON_DIR".into(), self.common_dir.clone().into()),
            ("GIT_WORK_TREE".into(), self.work_tree.clone().into()),
        ]
    }

    /// `GIT_DIR`: the worktree's directory in the mirror, or the scratch project's `.git`.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// `GIT_COMMON_DIR`: the mirror, or the scratch project's `.git`.
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// `GIT_WORK_TREE`: the slot's directory.
    pub fn work_tree(&self) -> &Path {
        &self.work_tree
    }

    /// Set the three variables on `cmd`, which a caller that starts `git` itself does after
    /// [`confine_git_env`](crate::confine_git_env) (an empty environment) and before it sets
    /// anything of its own. Run it in [`work_tree`](Self::work_tree).
    pub fn apply(&self, cmd: &mut Command) {
        cmd.env("GIT_DIR", &self.git_dir)
            .env("GIT_COMMON_DIR", &self.common_dir)
            .env("GIT_WORK_TREE", &self.work_tree);
    }

    /// Make the pin hold, and say what is wrong when it cannot. Call it before starting `git` with
    /// [`apply`](Self::apply); the `git` the workspace starts itself is checked as it starts.
    ///
    /// The environment variables are not all there is to it: git's **reference store** reads the
    /// `commondir` file of the git directory even when `GIT_COMMON_DIR` is set (*verified* with git
    /// 2.43.0), so a file that says anything but what git wrote (`../..`) moves the refs the coder's
    /// git reads and writes (the run's branch, the commit it pushes) into a directory of the run's
    /// choosing. A run pod cannot write it (`commondir` and `gitdir` are read-only mounts there, see
    /// `adam-env-kubernetes`), so this is **only a check**: it refuses, it never rewrites. A rewrite
    /// would open the file a run can replace by a link, and write through the link to any file the
    /// coder can write. Everything it looks at is looked at without following a link
    /// ([`safe_fs`](crate::safe_fs)): the file must be a regular file, so a pipe cannot make the
    /// coder wait.
    ///
    /// Also refused: a work tree or a git directory that is not a plain directory (a link a command
    /// put where the slot was, to send the coder's `git add -A` into another run's files), a
    /// `HEAD`, `index` or reflog (`logs/HEAD`, which git appends to through a link: *verified* with
    /// git 2.43.0) that is not a regular file, and, for a scratch project, any `commondir` at all.
    /// A command that swaps one of those files again between this check and git's read is a race
    /// this does not close; in a run pod the files that matter cannot be swapped (see the ADR).
    ///
    /// The slot's directory is also looked at against the descriptor the coder opened it with
    /// ([`still_here`](Self::still_here)), first and last: a slot that was moved away and replaced
    /// by another directory, or by a link, is refused. That is a **tripwire**, not a lock: `git`
    /// finds its work tree by name inside its own process, whatever descriptor the coder holds, so
    /// a swap in the instant between the last look and `git`'s first read of the name goes
    /// unseen. What closes that window is a slot that cannot be renamed in the run's pod (a mount
    /// point of its own), which is not built yet.
    ///
    /// # Errors
    ///
    /// What is wrong, in a sentence.
    pub async fn check(&self) -> Result<(), String> {
        self.still_here().await?;
        self.check_files().await?;
        self.still_here().await
    }

    /// Whether the name of the work tree still leads to the directory the coder opened as the slot
    /// ([`SlotDir::is_still_here`]): not moved, not replaced by another directory or a link.
    ///
    /// # Errors
    ///
    /// What is wrong, in a sentence.
    pub async fn still_here(&self) -> Result<(), String> {
        let root = Arc::clone(&self.root);
        match tokio::task::spawn_blocking(move || root.is_still_here()).await {
            Ok(true) => Ok(()),
            _ => Err(format!(
                "{} is not the directory the coder opened any more (a command moved it away, or replaced it by another directory or a link): git is not run on it",
                self.work_tree.display()
            )),
        }
    }

    async fn check_files(&self) -> Result<(), String> {
        if !safe_fs::is_real_directory(&self.work_tree).await {
            return Err(format!(
                "{} is not a plain directory (it is gone, or a command replaced it by a link or a file): git is not run on it",
                self.work_tree.display()
            ));
        }
        if !safe_fs::is_real_directory(&self.git_dir).await {
            return Err(format!(
                "{} is not a plain directory: git is not run on a project whose repository was replaced by a file or a link",
                self.git_dir.display()
            ));
        }
        let commondir = self.git_dir.join("commondir");
        if self.inside {
            return match tokio::fs::symlink_metadata(&commondir).await {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("cannot look at {}: {e}", commondir.display())),
                Ok(_) => Err(format!(
                    "{} exists: a project's own repository has none, and git would follow it to another repository",
                    commondir.display()
                )),
            };
        }
        match safe_fs::read_regular_text(&commondir, 64).await {
            Ok(text) if text.trim() == COMMONDIR => {}
            Ok(_) => {
                return Err(format!(
                    "{} does not say `{COMMONDIR}` any more, so git would find the references of another repository: git is not run on this worktree",
                    commondir.display()
                ));
            }
            Err(e) => {
                return Err(format!(
                    "{} cannot be trusted ({e}): git is not run on this worktree",
                    commondir.display()
                ));
            }
        }
        // (name, must be there, a directory and not a file)
        for (name, required, directory) in [
            ("HEAD", true, false),
            ("index", false, false),
            ("logs", false, true),
            ("logs/HEAD", false, false),
        ] {
            let path = self.git_dir.join(name);
            match tokio::fs::symlink_metadata(&path).await {
                Ok(meta) if meta.is_dir() == directory && (meta.is_dir() || meta.is_file()) => {}
                Ok(_) => {
                    return Err(format!(
                        "{} is not a plain {} (a link, a pipe or the other kind): git is not run on this worktree",
                        path.display(),
                        if directory { "directory" } else { "file" }
                    ));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => {}
                Err(e) => return Err(format!("cannot look at {}: {e}", path.display())),
            }
        }
        Ok(())
    }
}

/// What git writes in the `commondir` file of a linked worktree's directory
/// (`<mirror>/worktrees/<name>`): the way up to the mirror.
const COMMONDIR: &str = "../..";

/// The git sandbox of a deployment: where the commands that carry no credential run (the crate
/// README's "Where `git` runs: the git sandbox" says how). It is a handle on the socket of an `adam-git-sandbox` server
/// (`GIT_SANDBOX_SOCKET`), whose container mounts the same work volume at the same path.
///
/// Made by [`Workspaces::with_git_sandbox`](crate::Workspaces::with_git_sandbox); cheap to clone. A
/// sandbox that cannot be reached makes the commands that need it fail: **they are not run in this
/// container instead**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSandbox {
    client: Client,
    private_dir: PathBuf,
}

impl GitSandbox {
    /// The sandbox whose server listens on `socket`, for the work volume at `root`.
    pub fn new(socket: impl Into<PathBuf>, root: &Path) -> Self {
        Self {
            client: Client::new(socket),
            private_dir: root.join("staging").join("private"),
        }
    }

    /// The server's socket.
    pub fn socket(&self) -> &Path {
        self.client.socket()
    }

    /// A directory of the work volume that no run's pod mounts and the coder can write: where the
    /// copy of an index a command works on is made, because the sandbox does not see this container's
    /// own temporary directory but does see the volume.
    pub fn private_dir(&self) -> &Path {
        &self.private_dir
    }
}

/// One `git` command as data: what is run, where, with what environment and input. The same value
/// is run in this container ([`run_here`](Self::run_here)) or in the sandbox
/// ([`run_in`](Self::run_in)).
#[derive(Debug, Clone)]
pub(crate) struct Invocation {
    /// The arguments after `git`, global options (`-c ...`) first.
    pub(crate) args: Vec<OsString>,
    /// The whole environment: nothing else is inherited.
    pub(crate) env: BTreeMap<OsString, OsString>,
    /// The working directory.
    pub(crate) cwd: Option<PathBuf>,
    /// The work volume's root: where a command with no directory of its own runs in the sandbox,
    /// which runs git in the volume and nowhere else (this container's own directory is not a place
    /// it knows).
    pub(crate) root: Option<PathBuf>,
    /// The standard input; empty for none.
    pub(crate) stdin: Vec<u8>,
}

/// What a finished command left.
#[derive(Debug)]
pub(crate) struct Ran {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) code: Option<i32>,
    pub(crate) success: bool,
}

/// Why a command could not be run in the sandbox.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SandboxFailure {
    /// The command carries a credential. It is not sent, whatever asked for it.
    #[error("the command carries a credential and is never sent to the git sandbox")]
    Credential,
    /// The command names no directory and the workspace has no root to run it in.
    #[error("the command has no directory to run in")]
    NoDirectory,
    /// The sandbox could not run it.
    #[error(transparent)]
    Client(ClientError),
}

impl Invocation {
    /// Whether this command could carry a credential: a variable git reads configuration from (the
    /// way the token is handed to a network command), or a `-c` that sets a request header or a
    /// credential helper. Such a command runs in this container or not at all.
    pub(crate) fn carries_credentials(&self) -> bool {
        let in_env = self.env.keys().any(|name| {
            let name = name.to_string_lossy();
            name == "GIT_CONFIG_COUNT"
                || name == "GIT_CONFIG_PARAMETERS"
                || name.starts_with("GIT_CONFIG_KEY_")
                || name.starts_with("GIT_CONFIG_VALUE_")
        });
        let in_args = self.args.windows(2).any(|pair| {
            if pair[0] != "-c" {
                return false;
            }
            let setting = pair[1].to_string_lossy().to_ascii_lowercase();
            let (key, value) = setting.split_once('=').unwrap_or((&setting, ""));
            key.starts_with("http.") || (key.starts_with("credential.") && !value.is_empty())
        });
        in_env || in_args
    }

    /// Run `git` in this container: a child process of this one, from the environment in `env`
    /// and no other, killed if the future is dropped.
    pub(crate) async fn run_here(&self) -> io::Result<Ran> {
        let mut cmd = Command::new("git");
        cmd.args(&self.args).env_clear().envs(&self.env);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(if self.stdin.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        // Written from a task of its own: git may answer before it has read everything, and a
        // patch it stops reading (a parse error) breaks the pipe; its verdict is what counts.
        let feeder = child.stdin.take().map(|mut stdin| {
            let bytes = self.stdin.clone();
            tokio::spawn(async move {
                let _ = stdin.write_all(&bytes).await;
            })
        });
        let output = child.wait_with_output().await;
        if let Some(feeder) = feeder {
            let _ = feeder.await;
        }
        let output = output?;
        Ok(Ran {
            stdout: output.stdout,
            stderr: output.stderr,
            code: output.status.code(),
            success: output.status.success(),
        })
    }

    /// Run `git` in the sandbox. Only the variables the sandbox allows are sent
    /// ([`adam_git_sandbox::environment`]); a command that [carries a credential] is refused here,
    /// before anything is sent.
    ///
    /// [carries a credential]: Self::carries_credentials
    pub(crate) async fn run_in(&self, sandbox: &GitSandbox) -> Result<Ran, SandboxFailure> {
        if self.carries_credentials() {
            return Err(SandboxFailure::Credential);
        }
        let cwd = self
            .cwd
            .clone()
            .or_else(|| self.root.clone())
            .ok_or(SandboxFailure::NoDirectory)?;
        let env = environment::filter(self.env.clone())
            .into_iter()
            .filter(|(name, _)| name != "PATH")
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value)));
        let request = Request::new(self.args.clone(), cwd, env);
        let out = sandbox
            .client
            .output(request, &self.stdin)
            .await
            .map_err(SandboxFailure::Client)?;
        Ok(Ran {
            success: out.exit.success(),
            code: out.exit.code,
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

/// The subcommands that talk to a remote: they run in this container, with or without a token,
/// because what they read is the other end's, and because they are where a token goes.
const NETWORK_SUBCOMMANDS: &[&str] = &[
    "fetch",
    "push",
    "pull",
    "ls-remote",
    "clone",
    "remote",
    "fetch-pack",
    "send-pack",
    "submodule",
];

/// Builder for one `git` invocation.
pub(crate) struct GitCmd {
    args: Vec<OsString>,
    config: Vec<(String, String)>,
    cwd: Option<PathBuf>,
    ceiling: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
    auth: Option<Auth>,
    pin: Option<GitPin>,
    refusal: Option<String>,
    sandbox: Option<GitSandbox>,
    in_coder: bool,
}

impl GitCmd {
    pub(crate) fn new() -> Self {
        Self {
            args: Vec::new(),
            config: Vec::new(),
            cwd: None,
            ceiling: None,
            env: Vec::new(),
            auth: None,
            pin: None,
            refusal: None,
            sandbox: None,
            in_coder: false,
        }
    }

    /// Run in this sandbox, if the command is one that may ([`route`](Self::route)).
    pub(crate) fn sandbox(mut self, sandbox: Option<GitSandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Run in this container whatever the sandbox is: a command on a mirror that has no
    /// repository code in it yet (its `init`), or one that must be the coder's own.
    pub(crate) fn in_coder(mut self) -> Self {
        self.in_coder = true;
        self
    }

    /// Work on this slot's repository, wherever its files say it is not (see [`GitPin`]).
    pub(crate) fn pinned(mut self, pin: GitPin) -> Self {
        self.pin = Some(pin);
        self
    }

    /// Do not run: [`run`](Self::run) and [`run_status`](Self::run_status) fail with
    /// [`WorkspaceError::Corrupt`] and `reason`, and no process is started. For a slot whose
    /// repository cannot be told from the coder's own metadata: git must not find it in a file the
    /// run's commands can write.
    pub(crate) fn refusing(mut self, reason: impl Into<String>) -> Self {
        self.refusal = Some(reason.into());
        self
    }

    /// Operate on this bare repository explicitly (never discover one).
    pub(crate) fn git_dir(mut self, dir: &Path) -> Self {
        let mut arg = OsString::from("--git-dir=");
        arg.push(dir);
        self.args.insert(0, arg);
        self
    }

    pub(crate) fn cwd(mut self, dir: &Path) -> Self {
        self.cwd = Some(dir.to_owned());
        self
    }

    /// Repository discovery must not climb into or above this directory.
    pub(crate) fn ceiling(mut self, dir: &Path) -> Self {
        self.ceiling = Some(dir.to_owned());
        self
    }

    /// A `-c key=value` passed before the subcommand.
    pub(crate) fn config(mut self, key: &str, value: &str) -> Self {
        self.config.push((key.to_owned(), value.to_owned()));
        self
    }

    pub(crate) fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    pub(crate) fn maybe_auth(mut self, auth: Option<Auth>) -> Self {
        self.auth = auth;
        self
    }

    pub(crate) fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub(crate) fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    /// The subcommand name, for messages.
    fn subcommand(&self) -> String {
        self.args
            .iter()
            .map(|a| a.to_string_lossy())
            .find(|a| !a.starts_with('-'))
            .map_or_else(|| "git".to_owned(), |a| a.into_owned())
    }

    fn scrub(&self, text: &str) -> String {
        match &self.auth {
            Some(auth) => auth.scrub(text),
            None => text.to_owned(),
        }
    }

    /// **Where this command runs**: the sandbox, or `None` for this container.
    ///
    /// The sandbox, when the deployment has one, for every command that is not network: it carries no
    /// token, is not a [network subcommand](NETWORK_SUBCOMMANDS) (a `fetch` from a filesystem remote
    /// has no token and is still the coder's), and was not asked to stay ([`in_coder`](Self::in_coder)).
    pub(crate) fn route(&self) -> Option<&GitSandbox> {
        let sandbox = self.sandbox.as_ref()?;
        let network = NETWORK_SUBCOMMANDS.contains(&self.subcommand().as_str());
        (!self.in_coder && self.auth.is_none() && !network).then_some(sandbox)
    }

    /// The command as data: hardening options, the empty environment plus what git needs, the token's
    /// header if there is one, the pin, and the caller's own variables.
    pub(crate) fn invocation(&self) -> Invocation {
        // Never run repository hooks, credential helpers or background gc from
        // an orchestrator-driven command; these -c flags are protected config.
        let mut args: Vec<OsString> = [
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "credential.helper=",
            // A configured fsmonitor is a program git runs on every status.
            "-c",
            "core.fsmonitor=false",
            "-c",
            "commit.gpgsign=false",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        for (k, v) in &self.config {
            args.push("-c".into());
            args.push(format!("{k}={v}").into());
        }
        args.extend(self.args.iter().cloned());

        // An empty environment plus what git needs: the repository's own configuration can run a
        // program (a `filter.*.clean` written by a check script), and it must not find the coder's
        // secrets in the environment it runs in.
        let mut env: BTreeMap<OsString, OsString> =
            crate::git_env::inherited_pairs().into_iter().collect();
        for var in SCRUBBED_ENV {
            env.remove(OsStr::new(var));
        }
        for (name, value) in [
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_ASKPASS", "false"),
            ("SSH_ASKPASS", "false"),
            ("LC_ALL", "C"),
        ] {
            env.insert(name.into(), value.into());
        }
        // No global configuration, unless the operator of this process named one in its own
        // environment (`GIT_CONFIG_GLOBAL`): `$HOME/.gitconfig` is a file that code running in a
        // worktree can write, and a `url.*.insteadOf` in it would redirect what carries the token.
        env.insert(
            "GIT_CONFIG_GLOBAL".into(),
            std::env::var_os("GIT_CONFIG_GLOBAL").unwrap_or_else(|| "/dev/null".into()),
        );
        if let Some(ceiling) = &self.ceiling {
            env.insert("GIT_CEILING_DIRECTORIES".into(), ceiling.clone().into());
        }
        if let Some(auth) = &self.auth {
            env.insert("GIT_CONFIG_COUNT".into(), "1".into());
            env.insert("GIT_CONFIG_KEY_0".into(), auth.config_key().into());
            env.insert("GIT_CONFIG_VALUE_0".into(), auth.config_value().into());
        }
        if let Some(pin) = &self.pin {
            env.extend(pin.pairs());
        }
        env.extend(self.env.iter().cloned());
        Invocation {
            args,
            env,
            cwd: self.cwd.clone(),
            root: self.ceiling.clone(),
            stdin: Vec::new(),
        }
    }

    /// Run to completion; a non-zero exit is an error.
    #[tracing::instrument(level = "debug", skip(self), fields(git = %self.subcommand()))]
    pub(crate) async fn run(self) -> WorkspaceResult<GitOutput> {
        let (out, stderr) = self.exec().await?;
        if out.success {
            Ok(out)
        } else {
            Err(classify(&self.subcommand(), out.code, &self.scrub(&stderr)))
        }
    }

    /// Run to completion and report the exit status instead of failing on it,
    /// for probes such as `git diff --quiet`. Spawn failures and timeouts are
    /// still errors.
    #[tracing::instrument(level = "debug", skip(self), fields(git = %self.subcommand()))]
    pub(crate) async fn run_status(self) -> WorkspaceResult<GitOutput> {
        Ok(self.exec().await?.0)
    }

    async fn exec(&self) -> WorkspaceResult<(GitOutput, String)> {
        if let Some(reason) = &self.refusal {
            return Err(WorkspaceError::Corrupt(reason.clone()));
        }
        if let Some(pin) = &self.pin {
            pin.check().await.map_err(WorkspaceError::Corrupt)?;
            safe_fs::race::checkpoint(pin.work_tree(), "git");
            // Once more, as near to the start as the coder can look.
            pin.still_here().await.map_err(WorkspaceError::Corrupt)?;
        }
        let timeout = if self.auth.is_some() {
            NETWORK_TIMEOUT
        } else {
            LOCAL_TIMEOUT
        };
        let sub = self.subcommand();
        let invocation = self.invocation();
        let sandbox = self.route();
        let run = async {
            match sandbox {
                Some(sandbox) => invocation
                    .run_in(sandbox)
                    .await
                    .map_err(|e| sandbox_failed(&sub, e)),
                None => invocation
                    .run_here()
                    .await
                    .map_err(|e| WorkspaceError::io(format!("cannot run git {sub}"), e)),
            }
        };
        let ran = match tokio::time::timeout(timeout, run).await {
            Err(_) => {
                return Err(WorkspaceError::transient(format!(
                    "git {sub} timed out after {}s",
                    timeout.as_secs()
                )));
            }
            Ok(ran) => ran?,
        };
        let stderr = String::from_utf8_lossy(&ran.stderr).into_owned();
        Ok((
            GitOutput {
                stdout: ran.stdout,
                success: ran.success,
                code: ran.code,
            },
            stderr,
        ))
    }
}

/// The error of a command that could not be run in the sandbox. It was **not** run anywhere else.
fn sandbox_failed(sub: &str, failure: SandboxFailure) -> WorkspaceError {
    use adam_git_sandbox::RefusalReason;
    match failure {
        SandboxFailure::Credential => WorkspaceError::Corrupt(format!(
            "git {sub} carries a credential and was not sent to the git sandbox"
        )),
        SandboxFailure::NoDirectory => WorkspaceError::Corrupt(format!(
            "git {sub} has no directory to run in the git sandbox"
        )),
        SandboxFailure::Client(ClientError::Refused(refusal)) => match refusal.reason {
            RefusalReason::ShuttingDown => {
                WorkspaceError::transient(format!("the git sandbox is shutting down (git {sub})"))
            }
            RefusalReason::BadDirectory => WorkspaceError::Corrupt(format!(
                "the git sandbox does not run git {sub} there: {}",
                refusal.message
            )),
            _ => WorkspaceError::io(
                format!("the git sandbox refused git {sub}"),
                io::Error::other(refusal.message),
            ),
        },
        SandboxFailure::Client(e) => {
            WorkspaceError::transient(format!("the git sandbox cannot run git {sub}"))
                .with_source(e)
        }
    }
}

/// A `git` command that carries no credential, for the questions a caller asks of a repository
/// (what the coder's tools do: `status`, `diff`, `apply`). It runs where the workspace's own git would run a
/// command of its kind: in the git sandbox when there is one, else a child of this process, from
/// the confined environment ([`confine_git_env`](crate::confine_git_env)) and with hooks and the
/// file system monitor off. A sandbox that cannot be reached is an error; git is not run here
/// instead.
///
/// It is never a way to talk to a remote: a command that carries a credential is refused.
#[derive(Debug, Clone)]
pub struct LocalGit {
    invocation: Invocation,
    sandbox: Option<GitSandbox>,
}

/// What a [`LocalGit`] left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalOutput {
    /// Whether it exited with code 0.
    pub success: bool,
    /// What it wrote to its standard output.
    pub stdout: Vec<u8>,
    /// What it wrote to its standard error.
    pub stderr: Vec<u8>,
}

impl LocalGit {
    /// `git` in `cwd`, from an empty environment plus [`GIT_INHERITED_ENV`](crate::GIT_INHERITED_ENV),
    /// with no prompts, no system configuration and a `C` locale. Run it in `sandbox` if there is one.
    pub fn new(cwd: &Path, sandbox: Option<&GitSandbox>) -> Self {
        let mut env: BTreeMap<OsString, OsString> =
            crate::git_env::inherited_pairs().into_iter().collect();
        for (name, value) in [
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("LC_ALL", "C"),
        ] {
            env.insert(name.into(), value.into());
        }
        Self {
            invocation: Invocation {
                args: [
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "core.fsmonitor=false",
                ]
                .into_iter()
                .map(OsString::from)
                .collect(),
                env,
                cwd: Some(cwd.to_owned()),
                root: None,
                stdin: Vec::new(),
            },
            sandbox: sandbox.cloned(),
        }
    }

    /// Work on the repository `pin` names (`GIT_DIR`, `GIT_COMMON_DIR`, `GIT_WORK_TREE`).
    #[must_use]
    pub fn pinned(mut self, pin: &GitPin) -> Self {
        self.invocation.env.extend(pin.pairs());
        self
    }

    /// Add an argument.
    #[must_use]
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.invocation.args.push(arg.as_ref().to_owned());
        self
    }

    /// Add arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.invocation
            .args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    /// Set a variable of the command's environment.
    #[must_use]
    pub fn env(mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.invocation
            .env
            .insert(name.as_ref().to_owned(), value.as_ref().to_owned());
        self
    }

    /// The command's standard input.
    #[must_use]
    pub fn stdin(mut self, bytes: Vec<u8>) -> Self {
        self.invocation.stdin = bytes;
        self
    }

    /// Run it to the end. Dropping the future stops the command (in the sandbox, its process group).
    ///
    /// # Errors
    ///
    /// When git cannot be started, the sandbox cannot be reached or refuses the command, or the
    /// command carries a credential. A command that ran and failed is an `Ok` with
    /// `success: false`.
    pub async fn output(self) -> io::Result<LocalOutput> {
        let ran = match &self.sandbox {
            Some(sandbox) => self
                .invocation
                .run_in(sandbox)
                .await
                .map_err(io::Error::other)?,
            None => {
                if self.invocation.carries_credentials() {
                    return Err(io::Error::other(SandboxFailure::Credential));
                }
                self.invocation.run_here().await?
            }
        };
        Ok(LocalOutput {
            success: ran.success,
            stdout: ran.stdout,
            stderr: ran.stderr,
        })
    }
}

/// Turn a failed command's (already scrubbed) stderr into the closest
/// [`WorkspaceError`].
pub(crate) fn classify(command: &str, code: Option<i32>, stderr: &str) -> WorkspaceError {
    let message = truncate_tail(stderr.trim(), MAX_STDERR);
    let lower = message.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));

    if has(&[
        "authentication failed",
        "could not read username",
        "could not read password",
        "returned error: 401",
        "returned error: 403",
        "http basic: access denied",
        "permission denied",
    ]) {
        WorkspaceError::Auth(message)
    } else if has(&[
        "returned error: 404",
        "repository not found",
        "' not found",
        "does not appear to be a git repository",
        "couldn't find remote ref",
    ]) {
        WorkspaceError::NotFound(message)
    } else if has(&["[rejected]", "[remote rejected]", "non-fast-forward"]) {
        WorkspaceError::Invalid(message)
    } else if has(&[
        "could not resolve host",
        "failed to connect",
        "connection refused",
        "connection reset",
        "timed out",
        "early eof",
        "rpc failed",
        "returned error: 5",
        "returned error: 429",
        "the remote end hung up",
        "tls connection",
        "ssl_",
    ]) {
        WorkspaceError::transient(message)
    } else {
        WorkspaceError::Git {
            command: command.to_owned(),
            status: code.map_or_else(
                || "killed by signal".to_owned(),
                |c| format!("exit status {c}"),
            ),
            message,
        }
    }
}

/// Keep the end of `s`: git puts the reason last.
fn truncate_tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_owned();
    }
    let skip = count - max;
    let start = s.char_indices().nth(skip).map_or(0, |(i, _)| i);
    format!("...{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use adam_error::Classify as _;

    use super::*;

    #[test]
    fn scrub_removes_token_and_base64_forms() {
        let auth = Auth::new("s3cr3t-token".into(), Some("https://github.com/"));
        let b64 = auth.basic();
        let text = format!(
            "fatal: url with s3cr3t-token; header Authorization: Basic {b64}; bare {}",
            b64.trim_end_matches('=')
        );
        let scrubbed = auth.scrub(&text);
        assert!(!scrubbed.contains("s3cr3t-token"), "{scrubbed}");
        assert!(!scrubbed.contains(b64.trim_end_matches('=')), "{scrubbed}");
    }

    #[test]
    fn header_is_scoped_for_http_and_plain_for_local() {
        let scoped = Auth::new("t".into(), Some("https://github.com/"));
        assert_eq!(scoped.config_key(), "http.https://github.com/.extraHeader");
        assert_eq!(Auth::new("t".into(), None).config_key(), "http.extraHeader");
        assert_eq!(
            scoped.config_value(),
            format!(
                "Authorization: Basic {}",
                STANDARD.encode("x-access-token:t")
            )
        );
    }

    #[test]
    fn classifies_common_failures() {
        let cases = [
            ("fatal: Authentication failed for 'x'", "auth"),
            (
                "fatal: unable to access 'x': The requested URL returned error: 403",
                "auth",
            ),
            (
                "fatal: unable to access 'x': The requested URL returned error: 404",
                "not_found",
            ),
            (
                "fatal: repository 'http://h/o/r.git/' not found",
                "not_found",
            ),
            (
                "fatal: '/nope' does not appear to be a git repository",
                "not_found",
            ),
            (" ! [rejected] a -> a (non-fast-forward)", "invalid"),
            (
                "fatal: unable to access 'x': Failed to connect to h port 1",
                "transient",
            ),
            (
                "fatal: unable to access 'x': The requested URL returned error: 503",
                "transient",
            ),
            (
                "fatal: unable to access 'x': The requested URL returned error: 429",
                "transient",
            ),
            ("fatal: something odd", "git"),
        ];
        for (stderr, want) in cases {
            let got = match classify("fetch", Some(128), stderr) {
                WorkspaceError::Auth(_) => "auth",
                WorkspaceError::NotFound(_) => "not_found",
                WorkspaceError::Invalid(_) => "invalid",
                WorkspaceError::Transient { .. } => "transient",
                WorkspaceError::Git { .. } => "git",
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(got, want, "{stderr}");
        }
    }

    #[test]
    fn truncate_tail_keeps_the_end() {
        assert_eq!(truncate_tail("abcdef", 3), "...def");
        assert_eq!(truncate_tail("abc", 3), "abc");
    }

    fn sandbox() -> GitSandbox {
        GitSandbox::new("/run/git/git.sock", Path::new("/work"))
    }

    fn with_token(cmd: GitCmd) -> GitCmd {
        cmd.maybe_auth(Some(Auth::new(
            "ghp_s3cr3t-token".into(),
            Some("https://github.com/"),
        )))
    }

    #[test]
    fn a_command_without_a_credential_goes_to_the_sandbox_when_there_is_one() {
        for args in [
            vec!["status", "--porcelain"],
            vec!["add", "-A"],
            vec!["diff", "--cached", "--quiet"],
            vec!["commit-tree", "abc", "-m", "m"],
            vec!["worktree", "add", "--quiet", "/work/staging/x"],
            vec!["config", "branch.agent/x.adam-run", "run"],
            vec!["init", "--quiet", "/work/workspaces/run/proj"],
            vec![
                "--git-dir=/work/git/m.git",
                "update-ref",
                "refs/heads/x",
                "abc",
            ],
        ] {
            let cmd = GitCmd::new().sandbox(Some(sandbox())).args(&args);
            assert!(cmd.route().is_some(), "{args:?} stays here");
            assert!(
                GitCmd::new().args(&args).route().is_none(),
                "{args:?}: there is no sandbox to go to"
            );
        }
    }

    #[test]
    fn what_talks_to_a_remote_or_carries_a_token_stays_in_the_coder() {
        for sub in NETWORK_SUBCOMMANDS {
            // With no token at all (a filesystem remote): still the coder's.
            let cmd = GitCmd::new().sandbox(Some(sandbox())).args([
                "--git-dir=/work/git/m.git",
                sub,
                "/srv/r.git",
            ]);
            assert!(cmd.route().is_none(), "{sub} goes to the sandbox");
        }
        // With a token, whatever the subcommand.
        let cmd = with_token(GitCmd::new().sandbox(Some(sandbox())).arg("status"));
        assert!(cmd.route().is_none());
        // And a command that was asked to stay.
        let cmd = GitCmd::new()
            .sandbox(Some(sandbox()))
            .in_coder()
            .arg("init");
        assert!(cmd.route().is_none());
    }

    #[test]
    fn the_token_is_in_the_environment_of_the_command_that_carries_it() {
        let inv = with_token(GitCmd::new().arg("fetch")).invocation();
        let get = |name: &str| {
            inv.env
                .get(OsStr::new(name))
                .map(|v| v.to_string_lossy().into_owned())
        };
        assert_eq!(get("GIT_CONFIG_COUNT").as_deref(), Some("1"));
        assert_eq!(
            get("GIT_CONFIG_KEY_0").as_deref(),
            Some("http.https://github.com/.extraHeader")
        );
        let value = get("GIT_CONFIG_VALUE_0").unwrap();
        let want = format!(
            "Authorization: Basic {}",
            STANDARD.encode("x-access-token:ghp_s3cr3t-token")
        );
        assert_eq!(value, want);
        assert!(inv.carries_credentials());
        // The token is in no argument.
        assert!(
            !inv.args
                .iter()
                .any(|a| a.to_string_lossy().contains("s3cr3t")),
            "{:?}",
            inv.args
        );
    }

    #[test]
    fn the_hardening_options_alone_are_not_a_credential() {
        let inv = GitCmd::new().arg("status").invocation();
        assert!(
            !inv.carries_credentials(),
            "`-c credential.helper=` switches helpers off: it carries nothing"
        );
        assert!(
            inv.args
                .windows(2)
                .any(|w| w[0] == "-c" && w[1] == "credential.helper=")
        );
        // user.name and user.email, which commit-tree is given, are not either.
        let inv = GitCmd::new()
            .config("user.name", "Ada")
            .config("user.email", "ada@example.com")
            .arg("commit-tree")
            .invocation();
        assert!(!inv.carries_credentials());
    }

    #[tokio::test]
    async fn a_command_with_a_credential_is_never_sent_to_the_sandbox() {
        // Nobody listens at the socket: a command that was sent would fail as unreachable. These fail
        // as what they are, before any connection.
        let sandbox = sandbox();
        let token = with_token(GitCmd::new().arg("status")).invocation();
        let header = GitCmd::new()
            .config("http.extraHeader", "Authorization: Basic eA==")
            .arg("status")
            .invocation();
        let helper = GitCmd::new()
            .config("credential.helper", "!store")
            .arg("status")
            .invocation();
        let env = GitCmd::new()
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", "x")
            .arg("status")
            .invocation();
        for (what, inv) in [
            ("token", token),
            ("header", header),
            ("helper", helper),
            ("env", env),
        ] {
            let failure = inv.run_in(&sandbox).await.unwrap_err();
            assert!(
                matches!(failure, SandboxFailure::Credential),
                "{what}: {failure:?}"
            );
        }
        // The same without a credential does try the sandbox, and finds nobody.
        let plain = GitCmd::new()
            .cwd(Path::new("/work"))
            .arg("status")
            .invocation();
        let failure = plain.run_in(&sandbox).await.unwrap_err();
        assert!(
            matches!(
                failure,
                SandboxFailure::Client(ClientError::Unreachable { .. })
            ),
            "{failure:?}"
        );
        // A failure is an error of the command, and is not run here instead.
        let error = sandbox_failed("status", failure);
        assert!(error.is_retryable(), "{error:?}");
        assert!(error.to_string().contains("git sandbox"), "{error}");
    }

    #[tokio::test]
    async fn a_command_with_no_directory_runs_in_the_root_of_the_volume() {
        let cmd = GitCmd::new()
            .ceiling(Path::new("/work"))
            .arg("check-ref-format");
        let inv = cmd.invocation();
        assert_eq!(inv.cwd, None);
        assert_eq!(inv.root.as_deref(), Some(Path::new("/work")));
        // With nothing to run in, it is not sent.
        let bare = GitCmd::new().arg("check-ref-format").invocation();
        let failure = bare.run_in(&sandbox()).await.unwrap_err();
        assert!(
            matches!(failure, SandboxFailure::NoDirectory),
            "{failure:?}"
        );
    }

    #[tokio::test]
    async fn a_question_without_a_sandbox_is_a_child_of_this_process() {
        let tmp = tempfile::tempdir().unwrap();
        let out = LocalGit::new(tmp.path(), None)
            .args(["init", "--quiet", "--initial-branch=main"])
            .output()
            .await
            .unwrap();
        assert!(out.success, "{}", String::from_utf8_lossy(&out.stderr));
        assert!(tmp.path().join(".git").is_dir());
        let out = LocalGit::new(tmp.path(), None)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "HEAD");
        // A failing command is an output, not an error.
        let out = LocalGit::new(tmp.path(), None)
            .args(["rev-parse", "--verify", "nope"])
            .output()
            .await
            .unwrap();
        assert!(!out.success);
        // The input reaches it.
        let out = LocalGit::new(tmp.path(), None)
            .args(["hash-object", "--stdin"])
            .stdin(b"hello\n".to_vec())
            .output()
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    mod prop {
        use proptest::collection::vec;
        use proptest::prelude::*;

        use super::*;

        /// The forms a token takes in git's output.
        #[derive(Clone, Debug)]
        enum Leak {
            Token,
            Basic,
            BasicUnpadded,
            Header,
            ConfigValue,
            Url,
        }

        fn arb_leak() -> impl Strategy<Value = Leak> {
            prop_oneof![
                Just(Leak::Token),
                Just(Leak::Basic),
                Just(Leak::BasicUnpadded),
                Just(Leak::Header),
                Just(Leak::ConfigValue),
                Just(Leak::Url),
            ]
        }

        proptest! {
            /// Whatever surrounds them, and however often they repeat, the
            /// token and its base64 forms (padded or not, in a header, in a
            /// config value, in a URL) never survive scrubbing. Tokens are at
            /// least 12 characters, as real ones are (a token that is a
            /// substring of the `[REDACTED]` marker could not be scrubbed
            /// meaningfully).
            #[test]
            fn prop_scrub_removes_every_token_encoding(
                token in "[A-Za-z0-9_\\-]{12,60}",
                scoped in any::<bool>(),
                parts in vec(("\\PC{0,30}", arb_leak()), 1..8),
                tail in "\\PC{0,30}",
            ) {
                let auth = Auth::new(token.clone().into(), scoped.then_some("https://github.com/"));
                let b64 = auth.basic();
                let bare = b64.trim_end_matches('=').to_owned();
                let mut text = String::new();
                for (noise, leak) in &parts {
                    text.push_str(noise);
                    match leak {
                        Leak::Token => text.push_str(&token),
                        Leak::Basic => text.push_str(&b64),
                        Leak::BasicUnpadded => text.push_str(&bare),
                        Leak::Header => text.push_str(&format!("Authorization: Basic {b64}")),
                        Leak::ConfigValue => text.push_str(&auth.config_value()),
                        Leak::Url => text.push_str(&format!("https://x-access-token:{token}@github.com/o/r")),
                    }
                }
                text.push_str(&tail);
                let scrubbed = auth.scrub(&text);
                prop_assert!(!scrubbed.contains(&token), "token left in {scrubbed:?}");
                prop_assert!(!scrubbed.contains(&b64), "base64 left in {scrubbed:?}");
                prop_assert!(!scrubbed.contains(&bare), "unpadded base64 left in {scrubbed:?}");
            }

            /// Text without any secret passes through unchanged.
            #[test]
            fn prop_scrub_leaves_clean_text_alone(
                token in "[A-Za-z0-9_\\-]{12,60}",
                text in "[ -~]{0,200}",
            ) {
                let auth = Auth::new(token.clone().into(), None);
                prop_assume!(!text.contains(&token));
                prop_assume!(!text.contains(auth.basic().trim_end_matches('=')));
                prop_assert_eq!(auth.scrub(&text), text);
            }
        }
    }
}
