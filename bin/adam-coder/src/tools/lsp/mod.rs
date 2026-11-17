//! `file_diagnostics`, `find_definition`, `find_references`: what a language server says about a file
//! of the workspace (ADR 0044 slice 17). Three tools, read only, at the lowest access tier, and
//! allowed in a reference.
//!
//! A language server is a process that reads a whole project and keeps what it understood. The
//! coder holds one **per run, slot, language and project root, on this worker**, started on the first
//! call and kept until it is idle ([`CoderSettings::language_server_idle`](super::CoderSettings::language_server_idle)), the run parks, ends or
//! remakes its environment, or the worker stops ([`Servers`] is held by
//! [`Commands`](super::background::Commands), whose stop points are the servers' too).
//!
//! ```mermaid
//! sequenceDiagram
//!     participant M as model
//!     participant T as file_diagnostics
//!     participant L as Servers
//!     participant E as EnvSession
//!     participant S as server (stdio)
//!     M->>T: {path, wait}
//!     T->>E: probe: command -v, --version (once per run, slot, language)
//!     T->>E: safe mode only: snapshot of the slot
//!     T->>L: the server for (run, slot, language, root, mode)
//!     L->>E: prepare (background) and spawn, stdin piped
//!     L->>S: initialize, initialized
//!     T->>S: didOpen or didChange, didChangeWatchedFiles
//!     S-->>L: $/progress, publishDiagnostics
//!     L-->>T: ready, or the deadline of wait
//!     T->>E: safe mode only: snapshot again, put back what changed
//!     T->>T: sort, cut, scrub, page
//!     T-->>M: path:line:col lines, [end] or [still indexing]
//! ```
//!
//! # What the model gives and gets
//!
//! A position is a **line** (from 1) and the **symbol** on it (text), with `occurrence` when the
//! symbol is there more than once: the column is found here and counted in the encoding the server
//! agreed to ([`position`]). Only whole words count, and a symbol that is not on the line as a word is
//! answered with the line as it is. A line ends at `\r\n`, `\n` or a lone `\r`.
//! Every position the model reads is `path:line:col` (from 1, columns in characters), with paths
//! relative to the slot; a location in another slot of the workspace is `<slot>/<path>` and a
//! location anywhere else is `(outside the workspace) <last three components>:line`, never a path
//! of this machine.
//!
//! # Time
//!
//! `wait` (1 to 120 s, 30 by default) is how long a call waits for a server that is still indexing.
//! Past it the model gets what the server has said, under a line that says it is still indexing, and
//! the server stays: the next call finds it warm. A call that starts a server also has
//! [`INITIALIZE_TIMEOUT`] for `initialize`, and [`EXTRA`] for the probe and the sync. The tool's
//! time limit is the environment's start, [`BEYOND_ENVIRONMENT`] and the margin of every tool.
//!
//! # What a server may do
//!
//! The client never declares `workspace.applyEdit` and answers a server that asks anyway with
//! `applied: false`. A server has a [`Mode`]: **safe** in a reference and in a run at the access tier
//! `plan` (the reference settings: no build script, no procedural macro, no check on save, the image's
//! TypeScript with no plugin), **full** in a slot a run at a higher tier may write (as `run_checks`
//! there). For a safe server the slot is snapshotted (the snapshot of `run_command`) and compared
//! after each call, a reference against its state when the server started and a writable slot against
//! its state before the call: a difference is restored, the server is stopped and the tool says so.
//!
//! That is the whole guarantee: **git-visible state of the call's own slot, at call boundaries**. A
//! write after the last comparison, an ignored file, a path outside the slot and the project code a
//! server runs while it looks (`cargo metadata`, a `.cargo/config` wrapper, `go list`) are not covered,
//! and a full server is not compared at all. A server runs where `run_command` runs, as the same user,
//! with the same variables hidden. Everything it says passes through the redactor before it is cut or
//! shown, with direction controls and other paths written out.

pub mod position;
pub mod rpc;
pub mod server;
pub mod table;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use adam::prelude::*;
use adam_a2a::AccessTier;
use adam_llm_agent::paging;
use adam_workspace::{EnvSession, Slot, SlotDir};
use serde_json::{Value, json};

use super::files::{Access, confine};
use super::inspect::Snapshot;
use super::shell::shell_spec;
use super::skills::listing_text;
use super::{Outcome, ToolEnv, cancelled, non_empty, notes_error, run_error};
use crate::redact::Redactor;
pub use server::{Probe, ServerInfo, Servers};
pub use table::{Language, Mode, ServerSpec, ServerTable, Settings};

use position::{
    Encoding, Found, file_uri, find_symbol, path_of_uri, split_lines, to_chars, to_server,
};
use server::{
    Diagnostics, INITIALIZE_TIMEOUT, Key, PROBE_TIMEOUT, Reply, Server, StartError, Synced, Target,
    Want,
};

/// The names of the three tools.
pub const LANGUAGE_SERVER_TOOLS: [&str; 3] =
    ["file_diagnostics", "find_definition", "find_references"];

/// What `wait` is when it is left out, and the most it may be, in seconds.
pub const DEFAULT_WAIT_SECS: i64 = 30;
/// The most `wait` may be, in seconds.
pub const MAX_WAIT_SECS: i64 = 120;
/// What a call has besides `wait` and `initialize`: the probe of the program (20 s), the sync of the
/// files, the check of a reference and the formatting.
pub const EXTRA: Duration = Duration::from_secs(60);
/// What the time limit of a tool is, beyond the environment's start and the margin every tool has:
/// the longest `wait`, `initialize` and [`EXTRA`].
pub const BEYOND_ENVIRONMENT: Duration = Duration::from_secs(MAX_WAIT_SECS.unsigned_abs())
    .saturating_add(INITIALIZE_TIMEOUT)
    .saturating_add(EXTRA);

