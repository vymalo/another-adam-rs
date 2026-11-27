//! The `export` command every agent binary has (ADR 0085, slice E2): its arguments, its
//! environment and the run that writes the records.
//!
//! A binary reads its own files, builds its scrub from the secrets it knows and registers its
//! agents' starters; [`run`] opens the store ([`open_store`](crate::open_store)), builds a runtime
//! with those registrations and nothing else (no server, no worker, no schedule), and writes the
//! records of [`adam_export::export_runs`] as JSON Lines to standard output or a file. The summary
//! is one log line, never on standard output.
//!
//! ```text
//! <binary> export [--agent NAME]... [--status done|failed|all] [--require KIND=VALUE]...
//!                 [--since RFC3339] [--until RFC3339] [--include-compacted]
//!                 [--include-reasoning] [--samples run|turn] [--max-tokens N] [--out PATH]
//! ```

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use adam_core::store::RunStatus;
use adam_core::time::Timestamp;
use adam_error::{Classify, ErrorClass};
use adam_export::{ExportError, ExportOptions, Filter, NoRandomness, Pseudonyms, Samples, Summary};
use adam_runtime::Runtime;
use secrecy::SecretString;

use crate::{ConfigError, Register, ServeError};

/// The variable that holds the key of the pseudonyms, read as bytes. Unset: a random key, so the
/// ids of one export link to each other and to nothing else.
pub const PSEUDONYM_KEY_ENV: &str = "EXPORT_PSEUDONYM_KEY";

/// `EX_USAGE`: the command line is wrong (BSD `sysexits.h`, *unverified*, from memory).
pub const EX_USAGE: u8 = 64;

/// What `<binary> export --help` prints, and what a usage error ends with.
pub fn usage(binary: &str) -> String {
    format!(
        "usage: {binary} export [--agent NAME]... [--status done|failed|all] [--require KIND=VALUE]... \
         [--since RFC3339] [--until RFC3339] [--include-compacted] [--include-reasoning] \
         [--samples run|turn] [--max-tokens N] [--out PATH]\n\
         \n\
         Writes one JSON Lines record per run, or per assistant turn (ADR 0085), to standard output, \
         or to PATH (created or truncated, mode 0600).\n\
         \n\
         --agent NAME          an agent this binary registers (repeatable); default: every one\n\
         --status STATUS       done (default), failed, or all (every status, unfinished runs included)\n\
         --require KIND=VALUE  a facet every record must have, lang=rust or dep=axum (repeatable)\n\
         --since RFC3339       only runs last updated at or after this instant\n\
         --until RFC3339       only runs last updated before this instant\n\
         --include-compacted   keep runs whose history a compaction replaced (with --samples turn \
         they are kept anyway)\n\
         --include-reasoning   keep the reasoning text a run kept\n\
         --samples run|turn    one record per run (default), or one per assistant turn since the \
         run's last compaction: the request of that call, its answer last\n\
         --max-tokens N        leave out a record over N tokens (estimated: characters / 4); never cut\n\
         --out PATH            write to PATH instead of standard output\n\
         \n\
         Reads DATABASE_URL and the agent's files as serving does, and {PSEUDONYM_KEY_ENV} (the key of \
         the pseudonymous ids; unset: a random key)."
    )
}

/// The arguments of `export`, parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportArgs {
    /// `--agent`: the agents to export, in order; empty for every agent the binary registers.
    pub agents: Vec<String>,
    /// `--status`, `--require`, `--since`, `--until`, `--include-compacted`.
    pub filter: Filter,
    /// `--include-reasoning`.
    pub include_reasoning: bool,
    /// `--samples`: one record per run (the default) or per assistant turn.
    pub samples: Samples,
    /// `--max-tokens`: the most estimated tokens a record may hold; `None` for no bound.
    pub max_tokens: Option<u32>,
    /// `--out`: the file to write; `None` for standard output.
    pub out: Option<PathBuf>,
}

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Export with these arguments.
    Export(ExportArgs),
    /// `--help` or `-h`: print [`usage`] and stop.
    Help,
}

/// The command line of `export` is wrong: the message says what, and the binary ends with
/// [`EX_USAGE`] after printing [`usage`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct UsageError(pub String);

