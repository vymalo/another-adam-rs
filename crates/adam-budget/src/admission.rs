//! The admission port, its ask, its permit, its refusals and its report.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use adam_device_state::{DeviceReadings, DynDeviceState};
use adam_error::{Classify, ErrorClass};
use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::budget::Limit;

/// One kind of work: a pool of its own, so that one kind never starves another (ADR 0065
/// decision 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Pool {
    /// Runs advanced at the same time.
    Runs,
    /// Model calls in flight.
    ModelCalls,
    /// Tool calls in flight, in the whole process.
    ToolCalls,
    /// Processes: commands, language servers and stdio MCP servers.
    Processes,
    /// git processes.
    Git,
    /// MCP servers alive.
    McpServers,
    /// The memory reservations of commands, in MiB.
    CommandMemory,
}

impl Pool {
    /// Every pool, in the order a report lists them.
    pub const ALL: [Self; 7] = [
        Self::Runs,
        Self::ModelCalls,
        Self::ToolCalls,
        Self::Processes,
        Self::Git,
        Self::McpServers,
        Self::CommandMemory,
    ];

    /// The label of the pool in a report, a refusal or a metric (`model_calls`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Runs => "runs",
            Self::ModelCalls => "model_calls",
            Self::ToolCalls => "tool_calls",
            Self::Processes => "processes",
            Self::Git => "git",
            Self::McpServers => "mcp_servers",
            Self::CommandMemory => "command_memory",
        }
    }

    /// The unit of one [`Ask::units`] of this pool in a message: `a call`, `a process`, `a MiB`.
    #[must_use]
    pub const fn unit(self) -> &'static str {
        match self {
            Self::Runs => "a run",
            Self::ModelCalls | Self::ToolCalls => "a call",
            Self::Processes | Self::Git | Self::McpServers => "a process",
            Self::CommandMemory => "a MiB of command memory",
        }
    }
}

impl fmt::Display for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The identity a conversation's share is counted under: one thread, one child step or one run.
///
/// It is shared and cheap to clone, and it is an `Arc<str>` so that no name is copied for every
/// ask. A deployment must not put a person's name or a message in it: a report never carries it
/// (ADR 0065 decision 9).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConversationId(Arc<str>);

impl ConversationId {
    /// The key of `id`.
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    /// The key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ConversationId {
    fn from(id: &str) -> Self {
        Self::new(id)
    }
}

impl From<String> for ConversationId {
    fn from(id: String) -> Self {
        Self::new(id)
    }
}

impl From<Arc<str>> for ConversationId {
    fn from(id: Arc<str>) -> Self {
        Self::new(id)
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One question put to an [`Admission`]: a share of one [`Pool`], for one conversation.
///
/// `units` is in the pool's own unit ([`Pool::unit`]): one call, one process, one MiB. `tokens` is
/// what a model call is estimated to read and write, charged to the process's token bucket when the
/// budget meters one. `wait` is how long the ask may wait for room: **zero refuses at once**,
/// which is what a caller that cannot park uses.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Ask {
    /// The pool asked for.
    pub pool: Pool,
    /// Whose share it counts against.
    pub conversation: ConversationId,
    /// How much of the pool, in its unit. `0` is read as `1`.
    pub units: u32,
    /// The tokens this call is estimated to use (`Pool::ModelCalls` only).
    pub tokens: u32,
    /// How long the ask may wait for room. Zero refuses at once.
    pub wait: Duration,
}

impl Ask {
    /// One unit of `pool` for `conversation`, waiting for no room.
    pub fn new(pool: Pool, conversation: impl Into<ConversationId>) -> Self {
        Self {
            pool,
            conversation: conversation.into(),
            units: 1,
            tokens: 0,
            wait: Duration::ZERO,
        }
    }

    /// How much of the pool the ask takes, in its unit.
    #[must_use]
    pub fn units(mut self, units: u32) -> Self {
        self.units = units;
        self
    }

    /// The tokens the call is estimated to use.
    #[must_use]
    pub fn tokens(mut self, tokens: u32) -> Self {
        self.tokens = tokens;
        self
    }

