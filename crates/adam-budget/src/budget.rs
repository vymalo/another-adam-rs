//! The ceiling of a process's resources, and the only way to lower it.

use std::fmt;

use crate::admission::Pool;

/// A ratio in ten-thousandths: `10_000` is one whole, `5_000` one half.
///
/// Ratios carry no `f64` for the same reason money and time do not: a budget is compared, lowered
/// and reported, and a float compares inexactly at the boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ratio(u16);

impl Ratio {
    /// One whole, the identity of [`Budget::lower`]'s share: a conversation may hold the whole pool.
    pub const WHOLE: Self = Self(10_000);

    /// One half, the default of [`Budget::per_conversation_share`].
    pub const HALF: Self = Self(5_000);

    /// `per_ten_thousand` of a whole, when it is at most `10_000`.
    #[must_use]
    pub const fn new(per_ten_thousand: u16) -> Option<Self> {
        if (per_ten_thousand as u32) <= 10_000 {
            Some(Self(per_ten_thousand))
        } else {
            None
        }
    }

    /// The ratio in ten-thousandths.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// `value` times the ratio, rounded down. Computed without overflow at any `u64`.
    #[must_use]
    pub const fn apply_u64(self, value: u64) -> u64 {
        (value / 10_000) * self.0 as u64 + ((value % 10_000) * self.0 as u64) / 10_000
    }

    /// `value` times the ratio, rounded down.
    #[must_use]
    pub const fn apply_u32(self, value: u32) -> u32 {
        self.apply_u64(value as u64) as u32
    }
}

impl Default for Ratio {
    fn default() -> Self {
        Self::WHOLE
    }
}

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}%", self.0 / 100, self.0 % 100)
    }
}

/// What a disk budget always leaves free: a fixed number of bytes, or a proportion of the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DiskReserve {
    /// At least this many bytes stay free.
    Bytes(u64),
    /// At least this proportion of the disk stays free.
    Ratio(Ratio),
}

impl DiskReserve {
    /// The identity: no reserve, which lowers to anything.
    pub const NONE: Self = Self::Bytes(0);

    /// How much must stay free of a disk of `disk` bytes.
    #[must_use]
    pub const fn of_disk(self, disk: u64) -> u64 {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::Ratio(ratio) => ratio.apply_u64(disk),
        }
    }
}

impl Default for DiskReserve {
    fn default() -> Self {
        Self::NONE
    }
}

impl fmt::Display for DiskReserve {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes(bytes) => write!(f, "{bytes} bytes"),
            Self::Ratio(ratio) => write!(f, "{ratio} of the disk"),
        }
    }
}

/// A pool's ceiling in a report: a number of its unit, or no ceiling at all.
///
/// An absent limit is not a limit of zero: a deployment that sets nothing leaves every pool
/// unlimited, and a report says so (ADR 0065 decision 9). [`Limit::AtMost(0)`](Self::AtMost) is
/// the other thing: a ceiling a deployment closes on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Limit {
    /// No ceiling: [`Budget::UNLIMITED`]'s value for the pool, which [`Budget::lower`] cannot make
    /// smaller.
    Unlimited,
    /// At most this many, in the pool's unit.
    AtMost(u64),
}

impl Limit {
    /// The number, when there is one; `None` when the pool is unlimited.
    #[must_use]
    pub const fn at_most(self) -> Option<u64> {
        match self {
            Self::Unlimited => None,
            Self::AtMost(limit) => Some(limit),
        }
    }

    /// Whether no ceiling was set.
    #[must_use]
    pub const fn is_unlimited(self) -> bool {
        matches!(self, Self::Unlimited)
    }

    /// One count field of a [`Budget`]: its type's maximum is the identity of
    /// [`Budget::lower`], and reads as unlimited.
    const fn of_count(count: u32) -> Self {
        if count == u32::MAX {
            Self::Unlimited
        } else {
            Self::AtMost(count as u64)
        }
    }
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unlimited => f.write_str("unlimited"),
            Self::AtMost(limit) => write!(f, "{limit}"),
        }
    }
}