/// The most diagnostics, definitions and references a page holds.
const PAGE: u32 = 100;
/// The most locations a `find_definition` answer lists.
const MAX_DEFINITIONS: usize = 20;
/// The most references collected from one answer; the rest is counted.
pub const MAX_COLLECTED: usize = 2000;
/// The longest source line shown, and the longest diagnostic message, in characters.
const MAX_SOURCE_CHARS: usize = 200;
const MAX_MESSAGE_CHARS: usize = 1000;
/// The most files read for the source lines of one answer.
const MAX_SOURCE_FILES: usize = 200;
/// The longest symbol, in characters.
const MAX_SYMBOL_CHARS: usize = 200;

/// What a tool call asks for.
enum Op {
    Diagnostics,
    Definition,
    References { include_declaration: bool },
}

struct Ask {
    op: Op,
    path: String,
    position: Option<(i64, String, usize)>,
    repo: Option<String>,
    wait: Duration,
    offset: Option<i64>,
    limit: Option<i64>,
}

fn wait_of(wait: Option<i64>) -> Result<Duration, String> {
    match wait {
        None => Ok(Duration::from_secs(DEFAULT_WAIT_SECS.unsigned_abs())),
        Some(secs) if (1..=MAX_WAIT_SECS).contains(&secs) => {
            Ok(Duration::from_secs(secs.unsigned_abs()))
        }
        Some(secs) => Err(format!(
            "wait is {secs}: it is from 1 to {MAX_WAIT_SECS} seconds; leave it out for {DEFAULT_WAIT_SECS}"
        )),
    }
}

fn position_of(
    line: i64,
    symbol: &str,
    occurrence: Option<i64>,
) -> Result<(i64, String, usize), String> {
    if line < 1 {
        return Err("line counts from 1".to_owned());
    }
    let Some(symbol) = non_empty(symbol) else {
        return Err("symbol is required: the text on that line the position is at".to_owned());
    };
    if symbol.contains('\n') || symbol.chars().count() > MAX_SYMBOL_CHARS {
        return Err(format!(
            "symbol is text on one line, at most {MAX_SYMBOL_CHARS} characters"
        ));
    }
    let occurrence = occurrence.unwrap_or(1);
    if occurrence < 1 {
        return Err("occurrence counts from 1".to_owned());
    }
    Ok((
        line,
        symbol.to_owned(),
        usize::try_from(occurrence).unwrap_or(1),
    ))
}

