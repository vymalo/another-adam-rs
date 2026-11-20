//! The `adam.schedule` agent: a schedule as a parked run (ADR 0060, decisions 1 to 4).
//!
//! Its state is the schedule and what it fired. Each step reads the time from the journal, fires
//! the latest due time that has passed (a run of the root agent under the due time's id, through a
//! [`RunStarter`](adam_runtime::RunStarter)), and parks until the next one; a one-shot that fired
//! ends `Done`. A message that wakes it early changes nothing: it parks again.

use std::path::PathBuf;

use adam_agent_fs::Schedule;
use adam_core::RunId;
use adam_core::time::Timestamp;
use adam_runtime::{Agent, AgentError, Classify, Ctx, Inbound, Transition};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Due, ScheduleSpec, fired_conversation_id, fired_run_id};

/// The last segment of a scheduler's registered name: `<agent>/adam.schedule` ([`scheduler_agent_name`]).
pub const SCHEDULER_SUFFIX: &str = "adam.schedule";

/// The name the scheduler of `agent`'s schedules is registered under, `<agent>/adam.schedule`, as a
/// session round's is `<root>/<subagent>`: a worker claims the runs of the agents it registered, so
/// agents that share a database each step their own schedulers (an agent's name has no `.`, so no
/// subagent shares it).
pub fn scheduler_agent_name(agent: &str) -> String {
    format!("{agent}/{SCHEDULER_SUFFIX}")
}

/// The [`Inbound::kind`] of the message that starts a scheduler run: its payload is the
/// [`SchedulerState`] to begin with.
pub const SCHEDULER_START_KIND: &str = "adam.schedule.start";

/// The [`Inbound::kind`] of a firing's message: the root agent reads it as a person's message, the
/// prompt in `text`, the trigger beside it under `schedule`.
pub const FIRING_KIND: &str = "message";

/// The firings a scheduler's state keeps, newest last.
pub const FIRINGS_KEPT: usize = 20;

/// How long a scheduler waits before it tries a start again that failed with a retryable error
/// (the store unavailable, contention). It never fails for one: nothing would start it again
/// before the next worker start.
pub const FIRE_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// A schedule as the scheduler keeps it: the fields of the file, so a restart evaluates the same
/// schedule whatever the folder holds by then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleRecord {
    /// The agent it fires.
    pub agent: String,
    /// The schedule's name.
    pub name: String,
    /// `cron`, empty for a one-shot.
    pub cron: String,
    /// `at`, as written.
    pub at: Option<String>,
    /// `after`, as written.
    pub after: Option<String>,
    /// The zone's name.
    pub timezone: String,
    /// The prompt of each firing.
    pub prompt: String,
}

impl ScheduleRecord {
    /// The fields of `schedule` (its path is not kept).
    pub fn of(schedule: &Schedule) -> Self {
        Self {
            agent: schedule.agent.clone(),
            name: schedule.name.clone(),
            cron: schedule.cron.clone(),
            at: schedule.at.clone(),
            after: schedule.after.clone(),
            timezone: schedule.timezone.clone(),
            prompt: schedule.prompt.clone(),
        }
    }

    /// The schedule again, with no path.
    pub fn to_schedule(&self) -> Schedule {
        Schedule {
            name: self.name.clone(),
            cron: self.cron.clone(),
            at: self.at.clone(),
            after: self.after.clone(),
            timezone: self.timezone.clone(),
            agent: self.agent.clone(),
            prompt: self.prompt.clone(),
            path: PathBuf::new(),
        }
    }
}

/// One firing: a due time and the run it started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Firing {
    /// The due time it fired for.
    pub due: Timestamp,
    /// The run of the root agent ([`fired_run_id`]).
    pub run: RunId,
    /// The due times skipped before it (decision 3).
    pub missed: u64,
    /// Whether this scheduler created the run; `false` when it already existed (another
    /// scheduler of the schedule fired the due time first, or a replay found it), or when the
    /// start was refused.
    pub created: bool,
    /// Why the start was refused, when it was: a refusal no retry mends (an agent this runtime
    /// does not know, a first message it refuses). The due time is not fired, and the schedule
    /// goes on to the next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The state of a scheduler run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerState {
    /// The schedule.
    pub schedule: ScheduleRecord,
    /// [`ScheduleSpec::hash`] of it, which `reconcile` compares with the folder's.
    pub hash: String,
    /// What an `after` counts from: the time the reconciling runtime read when it started this
    /// run (decision 4).
    pub anchor: Timestamp,
    /// The last due time fired, `None` before the first.
    pub last_due: Option<Timestamp>,
    /// The last [`FIRINGS_KEPT`] firings, newest last.
    pub fired: Vec<Firing>,
}

