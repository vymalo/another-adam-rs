//! Schedules of adam-rs agent folders ([ADR 0060]): when a schedule is due, and the ids and
//! conversations of the runs it fires.
//!
//! A folder's `schedules/<name>.md` is read and checked by `adam-agent-fs`; [`ScheduleSpec`] is one
//! of them ready to evaluate, its time zone found in the bundled database. [`ScheduleSpec::due_after`]
//! and [`ScheduleSpec::latest_due`] say when it fires; [`fired_run_id`] and
//! [`fired_conversation_id`] name the run a due time starts, the same on every replica, and
//! [`trigger_of`] reads the trigger back from that run's conversation.
//!
//! The evaluation reads no clock and no store: the caller passes the instants. [`SchedulerAgent`]
//! fires schedules as parked runs, and [`reconcile`] reads the store and the runtime's clock to
//! make an agent's scheduler runs match its schedules.
//!
//! [ADR 0060]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0060-schedules-are-parked-runs.md

#![warn(missing_docs)]

mod agent;
mod due;
mod ids;
mod reconcile;

pub use adam_agent_fs::{At, Cron, When};
pub use agent::{
    FIRE_RETRY, FIRING_KIND, FIRINGS_KEPT, Firing, SCHEDULER_START_KIND, SCHEDULER_SUFFIX,
    ScheduleRecord, SchedulerAgent, SchedulerState, scheduler_agent_name,
};
pub use due::{Due, MISSED_MAX};
pub use ids::{Trigger, fired_conversation_id, fired_run_id, schedule_conversation_id, trigger_of};
pub use reconcile::{
    CHANGED_REASON, REMOVED_REASON, ReconcileError, Reconciled, reconcile, reconcile_store,
};

use adam_agent_fs::Schedule;
use jiff::tz::TimeZone;
use sha2::{Digest, Sha256};

/// Why a schedule cannot be evaluated: a value the loader would have refused, in a `Schedule`
/// built by hand or embedded by an older build.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("schedule {name:?} of agent {agent:?}: {why}")]
pub struct ScheduleError {
    /// The agent the schedule belongs to.
    pub agent: String,
    /// The schedule's name.
    pub name: String,
    /// What is wrong, completing "the schedule ...".
    pub why: String,
}

/// One schedule, ready to evaluate.
#[derive(Debug, Clone)]
pub struct ScheduleSpec {
    agent: String,
    name: String,
    when: When,
    timezone: String,
    zone: TimeZone,
    prompt: String,
    hash: String,
}

impl ScheduleSpec {
    /// The schedule `adam-agent-fs` read, with its time zone looked up in the bundled database.
    ///
    /// # Errors
    ///
    /// [`ScheduleError`] for none or more than one of `cron`, `at` and `after`, a value that does
    /// not parse, or a zone the database does not have.
    pub fn from_schedule(schedule: &Schedule) -> Result<Self, ScheduleError> {
        let refuse = |why: String| ScheduleError {
            agent: schedule.agent.clone(),
            name: schedule.name.clone(),
            why,
        };
        let when = schedule.when().map_err(|e| refuse(e.to_string()))?;
        let zone = adam_agent_fs::time_zone(&schedule.timezone)
            .map_err(|e| refuse(format!("`timezone: {}` {e}", schedule.timezone)))?;
        Ok(Self {
            hash: spec_hash(schedule),
            agent: schedule.agent.clone(),
            name: schedule.name.clone(),
            when,
            timezone: schedule.timezone.clone(),
            zone,
            prompt: schedule.prompt.clone(),
        })
    }

    /// The agent whose runs it fires.
    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// Its name, from the path: `schedules/team/weekly.md` is `team/weekly`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// When it fires.
    pub fn when(&self) -> When {
        self.when
    }

    /// The zone's name, as written (`UTC` when the file names none).
    pub fn timezone(&self) -> &str {
        &self.timezone
    }

    /// The prompt of each firing.
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// `sha256:<hex>` over every field the file gives (agent, name, `cron`, `at`, `after`,
    /// `timezone` and the prompt), so any change to the file is a new spec (decision 4).
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Whether it fires once (`at`, `after`) rather than repeating (`cron`).
    pub fn is_one_shot(&self) -> bool {
        !matches!(self.when, When::Cron(_))
    }
}

fn spec_hash(s: &Schedule) -> String {
    let mut hash = Sha256::new();
    for part in [
        "adam-schedule/spec/v1",
        &s.agent,
        &s.name,
        &s.cron,
        s.at.as_deref().unwrap_or_default(),
        s.after.as_deref().unwrap_or_default(),
        &s.timezone,
        &s.prompt,
    ] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    let digest = hash.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}
