//! Conformance cases for an implementation of `adam_code_tools::CodeWorkspace`, and a reference
//! implementation over directories ([`DirWorkspace`]).
//!
//! An implementation's test builds a [`Harness`] (the workspace, a tool context it answers for,
//! the name of a place a tool may write and optionally of one it may only read, and a secret its
//! redactor holds) and runs [`code_workspace_conformance!`] over it:
//!
//! ```ignore
//! adam_code_tools_testkit::code_workspace_conformance!(my_harness().await);
//! ```
//!
//! An implementation of `adam_code_tools::CommandWorkspace` also builds a [`CommandHarness`] and
//! runs [`command_workspace_conformance!`] over it ([`command_cases`]).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use adam_code_tools::git::GitAt;
use adam_code_tools::sandbox::Meter;
use adam_code_tools::{
    CodeWorkspace, CommandLimits, CommandWorkspace, DynCodeWorkspace, DynCommandWorkspace, Intent,
    Outcome, Place, Redactor,
};
use adam_llm_agent::{KeptOutputs, MemoryKept, ToolCtx, ToolError, ToolOutput};
use adam_runtime::CollectingSink;
use adam_workspace::{EnvSession, LocalSession, SlotDir};
use async_trait::async_trait;

/// What the cases need of an implementation.
pub struct Harness {
    /// The workspace under test.
    pub workspace: DynCodeWorkspace,
    /// A context the workspace answers for (its run, its state).
    pub ctx: ToolCtx,
    /// The name of the place a tool may write: the only one, so it is also the place a tool works
    /// in when it names none.
    pub writable: String,
    /// The name of a place a tool may only read, if the implementation has such places.
    pub read_only: Option<String>,
    /// A value the workspace's redactor holds, at least four bytes long.
    pub secret: String,
    /// What must live as long as the harness (a temporary directory, a fixture); `Box::new(())`
    /// when nothing does.
    pub alive: Box<dyn std::any::Any + Send>,
}

/// What the command cases need of an implementation of `CommandWorkspace`.
pub struct CommandHarness {
    /// The workspace under test.
    pub workspace: DynCommandWorkspace,
    /// A context the workspace answers for (its run, its state).
    pub ctx: ToolCtx,
    /// The name of a place a tool may write, where the commands run.
    pub writable: String,
    /// A value the workspace's redactor holds, at least four bytes long, with no `'` in it (the
    /// cases put it in a shell command).
    pub secret: String,
    /// What must live as long as the harness; `Box::new(())` when nothing does.
    pub alive: Box<dyn std::any::Any + Send>,
}

/// A tool context detached from any run, for a harness.
pub fn detached_ctx() -> ToolCtx {
    ToolCtx::detached("conformance", "call-1", Arc::new(CollectingSink::new()))
}

/// The conformance cases, each an `async fn(&Harness)` that panics on a failure.
pub mod cases {
    use std::sync::Arc;

    use super::{Harness, Intent};

    /// A name the model writes for a place that does not exist.
    const UNKNOWN: &str = "no-such-place-7c41";

    /// The writable place is given for a read and for a write, under its name, over a directory
    /// that exists.
    pub async fn a_writable_place_is_given_for_a_read_and_a_write(h: &Harness) {
        for intent in [Intent::Read, Intent::Write] {
            let place = match h.workspace.place(&h.ctx, Some(&h.writable), intent).await {
                Ok(place) => place,
                Err(answer) => panic!("{intent:?} of {:?}: {answer:?}", h.writable),
            };
            assert!(place.is_writable(), "{intent:?}");
            assert_eq!(place.name(), h.writable, "{intent:?}");
            assert!(
                place.path().is_dir(),
                "{intent:?}: {}",
                place.path().display()
            );
            // The directory a tool checks a path against is the one `git` is asked in.
            assert!(Arc::ptr_eq(place.root(), place.git().root()), "{intent:?}");
        }
    }