impl Classify for UsageError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// Parse the arguments that follow `export`. Strict: an unknown flag, a flag given twice that
/// takes one value, a value that does not read, or a positional argument is a [`UsageError`].
/// A flag's value follows it (`--out file`) or is joined to it (`--out=file`).
///
/// # Errors
///
/// [`UsageError`].
pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, UsageError> {
    let mut agents: Vec<String> = Vec::new();
    let mut status: Option<Vec<RunStatus>> = None;
    let mut require = Vec::new();
    let mut since = None;
    let mut until = None;
    let mut include_compacted = false;
    let mut include_reasoning = false;
    let mut samples: Option<Samples> = None;
    let mut max_tokens: Option<u32> = None;
    let mut out: Option<PathBuf> = None;

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        // `--out` takes a path, which need not be text; everything else is text.
        let (flag, joined) = match arg.to_str() {
            Some(text) => match text.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => {
                    (flag.to_owned(), Some(OsString::from(value)))
                }
                _ => (text.to_owned(), None),
            },
            None => {
                return Err(UsageError(format!("{} is not text", arg.to_string_lossy())));
            }
        };
        let mut value = |name: &str| -> Result<OsString, UsageError> {
            joined
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| UsageError(format!("{name} needs a value")))
        };
        let text = |name: &str, raw: OsString| -> Result<String, UsageError> {
            raw.into_string()
                .map_err(|_| UsageError(format!("the value of {name} is not text")))
        };
        let takes_no_value = |name: &str| -> Result<(), UsageError> {
            match &joined {
                Some(_) => Err(UsageError(format!("{name} takes no value"))),
                None => Ok(()),
            }
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--agent" => {
                let name = text("--agent", value("--agent")?)?;
                if name.trim().is_empty() {
                    return Err(UsageError("--agent needs a name".into()));
                }
                if !agents.contains(&name) {
                    agents.push(name);
                }
            }
            "--status" => {
                let raw = text("--status", value("--status")?)?;
                if status.is_some() {
                    return Err(UsageError("--status is given twice".into()));
                }
                status = Some(match raw.as_str() {
                    "done" => vec![RunStatus::Done],
                    "failed" => vec![RunStatus::Failed],
                    "all" => Vec::new(),
                    other => {
                        return Err(UsageError(format!(
                            "--status {other:?} is not done, failed or all"
                        )));
                    }
                });
            }
            "--require" => {
                let raw = text("--require", value("--require")?)?;
                require.push(
                    Filter::parse_requirement(&raw)
                        .map_err(|why| UsageError(format!("--require: {why}")))?,
                );
            }
            "--since" | "--until" => {
                let name = flag.clone();
                let raw = text(&name, value(&name)?)?;
                let at: Timestamp = raw.trim().parse().map_err(|_| {
                    UsageError(format!(
                        "{name} {raw:?} is not an RFC 3339 instant (2026-11-01T00:00:00Z)"
                    ))
                })?;
                let slot = if name == "--since" {
                    &mut since
                } else {
                    &mut until
                };
                if slot.replace(at).is_some() {
                    return Err(UsageError(format!("{name} is given twice")));
                }
            }
            "--include-compacted" => {
                takes_no_value("--include-compacted")?;
                include_compacted = true;
            }
            "--include-reasoning" => {
                takes_no_value("--include-reasoning")?;
                include_reasoning = true;
            }
            "--samples" => {
                let raw = text("--samples", value("--samples")?)?;
                let mode = match raw.as_str() {
                    "run" => Samples::Run,
                    "turn" => Samples::Turn,
                    other => {
                        return Err(UsageError(format!(
                            "--samples {other:?} is not run or turn"
                        )));
                    }
                };
                if samples.replace(mode).is_some() {
                    return Err(UsageError("--samples is given twice".into()));
                }
            }
            "--max-tokens" => {
                let raw = text("--max-tokens", value("--max-tokens")?)?;
                let n = raw
                    .parse::<u32>()
                    .ok()
                    .filter(|n| *n > 0 && !raw.starts_with('+'))
                    .ok_or_else(|| {
                        UsageError(format!(
                            "--max-tokens {raw:?} is not a whole number of tokens above 0"
                        ))
                    })?;
                if max_tokens.replace(n).is_some() {
                    return Err(UsageError("--max-tokens is given twice".into()));
                }
            }
            "--out" => {
                let path = value("--out")?;
                if path.is_empty() {
                    return Err(UsageError("--out needs a path".into()));
                }
                if out.replace(PathBuf::from(path)).is_some() {
                    return Err(UsageError("--out is given twice".into()));
                }
            }
            "--current-tools" => {
                return Err(UsageError(
                    "--current-tools is not available yet: the agent's current tool schemas are not \
                     reachable without assembling it (its model and MCP servers); a record says \
                     tools_source \"none\""
                        .into(),
                ));
            }
            other if other.starts_with('-') => {
                return Err(UsageError(format!("unknown flag {other}")));
            }
            other => {
                return Err(UsageError(format!("unexpected argument {other:?}")));
            }
        }
    }
    if let (Some(since), Some(until)) = (since, until)
        && since >= until
    {
        return Err(UsageError("--since must be before --until".into()));
    }
    let defaults = Filter::default();
    Ok(Command::Export(ExportArgs {
        agents,
        filter: Filter {
            statuses: status.unwrap_or(defaults.statuses),
            since,
            until,
            require,
            include_compacted,
            min_turns: defaults.min_turns,
        },
        include_reasoning,
        samples: samples.unwrap_or_default(),
        max_tokens,
        out,
    }))
}

