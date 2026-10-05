//! The command line of `adam-kube-exec`, which the session builds and the binary reads.
//!
//! ```text
//! adam-kube-exec --namespace NS --pod POD [--container NAME] [--adam-exec PATH]
//!                [--env NAME=VALUE]... [--unset NAME]...
//!                run|shell ID CWD :WORD [:WORD]...
//! ```
//!
//! Everything after `run` or `shell` is positional and is passed on as `adam-exec` reads it (the
//! devcontainer's script, `crates/adam-devcontainer/src/adam-exec.sh`): an exec id, the working
//! directory, and the words of the command, each with the leading `:` that `adam-exec` removes. The
//! options come first so that a word such as `--version` is never taken for one.
//!
//! The command run in the pod is
//! `[env [-u NAME]... [NAME=VALUE]...] PATH run|shell ID CWD :WORD...`: [`Invocation::remote_argv`].
//! `--env` and `--unset` are the variables the caller set and the ones it hid; a secret is never
//! one of them (the model key is a variable the pod's template sets from a Secret).

use std::fmt;

/// Where `adam-exec` is in the run pod unless told otherwise: the directory the template mounts the
/// tools in.
pub const DEFAULT_ADAM_EXEC: &str = "/opt/adam/bin/adam-exec";

/// What `adam-exec` is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A program and its arguments.
    Run,
    /// A command line, run by a login shell.
    Shell,
}

impl Mode {
    /// The word `adam-exec` knows it by.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Shell => "shell",
        }
    }
}

/// A command line that is wrong, said in a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

fn usage(why: impl Into<String>) -> UsageError {
    UsageError(why.into())
}

/// What `adam-kube-exec` is asked to do: one command in one pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The pod's namespace.
    pub namespace: String,
    /// The pod.
    pub pod: String,
    /// The container of the pod; `None`: the pod's default (its only container).
    pub container: Option<String>,
    /// Where `adam-exec` is in the container.
    pub adam_exec: String,
    /// Variables to set for the command, in order.
    pub env: Vec<(String, String)>,
    /// Variables to remove from the container's own environment, in order.
    pub unset: Vec<String>,
    /// What `adam-exec` is asked to do.
    pub mode: Mode,
    /// The id the command is recorded under, for `adam-exec kill`.
    pub id: String,
    /// The working directory, absolute.
    pub cwd: String,
    /// The words of the command, each with its leading `:`.
    pub words: Vec<String>,
}

/// A name `env` and a shell both take as a variable.
pub fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// What `adam-exec` takes for an id: letters, digits, dot, dash and underscore.
pub fn is_exec_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