    /// With no name, a tool works in the only place it may write, for a read and for a write: a
    /// place that may only be read is used when it is named, never by default.
    pub async fn no_name_is_the_place_a_tool_may_write(h: &Harness) {
        for intent in [Intent::Read, Intent::Write] {
            match h.workspace.place(&h.ctx, None, intent).await {
                Ok(place) => assert_eq!(place.name(), h.writable, "{intent:?}"),
                Err(answer) => panic!("{intent:?} with no name: {answer:?}"),
            }
        }
    }

    /// A name that names no place is an answer for the model (an error result), never a
    /// `ToolError`, so the model can pick another.
    pub async fn an_unknown_name_is_an_answer_for_the_model(h: &Harness) {
        for intent in [Intent::Read, Intent::Write] {
            match h.workspace.place(&h.ctx, Some(UNKNOWN), intent).await {
                Err(Ok(output)) => assert!(output.is_error, "{intent:?}: {output:?}"),
                other => panic!("{intent:?}: {other:?}"),
            }
        }
    }

    /// A place a tool may only read is given for a read, not writable, and never for a write: a
    /// write is an answer for the model. A harness with no such place passes it with nothing checked.
    pub async fn a_read_only_place_is_never_given_for_a_write(h: &Harness) {
        let Some(name) = &h.read_only else { return };
        match h.workspace.place(&h.ctx, Some(name), Intent::Read).await {
            Ok(place) => assert!(!place.is_writable(), "{name}"),
            Err(answer) => panic!("a read of {name}: {answer:?}"),
        }
        match h.workspace.place(&h.ctx, Some(name), Intent::Write).await {
            Err(Ok(output)) => assert!(output.is_error, "{output:?}"),
            other => panic!("a write of {name}: {other:?}"),
        }
    }

    /// What a tool returns through the redactor has no secret in it.
    pub async fn the_redactor_scrubs_the_secret(h: &Harness) {
        let text = format!("token={} done", h.secret);
        let scrubbed = h.workspace.redactor().scrub(&text);
        // On a failure the message says how long the secret is, never what it is.
        assert!(
            !scrubbed.contains(&h.secret),
            "the redactor does not hold the harness's secret ({} bytes)",
            h.secret.len()
        );
    }

    /// What follows a write ([`after_write`](adam_code_tools::CodeWorkspace::after_write)) on the
    /// writable place, twice: each call is `Ok`, its text is empty or starts with a space or a line
    /// break (it is appended to a sentence), and none holds the secret.
    pub async fn a_write_is_followed_by_the_hook(h: &Harness) {
        let place = match h
            .workspace
            .place(&h.ctx, Some(&h.writable), Intent::Write)
            .await
        {
            Ok(place) => place,
            Err(answer) => panic!("a write of {:?}: {answer:?}", h.writable),
        };
        let paths = ["conformance-after-write.txt".to_owned()];
        for call in 1..=2 {
            let tail = match h.workspace.after_write(&h.ctx, &place, &paths).await {
                Ok(tail) => tail,
                Err(e) => panic!("call {call}: {e:?}"),
            };
            assert!(
                tail.is_empty() || tail.starts_with(' ') || tail.starts_with('\n'),
                "call {call}: {tail:?}"
            );
            // The hook is given nothing that holds the secret: this guards an implementation that
            // writes a secret of its own into the text. A failure says the secret's length, not it.
            assert!(
                !tail.contains(&h.secret),
                "call {call}: the text holds the harness's secret ({} bytes)",
                h.secret.len()
            );
        }
    }
}

/// One `#[tokio::test]` per case of [`cases`], each over a harness built by `$harness` (an
/// expression, awaited inside the test when it is a future).
#[macro_export]
macro_rules! code_workspace_conformance {
    ($harness:expr) => {
        $crate::code_workspace_conformance!(@case $harness, a_writable_place_is_given_for_a_read_and_a_write);
        $crate::code_workspace_conformance!(@case $harness, no_name_is_the_place_a_tool_may_write);
        $crate::code_workspace_conformance!(@case $harness, an_unknown_name_is_an_answer_for_the_model);
        $crate::code_workspace_conformance!(@case $harness, a_read_only_place_is_never_given_for_a_write);
        $crate::code_workspace_conformance!(@case $harness, the_redactor_scrubs_the_secret);
        $crate::code_workspace_conformance!(@case $harness, a_write_is_followed_by_the_hook);
    };
    (@case $harness:expr, $case:ident) => {
        #[tokio::test]
        async fn $case() {
            let harness = $harness;
            $crate::cases::$case(&harness).await;
        }
    };
}