/// The key of the pseudonyms, from the raw value of [`PSEUDONYM_KEY_ENV`] (`std::env::var_os`):
/// its bytes, as they are. `None` when it is unset.
///
/// # Errors
///
/// [`ConfigError`] (exit 78) when it is set and empty or blank: a key that hides nothing.
pub fn pseudonym_key(raw: Option<OsString>) -> Result<Option<Vec<u8>>, ConfigError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let bytes = raw.into_encoded_bytes();
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(ConfigError::new(vec![format!(
            "{PSEUDONYM_KEY_ENV} is set and empty: give it a key (any bytes), or unset it for a \
             random one"
        )]));
    }
    Ok(Some(bytes))
}

/// The pseudonyms under `key`, or under a random key when there is none, which is said in the log.
///
/// # Errors
///
/// [`ExportCommandError::NoRandomness`] when there is no key and the system has no randomness.
pub fn pseudonyms(key: Option<&[u8]>) -> Result<Pseudonyms, ExportCommandError> {
    match key {
        Some(key) => Ok(Pseudonyms::new(key)),
        None => {
            tracing::info!(
                "{PSEUDONYM_KEY_ENV} is unset: the ids of this export are pseudonyms under a random \
                 key, so they link to each other and to nothing else (not to another export)"
            );
            Pseudonyms::random().map_err(ExportCommandError::NoRandomness)
        }
    }
}

/// Why the export stopped.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExportCommandError {
    /// The store cannot be opened (the scheme, the connection, the schema).
    #[error("opening the store (DATABASE_URL)")]
    Store(#[source] ServeError),
    /// `--agent` names an agent this binary does not register.
    #[error("--agent {name} is not an agent of this binary: it registers {}", known.join(", "))]
    UnknownAgent {
        /// The name given.
        name: String,
        /// The agents the binary registers.
        known: Vec<String>,
    },
    /// The file of `--out` cannot be created.
    #[error("creating {} (--out)", path.display())]
    Output {
        /// The path given.
        path: PathBuf,
        /// What the system said.
        #[source]
        source: std::io::Error,
    },
    /// A listing, a write or a record failed.
    #[error("exporting the runs")]
    Export(#[source] ExportError),
    /// There is no key and the system has no randomness for one.
    #[error("making a random pseudonym key")]
    NoRandomness(#[source] NoRandomness),
}

impl ExportCommandError {
    /// The class of the errors whose cause says nothing more, for the exit code; `None` for the
    /// rest, which the chain decides further down.
    pub(crate) fn own_class(&self) -> Option<ErrorClass> {
        match self {
            Self::UnknownAgent { .. } => Some(ErrorClass::Invalid),
            Self::NoRandomness(_) => Some(ErrorClass::Internal),
            Self::Store(_) | Self::Output { .. } | Self::Export(_) => None,
        }
    }
}

impl Classify for ExportCommandError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Store(ServeError::Connect { source, .. } | ServeError::Migrate(source)) => {
                source.class()
            }
            Self::Store(_) | Self::UnknownAgent { .. } => ErrorClass::Invalid,
            Self::Output { .. } => ErrorClass::Transient,
            Self::Export(e) => e.class(),
            Self::NoRandomness(_) => ErrorClass::Internal,
        }
    }
}