/// What a language server says is wrong in a file: errors and warnings as `path:line:col: error:
/// message [code]`, sorted, a page of at most 100 (the last line says `[more: ... offset=N]` or
/// `[end: ...]`). The server (Rust, TypeScript and JavaScript, Python, Go) runs in the workspace's
/// environment and is kept between calls; the first call on a project may wait for it to index
/// (`wait` seconds, 1 to 120, default 30; a first line says when it still is). Nothing is changed.
/// With no server in the environment the answer says so: use run_checks or run_command. Example:
/// {"path":"src/lib.rs"}
#[tool(label = "Diagnostics of a file", icon = "search", paged)]
pub async fn file_diagnostics(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// The file, relative to the worktree root (.rs, .ts .tsx .js .jsx, .py, .go)
    path: String,
    /// The slot the file is in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
    /// Seconds to wait for a server that is still indexing, 1 to 120 (default 30)
    wait: Option<i64>,
    /// First diagnostic, from 1; -20 = the last 20
    offset: Option<i64>,
    /// Most diagnostics (at most 100)
    limit: Option<i64>,
) -> Outcome {
    let wait = match wait_of(wait) {
        Ok(wait) => wait,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    serve(
        &env,
        ctx,
        Ask {
            op: Op::Diagnostics,
            path,
            position: None,
            repo,
            wait,
            offset,
            limit,
        },
    )
    .await
}

/// Where the symbol on a line of a file is defined, by a language server (Rust, TypeScript and
/// JavaScript, Python, Go): at most 20 locations, each as `path:line:col: the source line`. Give
/// the `line` (from 1) and the `symbol` on it, and `occurrence` when it is there more than once; if
/// the symbol is not on that line the answer shows the line. A first call on a project starts the
/// server and may wait for it to index (`wait` seconds, 1 to 120, default 30); where there is no
/// server the answer says so. Nothing is changed. Example:
/// {"path":"src/main.rs","line":12,"symbol":"parse_config"}
#[tool(label = "Find a definition", icon = "search")]
#[allow(clippy::too_many_arguments)] // the model's arguments, one each
pub async fn find_definition(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// The file, relative to the worktree root
    path: String,
    /// The line, from 1
    line: i64,
    /// The text on that line the position is at (a name)
    symbol: String,
    /// Which one when the symbol is on the line more than once, from 1 (default 1)
    occurrence: Option<i64>,
    /// The slot the file is in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
    /// Seconds to wait for a server that is still indexing, 1 to 120 (default 30)
    wait: Option<i64>,
) -> Outcome {
    let wait = match wait_of(wait) {
        Ok(wait) => wait,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    let position = match position_of(line, &symbol, occurrence) {
        Ok(position) => position,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    serve(
        &env,
        ctx,
        Ask {
            op: Op::Definition,
            path,
            position: Some(position),
            repo,
            wait,
            offset: None,
            limit: None,
        },
    )
    .await
}

/// Where the symbol on a line of a file is used, by a language server (Rust, TypeScript and
/// JavaScript, Python, Go): `path:line:col: the source line`, sorted, a page of at most 100 (the last
/// line says `[more: ... offset=N]` or `[end: ...]`); at most 2000 are collected. Give the `line`
/// (from 1) and the `symbol` on it, and `occurrence` when it is there more than once;
/// `include_declaration` (default true) lists the definition too. The first call on a project may
/// wait for the server to index (`wait` seconds, 1 to 120, default 30). Nothing is changed. Example:
/// {"path":"src/main.rs","line":12,"symbol":"parse_config"}
#[tool(label = "Find references", icon = "search", paged)]
#[allow(clippy::too_many_arguments)] // the model's arguments, one each
pub async fn find_references(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// The file, relative to the worktree root
    path: String,
    /// The line, from 1
    line: i64,
    /// The text on that line the position is at (a name)
    symbol: String,
    /// Which one when the symbol is on the line more than once, from 1 (default 1)
    occurrence: Option<i64>,
    /// List the definition among the references (default true)
    include_declaration: Option<bool>,
    /// The slot the file is in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
    /// Seconds to wait for a server that is still indexing, 1 to 120 (default 30)
    wait: Option<i64>,
    /// First reference, from 1; -20 = the last 20
    offset: Option<i64>,
    /// Most references (at most 100)
    limit: Option<i64>,
) -> Outcome {
    let wait = match wait_of(wait) {
        Ok(wait) => wait,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    let position = match position_of(line, &symbol, occurrence) {
        Ok(position) => position,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    serve(
        &env,
        ctx,
        Ask {
            op: Op::References {
                include_declaration: include_declaration.unwrap_or(true),
            },
            path,
            position: Some(position),
            repo,
            wait,
            offset,
            limit,
        },
    )
    .await
}

/// The project root of `rel` for `language`: the nearest directory, from the file's own up to the
/// slot's root, that holds one of the language's marker files; the slot's root when none does.
fn project_root(slot: &Path, rel: &Path, language: Language) -> PathBuf {
    let mut dir = rel.parent().map(Path::to_path_buf).unwrap_or_default();
    loop {
        let abs = slot.join(&dir);
        let marked = language.markers().iter().any(|marker| {
            std::fs::symlink_metadata(abs.join(marker)).is_ok_and(|meta| meta.is_file())
        });
        if marked {
            return abs;
        }
        match dir.parent() {
            Some(up) if !dir.as_os_str().is_empty() => dir = up.to_path_buf(),
            _ => return slot.to_path_buf(),
        }
    }
}

/// The program as the model reads it: its name, never a path of this machine.
fn shown(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// `text` as one shell word.
fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// What to do instead when there is no server, for each language.
fn instead(language: Language) -> &'static str {
    match language {
        Language::Rust => "`cargo check`",
        Language::TypeScript => "`tsc --noEmit`",
        Language::Python => "a type checker or the tests",
        Language::Go => "`go vet ./...` or `go build ./...`",
    }
}

async fn serve(env: &ToolEnv, ctx: &ToolCtx, ask: Ask) -> Outcome {
    if !env.settings.language_servers {
        return Ok(ToolOutput::error(
            "language servers are off in this deployment (LANGUAGE_SERVERS=off)",
        ));
    }
    let Some(path) = non_empty(&ask.path).map(str::to_owned) else {
        return Ok(ToolOutput::error("path is required"));
    };
    let Some((language, language_id)) = Language::of_path(&path) else {
        return Ok(ToolOutput::text(format!(
            "No language server serves `{path}`: they serve .rs (Rust), .ts .tsx .mts .cts .js .jsx \
             .mjs .cjs (TypeScript and JavaScript), .py .pyi (Python) and .go (Go). Read the file \
             with read_file, and look for its uses with search_files."
        )));
    };
    let slot = match env.slot_to_read(ctx, ask.repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    let rel = match confine(slot.root(), &path, Access::Read) {
        Ok(confined) => confined.rel(),
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    let text = {
        let (root, rel) = (Arc::clone(slot.root()), rel.clone());
        match tokio::task::spawn_blocking(move || server::read_text(&root, &rel)).await {
            Ok(Ok(text)) => text,
            Ok(Err(reason)) => return Ok(ToolOutput::error(reason)),
            Err(e) => return Ok(ToolOutput::error(format!("the read was interrupted: {e}"))),
        }
    };
    ctx.emit_progress(format!("language server: {path} ({})", slot.dir()))
        .await;
    // Where the server runs: made before the budget starts, since it may take a while.
    let session = env.session(ctx).await?;
    let budget = ask.wait + INITIALIZE_TIMEOUT + EXTRA;
    let call = Call {
        env,
        ctx,
        ask: &ask,
        slot: &slot,
        session: &session,
        language,
        language_id,
        rel: &rel,
        text: &text,
        started: Instant::now(),
    };
    tokio::select! {
        biased;
        () = ctx.cancelled() => Err(cancelled("the language server call was stopped")),
        done = tokio::time::timeout(budget, call.run()) => match done {
            Ok(outcome) => outcome,
            Err(_) => Ok(ToolOutput::error(format!(
                "The language server call did not finish within {} s. A server that was started is \
                 kept: call again, or use run_checks or run_command.",
                budget.as_secs()
            ))),
        },
    }
}

struct Call<'a> {
    env: &'a ToolEnv,
    ctx: &'a ToolCtx,
    ask: &'a Ask,
    slot: &'a Slot,
    session: &'a Arc<dyn EnvSession>,
    language: Language,
    language_id: &'static str,
    rel: &'a Path,
    text: &'a str,
    started: Instant,
}

impl Call<'_> {
    fn redactor(&self) -> &Redactor {
        &self.env.redactor
    }

    /// The path the model gave, as one safe line.
    fn path_shown(&self) -> String {
        safe_name(&self.rel.display().to_string())
    }

    /// The text with this machine's paths to the slot taken out, scrubbed.
    fn clean(&self, text: &str) -> String {
        clean_text(self.redactor(), self.slot, text)
    }

    async fn run(&self) -> Outcome {
        let env = self.env;
        let run = self.ctx.root_run_id().to_string();
        let slot_dir = self.slot.dir().to_owned();
        let spec = env.language_table.get(self.language);
        let root = project_root(self.slot.path(), self.rel, self.language);

        // Is there a server program in this environment? Asked once per run, slot and language.
        match self.probe(spec, &root, &run).await? {
            Probe::Found => {}
            Probe::Missing(why) => return Ok(ToolOutput::text(why)),
        }

        // What a server may do depends on where it runs and on the tier of the run: a reference, and
        // a run at `plan`, are never changed, so the server there is safe (it builds nothing and
        // runs no project code beyond what looking needs) and the slot is held to what it was. A
        // run that may write, in a slot it may write, gets the server `run_checks` there would run.
        let reference = !self.slot.writable();
        let tier = self.tier(&run).await?;
        let mode = if reference || tier == AccessTier::Plan {
            Mode::Safe
        } else {
            Mode::Full
        };
        let before = if mode == Mode::Safe {
            let Some(state) = Snapshot::of_slot(self.slot).await else {
                return Ok(ToolOutput::error(
                    "Cannot read the state of the workspace, so no language server was used. Try \
                     again; if it persists, tell the person.",
                ));
            };
            if let Some(refusal) = state.refusal() {
                return Ok(ToolOutput::error(refusal));
            }
            Some(state)
        } else {
            None
        };

        let want = Want {
            key: Key {
                run: run.clone(),
                slot: slot_dir,
                language: self.language,
                root: root.clone(),
                mode,
            },
            spec,
            session: self.session.clone(),
            mode,
            idle: env.settings.language_server_idle,
            max: env.settings.language_servers_max,
            kill_wait: env.settings.background_kill_wait,
            hide: Vec::new(),
            init_timeout: INITIALIZE_TIMEOUT,
            redactor: env.redactor.clone(),
        };
        let server = match env.commands.language_servers().get_or_start(want).await {
            Ok(server) => server,
            Err(e) => return Ok(self.start_failed(spec, &e)),
        };

        let left = (self.started + self.ask.wait + INITIALIZE_TIMEOUT + EXTRA)
            .saturating_duration_since(Instant::now());
        let Some(mut gate) = server.lock(left).await else {
            return Ok(ToolOutput::error(
                "The language server of this project is busy with another call. Try again shortly.",
            ));
        };
        // What the slot must be after this call: a reference, what it was when the server started
        // (a server may write between calls); a writable slot at `plan`, what it was before this one.
        let mut per_call = None;
        if let Some(state) = before {
            if reference {
                match gate.reference() {
                    None => gate.remember_reference(state),
                    Some(was) if *was != state => {
                        return Ok(self.wrote_to_slot(&server, was).await);
                    }
                    Some(_) => {}
                }
            } else {
                per_call = Some(state);
            }
        }

        let abs = self.slot.path().join(self.rel);
        let target = Target {
            rel: self.rel,
            abs: abs.clone(),
            text: self.text,
            language_id: self.language_id,
        };
        let uri = file_uri(&abs);
        let seq = gate.published_seq(&uri);
        let synced = match gate.sync(self.slot.root(), &target).await {
            Ok(synced) => synced,
            Err(e) => return Ok(self.gone(&server, &e.to_string()).await),
        };
        // The wait starts when the server is up and told what changed: starting it has its own time.
        let deadline = Instant::now() + self.ask.wait;
        let ready = server.ready(deadline).await;

        let answer = match self.ask.op {
            Op::Diagnostics => self.diagnostics(&gate, &uri, seq, synced, deadline).await,
            Op::Definition => self.locations(&server, &uri, deadline, false, true).await,
            Op::References {
                include_declaration,
            } => {
                self.locations(&server, &uri, deadline, include_declaration, false)
                    .await
            }
        };

        // Did the server write to a slot that is not to change? Looked at after the work it did for
        // this call.
        let must_be = if reference {
            gate.reference()
        } else {
            per_call.as_ref()
        };
        if let Some(was) = must_be
            && let Some(now) = Snapshot::of_slot(self.slot).await
            && *was != now
        {
            return Ok(self.wrote_to_slot(&server, was).await);
        }
        drop(gate);

        let (lines_or_text, still) = match answer {
            Answer::Page(items, notes, empty, unit) => {
                let still = !ready || server.indexing();
                match self.page(&items, &notes, empty, unit) {
                    Ok(page) => (page, still),
                    Err(reason) => return Ok(ToolOutput::error(reason)),
                }
            }
            Answer::Text(text, indexing) => (text, indexing || !ready || server.indexing()),
            Answer::Failed(output) => return Ok(output),
            Answer::Gone(why) => return Ok(self.gone(&server, &why).await),
        };
        let mut out = String::new();
        if still {
            out.push_str(&format!(
                "[still indexing after {} s (server up {} s): the result may be incomplete; call \
                 again, the server is kept]\n",
                self.ask.wait.as_secs(),
                server.up().as_secs()
            ));
        }
        out.push_str(&lines_or_text);
        Ok(ToolOutput::text(out))
    }

    fn page(
        &self,
        items: &[String],
        notes: &[String],
        empty: &str,
        unit: &str,
    ) -> Result<String, String> {
        let (offset, limit) = paging::window_args(self.ask.offset, self.ask.limit, PAGE)?;
        let notes: Vec<&str> = notes.iter().map(String::as_str).collect();
        paging::page_items(items, offset, limit, unit, empty, unit, &notes).map_err(|e| match e {
            paging::PageError::PastEnd { offset, total } => {
                format!("there are {total} {unit}: offset {offset} is past the end")
            }
            other => other.to_string(),
        })
    }

    /// The program is there, or what to say when it is not (cached for the run, slot and language
    /// while the environment is the same).
    async fn probe(&self, spec: &ServerSpec, root: &Path, run: &str) -> Result<Probe, ToolError> {
        let described = self.session.describe();
        let servers = self.env.commands.language_servers();
        if let Some(known) = servers.probed(run, self.slot.dir(), self.language, &described.summary)
        {
            return Ok(known);
        }
        let program = sh_quote(&spec.program);
        let mut command = format!("command -v {program} >/dev/null 2>&1 || exit 127");
        if spec.version_probe {
            command.push_str(&format!("; {program} --version"));
        }
        let outcome = self
            .env
            .run(
                self.ctx,
                &**self.session,
                shell_spec(root, &command),
                PROBE_TIMEOUT,
                1024,
                false,
            )
            .await
            .map_err(|e| run_error(self.redactor(), &e))?;
        let probe = if outcome.passed() {
            Probe::Found
        } else if outcome.exit_code == Some(127) && !outcome.timed_out {
            Probe::Missing(format!(
                "No language server for {} in this environment (`{}` is not on PATH in {}). Use \
                 `run_checks` or `run_command` with {} instead.",
                self.language.name(),
                shown(&spec.program),
                described.summary,
                instead(self.language)
            ))
        } else {
            let said = self.clean(outcome.tail.trim());
            Probe::Missing(format!(
                "No language server for {} in this environment: `{} --version` did not run in {} \
                 ({}). Use `run_checks` or `run_command` with {} instead.",
                self.language.name(),
                shown(&spec.program),
                self.slot.dir(),
                if outcome.timed_out {
                    "it took longer than 20 s".to_owned()
                } else {
                    said
                },
                instead(self.language)
            ))
        };
        servers.remember_probe(
            run,
            self.slot.dir(),
            self.language,
            &described.summary,
            probe.clone(),
        );
        Ok(probe)
    }

    fn start_failed(&self, spec: &ServerSpec, e: &StartError) -> ToolOutput {
        let text = match e {
            StartError::Prepare(e) => format!(
                "The language server `{}` could not be prepared: {}",
                shown(&spec.program),
                adam_error::report(e)
            ),
            StartError::Spawn(e) => format!(
                "The language server `{}` could not be started: {e}",
                shown(&spec.program)
            ),
            StartError::Failed(why) => format!(
                "The language server `{}` did not start: {why}",
                shown(&spec.program)
            ),
            StartError::Busy(max) => format!(
                "This worker holds {max} language servers (LANGUAGE_SERVERS_MAX) and every one is \
                 working for a call: try again shortly."
            ),
            StartError::Stopped => "The language servers of this run were stopped while this one \
                 was starting (the run parked, or its environment was made again): call again."
                .to_owned(),
        };
        ToolOutput::error(self.clean(&text))
    }

    /// The server ended, or sent what cannot be read: why, the end of its stderr, and that the
    /// next call starts it again.
    async fn gone(&self, server: &Server, why: &str) -> ToolOutput {
        server.stop("it ended").await;
        let mut text = format!(
            "The language server `{}` stopped: {}.",
            shown(server.program()),
            self.clean(why)
        );
        if let Some(end) = server.end_status() {
            text.push_str(&format!(" ({})", self.clean(&end)));
        }
        let tail = server.stderr_tail(self.redactor());
        if !tail.is_empty() {
            text.push_str(&format!(
                "\nThe end of its stderr:\n{}",
                self.clean(&last_chars(&tail, 4000))
            ));
        }
        text.push_str("\nThe next call starts it again.");
        ToolOutput::error(text)
    }

    /// The access tier the run works under, as its notes say (`plan` when none were written).
    async fn tier(&self, run: &str) -> Result<AccessTier, ToolError> {
        Ok(self
            .env
            .notes
            .load_existing(run)
            .await
            .map_err(|e| notes_error(&e))?
            .map_or(AccessTier::Plan, |notes| self.env.tier_of(&notes)))
    }

    /// A server wrote into a slot that is not to change: put it back, stop the server, say so.
    async fn wrote_to_slot(&self, server: &Server, baseline: &Snapshot) -> ToolOutput {
        let restored = baseline.restore(self.slot).await;
        server.stop("it changed files it must not").await;
        let why = if self.slot.writable() {
            "and this run is at the `plan` tier, where nothing is changed"
        } else {
            "which is a reference (read, never changed)"
        };
        ToolOutput::error(format!(
            "The language server changed files of `{}`, {why}. {} The server was stopped and its \
             answer dropped; the next call starts it again. Use run_checks or run_command to see \
             what the compiler says.",
            safe_name(self.slot.dir()),
            if restored {
                "The files are as they were."
            } else {
                "They could not all be put back: tell the person."
            }
        ))
    }

    async fn diagnostics(
        &self,
        gate: &server::Gate<'_>,
        uri: &str,
        seq: u64,
        synced: Synced,
        deadline: Instant,
    ) -> Answer {
        let server = gate.server();
        let items = match gate.diagnostics(uri, seq, synced, deadline).await {
            Ok(Diagnostics::Items(items)) => items,
            Ok(Diagnostics::Nothing(waited)) => {
                // Not an empty set: the server said nothing, so nothing is known of the file.
                return Answer::Text(
                    format!(
                        "No diagnostics received for `{}` within {} s: the server published none \
                         for it (it may not have looked at the file yet, or it publishes only when \
                         there is something to say). Call again, with a longer `wait` if it was \
                         still starting, or use run_checks or run_command with {}.",
                        self.path_shown(),
                        waited.as_secs_f64().ceil(),
                        instead(self.language)
                    ),
                    server.indexing(),
                );
            }
            Err(Reply::Indexing) => {
                return Answer::Text(
                    format!(
                        "No diagnostics yet for `{}`: the server is still indexing.",
                        self.path_shown()
                    ),
                    true,
                );
            }
            Err(other) => return self.failed(server, other),
        };
        let lines: Vec<&str> = split_lines(self.text);
        let enc = server.encoding();
        let mut found: Vec<(u32, u32, String, bool)> = Vec::new();
        for item in &items {
            let severity = item.get("severity").and_then(Value::as_i64).unwrap_or(1);
            if !(1..=2).contains(&severity) {
                continue;
            }
            let line = item
                .pointer("/range/start/line")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0);
            let character = item
                .pointer("/range/start/character")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0);
            let source_line = lines.get(line as usize).copied().unwrap_or_default();
            let col = to_chars(source_line, character, enc) + 1;
            let message = item
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let message = cut_chars(&self.clean(message), MAX_MESSAGE_CHARS);
            let code = match item.get("code") {
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                Some(Value::Number(n)) => Some(n.to_string()),
                _ => None,
            };
            let source = item
                .get("source")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let mut tags = String::new();
            if let Some(code) = code {
                tags.push_str(&format!("[{}]", cut_chars(&self.clean(&code), 60)));
            }
            if let Some(source) = source {
                if !tags.is_empty() {
                    tags.push(' ');
                }
                tags.push_str(&format!("({})", cut_chars(&self.clean(source), 60)));
            }
            let kind = if severity == 2 { "warning" } else { "error" };
            let mut line_text = format!(
                "{}:{}:{col}: {kind}: {}",
                self.path_shown(),
                line + 1,
                paging::escape_text(message.as_bytes()),
            );
            if !tags.is_empty() {
                line_text.push(' ');
                line_text.push_str(&paging::escape_text(tags.as_bytes()));
            }
            found.push((
                line,
                u32::try_from(col).unwrap_or(u32::MAX),
                line_text,
                severity == 2,
            ));
        }
        found.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
        let warnings = found.iter().filter(|f| f.3).count();
        let errors = found.len() - warnings;
        let lines: Vec<String> = found.into_iter().map(|f| f.2).collect();
        let notes = vec![format!(
            "{errors} error{}, {warnings} warning{} of {}",
            if errors == 1 { "" } else { "s" },
            if warnings == 1 { "" } else { "s" },
            shown(server.program())
        )];
        Answer::Page(lines, notes, "(no errors or warnings)", "diagnostics")
    }

    fn failed(&self, server: &Server, reply: Reply) -> Answer {
        match reply {
            Reply::Hung(why) => Answer::Failed(self.hung(server, &why)),
            Reply::Died(why) => Answer::Gone(why),
            Reply::Refused { code, message } => Answer::Failed(ToolOutput::error(format!(
                "The language server refused the request ({code}): {}",
                cut_chars(&self.clean(&message), MAX_MESSAGE_CHARS)
            ))),
            Reply::Indexing => {
                Answer::Text("No answer yet: the server is still indexing.".into(), true)
            }
            Reply::Done(_) => Answer::Text(String::new(), false),
        }
    }

    fn hung(&self, server: &Server, why: &str) -> ToolOutput {
        let mut text = format!(
            "The language server `{}` stopped: {}.",
            shown(server.program()),
            self.clean(why)
        );
        let tail = server.stderr_tail(self.redactor());
        if !tail.is_empty() {
            text.push_str(&format!(
                "\nThe end of its stderr:\n{}",
                self.clean(&last_chars(&tail, 4000))
            ));
        }
        text.push_str("\nThe next call starts it again.");
        ToolOutput::error(text)
    }

    /// `textDocument/definition` or `textDocument/references` at the symbol the model named.
    async fn locations(
        &self,
        server: &Server,
        uri: &str,
        deadline: Instant,
        include_declaration: bool,
        definition: bool,
    ) -> Answer {
        let Some((line, symbol, occurrence)) = self.ask.position.clone() else {
            return Answer::Text(String::new(), false);
        };
        let lines: Vec<&str> = split_lines(self.text);
        let index = usize::try_from(line - 1).unwrap_or(usize::MAX);
        let Some(source_line) = lines.get(index).copied() else {
            return Answer::Failed(ToolOutput::error(format!(
                "`{}` has {} lines: line {line} is past the end",
                self.path_shown(),
                lines.len()
            )));
        };
        let at = match find_symbol(source_line, &symbol, occurrence) {
            Found::At(at) => at,
            Found::Missing => {
                return Answer::Failed(ToolOutput::error(format!(
                    "`{}` is not on line {line} of `{}`. The line reads: {}",
                    cut_chars(&self.clean(&symbol), MAX_SYMBOL_CHARS),
                    self.path_shown(),
                    paging::escape_text(
                        cut_chars(&self.clean(source_line), MAX_SOURCE_CHARS * 2).as_bytes()
                    )
                )));
            }
            Found::NotAsWord => {
                return Answer::Failed(ToolOutput::error(format!(
                    "`{}` is not on line {line} of `{}` as a word (it is there only inside longer \
                     words). The line reads: {}",
                    cut_chars(&self.clean(&symbol), MAX_SYMBOL_CHARS),
                    self.path_shown(),
                    paging::escape_text(
                        cut_chars(&self.clean(source_line), MAX_SOURCE_CHARS * 2).as_bytes()
                    )
                )));
            }
            Found::Fewer(n) => {
                return Answer::Failed(ToolOutput::error(format!(
                    "`{}` is on line {line} of `{}` {n} time{}, not {occurrence}. The line reads: {}",
                    cut_chars(&self.clean(&symbol), MAX_SYMBOL_CHARS),
                    self.path_shown(),
                    if n == 1 { "" } else { "s" },
                    paging::escape_text(
                        cut_chars(&self.clean(source_line), MAX_SOURCE_CHARS * 2).as_bytes()
                    )
                )));
            }
        };
        let enc = server.encoding();
        let position = json!({"line": line - 1, "character": to_server(source_line, at, enc)});
        let (method, params) = if definition {
            (
                "textDocument/definition",
                json!({"textDocument": {"uri": uri}, "position": position}),
            )
        } else {
            (
                "textDocument/references",
                json!({
                    "textDocument": {"uri": uri}, "position": position,
                    "context": {"includeDeclaration": include_declaration},
                }),
            )
        };
        let value = match server.ask(method, params, deadline).await {
            Reply::Done(value) => value,
            Reply::Indexing => {
                return Answer::Text(
                    format!(
                        "No answer yet for `{}`: the server is still indexing.",
                        safe_line(&symbol)
                    ),
                    true,
                );
            }
            // The server was asked while it worked and said so: not an error.
            Reply::Refused { code, .. }
                if (-32802..=-32800).contains(&code) && server.indexing() =>
            {
                return Answer::Text(
                    format!(
                        "No answer yet for `{}`: the server is still indexing.",
                        safe_line(&symbol)
                    ),
                    true,
                );
            }
            other => return self.failed(server, other),
        };
        let mut all = locations_of(&value);
        let total = all.len();
        let (cap, unit, empty) = if definition {
            (MAX_DEFINITIONS, "definitions", "(no definition found)")
        } else {
            (MAX_COLLECTED, "references", "(no references found)")
        };
        let cut = total > cap;
        all.truncate(cap);
        let mapper = Mapper::new(self.env, self.ctx, self.slot).await;
        let redactor = self.redactor().clone();
        let strip = path_strips(self.slot);
        let rows = tokio::task::spawn_blocking(move || {
            format_locations(&mapper, &redactor, &strip, all, enc)
        })
        .await
        .unwrap_or_default();
        let mut notes = Vec::new();
        if cut {
            notes.push(format!(
                "the server returned {total}; only the first {cap} are listed"
            ));
        }
        Answer::Page(rows, notes, empty, unit)
    }
}

/// What a call made of the server's answer.
enum Answer {
    /// Lines to page, the notes after the end line, what an empty page says, the unit.
    Page(Vec<String>, Vec<String>, &'static str, &'static str),
    /// A text as it is, and whether the server was still indexing.
    Text(String, bool),
    /// The call's result already.
    Failed(ToolOutput),
    /// The server ended: why.
    Gone(String),
}

/// What `text` says with the paths of this machine to the slot taken out, scrubbed.
fn clean_text(redactor: &Redactor, slot: &Slot, text: &str) -> String {
    let mut text = redactor.scrub(text).into_owned();
    for prefix in path_strips(slot) {
        text = text.replace(&prefix, "");
    }
    listing_text(&hide_foreign_paths(&text))
}

/// `text` with every path of another place (a run of non-space characters that starts with `/` and
/// has two names in it: `/usr/local/go/src/fmt/print.go:12:3`) written as `(outside the workspace)
/// <its last three names>`, as locations are. **Best effort**: a path a server writes in another way
/// (with a space in it, or without its first `/`) is not found.
fn hide_foreign_paths(text: &str) -> String {
    let starts = |c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '(' | '[' | '{' | '<' | '"' | '\'' | '`' | ',' | ';' | '='
            )
    };
    let ends = |c: char| {
        c.is_whitespace() || matches!(c, ')' | ']' | '}' | '>' | '"' | '\'' | '`' | ',' | ';')
    };
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < chars.len() {
        if chars[at] == '/' && (at == 0 || starts(chars[at - 1])) {
            let mut end = at;
            while end < chars.len() && !ends(chars[end]) {
                end += 1;
            }
            let token: String = chars[at..end].iter().collect();
            let names = Path::new(&token)
                .components()
                .filter(|c| matches!(c, Component::Normal(_)))
                .count();
            if names >= 2 {
                out.push_str("(outside the workspace) ");
                out.push_str(&last_components(Path::new(&token)));
                at = end;
                continue;
            }
        }
        out.push(chars[at]);
        at += 1;
    }
    out
}

/// `text` as one safe line: a control character is `\xNN`, a direction control or a line separator
/// is written out (`\u{202e}`), anything else is as it is.
fn safe_line(text: &str) -> String {
    listing_text(&paging::escape_text(text.as_bytes()))
}

/// [`safe_line`] for the name of a file.
fn safe_name(text: &str) -> String {
    listing_text(&paging::escape_name(text.as_bytes()))
}

/// The prefixes of this machine's paths to `slot`, each with its closing `/`.
fn path_strips(slot: &Slot) -> Vec<String> {
    let mut strips = Vec::new();
    let path = slot.path();
    if let Ok(canon) = std::fs::canonicalize(path) {
        strips.push(format!("{}/", canon.display()));
    }
    strips.push(format!("{}/", path.display()));
    strips.sort_by_key(|s| std::cmp::Reverse(s.len()));
    strips.dedup();
    strips
}

/// The last `max` characters of `text`, with an ellipsis before them when it was longer.
fn last_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().skip(count - max).collect();
    format!("…{kept}")
}