/// The conformance cases of a `CommandWorkspace`, each an `async fn(&CommandHarness)` that panics on
/// a failure. A command runs through `adam_code_tools::commands::run` in the writable place, and
/// what it printed is settled by `adam_code_tools::output::settle`, as the command tools do.
pub mod command_cases {
    use adam_code_tools::redact::REDACTED;
    use adam_code_tools::shell::{ShellOutcome, shell_spec};
    use adam_code_tools::{Place, commands, output};

    use super::{CommandHarness, Intent};

    /// The writable place of the harness, for a write.
    async fn writable(h: &CommandHarness) -> Place {
        match h
            .workspace
            .place(&h.ctx, Some(&h.writable), Intent::Write)
            .await
        {
            Ok(place) => place,
            Err(answer) => panic!("a write of {:?}: {answer:?}", h.writable),
        }
    }

    /// `command` run in the writable place within the workspace's limits; `keep`: its output is
    /// captured, and settled.
    async fn run(h: &CommandHarness, command: &str, keep: bool) -> ShellOutcome {
        let place = writable(h).await;
        let session = match h.workspace.session(&h.ctx).await {
            Ok(session) => session,
            Err(e) => panic!("no session: {e:?}"),
        };
        let limits = h.workspace.limits();
        let spec = shell_spec(place.path(), command);
        let mut outcome = match commands::run(
            &*h.workspace,
            &h.ctx,
            &*session,
            spec,
            limits.timeout,
            limits.output_tail,
            keep,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => panic!("{command:?} did not run: {e:?}"),
        };
        if keep {
            output::settle(&*h.workspace, &h.ctx, &mut outcome).await;
        }
        outcome
    }

    /// The secret, quoted for the shell.
    fn quoted(h: &CommandHarness) -> String {
        assert!(
            !h.secret.contains('\''),
            "the harness's secret holds a quote"
        );
        format!("'{}'", h.secret)
    }

    /// A command runs in the writable place: a file it writes appears there.
    pub async fn a_command_runs_in_the_writable_place(h: &CommandHarness) {
        const NAME: &str = "conformance-ran.txt";
        let place = writable(h).await;
        let outcome = run(h, &format!("printf ran > {NAME}"), false).await;
        assert_eq!(outcome.exit_code, Some(0), "{outcome:?}");
        let written = place.path().join(NAME);
        let text = std::fs::read_to_string(&written);
        let _ = std::fs::remove_file(&written);
        assert_eq!(text.ok().as_deref(), Some("ran"), "{}", written.display());
    }