impl SchedulerState {
    /// The first state of a scheduler for `spec`, anchored at `anchor`.
    pub fn new(schedule: &Schedule, spec: &ScheduleSpec, anchor: Timestamp) -> Self {
        Self {
            schedule: ScheduleRecord::of(schedule),
            hash: spec.hash().to_owned(),
            anchor,
            last_due: None,
            fired: Vec::new(),
        }
    }
}

/// The scheduler of one agent's schedules, registered as [`scheduler_agent_name`] of it. Register it
/// on every runtime that steps that agent; it needs nothing else.
#[derive(Debug, Clone)]
pub struct SchedulerAgent {
    agent: String,
    name: String,
}

impl SchedulerAgent {
    /// The scheduler of `agent`'s schedules.
    pub fn new(agent: impl Into<String>) -> Self {
        let agent = agent.into();
        Self {
            name: scheduler_agent_name(&agent),
            agent,
        }
    }

    /// The agent whose schedules it fires.
    pub fn agent(&self) -> &str {
        &self.agent
    }
}

/// What the fire step records: whether it created the run, or why the start failed.
#[derive(Serialize, Deserialize)]
struct StartFailed {
    message: String,
    retryable: bool,
}

#[async_trait]
impl Agent for SchedulerAgent {
    type State = SchedulerState;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<SchedulerState, AgentError> {
        if input.kind != SCHEDULER_START_KIND {
            return Err(AgentError::permanent(format!(
                "a scheduler starts from a `{SCHEDULER_START_KIND}` message, not `{}`",
                input.kind
            )));
        }
        serde_json::from_value(input.payload)
            .map_err(|e| AgentError::permanent(format!("not a scheduler's state: {e}")))
    }

    async fn step(
        &self,
        ctx: &mut Ctx,
        mut state: SchedulerState,
    ) -> Result<Transition<SchedulerState>, AgentError> {
        // A message is no instruction to a scheduler: it only woke it early.
        drop(ctx.take_inbox());
        if state.schedule.agent != self.agent {
            return Err(AgentError::permanent(format!(
                "the scheduler of `{}` was given a schedule of `{}`",
                self.agent, state.schedule.agent
            )));
        }
        let spec = ScheduleSpec::from_schedule(&state.schedule.to_schedule())
            .map_err(AgentError::permanent)?;
        let now = ctx.now_journaled().await?;
        // A one-shot that has not fired is due whenever its time is, even before the anchor: an
        // `at` that passed while nothing ran fires late, once (decision 3).
        let since = match state.last_due {
            Some(last) => last,
            None if spec.is_one_shot() => Timestamp::MIN,
            None => state.anchor,
        };

        if let Some(Due { at, missed }) = spec.latest_due(since, now, state.anchor) {
            let run = fired_run_id(spec.agent(), spec.name(), at);
            let conversation = fired_conversation_id(spec.agent(), spec.name(), at);
            let input = firing(&spec, at, missed);
            let starter = ctx.run_starter();
            let agent = spec.agent().to_owned();
            let started = ctx
                .step("fire", || async move {
                    starter
                        .start_with_id(run, &agent, input, &conversation)
                        .await
                        .map_err(|e| StartFailed {
                            message: e.to_string(),
                            retryable: e.is_retryable(),
                        })
                })
                .await?;
            let (created, error) = match started {
                Ok(created) => (created, None),
                Err(e) if e.retryable => {
                    // Try again soon, from a fresh transition that reads the clock again; the due
                    // time is not consumed, so a later one may replace it (and count it missed).
                    tracing::warn!(schedule = spec.name(), error = %e.message, "a firing will be tried again");
                    let wake_at = adam_core::time::Timestamp(
                        now.0
                            .checked_add(
                                jiff::SignedDuration::try_from(FIRE_RETRY).unwrap_or_default(),
                            )
                            .unwrap_or(now.0),
                    );
                    return Ok(Transition::Park {
                        state,
                        wake_at: Some(wake_at),
                    });
                }
                Err(e) => {
                    tracing::warn!(schedule = spec.name(), error = %e.message, "a firing was refused");
                    (false, Some(e.message))
                }
            };
            state.last_due = Some(at);
            state.fired.push(Firing {
                due: at,
                run,
                missed,
                created,
                error,
            });
            let drop_count = state.fired.len().saturating_sub(FIRINGS_KEPT);
            state.fired.drain(..drop_count);
        }

        match spec.due_after(state.last_due.unwrap_or(since), state.anchor) {
            Some(next) => Ok(Transition::Park {
                state,
                wake_at: Some(next),
            }),
            None => {
                let output = json!({ "fired": state.fired });
                Ok(Transition::Done { state, output })
            }
        }
    }
}

/// The message a firing starts the root agent with.
fn firing(spec: &ScheduleSpec, due: Timestamp, missed: u64) -> Inbound {
    let trigger: Value = json!({
        "name": spec.name(),
        "due": due.0.to_string(),
        "missed": missed,
    });
    Inbound::new(
        FIRING_KIND,
        json!({ "text": spec.prompt(), "schedule": trigger }),
    )
}