impl Invocation {
    /// The arguments of `adam-kube-exec` for this invocation, in the order [`parse`](Self::parse)
    /// reads them.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "--namespace".to_owned(),
            self.namespace.clone(),
            "--pod".to_owned(),
            self.pod.clone(),
        ];
        if let Some(container) = &self.container {
            args.extend(["--container".to_owned(), container.clone()]);
        }
        args.extend(["--adam-exec".to_owned(), self.adam_exec.clone()]);
        for (name, value) in &self.env {
            args.extend(["--env".to_owned(), format!("{name}={value}")]);
        }
        for name in &self.unset {
            args.extend(["--unset".to_owned(), name.clone()]);
        }
        args.extend([
            self.mode.as_str().to_owned(),
            self.id.clone(),
            self.cwd.clone(),
        ]);
        args.extend(self.words.iter().cloned());
        args
    }

    /// Read the arguments of `adam-kube-exec` (without the program's own name).
    ///
    /// # Errors
    ///
    /// [`UsageError`] says what is wrong: a missing or unknown option, a mode that is neither `run`
    /// nor `shell`, an id or a variable name `adam-exec` would not take, a working directory that is
    /// not absolute, a word without its `:`, or no command at all.
    pub fn parse<I, S>(args: I) -> Result<Self, UsageError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut args = args.into_iter().map(Into::into);
        let mut namespace = None;
        let mut pod = None;
        let mut container = None;
        let mut adam_exec = None;
        let mut env = Vec::new();
        let mut unset = Vec::new();
        let mode = loop {
            let Some(arg) = args.next() else {
                return Err(usage("there is no command: expected run or shell"));
            };
            let mut value = |option: &str| {
                args.next()
                    .ok_or_else(|| usage(format!("{option} needs a value")))
            };
            match arg.as_str() {
                "--namespace" => namespace = Some(value("--namespace")?),
                "--pod" => pod = Some(value("--pod")?),
                "--container" => container = Some(value("--container")?),
                "--adam-exec" => adam_exec = Some(value("--adam-exec")?),
                "--env" => {
                    let pair = value("--env")?;
                    let Some((name, val)) = pair.split_once('=') else {
                        return Err(usage(format!("--env {pair:?} is not NAME=VALUE")));
                    };
                    if !is_env_name(name) {
                        return Err(usage(format!("{name:?} is not a variable name")));
                    }
                    if val.contains('\0') {
                        return Err(usage(format!("the value of {name} has a NUL byte")));
                    }
                    env.push((name.to_owned(), val.to_owned()));
                }
                "--unset" => {
                    let name = value("--unset")?;
                    if !is_env_name(&name) {
                        return Err(usage(format!("{name:?} is not a variable name")));
                    }
                    unset.push(name);
                }
                "run" => break Mode::Run,
                "shell" => break Mode::Shell,
                other if other.starts_with('-') => {
                    return Err(usage(format!("unknown option {other}")));
                }
                other => {
                    return Err(usage(format!("expected run or shell, got {other:?}")));
                }
            }
        };
        let namespace = namespace
            .filter(|n| !n.is_empty())
            .ok_or_else(|| usage("--namespace is required"))?;
        let pod = pod
            .filter(|p| !p.is_empty())
            .ok_or_else(|| usage("--pod is required"))?;
        let id = args.next().ok_or_else(|| usage("the exec id is missing"))?;
        if !is_exec_id(&id) {
            return Err(usage(format!(
                "{id:?} is not an exec id (letters, digits, dot, dash and underscore)"
            )));
        }
        let cwd = args
            .next()
            .ok_or_else(|| usage("the working directory is missing"))?;
        if !cwd.starts_with('/') || cwd.contains(['\0', '\n']) {
            return Err(usage(format!(
                "the working directory {cwd:?} must be an absolute path on one line"
            )));
        }
        let words: Vec<String> = args.collect();
        if words.is_empty() {
            return Err(usage("there is no command to run"));
        }
        if mode == Mode::Shell && words.len() != 1 {
            return Err(usage("shell takes exactly one command line"));
        }
        if let Some(bad) = words.iter().find(|w| !w.starts_with(':')) {
            return Err(usage(format!(
                "{bad:?} has no leading `:` (adam-exec removes it from every word)"
            )));
        }
        if words.iter().any(|w| w.contains('\0')) {
            return Err(usage("a word of the command has a NUL byte"));
        }
        Ok(Self {
            namespace,
            pod,
            container: container.filter(|c| !c.is_empty()),
            adam_exec: adam_exec.unwrap_or_else(|| DEFAULT_ADAM_EXEC.to_owned()),
            env,
            unset,
            mode,
            id,
            cwd,
            words,
        })
    }

    /// The command run in the pod: `env` first when there are variables to set or to remove, then
    /// `adam-exec`.
    pub fn remote_argv(&self) -> Vec<String> {
        let mut argv = Vec::new();
        if !self.env.is_empty() || !self.unset.is_empty() {
            argv.push("env".to_owned());
            for name in &self.unset {
                argv.extend(["-u".to_owned(), name.clone()]);
            }
            for (name, value) in &self.env {
                argv.push(format!("{name}={value}"));
            }
        }
        argv.push(self.adam_exec.clone());
        argv.extend([
            self.mode.as_str().to_owned(),
            self.id.clone(),
            self.cwd.clone(),
        ]);
        argv.extend(self.words.iter().cloned());
        argv
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Invocation {
        Invocation {
            namespace: "ci".to_owned(),
            pod: "adam-run-0123456789ab".to_owned(),
            container: Some("run".to_owned()),
            adam_exec: DEFAULT_ADAM_EXEC.to_owned(),
            env: vec![
                ("CI".to_owned(), "1".to_owned()),
                ("JSON".to_owned(), r#"{"a": "b = c"}"#.to_owned()),
            ],
            unset: vec!["MODEL_API_KEY".to_owned()],
            mode: Mode::Run,
            id: "kp-1a2b-7".to_owned(),
            cwd: "/work/workspaces/r/app".to_owned(),
            words: vec![":cargo".to_owned(), ":test".to_owned(), ":--".to_owned()],
        }
    }

    fn parse(args: &[&str]) -> Result<Invocation, UsageError> {
        Invocation::parse(args.iter().copied())
    }

    #[test]
    fn the_arguments_are_read_back_as_they_were_made() {
        let one = sample();
        assert_eq!(Invocation::parse(one.to_args()).unwrap(), one);
        let shell = Invocation {
            mode: Mode::Shell,
            words: vec![": echo 'a b' && exit 3 ".to_owned()],
            container: None,
            env: Vec::new(),
            unset: Vec::new(),
            ..sample()
        };
        assert_eq!(Invocation::parse(shell.to_args()).unwrap(), shell);
    }

    #[test]
    fn the_remote_command_is_env_then_adam_exec_with_the_words_untouched() {
        let argv = sample().remote_argv();
        assert_eq!(
            argv,
            [
                "env",
                "-u",
                "MODEL_API_KEY",
                "CI=1",
                r#"JSON={"a": "b = c"}"#,
                "/opt/adam/bin/adam-exec",
                "run",
                "kp-1a2b-7",
                "/work/workspaces/r/app",
                ":cargo",
                ":test",
                ":--",
            ]
        );
    }

    #[test]
    fn without_variables_adam_exec_is_run_directly() {
        let argv = Invocation {
            env: Vec::new(),
            unset: Vec::new(),
            ..sample()
        }
        .remote_argv();
        assert_eq!(argv[0], "/opt/adam/bin/adam-exec");
    }

    #[test]
    fn a_word_that_looks_like_an_option_is_a_word_after_the_mode() {
        let parsed = parse(&[
            "--namespace",
            "n",
            "--pod",
            "p",
            "run",
            "id1",
            "/w",
            ":--version",
            ":--pod",
        ])
        .unwrap();
        assert_eq!(parsed.words, [":--version", ":--pod"]);
        assert_eq!(parsed.pod, "p");
        assert_eq!(parsed.adam_exec, DEFAULT_ADAM_EXEC);
        assert_eq!(parsed.container, None);
    }

    #[test]
    fn an_equals_sign_in_a_value_stays_in_the_value() {
        let parsed = parse(&[
            "--namespace",
            "n",
            "--pod",
            "p",
            "--env",
            "A=b=c",
            "--env",
            "EMPTY=",
            "shell",
            "id1",
            "/w",
            ":true",
        ])
        .unwrap();
        assert_eq!(
            parsed.env,
            [
                ("A".to_owned(), "b=c".to_owned()),
                ("EMPTY".to_owned(), String::new())
            ]
        );
    }

    #[test]
    fn what_adam_exec_would_not_take_is_refused_here() {
        let base = ["--namespace", "n", "--pod", "p"];
        let with = |rest: &[&str]| {
            let mut all: Vec<&str> = base.to_vec();
            all.extend_from_slice(rest);
            parse(&all).unwrap_err().0
        };
        assert!(with(&["run", "bad id", "/w", ":x"]).contains("not an exec id"));
        assert!(with(&["run", "../x", "/w", ":x"]).contains("not an exec id"));
        assert!(with(&["run", "id", "relative", ":x"]).contains("absolute"));
        assert!(with(&["run", "id", "/w", "x"]).contains("leading `:`"));
        assert!(with(&["run", "id", "/w"]).contains("no command"));
        assert!(with(&["shell", "id", "/w", ":a", ":b"]).contains("exactly one"));
        assert!(with(&["--env", "1BAD=x", "run", "id", "/w", ":x"]).contains("variable name"));
        assert!(with(&["--env", "NOEQUALS", "run", "id", "/w", ":x"]).contains("NAME=VALUE"));
        assert!(with(&["--unset", "a-b", "run", "id", "/w", ":x"]).contains("variable name"));
        assert!(with(&["--bogus", "run"]).contains("unknown option"));
        assert!(with(&["walk", "id", "/w", ":x"]).contains("expected run or shell"));
        assert!(with(&["--container"]).contains("needs a value"));
        assert!(with(&[]).contains("no command"));
    }

    #[test]
    fn the_pod_and_the_namespace_are_required() {
        let err = parse(&["--pod", "p", "run", "id", "/w", ":x"]).unwrap_err();
        assert!(err.0.contains("--namespace"));
        let err = parse(&["--namespace", "n", "run", "id", "/w", ":x"]).unwrap_err();
        assert!(err.0.contains("--pod"));
    }

    #[test]
    fn env_names_and_exec_ids_follow_the_shell_and_adam_exec() {
        assert!(is_env_name("MODEL_API_KEY") && is_env_name("_x1"));
        assert!(
            !is_env_name("") && !is_env_name("1x") && !is_env_name("a-b") && !is_env_name("a b")
        );
        assert!(
            is_exec_id("kp-1.2_3") && !is_exec_id("") && !is_exec_id("a/b") && !is_exec_id("a b")
        );
    }
}