    /// A long output that holds the secret is shown as a head and a tail and kept, both scrubbed:
    /// the kept text read back has the redaction and never the secret.
    pub async fn a_long_output_is_shown_and_kept_scrubbed(h: &CommandHarness) {
        let tail = h.workspace.limits().output_tail;
        // A line is at least 18 bytes once scrubbed (`line 1 [redacted]`), and the view is cut by
        // the scrubbed length: four times the view.
        let lines = tail / 4 + 100;
        let command = format!(
            "i=0; while [ $i -lt {lines} ]; do i=$((i+1)); printf 'line %s %s\\n' $i {}; done",
            quoted(h)
        );
        let outcome = run(h, &command, true).await;
        assert_eq!(outcome.exit_code, Some(0), "{:?}", outcome.tail);
        assert!(outcome.truncated, "{lines} lines fit {tail} bytes");
        // A failure says the secret's length, never the secret.
        let leak = |what: &str, text: &str| {
            assert!(
                !text.contains(&h.secret),
                "{what} holds the harness's secret ({} bytes)",
                h.secret.len()
            );
        };
        leak("the head", &outcome.head);
        leak("the tail", &outcome.tail);
        assert!(outcome.tail.contains(REDACTED), "{:?}", outcome.tail);
        let Some(note) = outcome.kept.as_ref() else {
            panic!("a long output is not kept");
        };
        assert_eq!(note.refused, None);
        let view = output::section(&outcome);
        leak("the view", &view);
        assert!(view.contains(&note.id), "{view}");
        let run = h.ctx.root_run_id().to_string();
        let mut read = String::new();
        let mut offset = 1;
        while offset <= note.lines {
            let page = match h
                .workspace
                .kept()
                .page(
                    &run,
                    &note.id,
                    i64::try_from(offset).unwrap_or(i64::MAX),
                    500,
                )
                .await
            {
                Ok(page) => page,
                Err(e) => panic!("the page at {offset} of {}: {e}", note.id),
            };
            read.push_str(&page);
            offset += 500;
        }
        leak("the kept text", &read);
        assert!(read.contains(REDACTED), "the kept text has no {REDACTED}");
        assert!(
            read.contains(&format!("line {lines} ")),
            "the kept text is cut"
        );
    }

    /// `timeout_of` refuses 0 and a timeout over the workspace's limit, and gives the limit when
    /// none is asked for.
    pub async fn timeout_of_keeps_to_the_limit(h: &CommandHarness) {
        let limits = h.workspace.limits();
        let limit = i64::try_from(limits.timeout.as_secs()).unwrap_or(i64::MAX);
        assert_eq!(output::timeout_of(&limits, None), Ok(limits.timeout));
        assert!(output::timeout_of(&limits, Some(0)).is_err());
        assert!(output::timeout_of(&limits, Some(-1)).is_err());
        assert!(output::timeout_of(&limits, Some(limit.saturating_add(1))).is_err());
        if limit >= 1 {
            assert_eq!(
                output::timeout_of(&limits, Some(limit)),
                Ok(std::time::Duration::from_secs(limits.timeout.as_secs()))
            );
        }
    }

    /// A short output is shown whole, scrubbed, and nothing is kept.
    pub async fn a_short_output_is_shown_whole_scrubbed_and_not_kept(h: &CommandHarness) {
        let outcome = run(h, &format!("printf 'short %s\\n' {}", quoted(h)), true).await;
        assert_eq!(outcome.exit_code, Some(0), "{:?}", outcome.tail);
        assert!(!outcome.truncated);
        assert!(outcome.kept.is_none(), "{:?}", outcome.kept);
        assert_eq!(outcome.tail, format!("short {REDACTED}\n"));
    }
}

/// One `#[tokio::test]` per case of [`command_cases`], each over a [`CommandHarness`] built by
/// `$harness` (an expression, awaited inside the test when it is a future).
#[macro_export]
macro_rules! command_workspace_conformance {
    ($harness:expr) => {
        $crate::command_workspace_conformance!(@case $harness, a_command_runs_in_the_writable_place);
        $crate::command_workspace_conformance!(@case $harness, a_long_output_is_shown_and_kept_scrubbed);
        $crate::command_workspace_conformance!(@case $harness, timeout_of_keeps_to_the_limit);
        $crate::command_workspace_conformance!(@case $harness, a_short_output_is_shown_whole_scrubbed_and_not_kept);
    };
    (@case $harness:expr, $case:ident) => {
        #[tokio::test]
        async fn $case() {
            let harness = $harness;
            $crate::command_cases::$case(&harness).await;
        }
    };
}

/// A workspace of named directories, for the tests of the tools and as the reference
/// implementation of the cases. A place is found by its name; with no name, the only place that may
/// be written, if there is exactly one (as the coder finds a slot).
///
/// Its commands run in this process's own environment ([`LocalSession`]), within
/// [`DEFAULT_LIMITS`] unless [`with_limits`](Self::with_limits), and a long output is kept in
/// memory ([`MemoryKept`]), scrubbed by the workspace's redactor.
pub struct DirWorkspace {
    places: Vec<Place>,
    redactor: Redactor,
    limits: CommandLimits,
    kept: MemoryKept,
}

