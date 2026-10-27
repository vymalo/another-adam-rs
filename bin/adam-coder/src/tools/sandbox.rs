//! The time a run's commands run in a sandbox, reported as usage (ADR 0043).
//!
//! A pod's commands are metered: the wall time of each, from the moment the process is spawned to its
//! exit (a timeout or a cancel kill included), is reported to the call's context
//! ([`ToolCtx::report_sandbox`]) with the shape the pod's session says ([`EnvSession::sandbox_shape`],
//! the requests of the template). The loop adds it to the run's state with the call's result. The
//! coder's own container and a dev container are not metered: asking for their session reports only
//! where the run's commands run, which reads as *not measured* and never as zero.

use std::time::{Duration, Instant};

use adam::prelude::ToolCtx;
use adam_runtime::{SandboxEnvironment, SandboxUsage};
use adam_workspace::{EnvKind, EnvSession, SandboxShape};

/// What the usage of a run says about the environment a session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Meter {
    /// A sandbox whose commands are timed, weighted by this shape.
    Measured(SandboxShape),
    /// An environment whose commands are not timed.
    Unmeasured(SandboxEnvironment),
}

impl Meter {
    /// How `session` is metered. `None` for an environment this build does not know, and for one that
    /// says it is a pod and gives no shape: it reports nothing, rather than a zero it did not measure.
    pub(crate) fn of(session: &dyn EnvSession) -> Option<Self> {
        if let Some(shape) = session.sandbox_shape() {
            return Some(Self::Measured(shape));
        }
        match session.describe().kind {
            EnvKind::Local => Some(Self::Unmeasured(SandboxEnvironment::Local)),
            EnvKind::DevContainer { .. } => {
                Some(Self::Unmeasured(SandboxEnvironment::Devcontainer))
            }
            _ => None,
        }
    }

    /// The run asked for this environment: the usage names it, and for a pod, with no command yet, it
    /// is a measured zero.
    pub(crate) fn report_session(self, ctx: &ToolCtx) {
        let environment = match self {
            Self::Measured(_) => SandboxEnvironment::Kubernetes,
            Self::Unmeasured(environment) => environment,
        };
        ctx.report_sandbox(&SandboxUsage::in_environment(environment));
    }

    /// A command ran for `elapsed`. Counted in a metered sandbox; in any other it is not measured.
    pub(crate) fn report_command(self, ctx: &ToolCtx, elapsed: Duration) {
        if let Self::Measured(shape) = self {
            ctx.report_sandbox(&SandboxUsage::ran_command(
                shape.cpu_millis,
                shape.memory_bytes,
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            ));
        }
    }

    /// Start timing a process that is already running; the time is reported when the clock is dropped,
    /// whichever way the call ends.
    pub(crate) fn clock(self, ctx: &ToolCtx) -> CommandClock<'_> {
        CommandClock {
            ctx,
            meter: self,
            started: Instant::now(),
        }
    }
}

/// Times a process the tool keeps for the length of its call (OpenCode), and reports it when dropped.
pub(crate) struct CommandClock<'a> {
    ctx: &'a ToolCtx,
    meter: Meter,
    started: Instant,
}

impl Drop for CommandClock<'_> {
    fn drop(&mut self) {
        self.meter.report_command(self.ctx, self.started.elapsed());
    }
}