    /// How long the ask may wait for room. Zero refuses at once.
    #[must_use]
    pub fn wait(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    /// Put this ask: [`Admission::admit`], for a caller that holds the port as a trait object.
    ///
    /// # Errors
    ///
    /// [`Refused`], classified through [`Classify`].
    pub async fn admit(&self, admission: &dyn Admission) -> Result<Permit, Refused> {
        admission.admit(self.clone()).await
    }
}

/// One share of a pool, held until it is dropped.
///
/// Dropping it gives the share back, so a cancelled, failed or panicking holder frees it: nothing
/// has to be released by hand, and a permit is never released twice. An implementation makes one
/// with [`Permit::new`], handing a release that drops what it acquired.
#[must_use = "a permit is released as soon as it is dropped"]
pub struct Permit {
    pool: Pool,
    conversation: ConversationId,
    units: u32,
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl Permit {
    /// A permit of `units` of `pool` for `conversation`; `release` gives the share back and runs
    /// once, when the permit is dropped.
    pub fn new(
        pool: Pool,
        conversation: impl Into<ConversationId>,
        units: u32,
        release: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            pool,
            conversation: conversation.into(),
            units,
            release: Some(Box::new(release)),
        }
    }

    /// The pool.
    #[must_use]
    pub fn pool(&self) -> Pool {
        self.pool
    }

    /// Whose share it counts against.
    #[must_use]
    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }

    /// How much of the pool it holds, in its unit.
    #[must_use]
    pub fn units(&self) -> u32 {
        self.units
    }

    /// The pool, the conversation and the share, without what releases them.
    #[must_use]
    pub fn share(&self) -> Ask {
        Ask {
            pool: self.pool,
            conversation: self.conversation.clone(),
            units: self.units,
            tokens: 0,
            wait: Duration::ZERO,
        }
    }
}

impl fmt::Debug for Permit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Permit")
            .field("pool", &self.pool)
            .field("units", &self.units)
            .field("held", &self.release.is_some())
            .finish()
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Why an ask got no permit. Each variant has the class the caller reacts to
/// ([`Classify`], ADR 0065 decision 4; **no new `ErrorClass`**).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refused {
    /// The pool is full and the ask's wait ran out: `RateLimited`, park until `retry_after` and
    /// ask again. Never a failed run.
    #[error("the {pool} pool is full")]
    Busy {
        /// The pool that is full.
        pool: Pool,
        /// When to ask again, when the admission can say.
        retry_after: Option<Duration>,
    },
    /// The process's token bucket has nothing left for this call: `RateLimited`.
    #[error("the model's tokens per minute are used up; retry in {retry_after:?}")]
    TokensPerMinute {
        /// When enough tokens will have refilled.
        retry_after: Duration,
    },
    /// The disk budget is full: `Rejected`, the person is asked to free space or raise the limit.
    #[error("the disk budget is full: {used} of {limit} bytes used")]
    DiskFull {
        /// The bytes in use.
        used: u64,
        /// The budget.
        limit: u64,
    },
    /// The ask can never fit: `Invalid`, refused, naming the limit and its value.
    #[error("{what} asked for {asked}, over the limit of {limit}")]
    TooLarge {
        /// What is over its limit (`a run`, `a MiB of command memory`).
        what: &'static str,
        /// What the ask wanted.
        asked: u64,
        /// The limit it is over.
        limit: u64,
    },
}

impl Classify for Refused {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Busy { .. } | Self::TokensPerMinute { .. } => ErrorClass::RateLimited,
            Self::DiskFull { .. } => ErrorClass::Rejected,
            Self::TooLarge { .. } => ErrorClass::Invalid,
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Busy { retry_after, .. } => *retry_after,
            Self::TokensPerMinute { retry_after } => Some(*retry_after),
            Self::DiskFull { .. } | Self::TooLarge { .. } => None,
        }
    }
}

/// What one pool holds now.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PoolUsage {
    pool: Pool,
    limit: Limit,
    in_use: u64,
    waiting: u32,
}

impl PoolUsage {
    /// One pool's line of a report.
    #[must_use]
    pub fn new(pool: Pool, limit: Limit, in_use: u64, waiting: u32) -> Self {
        Self {
            pool,
            limit,
            in_use: in_use.min(limit.at_most().unwrap_or(u64::MAX)),
            waiting,
        }
    }

    /// The pool.
    #[must_use]
    pub fn pool(&self) -> Pool {
        self.pool
    }

    /// The ceiling of the pool, in its unit, or [`Limit::Unlimited`] when it has none.
    #[must_use]
    pub fn limit(&self) -> Limit {
        self.limit
    }

    /// How much of it is held, in its unit.
    #[must_use]
    pub fn in_use(&self) -> u64 {
        self.in_use
    }