/// The limits of a [`DirWorkspace`]'s commands unless it is given others: 30 seconds, and 16 KiB of
/// output shown.
pub const DEFAULT_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(30),
    output_tail: 16 * 1024,
};

impl fmt::Debug for DirWorkspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirWorkspace")
            .field("places", &self.places)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Default for DirWorkspace {
    fn default() -> Self {
        Self::new::<&str>([])
    }
}

impl DirWorkspace {
    /// A workspace with no place, whose redactor holds `secrets`.
    pub fn new<S: AsRef<str>>(secrets: impl IntoIterator<Item = S>) -> Self {
        let redactor = Redactor::new(secrets);
        // A clone shares the secrets: one added later is scrubbed from what is kept too.
        let scrubber = redactor.clone();
        let kept = MemoryKept::new()
            .with_margin(redactor.margin())
            .scrubbing(move |text| scrubber.scrub(text).into_owned());
        Self {
            places: Vec::new(),
            redactor,
            limits: DEFAULT_LIMITS,
            kept,
        }
    }

    /// The same workspace, whose commands run within `limits`.
    #[must_use]
    pub fn with_limits(mut self, limits: CommandLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Add the directory `dir` as a place named `name` that tools may write; `git` is discovered
    /// from the directory, as a test's is.
    ///
    /// # Errors
    ///
    /// The directory cannot be opened.
    pub fn place(self, name: &str, dir: &Path) -> std::io::Result<Self> {
        self.add(name, dir, true)
    }

    /// Add `dir` as a place named `name` that tools may only read.
    ///
    /// # Errors
    ///
    /// The directory cannot be opened.
    pub fn read_only_place(self, name: &str, dir: &Path) -> std::io::Result<Self> {
        self.add(name, dir, false)
    }

    fn add(mut self, name: &str, dir: &Path, writable: bool) -> std::io::Result<Self> {
        // `GitAt::plain` holds the directory open itself; this says, as an error, when it cannot.
        SlotDir::open(dir)?;
        let place = Place::new(name, GitAt::plain(dir));
        self.places
            .push(if writable { place } else { place.read_only() });
        Ok(self)
    }
}

#[async_trait]
impl CodeWorkspace for DirWorkspace {
    async fn place(
        &self,
        _ctx: &ToolCtx,
        repo: Option<&str>,
        intent: Intent,
    ) -> Result<Place, Outcome> {
        let place = match repo.map(str::trim).filter(|r| !r.is_empty()) {
            Some(name) => self.places.iter().find(|p| p.name() == name),
            None => {
                let mut writable = self.places.iter().filter(|p| p.is_writable());
                match (writable.next(), writable.next()) {
                    (Some(only), None) => Some(only),
                    _ => None,
                }
            }
        };
        let Some(place) = place else {
            let names: Vec<&str> = self.places.iter().map(Place::name).collect();
            return Err(Ok(ToolOutput::error(format!(
                "no such place; the places are: {}",
                names.join(", ")
            ))));
        };
        if intent == Intent::Write && !place.is_writable() {
            return Err(Ok(ToolOutput::error(format!(
                "{} can be read, not written",
                place.name()
            ))));
        }
        Ok(place.clone())
    }

    fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    /// Nothing follows a write here.
    async fn after_write(
        &self,
        _ctx: &ToolCtx,
        _place: &Place,
        _paths: &[String],
    ) -> Result<String, ToolError> {
        Ok(String::new())
    }
}

#[async_trait]
impl CommandWorkspace for DirWorkspace {
    async fn session(&self, ctx: &ToolCtx) -> Result<Arc<dyn EnvSession>, ToolError> {
        let session = LocalSession::shared();
        if let Some(meter) = Meter::of(&*session) {
            meter.report_session(ctx);
        }
        Ok(session)
    }

    fn limits(&self) -> CommandLimits {
        self.limits
    }

    fn kept(&self) -> &dyn KeptOutputs {
        &self.kept
    }
}