/// Export the runs `args` asks for: open the store `database_url` names, build a runtime with
/// what `register` puts on it (the agents' starters: what joins a run's transcript) and nothing
/// else, and write the records to `args.out` or standard output. `args.samples` and
/// `args.max_tokens` replace those of `options`. The summary is one `info` line of the log.
///
/// # Errors
///
/// [`ExportCommandError`].
pub async fn run(
    database_url: &SecretString,
    register: Register,
    args: &ExportArgs,
    options: &ExportOptions,
) -> Result<Summary, ExportCommandError> {
    let store = crate::open_store(database_url)
        .await
        .map_err(ExportCommandError::Store)?;
    let runtime = register(Runtime::builder(store)).build();
    let agents = agents_to_export(&args.agents, &runtime.agent_names())?;
    let statuses: Vec<&str> = args.filter.statuses.iter().map(|s| s.as_str()).collect();
    tracing::info!(
        agents = ?agents,
        statuses = ?statuses,
        require = ?args.filter.require,
        out = args.out.as_deref().map(|p| p.display().to_string()),
        "exporting runs"
    );
    let options = options
        .clone()
        .samples(args.samples)
        .max_tokens(args.max_tokens);
    let mut out = open_out(args.out.as_deref())?;
    let summary = adam_export::export_runs(&runtime, &agents, &args.filter, &options, &mut out)
        .await
        .map_err(ExportCommandError::Export)?;
    tracing::info!(
        seen = summary.seen,
        written = summary.written,
        filtered = summary.filtered,
        not_a_conversation = summary.not_a_conversation,
        unreadable = summary.unreadable,
        over_budget = summary.over_budget,
        turns_lost_to_compaction = summary.turns_lost_to_compaction,
        inexact = summary.inexact,
        "export finished"
    );
    Ok(summary)
}

/// `asked`, each checked against `registered`; every registered agent when `asked` is empty.
fn agents_to_export(
    asked: &[String],
    registered: &[String],
) -> Result<Vec<String>, ExportCommandError> {
    if asked.is_empty() {
        return Ok(registered.to_vec());
    }
    for name in asked {
        if !registered.contains(name) {
            return Err(ExportCommandError::UnknownAgent {
                name: name.clone(),
                known: registered.to_vec(),
            });
        }
    }
    Ok(asked.to_vec())
}