    /// How many asks are waiting for room.
    #[must_use]
    pub fn waiting(&self) -> u32 {
        self.waiting
    }
}

/// The token bucket's state, when the budget meters one.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TokenUsage {
    limit: u32,
    available: u32,
}

impl TokenUsage {
    /// The bucket's line of a report.
    #[must_use]
    pub fn new(limit: u32, available: u32) -> Self {
        Self {
            limit,
            available: available.min(limit),
        }
    }

    /// Tokens per minute.
    #[must_use]
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Tokens available now.
    #[must_use]
    pub fn available(&self) -> u32 {
        self.available
    }
}

/// What an [`Admission`] holds now: one line per pool and, when it meters them, the bucket.
///
/// It names no person, no conversation and no run (ADR 0065 decision 9): a conversation's share is
/// enforced, never reported.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Usage {
    pools: BTreeMap<Pool, PoolUsage>,
    tokens: Option<TokenUsage>,
}

impl Usage {
    /// A report of `pools` and, when the budget meters tokens, the bucket.
    #[must_use]
    pub fn new(pools: impl IntoIterator<Item = PoolUsage>, tokens: Option<TokenUsage>) -> Self {
        Self {
            pools: pools.into_iter().map(|line| (line.pool, line)).collect(),
            tokens,
        }
    }

    /// Every pool's line, in [`Pool::ALL`] order.
    pub fn pools(&self) -> impl Iterator<Item = &PoolUsage> {
        self.pools.values()
    }

    /// One pool's line.
    #[must_use]
    pub fn pool(&self, pool: Pool) -> Option<&PoolUsage> {
        self.pools.get(&pool)
    }

    /// The bucket, when the budget meters tokens.
    #[must_use]
    pub fn tokens(&self) -> Option<&TokenUsage> {
        self.tokens.as_ref()
    }
}

/// The admission port: is there room in the budget?
///
/// Every implementation passes `admission_conformance!` (in `adam-admission-testkit`): never over a limit under many
/// concurrent asks, a dropped or cancelled holder gives its share back, one conversation never
/// holds more than its share of a pool, `snapshot` adds up, and a refusal that may pass says when
/// to ask again.
#[async_trait]
pub trait Admission: Send + Sync {
    /// Ask for a share of one pool.
    ///
    /// # Errors
    ///
    /// [`Refused`]: the pool stayed full for the ask's whole wait ([`Refused::Busy`], park and
    /// ask again), the token bucket is empty ([`Refused::TokensPerMinute`]), the disk budget is
    /// full ([`Refused::DiskFull`]), or the ask can never fit ([`Refused::TooLarge`]).
    async fn admit(&self, ask: Ask) -> Result<Permit, Refused>;

    /// What is held now, pool by pool.
    fn snapshot(&self) -> Usage;
}

/// An [`Admission`] held behind an `Arc`: what a host, an agent or a runtime is given.
pub type DynAdmission = Arc<dyn Admission>;

#[async_trait]
impl<T: Admission + ?Sized> Admission for Arc<T> {
    async fn admit(&self, ask: Ask) -> Result<Permit, Refused> {
        (**self).admit(ask).await
    }

    fn snapshot(&self) -> Usage {
        (**self).snapshot()
    }
}

impl fmt::Debug for dyn Admission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let usage = self.snapshot();
        let mut list = f.debug_list();
        for line in usage.pools() {
            list.entry(&format_args!(
                "{}={}/{}",
                line.pool(),
                line.in_use(),
                line.limit()
            ));
        }
        list.finish()
    }
}

/// The battery charge, in percent, below which the default rule table lowers the budget.
pub const LOW_BATTERY_PERCENT: u8 = 20;

/// What the device's readings ask the budget to give up now (ADR 0065 decision 8).
///
/// It is the output of [`DeviceRules::constraint`]: a value, with no state, that
/// [`Throttled`] reads at every ask. [`Constraint::NONE`] holds nothing back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Constraint {
    max_runs: Option<u32>,
    hold_new_steps: bool,
    no_background_commands: bool,
    ask_before_large_fetch: bool,
}

impl Constraint {
    /// Nothing is held back.
    pub const NONE: Self = Self {
        max_runs: None,
        hold_new_steps: false,
        no_background_commands: false,
        ask_before_large_fetch: false,
    };

    /// At most `runs` runs at once, never below one.
    #[must_use]
    pub const fn with_max_runs(mut self, runs: u32) -> Self {
        self.max_runs = Some(if runs == 0 { 1 } else { runs });
        self
    }