/// One ceiling for one process: what may run at once, and how much of the machine.
///
/// It is [`non_exhaustive`](https://doc.rust-lang.org/reference/attributes/type_system.html) and
/// built with [`Budget::builder`] from [`Budget::UNLIMITED`], the identity of [`lower`](Self::lower).
/// The deployment (or the device) builds the ceiling; a file or a person may only lower it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Budget {
    /// Runs advanced at the same time (`WORKERS`, [`Pool::Runs`]).
    pub runs: u32,
    /// Tool calls in flight in the whole process (`MAX_TOOL_CALLS_IN_FLIGHT`, [`Pool::ToolCalls`]).
    pub tool_calls: u32,
    /// Processes at once: commands, git, servers (`MAX_PROCESSES`, [`Pool::Processes`]).
    pub processes: u32,
    /// git processes (`MAX_GIT_PROCESSES`, [`Pool::Git`]).
    pub git: u32,
    /// MCP servers alive (`MAX_MCP_SERVERS`, [`Pool::McpServers`]).
    pub mcp_servers: u32,
    /// Model calls in flight (`MAX_MODEL_CALLS`, [`Pool::ModelCalls`]).
    pub model_calls: u32,
    /// A model's tokens per minute (`MODEL_TOKENS_PER_MINUTE`); `None` meters none.
    pub tokens_per_minute: Option<u32>,
    /// The memory one command may use, in bytes (`COMMAND_MEMORY_MAX`, [`Pool::CommandMemory`]).
    pub command_memory: u64,
    /// The CPU one command may use, in millicores (`COMMAND_CPU_MAX`).
    pub command_cpu: u32,
    /// The disk workspaces and the cache may use, in bytes (`WORKSPACE_DISK_MAX`).
    pub disk: u64,
    /// The free space the disk budget always keeps (`WORKSPACE_DISK_RESERVE`).
    pub disk_reserve: DiskReserve,
    /// The most of a pool one conversation may hold at once: `1/2`, so no conversation starves
    /// another.
    pub per_conversation_share: Ratio,
}

impl Budget {
    /// The identity of [`lower`](Self::lower): every ceiling as high as its type allows, no token
    /// rate, no reserve, and a conversation may hold a whole pool. Lowering it by a budget gives
    /// that budget, and lowering that budget by it gives it back unchanged.
    pub const UNLIMITED: Self = Self {
        runs: u32::MAX,
        tool_calls: u32::MAX,
        processes: u32::MAX,
        git: u32::MAX,
        mcp_servers: u32::MAX,
        model_calls: u32::MAX,
        tokens_per_minute: None,
        command_memory: u64::MAX,
        command_cpu: u32::MAX,
        disk: u64::MAX,
        disk_reserve: DiskReserve::NONE,
        per_conversation_share: Ratio::WHOLE,
    };

    /// A builder that starts from [`UNLIMITED`](Self::UNLIMITED): set what the ceiling is.
    pub fn builder() -> BudgetBuilder {
        BudgetBuilder {
            budget: Self::UNLIMITED,
        }
    }

    /// The lower of two budgets, field by field: the only way to combine a deployment's ceiling
    /// with a folder's or a person's, so that neither can raise what the other allows.
    ///
    /// * Every count, byte count and millicore takes the smaller value.
    /// * [`tokens_per_minute`](Self::tokens_per_minute): `None` meters nothing, so it is the
    ///   identity; otherwise the smaller rate.
    /// * [`disk_reserve`](Self::disk_reserve) takes the **stricter** value, the one that keeps
    ///   more free: lowering a budget must not free less disk. Two fixed reserves take the larger;
    ///   two ratios the larger. A fixed reserve against a ratio cannot be compared without the
    ///   disk, so the ratio is kept: it follows the disk.
    /// * [`per_conversation_share`](Self::per_conversation_share) takes the smaller share.
    #[must_use]
    pub fn lower(&self, other: &Self) -> Self {
        Self {
            runs: self.runs.min(other.runs),
            tool_calls: self.tool_calls.min(other.tool_calls),
            processes: self.processes.min(other.processes),
            git: self.git.min(other.git),
            mcp_servers: self.mcp_servers.min(other.mcp_servers),
            model_calls: self.model_calls.min(other.model_calls),
            tokens_per_minute: match (self.tokens_per_minute, other.tokens_per_minute) {
                (None, other) | (other, None) => other,
                (Some(a), Some(b)) => Some(a.min(b)),
            },
            command_memory: self.command_memory.min(other.command_memory),
            command_cpu: self.command_cpu.min(other.command_cpu),
            disk: self.disk.min(other.disk),
            disk_reserve: stricter_reserve(self.disk_reserve, other.disk_reserve),
            per_conversation_share: self
                .per_conversation_share
                .min(other.per_conversation_share),
        }
    }

