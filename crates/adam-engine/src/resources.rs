//! The resource report: what the process's pools hold, and what the host's device says.
//!
//! [ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
//! decision 9: the engine emits one report and a host shows it on request. It names no person, no
//! run and no conversation: a conversation's share is enforced, never reported. The report is
//! assembled from the process's [`Admission`](adam_budget::Admission) and, when the host pushed
//! one, from its [`DeviceState`](adam_device_state::DeviceState).

use adam_budget::{Budget, Pool, PoolUsage, TokenUsage, Usage};
use adam_device_state::DeviceReadings;

/// What a process and its host use now: one line per pool of the process's budget, the token
/// bucket when it meters one, and the device's readings when the host supplies them.
///
/// An absent limit is [`Limit::Unlimited`](adam_budget::Limit::Unlimited), never a zero: a
/// deployment that sets nothing admits everything its machine allows, and a report says so.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResourceReport {
    usage: Usage,
    device: Option<DeviceReadings>,
}

impl ResourceReport {
    /// A report of `usage` and, when the host pushed a device, its readings.
    #[must_use]
    pub fn new(usage: Usage, device: Option<DeviceReadings>) -> Self {
        Self { usage, device }
    }

    /// The pools, and the token bucket when the budget meters one.
    #[must_use]
    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    /// One pool's line, when the report lists it.
    #[must_use]
    pub fn pool(&self, pool: Pool) -> Option<&PoolUsage> {
        self.usage.pool(pool)
    }

    /// The token bucket, when the budget meters tokens per minute.
    #[must_use]
    pub fn tokens(&self) -> Option<&TokenUsage> {
        self.usage.tokens()
    }

    /// The host's readings, when it pushed a device; `None` on a host that cannot say, which is
    /// not the same as a device on mains and cool.
    #[must_use]
    pub fn device(&self) -> Option<&DeviceReadings> {
        self.device.as_ref()
    }

    /// Every pool with no ceiling and nothing held: what a process that was given no admission
    /// reports.
    pub(crate) fn unlimited() -> Usage {
        Usage::new(
            Pool::ALL.map(|pool| PoolUsage::new(pool, Budget::UNLIMITED.ceiling(pool), 0, 0)),
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use adam_budget::Limit;
    use adam_device_state::{MemoryPressure, Network, Power, Thermal};

    use super::*;

    /// ADR 0065 decision 9: a process with no admission reports `unlimited` for every pool, never
    /// a zero ceiling.
    #[test]
    fn an_unlimited_report_says_so_and_holds_nothing() {
        let report = ResourceReport::new(
            ResourceReport::unlimited(),
            Some(DeviceReadings::new(
                Power::on_battery(12, false),
                Thermal::Serious,
                Network::Metered,
                MemoryPressure::Warn,
            )),
        );
        for pool in Pool::ALL {
            let line = report.pool(pool).expect("every pool is listed");
            assert_eq!(line.limit(), Limit::Unlimited, "{pool}");
            assert_eq!(line.in_use(), 0, "{pool}");
            assert_eq!(line.waiting(), 0, "{pool}");
        }
        assert!(report.tokens().is_none(), "no budget meters tokens");
        assert_eq!(
            report.device().map(|readings| readings.power),
            Some(Power::on_battery(12, false))
        );
        assert_eq!(
            ResourceReport::new(ResourceReport::unlimited(), None).device(),
            None
        );
    }
}
