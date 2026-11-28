//! Durable agent-loop runtime for adam-rs.
//!
//! Any agent (an LLM tool loop, a coordinator, a deterministic workflow)
//! becomes durable by implementing [`Agent`]: a state machine that advances by
//! one [`Transition`] per [`Agent::step`]. The [`Runtime`] persists the state
//! after every transition, compare-and-swap on the run's version, so a worker
//! that dies loses nothing: another worker resumes from the last commit.
//!
//! # Starting without stepping
//!
//! Starting a run needs only a name and the initial state
//! ([`AgentStarter`]); stepping it needs the whole [`Agent`]. A process that
//! only accepts requests registers a starter with [`RuntimeBuilder::starter`]
//! and holds none of the agent's dependencies; workers register the agent
//! under the same name and step what the front started.
//!
//! # Side effects and replay
//!
//! `step` may run again after a crash, a lost lease or a retry. Side effects
//! therefore go through [`Ctx::step`], which records each outcome (`Ok` or
//! `Err`) in the journal at the next `seq`; a re-execution gets the recorded
//! result back instead of running the effect again. Asking for a different
//! step name at a recorded `seq` fails the run with
//! [`AgentError::NonDeterminism`] rather than diverging silently.
//!
//! # Run lifecycle
//!
//! * [`Transition::Continue`]: commit, stay runnable.
//! * [`Transition::Park`]: wait for a timer and/or an inbound message
//!   ([`Runtime::deliver`]).
//! * [`Transition::Done`] / [`Transition::Fail`]: terminal.
//! * [`AgentError::Transient`]: retried with exponential backoff
//!   ([`RetryPolicy`]), then `Failed`. With a `retry_after` (a rate limit's
//!   `Retry-After`, see [`AgentError::with_retry_after`]) the retry waits
//!   `max(backoff, hint)`. [`AgentError::Permanent`]: `Failed`. Errors carry an
//!   [`ErrorClass`] ([`Classify`]); the runtime decides from the class.
//! * [`Runtime::cancel`]: `Failed` with the reason, unless already finished.
//!   A step that is running at that moment can observe it through
//!   [`Ctx::cancelled`] / [`CancelToken`] and stop early: at once when the
//!   runtime that holds it is the one cancelling or shares a [`Notifier`],
//!   within one `cancel_poll_interval` otherwise.
//!
//! # Several processes
//!
//! Processes share nothing but the store, and find work by polling it
//! (`poll_interval`). A [`Notifier`] ([`RuntimeBuilder::notifier`]) makes them
//! react at once: `start` and `deliver` publish [`Signal::Runnable`] so a
//! worker of another process polls now, and `cancel` publishes
//! [`Signal::Finished`] so a step of another process sees its
//! [`CancelToken`] fire. Signals are hints: they may be lost, and polling
//! stays on. [`LocalNotifier`] connects runtimes inside one process; an adapter
//! crate connects processes.
//!
//! # Observing runs
//!
//! Two channels, deliberately different:
//!
//! * **Events** ([`EventSink`], [`RunEvent`]): live and best effort, lost if
//!   the process dies. In-process fan-out: [`BroadcastSink`].
//! * **The durable record** ([`Runtime::view`] -> [`RunView`]): status,
//!   output, error, attempt, timers, emitted artifacts and the agent's head
//!   (its state without the transcript), all read from the store. A consumer that restarts (or
//!   lives in another process) polls this to rebuild what it missed; [`Runtime::state`] is the one
//!   read that joins the head and the body generation it names.
//!
//! # Guarantees
//!
//! * The version CAS is what guarantees correctness; leases only avoid
//!   wasted work. A worker whose lease expired mid-step can not overwrite
//!   newer state: its commit is rejected and it drops its result.
//! * A worker never claims a run it is stepping, even once the lease on it
//!   has lapsed, and it releases a run's lease before it counts the run as
//!   free again, so it never starts a second step on a snapshot that its own
//!   first step is about to make stale.
//! * A recorded step outcome is never re-executed, but a side effect is
//!   at-least-once: a crash between the effect and its journal write, or a
//!   transient retry, runs it again. Keep effects idempotent (see
//!   [`Ctx::step`]).
//! * This crate depends on `adam-core` for the store and on `adam-model` for the data type of a
//!   model call's tokens only. Store, sink and clock are swappable behind traits.

#![warn(missing_docs)]

mod agent;
mod artifact_ref;
mod blob;
mod cancel;
mod child;
mod clock;
mod ctx;
mod envelope;
mod erased;
mod events;
mod file;
mod head;
mod lines;
mod notify;
mod retry;
mod runtime;
mod seal;
mod session;
mod starter;
mod step;
mod text;
mod usage;
mod worker;