    /// New model calls and tool calls wait.
    #[must_use]
    pub const fn with_hold_new_steps(mut self) -> Self {
        self.hold_new_steps = true;
        self
    }

    /// No new background command starts.
    #[must_use]
    pub const fn with_no_background_commands(mut self) -> Self {
        self.no_background_commands = true;
        self
    }

    /// A clone or a large fetch needs the person's say first.
    #[must_use]
    pub const fn with_ask_before_large_fetch(mut self) -> Self {
        self.ask_before_large_fetch = true;
        self
    }

    /// The most runs at once, when the device caps them; always at least one.
    #[must_use]
    pub const fn max_runs(&self) -> Option<u32> {
        self.max_runs
    }

    /// Whether new model calls and tool calls wait ([`Throttled`] enforces it).
    #[must_use]
    pub const fn hold_new_steps(&self) -> bool {
        self.hold_new_steps
    }

    /// Whether no new background command may start. Not enforced by [`Throttled`]: an
    /// [`Ask`] does not say whether a process is a background command.
    #[must_use]
    pub const fn no_background_commands(&self) -> bool {
        self.no_background_commands
    }

    /// Whether a clone or a large fetch needs the person's say first. Not enforced by
    /// [`Throttled`]: an [`Ask`] does not say what a process fetches.
    #[must_use]
    pub const fn ask_before_large_fetch(&self) -> bool {
        self.ask_before_large_fetch
    }
}

/// The fixed rule table of ADR 0065 decision 8, one switch per rule, every one on by default.
///
/// | Rule | Reading | [`Constraint`] |
/// |---|---|---|
/// | low battery | on battery below [`LOW_BATTERY_PERCENT`] or in low-power mode | one run, no new background commands |
/// | hot | thermal `Serious` or `Critical` | new steps held |
/// | metered | the network is metered | a clone or a large fetch is asked first |
///
/// A rule only ever lowers the budget, and a switch turned off removes its row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceRules {
    low_battery: bool,
    hot: bool,
    metered: bool,
}

impl Default for DeviceRules {
    fn default() -> Self {
        Self::ALL
    }
}

impl DeviceRules {
    /// Every rule on: the default.
    pub const ALL: Self = Self {
        low_battery: true,
        hot: true,
        metered: true,
    };

    /// Every rule off: the device never lowers the budget.
    pub const OFF: Self = Self {
        low_battery: false,
        hot: false,
        metered: false,
    };

    /// Turn the low-battery rule on or off.
    #[must_use]
    pub const fn low_battery(mut self, on: bool) -> Self {
        self.low_battery = on;
        self
    }

    /// Turn the hot-device rule on or off.
    #[must_use]
    pub const fn hot(mut self, on: bool) -> Self {
        self.hot = on;
        self
    }

    /// Turn the metered-network rule on or off.
    #[must_use]
    pub const fn metered(mut self, on: bool) -> Self {
        self.metered = on;
        self
    }

    /// What `readings` ask the budget to give up under these rules.
    #[must_use]
    pub fn constraint(&self, readings: &DeviceReadings) -> Constraint {
        let mut constraint = Constraint::NONE;
        if self.low_battery && readings.power.is_low(LOW_BATTERY_PERCENT) {
            constraint = constraint.with_max_runs(1).with_no_background_commands();
        }
        if self.hot && readings.thermal.is_hot() {
            constraint = constraint.with_hold_new_steps();
        }
        if self.metered && readings.network.is_metered() {
            constraint = constraint.with_ask_before_large_fetch();
        }
        constraint
    }
}

/// How often a waiting ask looks again at the device, which may have changed without a release.
const POLL: Duration = Duration::from_millis(50);

/// What [`Refused::Busy`] says to wait when a device rule held an ask for its whole wait.
const BUSY_RETRY: Duration = Duration::from_millis(250);

fn busy(pool: Pool) -> Refused {
    Refused::Busy {
        pool,
        retry_after: Some(BUSY_RETRY),
    }
}

#[derive(Default)]
struct RunGate {
    held: Mutex<u32>,
    changed: Notify,
}

impl RunGate {
    fn try_take(&self, units: u32, cap: Option<u32>) -> bool {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let fits = match cap {
            None => true,
            Some(cap) => *held == 0 || held.saturating_add(units) <= cap,
        };
        if fits {
            *held = held.saturating_add(units);
        }
        fits
    }
}