    /// The ceiling of `pool`, in the pool's own unit: a count, or MiB for
    /// [`Pool::CommandMemory`]. `0` admits nothing.
    #[must_use]
    pub fn limit(&self, pool: Pool) -> u64 {
        match pool {
            Pool::Runs => self.runs as u64,
            Pool::ModelCalls => self.model_calls as u64,
            Pool::ToolCalls => self.tool_calls as u64,
            Pool::Processes => self.processes as u64,
            Pool::Git => self.git as u64,
            Pool::McpServers => self.mcp_servers as u64,
            // A MiB is the unit of a memory reservation; a budget under one is one.
            Pool::CommandMemory => {
                const MIB: u64 = 1024 * 1024;
                self.command_memory.saturating_add(MIB - 1) / MIB
            }
        }
    }

    /// The ceiling of `pool` as a report reads it: [`Limit::Unlimited`] when this budget leaves
    /// the pool at [`Budget::UNLIMITED`]'s value, its number otherwise.
    ///
    /// The distinction is not cosmetic: [`Budget::limit`] gives the identity a number
    /// (`u32::MAX`, or `u64::MAX` bytes rounded up to MiB), which a host would print as a ceiling
    /// of four billion runs rather than as none.
    #[must_use]
    pub const fn ceiling(&self, pool: Pool) -> Limit {
        match pool {
            Pool::Runs => Limit::of_count(self.runs),
            Pool::ModelCalls => Limit::of_count(self.model_calls),
            Pool::ToolCalls => Limit::of_count(self.tool_calls),
            Pool::Processes => Limit::of_count(self.processes),
            Pool::Git => Limit::of_count(self.git),
            Pool::McpServers => Limit::of_count(self.mcp_servers),
            Pool::CommandMemory => {
                if self.command_memory == u64::MAX {
                    Limit::Unlimited
                } else {
                    const MIB: u64 = 1024 * 1024;
                    Limit::AtMost(self.command_memory.saturating_add(MIB - 1) / MIB)
                }
            }
        }
    }
}

/// The stricter of two reserves: see [`Budget::lower`].
fn stricter_reserve(a: DiskReserve, b: DiskReserve) -> DiskReserve {
    match (a, b) {
        (DiskReserve::Bytes(a), DiskReserve::Bytes(b)) => DiskReserve::Bytes(a.max(b)),
        (DiskReserve::Ratio(a), DiskReserve::Ratio(b)) => DiskReserve::Ratio(a.max(b)),
        (DiskReserve::Ratio(r), DiskReserve::Bytes(_))
        | (DiskReserve::Bytes(_), DiskReserve::Ratio(r)) => DiskReserve::Ratio(r),
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::UNLIMITED
    }
}

/// Builds a [`Budget`], starting from [`Budget::UNLIMITED`].
///
/// A setter that takes a `u32` sets a count; the ones for bytes and millicores say so. Nothing is
/// clamped: a value of `0` is a ceiling that admits nothing, which a deployment may mean and a
/// test needs.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "a builder does nothing until it is built"]
pub struct BudgetBuilder {
    budget: Budget,
}

macro_rules! count_setter {
    ($($name:ident),* $(,)?) => {
        $(
            #[doc = concat!("Set `", stringify!($name), "`.")]
            pub fn $name(mut self, n: u32) -> Self {
                self.budget.$name = n;
                self
            }
        )*
    };
}