/// `text` cut at `max` characters, with an ellipsis.
fn cut_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max).collect();
    cut.push('…');
    cut
}

/// A place the server named.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Loc {
    uri: String,
    line: u32,
    character: u32,
}

/// The locations an answer holds: a `Location`, a list of them, or a list of `LocationLink`s.
fn locations_of(value: &Value) -> Vec<Loc> {
    let one = |v: &Value| -> Option<Loc> {
        let (uri, position) = match v.get("targetUri") {
            Some(uri) => (
                uri.as_str()?,
                v.pointer("/targetSelectionRange/start")
                    .or_else(|| v.pointer("/targetRange/start"))?,
            ),
            None => (v.get("uri")?.as_str()?, v.pointer("/range/start")?),
        };
        Some(Loc {
            uri: uri.to_owned(),
            line: u32::try_from(position.get("line")?.as_u64()?).ok()?,
            character: u32::try_from(position.get("character")?.as_u64()?).ok()?,
        })
    };
    match value {
        Value::Array(items) => items.iter().filter_map(one).collect(),
        Value::Object(_) => one(value).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// The slots of the run, to say where a location is and to read its line.
struct Mapper {
    slots: Vec<MappedSlot>,
    current: usize,
}

struct MappedSlot {
    dir: String,
    path: PathBuf,
    canon: PathBuf,
    root: Arc<SlotDir>,
}

/// Where a location is.
enum Place {
    In { slot: usize, rel: PathBuf },
    Outside(PathBuf),
}

impl Mapper {
    async fn new(env: &ToolEnv, ctx: &ToolCtx, current: &Slot) -> Self {
        let slots = match env.workspaces.run(&ctx.root_run_id().to_string()) {
            Ok(workspace) => workspace.slots().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let mut mapped: Vec<MappedSlot> = slots
            .iter()
            .map(|slot| MappedSlot {
                dir: slot.dir().to_owned(),
                path: slot.path().to_path_buf(),
                canon: std::fs::canonicalize(slot.path())
                    .unwrap_or_else(|_| slot.path().to_path_buf()),
                root: Arc::clone(slot.root()),
            })
            .collect();
        let at = mapped.iter().position(|m| m.dir == current.dir());
        let at = at.unwrap_or_else(|| {
            mapped.push(MappedSlot {
                dir: current.dir().to_owned(),
                path: current.path().to_path_buf(),
                canon: std::fs::canonicalize(current.path())
                    .unwrap_or_else(|_| current.path().to_path_buf()),
                root: Arc::clone(current.root()),
            });
            mapped.len() - 1
        });
        Self {
            slots: mapped,
            current: at,
        }
    }

    fn place(&self, abs: &Path) -> Place {
        if abs
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Place::Outside(abs.to_path_buf());
        }
        let order = std::iter::once(self.current)
            .chain((0..self.slots.len()).filter(|i| *i != self.current));
        for at in order {
            let slot = &self.slots[at];
            for base in [&slot.canon, &slot.path] {
                if let Ok(rel) = abs.strip_prefix(base)
                    && !rel.as_os_str().is_empty()
                {
                    return Place::In {
                        slot: at,
                        rel: rel.to_path_buf(),
                    };
                }
            }
        }
        Place::Outside(abs.to_path_buf())
    }

    fn shown(&self, slot: usize, rel: &Path) -> String {
        if slot == self.current {
            rel.display().to_string()
        } else {
            format!("{}/{}", self.slots[slot].dir, rel.display())
        }
    }
}

/// The last three components of `path`, for a place that is not in the workspace.
fn last_components(path: &Path) -> String {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let from = parts.len().saturating_sub(3);
    parts[from..].join("/")
}

/// One line for each location, sorted: `path:line:col: source`.
fn format_locations(
    mapper: &Mapper,
    redactor: &Redactor,
    strip: &[String],
    locations: Vec<Loc>,
    enc: Encoding,
) -> Vec<String> {
    let mut lines_of: std::collections::HashMap<(usize, PathBuf), Option<Vec<String>>> =
        std::collections::HashMap::new();
    let mut read_files = 0;
    let mut rows: Vec<(String, u32, u32, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for loc in locations {
        if !seen.insert((loc.uri.clone(), loc.line, loc.character)) {
            continue;
        }
        let Some(abs) = path_of_uri(&loc.uri) else {
            rows.push((
                "~".to_owned(),
                loc.line,
                loc.character,
                format!(
                    "(outside the workspace) (a location that is not a file):{}",
                    loc.line + 1
                ),
            ));
            continue;
        };
        match mapper.place(&abs) {
            Place::Outside(path) => rows.push((
                format!("~{}", last_components(&path)),
                loc.line,
                loc.character,
                format!(
                    "(outside the workspace) {}:{}",
                    safe_line(&last_components(&path)),
                    loc.line + 1
                ),
            )),
            Place::In { slot, rel } => {
                let key = (slot, rel.clone());
                if !lines_of.contains_key(&key) {
                    let lines = if read_files < MAX_SOURCE_FILES {
                        read_files += 1;
                        server::read_text(&mapper.slots[slot].root, &rel)
                            .ok()
                            .map(|text| split_lines(&text).into_iter().map(str::to_owned).collect())
                    } else {
                        None
                    };
                    lines_of.insert(key.clone(), lines);
                }
                let source = lines_of
                    .get(&key)
                    .and_then(Option::as_ref)
                    .and_then(|lines| lines.get(loc.line as usize));
                let (col, shown_source) = match source {
                    Some(line) => (to_chars(line, loc.character, enc) + 1, Some(line.as_str())),
                    None => (loc.character as usize + 1, None),
                };
                let name = mapper.shown(slot, &rel);
                let mut text = format!("{}:{}:{col}", safe_name(&name), loc.line + 1);
                if let Some(source) = shown_source {
                    let mut said = redactor.scrub(source.trim()).into_owned();
                    for prefix in strip {
                        said = said.replace(prefix.as_str(), "");
                    }
                    let said = cut_chars(&said, MAX_SOURCE_CHARS);
                    text.push_str(": ");
                    text.push_str(&safe_line(&said));
                }
                rows.push((name, loc.line, u32::try_from(col).unwrap_or(u32::MAX), text));
            }
        }
    }
    rows.sort_by(|a, b| (&a.0, a.1, a.2).cmp(&(&b.0, b.1, b.2)));
    rows.into_iter().map(|r| r.3).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_project_root_is_the_nearest_marked_directory_below_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path();
        std::fs::create_dir_all(slot.join("crates/a/src")).unwrap();
        std::fs::write(slot.join("Cargo.toml"), "[workspace]").unwrap();
        std::fs::write(slot.join("crates/a/Cargo.toml"), "[package]").unwrap();
        let root = |rel: &str, l| project_root(slot, Path::new(rel), l);
        assert_eq!(
            root("crates/a/src/lib.rs", Language::Rust),
            slot.join("crates/a")
        );
        assert_eq!(root("top.rs", Language::Rust), slot);
        // No marker of the language: the slot's root.
        assert_eq!(root("crates/a/src/x.go", Language::Go), slot);
        std::fs::write(slot.join("crates/a/go.mod"), "module a").unwrap();
        assert_eq!(
            root("crates/a/src/x.go", Language::Go),
            slot.join("crates/a")
        );
    }

    #[test]
    fn wait_and_position_are_checked() {
        assert_eq!(wait_of(None).unwrap(), Duration::from_secs(30));
        assert_eq!(wait_of(Some(120)).unwrap(), Duration::from_secs(120));
        assert!(wait_of(Some(0)).unwrap_err().contains("1 to 120"));
        assert!(wait_of(Some(121)).is_err());
        assert!(position_of(0, "x", None).is_err());
        assert!(position_of(1, "  ", None).is_err());
        assert!(position_of(1, "a\nb", None).is_err());
        assert!(position_of(1, "x", Some(0)).is_err());
        assert_eq!(
            position_of(3, " name ", Some(2)).unwrap(),
            (3, "name".to_owned(), 2)
        );
    }

    #[test]
    fn locations_come_as_one_a_list_or_links() {
        let at = |line, character| json!({"start": {"line": line, "character": character}});
        let one = json!({"uri": "file:///a.rs", "range": at(1, 2)});
        assert_eq!(locations_of(&one).len(), 1);
        let link = json!([{"targetUri": "file:///b.rs", "targetRange": at(0, 0), "targetSelectionRange": at(4, 5)}]);
        assert_eq!(
            locations_of(&link),
            vec![Loc {
                uri: "file:///b.rs".into(),
                line: 4,
                character: 5
            }]
        );
        assert!(locations_of(&Value::Null).is_empty());
        assert!(locations_of(&json!([{"bad": 1}])).is_empty());
    }

    #[test]
    fn a_text_is_cut_on_a_character_and_marked() {
        assert_eq!(cut_chars("abc", 3), "abc");
        assert_eq!(cut_chars("éééé", 2), "éé…");
    }

    #[test]
    fn a_program_is_quoted_for_the_shell() {
        assert_eq!(sh_quote("rust-analyzer"), "'rust-analyzer'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn the_time_limit_beyond_the_environment_is_wait_initialize_and_the_rest() {
        assert_eq!(BEYOND_ENVIRONMENT, Duration::from_secs(120 + 60 + 60));
    }
}