struct RunSeat {
    gate: Arc<RunGate>,
    units: u32,
}

impl Drop for RunSeat {
    fn drop(&mut self) {
        {
            let mut held = self
                .gate
                .held
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *held = held.saturating_sub(self.units);
        }
        self.gate.changed.notify_waiters();
    }
}

/// An [`Admission`] that lowers another by the device's readings (ADR 0065 decision 8).
///
/// At every [`admit`](Admission::admit) it reads the live readings, maps them through its
/// [`DeviceRules`] and enforces the [`Constraint`]:
///
/// * **One run.** While the constraint caps runs, a [`Pool::Runs`] ask waits for the runs it
///   already admitted, whatever the device said when they started, and is [`Refused::Busy`] after
///   its wait. The cap is never below one: a run on its own is always admitted.
/// * **New steps held.** While the device is hot, a [`Pool::ModelCalls`] or [`Pool::ToolCalls`]
///   ask waits for it to cool and is [`Refused::Busy`] after its wait. A call already in flight
///   is not touched, and a run already admitted is held at its next step, not stopped.
///
/// What an [`Ask`] cannot say is not enforced: see [`Constraint::no_background_commands`] and
/// [`Constraint::ask_before_large_fetch`]. Every other ask passes to the inner admission, with the
/// time left of its wait.
pub struct Throttled<A> {
    inner: A,
    rules: Mutex<DeviceRules>,
    device: DynDeviceState,
    runs: Arc<RunGate>,
}

impl<A: Admission> Throttled<A> {
    /// `inner`, lowered by `device`'s readings under `rules`.
    pub fn new(inner: A, rules: DeviceRules, device: DynDeviceState) -> Self {
        Self {
            inner,
            rules: Mutex::new(rules),
            device,
            runs: Arc::new(RunGate::default()),
        }
    }

    /// Replace the rules from the next ask on.
    pub fn set_rules(&self, rules: DeviceRules) {
        *self.rules.lock().unwrap_or_else(PoisonError::into_inner) = rules;
    }

    /// The rules now.
    #[must_use]
    pub fn rules(&self) -> DeviceRules {
        *self.rules.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the device asks of the budget now.
    #[must_use]
    pub fn constraint(&self) -> Constraint {
        self.rules().constraint(&self.device.readings())
    }

    async fn hold_steps(&self, ask: &Ask, started: Instant) -> Result<(), Refused> {
        while self.constraint().hold_new_steps() {
            let left = ask.wait.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Err(busy(ask.pool));
            }
            tokio::time::sleep(left.min(POLL)).await;
        }
        Ok(())
    }

    async fn take_run(&self, ask: &Ask, started: Instant) -> Result<RunSeat, Refused> {
        let units = ask.units.max(1);
        loop {
            let mut notified = std::pin::pin!(self.runs.changed.notified());
            notified.as_mut().enable();
            if self.runs.try_take(units, self.constraint().max_runs()) {
                return Ok(RunSeat {
                    gate: Arc::clone(&self.runs),
                    units,
                });
            }
            let left = ask.wait.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Err(busy(ask.pool));
            }
            let _ = tokio::time::timeout(left.min(POLL), notified).await;
        }
    }
}

#[async_trait]
impl<A: Admission> Admission for Throttled<A> {
    async fn admit(&self, mut ask: Ask) -> Result<Permit, Refused> {
        let started = Instant::now();
        let wait = ask.wait;
        let mut seat = None;
        match ask.pool {
            Pool::ModelCalls | Pool::ToolCalls => self.hold_steps(&ask, started).await?,
            Pool::Runs => seat = Some(self.take_run(&ask, started).await?),
            _ => {}
        }
        ask.wait = wait.saturating_sub(started.elapsed());
        let permit = self.inner.admit(ask).await?;
        match seat {
            None => Ok(permit),
            Some(seat) => Ok(Permit::new(
                permit.pool(),
                permit.conversation().clone(),
                permit.units(),
                move || {
                    drop(permit);
                    drop(seat);
                },
            )),
        }
    }