/// Standard output, or `path` created or truncated, readable by its owner only.
fn open_out(path: Option<&Path>) -> Result<Box<dyn Write + Send>, ExportCommandError> {
    let Some(path) = path else {
        return Ok(Box::new(std::io::BufWriter::new(std::io::stdout())));
    };
    let failed = |source| ExportCommandError::Output {
        path: path.to_path_buf(),
        source,
    };
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        open.mode(0o600);
    }
    let file = open.open(path).map_err(failed)?;
    // A file that existed keeps its mode through `open`: it is narrowed too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(failed)?;
    }
    Ok(Box::new(std::io::BufWriter::new(file)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, UsageError> {
        parse_args(args.iter().map(OsString::from))
    }

    fn export(args: &[&str]) -> ExportArgs {
        match parse(args) {
            Ok(Command::Export(args)) => args,
            other => panic!("not an export: {other:?}"),
        }
    }

    #[test]
    fn no_flag_exports_every_agent_s_finished_runs_to_standard_output() {
        let args = export(&[]);
        assert!(args.agents.is_empty());
        assert_eq!(args.filter, Filter::default());
        assert!(!args.include_reasoning);
        assert_eq!(args.samples, Samples::Run);
        assert_eq!(args.max_tokens, None);
        assert_eq!(args.out, None);
    }

    #[test]
    fn samples_and_a_token_budget_read() {
        let args = export(&["--samples", "turn", "--max-tokens", "16000"]);
        assert_eq!(args.samples, Samples::Turn);
        assert_eq!(args.max_tokens, Some(16_000));
        let joined = export(&["--samples=run", "--max-tokens=8000"]);
        assert_eq!(joined.samples, Samples::Run);
        assert_eq!(joined.max_tokens, Some(8_000));
        for args in [
            &["--samples"][..],
            &["--samples", "turns"],
            &["--samples", "run", "--samples", "turn"],
            &["--max-tokens"],
            &["--max-tokens", "0"],
            &["--max-tokens", "-5"],
            &["--max-tokens", "+5"],
            &["--max-tokens", "1.5"],
            &["--max-tokens", "16k"],
            &["--max-tokens", "99999999999"],
            &["--max-tokens", "1", "--max-tokens", "2"],
        ] {
            assert!(parse(args).is_err(), "{args:?} was accepted");
        }
        assert_eq!(
            parse(&["--samples", "turns"]),
            Err(UsageError("--samples \"turns\" is not run or turn".into()))
        );
    }

    #[test]
    fn every_flag_reads() {
        let args = export(&[
            "--agent",
            "coder",
            "--agent=coder/researcher",
            "--agent",
            "coder",
            "--status",
            "failed",
            "--require",
            "lang=rust",
            "--require=dep=Axum",
            "--since",
            "2026-11-01T00:00:00Z",
            "--until=2026-11-27T00:00:00Z",
            "--include-compacted",
            "--include-reasoning",
            "--out",
            "runs.jsonl",
        ]);
        assert_eq!(args.agents, ["coder", "coder/researcher"]);
        assert_eq!(args.filter.statuses, [RunStatus::Failed]);
        assert_eq!(
            args.filter.require,
            [
                ("lang".to_owned(), "rust".to_owned()),
                ("dep".to_owned(), "axum".to_owned())
            ]
        );
        assert_eq!(
            args.filter.since,
            Some("2026-11-01T00:00:00Z".parse().expect("an instant"))
        );
        assert_eq!(
            args.filter.until,
            Some("2026-11-27T00:00:00Z".parse().expect("an instant"))
        );
        assert!(args.filter.include_compacted);
        assert!(args.include_reasoning);
        assert_eq!(args.out, Some(PathBuf::from("runs.jsonl")));
        assert_eq!(export(&["--status", "all"]).filter.statuses, []);
        assert_eq!(
            export(&["--status=done"]).filter.statuses,
            [RunStatus::Done]
        );
    }

    #[test]
    fn help_is_asked_for() {
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["--agent", "x", "-h"]), Ok(Command::Help));
        assert!(usage("adam-agent").starts_with("usage: adam-agent export "));
    }

    #[test]
    fn a_wrong_command_line_is_a_usage_error() {
        for args in [
            &["--bogus"][..],
            &["positional"],
            &["--agent"],
            &["--agent", ""],
            &["--status", "parked"],
            &["--status", "done", "--status", "failed"],
            &["--require", "rust"],
            &["--since", "yesterday"],
            &[
                "--since",
                "2026-11-02T00:00:00Z",
                "--since",
                "2026-11-03T00:00:00Z",
            ],
            &[
                "--since",
                "2026-11-02T00:00:00Z",
                "--until",
                "2026-11-01T00:00:00Z",
            ],
            &["--include-reasoning=yes"],
            &["--out"],
            &["--out", "a", "--out", "b"],
            &["--current-tools"],
        ] {
            assert!(parse(args).is_err(), "{args:?} was accepted");
        }
        assert_eq!(
            parse(&["--bogus"]),
            Err(UsageError("unknown flag --bogus".into()))
        );
    }

    #[test]
    fn the_key_is_bytes_and_an_empty_one_is_refused() {
        assert_eq!(pseudonym_key(None).expect("unset is fine"), None);
        assert_eq!(
            pseudonym_key(Some("s3cret".into())).expect("a key"),
            Some(b"s3cret".to_vec())
        );
        for empty in ["", "  "] {
            let e = pseudonym_key(Some(empty.into())).expect_err("an empty key");
            assert!(e.problems[0].contains(PSEUDONYM_KEY_ENV), "{e}");
            assert_eq!(crate::exit_code(&e), crate::EX_CONFIG);
        }
    }

    #[test]
    fn an_agent_must_be_registered() {
        let registered = ["coder".to_owned(), "coder/researcher".to_owned()];
        assert_eq!(
            agents_to_export(&[], &registered).expect("every agent"),
            registered
        );
        let e = agents_to_export(&["other".to_owned()], &registered).expect_err("unknown");
        assert_eq!(e.class(), ErrorClass::Invalid);
        assert_eq!(crate::exit_code(&e), crate::EX_CONFIG);
    }

    #[cfg(unix)]
    #[test]
    fn the_output_file_is_its_owner_s_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("a directory");
        let path = dir.path().join("runs.jsonl");
        std::fs::write(&path, "old content\n").expect("a file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("a mode");
        let mut out = open_out(Some(&path)).expect("opened");
        out.write_all(b"{}\n").expect("written");
        out.flush().expect("flushed");
        drop(out);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{}\n");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// The whole command over a fresh SQLite file: no run, no record, a summary of zeros.
    #[tokio::test]
    async fn an_empty_store_exports_nothing() {
        let dir = tempfile::tempdir().expect("a directory");
        let url = SecretString::from(format!(
            "sqlite://{}",
            dir.path().join("adam.sqlite").display()
        ));
        let out = dir.path().join("runs.jsonl");
        let Command::Export(args) = parse_args(
            ["--status", "all", "--out"]
                .into_iter()
                .map(OsString::from)
                .chain([out.clone().into_os_string()]),
        )
        .expect("arguments") else {
            panic!("not an export");
        };
        let register: Register =
            Box::new(|builder| builder.starter(adam_llm_agent::LlmStarter::new("chat")));
        let options = ExportOptions::new(Pseudonyms::new(b"k"));
        let summary = run(&url, register, &args, &options)
            .await
            .expect("exported");
        assert_eq!(summary, Summary::default());
        assert_eq!(std::fs::read_to_string(&out).expect("read"), "");
    }
}