pub use adam_core::RunHeader;
pub use adam_error::{Classify, ErrorClass};
pub use adam_model::Usage;
pub use agent::{Agent, AgentError, AgentStarter, Inbound, Transition};
pub use artifact_ref::{ArtifactRef, FileRef, FileSource};
pub use blob::{BlobWriter, blob_id};
pub use cancel::CancelToken;
pub use child::{
    CHILD_PARKED_KIND, CHILD_REPORTED_KIND, ChildLook, ChildStarter, ChildStatus, RESUME_KIND,
    RUN_FINISHED_KIND, child_run_id,
};
pub use clock::{Clock, DynClock, ManualClock, SystemClock};
pub use ctx::{Ctx, Emitter, ReservedStep};
pub use events::{
    Artifact, ArtifactFile, ArtifactFileError, BroadcastSink, CollectingSink, DynEventSink,
    EventSink, MAX_ARTIFACT_FILE_BYTES, MAX_ARTIFACT_FILENAME_BYTES, MAX_RUN_FILE_BYTES,
    MAX_UPLOAD_ID_BYTES, NoopSink, REPLAY_EVENTS_PER_RUN, REPLAY_MAX_AGE, REPLAY_MAX_BYTES,
    REPLAY_MAX_RUNS, Received, RunEvent, RunSubscription, SinkEvent, UPLOAD_REF_MEDIA_TYPE,
    UploadRef, upload_ref_of,
};
pub use file::{
    MAX_FILES_PER_RESULT, ReceivedFiles, checked_media_type, extension_of, human_size, image_size,
    sniff_image,
};
pub use lines::{SystemLine, is_system_line, safe_file_name};
pub use notify::{Delivery, DynNotifier, LocalNotifier, Notifier, Signal};
pub use retry::{
    MAX_CONSECUTIVE_FAILURES, MAX_RETRY_AFTER, RUNTIME_RETRY_KEY, RetryClasses, RetryPolicy,
    StepRetry,
};
pub use runtime::{RunView, Runtime, RuntimeBuilder, RuntimeError, view_from_record};
pub use session::{
    CONTEXT_WINDOW_HEADROOM_TOKENS, CONTEXT_WINDOW_PROMPT_TOKENS, DEFAULT_SESSION_IDLE_TTL,
    MAX_REPORT_SCHEMA_BYTES, MAX_SESSION_CONVERSATION_BYTES, MAX_SESSION_INSTRUCTIONS_CHARS,
    MAX_SESSION_MAX_OUTPUT_TOKENS, MAX_SESSION_PAGE, MAX_SESSION_ROUNDS_SCANNED,
    MAX_SESSION_TOKEN_BUDGET, MAX_SESSION_TOOL_ENTRIES, MAX_SESSION_TOOL_ENTRY_CHARS,
    MIN_SESSION_MAX_OUTPUT_TOKENS, MIN_SESSION_TOKEN_BUDGET, Refusal, ReportRound, RoundOutcome,
    SESSION_CLOSE_KIND, SESSION_CLOSE_ROUND_KEY, SESSION_STATE_KEY, SessionBounds, SessionConfig,
    SessionHeader, SessionLimits, SessionList, SessionMode, SessionState, SessionSummary,
    SessionView, closing_round_payload, glob, parse_session_conversation_id,
    session_conversation_id, session_conversation_prefix,
};
pub use starter::{RunStarter, SCHEDULE_CONVERSATION_PREFIX, UNATTENDED_CONTEXT_KEY};
pub use step::{
    MAX_STEP_DETAIL_CHARS, MAX_STEP_ID_BYTES, MAX_STEP_LABEL_CHARS, STEP_INPUT_MAX_BYTES,
    STEP_INPUT_STRING_MAX_CHARS, STEP_OUTPUT_MAX_BYTES, StepEvent, StepIcon, StepKind, StepOutput,
    StepState,
};
pub use text::{AGENT_TEXT_KIND, MAX_STREAM_ID_BYTES, MAX_TEXT_DELTA_BYTES, floor_boundary};
pub use usage::{
    MAX_SANDBOX_ENTRIES, MAX_USAGE_CALL_BYTES, MAX_USAGE_LABEL_BYTES, MAX_USAGE_TOTALS,
    SandboxEntry, SandboxEnvironment, SandboxUsage, ShorteningUsage, UsageEvent, UsageTotal,
    UsageTotals,
};