    fn snapshot(&self) -> Usage {
        self.inner.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_refusal_has_a_class_and_no_new_one() {
        let busy = Refused::Busy {
            pool: Pool::ToolCalls,
            retry_after: Some(Duration::from_millis(50)),
        };
        assert_eq!(busy.class(), ErrorClass::RateLimited);
        assert_eq!(busy.retry_after(), Some(Duration::from_millis(50)));
        assert!(busy.is_retryable());
        let tokens = Refused::TokensPerMinute {
            retry_after: Duration::from_secs(1),
        };
        assert_eq!(tokens.class(), ErrorClass::RateLimited);
        assert_eq!(tokens.retry_after(), Some(Duration::from_secs(1)));
        let disk = Refused::DiskFull { used: 9, limit: 8 };
        assert_eq!(disk.class(), ErrorClass::Rejected);
        assert_eq!(disk.retry_after(), None);
        let large = Refused::TooLarge {
            what: "a run",
            asked: 9,
            limit: 8,
        };
        assert_eq!(large.class(), ErrorClass::Invalid);
        assert!(!large.is_retryable());
        assert!(large.to_string().contains("over the limit of 8"), "{large}");
    }

    #[test]
    fn a_permit_releases_once_and_frees_its_share_on_drop() {
        let released = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = Arc::clone(&released);
        let permit = Permit::new(Pool::Runs, "c", 1, move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        assert_eq!(permit.pool(), Pool::Runs);
        assert_eq!(permit.units(), 1);
        assert_eq!(permit.share().conversation.as_str(), "c");
        drop(permit);
        assert_eq!(released.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_usage_adds_up_by_pool() {
        let usage = Usage::new(
            [
                PoolUsage::new(Pool::ToolCalls, Limit::AtMost(4), 3, 1),
                PoolUsage::new(Pool::ModelCalls, Limit::AtMost(2), 9, 0),
            ],
            Some(TokenUsage::new(100, 250)),
        );
        let calls = usage.pool(Pool::ToolCalls).expect("listed");
        assert_eq!(
            (calls.in_use(), calls.limit(), calls.waiting()),
            (3, Limit::AtMost(4), 1)
        );
        let models = usage.pool(Pool::ModelCalls).expect("listed");
        assert_eq!(
            models.in_use(),
            2,
            "a report never says more in use than the ceiling"
        );
        assert_eq!(usage.tokens().map(TokenUsage::available), Some(100));
        assert_eq!(usage.pool(Pool::Git), None);
    }

    #[test]
    fn the_rule_table_lowers_by_each_reading_and_never_below_one_run() {
        use adam_device_state::{MemoryPressure, Network, Power, Thermal};

        let on = DeviceRules::default();
        assert_eq!(on.constraint(&DeviceReadings::NOMINAL), Constraint::NONE);
        let low = |power| DeviceReadings {
            power,
            ..DeviceReadings::NOMINAL
        };
        for power in [Power::on_battery(19, false), Power::on_battery(90, true)] {
            let constraint = on.constraint(&low(power));
            assert_eq!(constraint.max_runs(), Some(1), "{power}");
            assert!(constraint.no_background_commands(), "{power}");
        }
        assert_eq!(
            on.constraint(&low(Power::on_battery(20, false))),
            Constraint::NONE
        );
        assert_eq!(Constraint::NONE.with_max_runs(0).max_runs(), Some(1));
        let hot = DeviceReadings {
            thermal: Thermal::Serious,
            ..DeviceReadings::NOMINAL
        };
        assert!(on.constraint(&hot).hold_new_steps());
        assert_eq!(on.constraint(&hot).max_runs(), None);
        let metered = DeviceReadings {
            network: Network::Metered,
            memory_pressure: MemoryPressure::Normal,
            ..DeviceReadings::NOMINAL
        };
        assert!(on.constraint(&metered).ask_before_large_fetch());
        let all = DeviceReadings::new(
            Power::on_battery(5, true),
            Thermal::Critical,
            Network::Metered,
            MemoryPressure::Critical,
        );
        assert_ne!(on.constraint(&all), Constraint::NONE);
        assert_eq!(DeviceRules::OFF.constraint(&all), Constraint::NONE);
        assert_eq!(
            on.low_battery(false)
                .constraint(&low(Power::on_battery(1, true))),
            Constraint::NONE
        );
        assert_eq!(on.hot(false).constraint(&hot), Constraint::NONE);
        assert_eq!(on.metered(false).constraint(&metered), Constraint::NONE);
    }

    #[test]
    fn a_pool_names_itself_for_a_metric() {
        assert_eq!(Pool::ModelCalls.as_str(), "model_calls");
        assert_eq!(Pool::CommandMemory.unit(), "a MiB of command memory");
        assert_eq!(Pool::ALL.len(), 7);
    }
}