impl BudgetBuilder {
    count_setter!(
        runs,
        tool_calls,
        processes,
        git,
        mcp_servers,
        model_calls,
        command_cpu,
    );

    /// Set the model's tokens per minute.
    pub fn tokens_per_minute(mut self, tokens: u32) -> Self {
        self.budget.tokens_per_minute = Some(tokens);
        self
    }

    /// Meter no tokens per minute (the identity).
    pub fn no_token_limit(mut self) -> Self {
        self.budget.tokens_per_minute = None;
        self
    }

    /// Set the memory one command may use, in bytes.
    pub fn command_memory(mut self, bytes: u64) -> Self {
        self.budget.command_memory = bytes;
        self
    }

    /// Set the disk workspaces and the cache may use, in bytes.
    pub fn disk(mut self, bytes: u64) -> Self {
        self.budget.disk = bytes;
        self
    }

    /// Set the free space the disk budget keeps.
    pub fn disk_reserve(mut self, reserve: DiskReserve) -> Self {
        self.budget.disk_reserve = reserve;
        self
    }

    /// Set the most of a pool one conversation may hold.
    pub fn per_conversation_share(mut self, share: Ratio) -> Self {
        self.budget.per_conversation_share = share;
        self
    }

    /// Set the ceiling of one pool, whatever [`Pool`] it is: a deployment's variables set them one
    /// by one.
    pub fn pool_limit(mut self, pool: Pool, limit: u32) -> Self {
        match pool {
            Pool::Runs => self.budget.runs = limit,
            Pool::ModelCalls => self.budget.model_calls = limit,
            Pool::ToolCalls => self.budget.tool_calls = limit,
            Pool::Processes => self.budget.processes = limit,
            Pool::Git => self.budget.git = limit,
            Pool::McpServers => self.budget.mcp_servers = limit,
            // The memory pool's unit is a MiB; a budget sets bytes.
            Pool::CommandMemory => self.budget.command_memory = u64::from(limit) * 1024 * 1024,
        }
        self
    }

