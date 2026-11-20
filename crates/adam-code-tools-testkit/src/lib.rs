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

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::Path;
use std::sync::Arc;

use adam_code_tools::git::GitAt;
use adam_code_tools::{CodeWorkspace, DynCodeWorkspace, Intent, Outcome, Place, Redactor};
use adam_llm_agent::{ToolCtx, ToolError, ToolOutput};
use adam_runtime::CollectingSink;
use adam_workspace::SlotDir;
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
            // On a failure the message says how long the secret is, never what it is.
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

/// A workspace of named directories, for the tests of the tools and as the reference
/// implementation of the cases. A place is found by its name; with no name, the only place that may
/// be written, if there is exactly one (as the coder finds a slot).
#[derive(Debug, Default)]
pub struct DirWorkspace {
    places: Vec<Place>,
    redactor: Redactor,
}

impl DirWorkspace {
    /// A workspace with no place, whose redactor holds `secrets`.
    pub fn new<S: AsRef<str>>(secrets: impl IntoIterator<Item = S>) -> Self {
        Self {
            places: Vec::new(),
            redactor: Redactor::new(secrets),
        }
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