    /// The ceiling.
    #[must_use]
    pub fn build(self) -> Budget {
        self.budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The desktop defaults of ADR 0065 decision 11.
    fn desktop() -> Budget {
        Budget::builder()
            .runs(2)
            .tool_calls(4)
            .processes(4)
            .git(2)
            .mcp_servers(4)
            .model_calls(2)
            .command_memory(2 * 1024 * 1024 * 1024)
            .command_cpu(1_000)
            .disk(20 * 1024 * 1024 * 1024)
            .disk_reserve(DiskReserve::Ratio(Ratio::new(1_000).expect("10%")))
            .per_conversation_share(Ratio::HALF)
            .build()
    }

    #[test]
    fn unlimited_is_the_identity_of_lower() {
        let budget = desktop();
        assert_eq!(Budget::UNLIMITED.lower(&budget), budget);
        assert_eq!(budget.lower(&Budget::UNLIMITED), budget);
        assert_eq!(
            Budget::UNLIMITED.lower(&Budget::UNLIMITED),
            Budget::UNLIMITED
        );
    }

    #[test]
    fn lower_takes_the_smaller_of_every_count_and_rate() {
        let ceiling = desktop();
        let folder = Budget::builder()
            .runs(1)
            .tool_calls(16)
            .model_calls(1)
            .tokens_per_minute(20_000)
            .disk_reserve(DiskReserve::Bytes(1_000))
            .per_conversation_share(Ratio::new(2_500).expect("a quarter"))
            .build();
        let got = ceiling.lower(&folder);
        assert_eq!(got.runs, 1, "a folder lowers runs");
        assert_eq!(got.tool_calls, 4, "a folder cannot raise tool calls");
        assert_eq!(got.model_calls, 1);
        assert_eq!(
            got.processes, 4,
            "a field the folder leaves alone keeps the ceiling's"
        );
        assert_eq!(
            got.per_conversation_share,
            Ratio::new(2_500).expect("a quarter")
        );
        // The only rate is the folder's: `None` is the identity, not zero.
        assert_eq!(got.tokens_per_minute, Some(20_000));
        assert_eq!(ceiling.lower(&ceiling).tokens_per_minute, None);
        // The reserve is the stricter of the two, never the looser: a fixed reserve under a
        // ratio cannot be compared without the disk, so the ratio is kept.
        assert_eq!(
            got.disk_reserve,
            DiskReserve::Ratio(Ratio::new(1_000).expect("10%"))
        );
        assert_eq!(
            stricter_reserve(DiskReserve::Bytes(5), DiskReserve::Bytes(9)),
            DiskReserve::Bytes(9),
            "a fixed reserve is the larger of the two"
        );
        assert_eq!(
            stricter_reserve(
                DiskReserve::Ratio(Ratio::HALF),
                DiskReserve::Ratio(Ratio::new(1_000).expect("10%"))
            ),
            DiskReserve::Ratio(Ratio::HALF),
            "a ratio is the larger of the two"
        );
    }

    #[test]
    fn a_ratio_applies_without_overflow() {
        let half = Ratio::HALF;
        assert_eq!(half.apply_u64(0), 0);
        assert_eq!(half.apply_u64(20), 10);
        assert_eq!(half.apply_u64(3), 1);
        assert_eq!(half.apply_u32(2), 1);
        assert_eq!(Ratio::WHOLE.apply_u64(u64::MAX), u64::MAX);
        assert_eq!(Ratio::new(10_001), None);
        assert_eq!(DiskReserve::Ratio(half).of_disk(1_000), 500);
    }

    #[test]
    fn a_pool_limit_is_its_field_in_its_unit() {
        let budget = desktop();
        assert_eq!(budget.limit(Pool::ToolCalls), 4);
        assert_eq!(budget.limit(Pool::CommandMemory), 2_048);
        assert_eq!(
            Budget::UNLIMITED.limit(Pool::CommandMemory),
            u64::MAX / (1024 * 1024),
            "a saturated addition rounds the same way"
        );
        assert_eq!(
            Budget::builder()
                .command_memory(1)
                .build()
                .limit(Pool::CommandMemory),
            1
        );
    }

    #[test]
    fn every_pool_has_a_setter() {
        for pool in Pool::ALL {
            let budget = Budget::builder().pool_limit(pool, 7).build();
            assert_eq!(budget.limit(pool), 7, "{pool}");
        }
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;

    /// ADR 0065 decision 9: an absent limit is not a zero limit. A budget that sets nothing
    /// reports every pool as unlimited, a pool it sets reports its number, and a pool set to zero
    /// stays a zero limit.
    #[test]
    fn no_limit_is_not_a_zero_limit() {
        for pool in Pool::ALL {
            assert_eq!(
                Budget::UNLIMITED.ceiling(pool),
                Limit::Unlimited,
                "{pool} with no limit"
            );
            assert_eq!(
                Budget::builder().build().ceiling(pool),
                Limit::Unlimited,
                "{pool} of an empty budget"
            );
        }
        let budget = Budget::builder()
            .runs(2)
            .command_memory(2 * 1024 * 1024 * 1024)
            .build();
        assert_eq!(budget.ceiling(Pool::Runs), Limit::AtMost(2));
        assert_eq!(budget.ceiling(Pool::ToolCalls), Limit::Unlimited);
        assert_eq!(budget.ceiling(Pool::CommandMemory), Limit::AtMost(2_048));
        assert_eq!(
            Budget::builder().runs(0).build().ceiling(Pool::Runs),
            Limit::AtMost(0),
            "a pool a deployment closes is a limit of zero"
        );
    }

    /// The limit says itself: `unlimited`, never a number a reader could take for a ceiling.
    #[test]
    fn a_limit_names_itself() {
        assert_eq!(Limit::Unlimited.to_string(), "unlimited");
        assert_eq!(Limit::AtMost(4).to_string(), "4");
        assert_eq!(Limit::Unlimited.at_most(), None);
        assert_eq!(Limit::AtMost(4).at_most(), Some(4));
        assert!(Limit::Unlimited.is_unlimited());
        assert!(!Limit::AtMost(0).is_unlimited());
    }
}
