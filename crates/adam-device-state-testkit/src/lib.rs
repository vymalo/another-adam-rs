//! The conformance suite of [`adam_device_state::DeviceState`], and the [`Plan`] a test reads with.
//!
//! [`device_state_conformance!`] writes the cases of ADR 0065 decision 8 into a module of the
//! implementing crate:
//!
//! * a pushed set of readings is what the port reports;
//! * every value the ADR names is reported faithfully, in every combination;
//! * reading twice gives the same answers;
//! * one thread reads what the host pushed for another.
//!
//! Each case is a function here, so the suite is compiled once and the macro only names it.
//!
//! ```text
//! use std::sync::Arc;
//!
//! use adam_device_state::{DynDeviceState, FakeDevice};
//! use adam_device_state_testkit::Plan;
//!
//! fn make(plan: &Plan) -> DynDeviceState {
//!     Arc::new(FakeDevice::new(plan.readings))
//! }
//!
//! adam_device_state_testkit::device_state_conformance!(crate::make);
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // a test kit asserts by unwrapping

pub use adam_device_state::{
    DeviceReadings, DynDeviceState, MemoryPressure, Network, Power, Thermal,
};

/// What one conformance run reads: the set of readings the host pushes.
///
/// A real implementation reads its platform; a test implementation is built by the `$make` function
/// from this plan, so the suite can ask for every value the ADR names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Plan {
    /// The readings the implementation must report.
    pub readings: DeviceReadings,
}

impl Plan {
    /// A plan that pushes `readings`.
    #[must_use]
    pub fn new(readings: DeviceReadings) -> Self {
        Self { readings }
    }

    /// Every combination of the values ADR 0065 decision 8 names: five power readings, four
    /// thermal states, three network states and three memory pressures.
    ///
    /// The cross product is on purpose: an implementation that drops one field, or maps one value
    /// onto its neighbour, fails on one combination even when the others hide it.
    #[must_use]
    pub fn every() -> Vec<Self> {
        let powers = [
            Power::OnMains,
            Power::on_battery(100, false),
            Power::on_battery(19, false),
            Power::on_battery(50, true),
            Power::on_battery(0, true),
        ];
        let thermals = [
            Thermal::Nominal,
            Thermal::Fair,
            Thermal::Serious,
            Thermal::Critical,
        ];
        let networks = [Network::Unmetered, Network::Metered, Network::Offline];
        let pressures = [
            MemoryPressure::Normal,
            MemoryPressure::Warn,
            MemoryPressure::Critical,
        ];
        let mut plans =
            Vec::with_capacity(powers.len() * thermals.len() * networks.len() * pressures.len());
        for power in powers {
            for thermal in thermals {
                for network in networks {
                    for memory_pressure in pressures {
                        plans.push(Self::new(DeviceReadings::new(
                            power,
                            thermal,
                            network,
                            memory_pressure,
                        )));
                    }
                }
            }
        }
        plans
    }
}

impl Default for Plan {
    /// A device with nothing to conserve: [`DeviceReadings::NOMINAL`].
    fn default() -> Self {
        Self::new(DeviceReadings::NOMINAL)
    }
}

/// A pushed set of readings is what the port reports, field by field.
///
/// # Panics
///
/// When one of the four readings differs from the plan's.
pub fn reports_the_pushed_readings(device: DynDeviceState, plan: Plan) {
    let readings = device.readings();
    assert_eq!(
        readings, plan.readings,
        "the port reported {readings:?}, not the pushed {:?}",
        plan.readings
    );
    assert_eq!(readings.power, plan.readings.power, "power");
    assert_eq!(readings.thermal, plan.readings.thermal, "thermal");
    assert_eq!(readings.network, plan.readings.network, "network");
    assert_eq!(
        readings.memory_pressure, plan.readings.memory_pressure,
        "memory pressure"
    );
}

/// Every value ADR 0065 decision 8 names is reported faithfully, in every combination.
///
/// # Panics
///
/// When the implementation reports a reading the plan did not push, or loses a field.
pub fn every_named_reading_is_reported(make: fn(&Plan) -> DynDeviceState) {
    for plan in Plan::every() {
        let device = make(&plan);
        let readings = device.readings();
        assert_eq!(
            readings, plan.readings,
            "the port reported {readings:?} for the pushed {:?}",
            plan.readings
        );
    }
}

/// Reading twice, and again, gives the same answers.
///
/// A port that fabricated a value, or that cycled through one, would pass a single read and fail
/// here.
///
/// # Panics
///
/// When two reads of an unchanged device disagree.
pub fn reading_twice_agrees(device: DynDeviceState, plan: Plan) {
    let first = device.readings();
    assert_eq!(first, plan.readings);
    for _ in 0..3 {
        let again = device.readings();
        assert_eq!(again, first, "a later read disagreed with the first");
    }
}

/// A reader on another thread sees the same readings: the port is `Send + Sync`.
///
/// # Panics
///
/// When the other thread reads something else, or the thread cannot be joined.
pub fn another_thread_reads_the_same(device: DynDeviceState, plan: Plan) {
    let seen = std::thread::spawn(move || device.readings())
        .join()
        .expect("the reading thread does not panic");
    assert_eq!(seen, plan.readings);
}

/// Writes the conformance cases of ADR 0065 decision 8 into a `mod device_state_conformance` of the
/// crate that calls it, for the device `$make` builds from a [`Plan`].
///
/// `$make` is a path to a `fn(&Plan) -> DynDeviceState`; in a test file of a crate, that is
/// `crate::make_device`. The cases are synchronous, so the calling crate needs no runtime.
#[macro_export]
macro_rules! device_state_conformance {
    ($make:path) => {
        #[allow(missing_docs)]
        mod device_state_conformance {
            use super::*;
            use $crate::Plan;

            fn make(plan: &Plan) -> $crate::DynDeviceState {
                $make(plan)
            }

            #[test]
            fn reports_the_pushed_readings() {
                $crate::reports_the_pushed_readings(make(&Plan::default()), Plan::default());
            }

            #[test]
            fn every_named_reading_is_reported() {
                $crate::every_named_reading_is_reported(make);
            }

            #[test]
            fn reading_twice_agrees() {
                let plan = Plan::new($crate::DeviceReadings::new(
                    $crate::Power::on_battery(7, true),
                    $crate::Thermal::Critical,
                    $crate::Network::Metered,
                    $crate::MemoryPressure::Critical,
                ));
                $crate::reading_twice_agrees(make(&plan), plan);
            }

            #[test]
            fn another_thread_reads_the_same() {
                let plan = Plan::default();
                $crate::another_thread_reads_the_same(make(&plan), plan);
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_covers_every_value_of_the_adr() {
        let plans = Plan::every();
        assert_eq!(plans.len(), 5 * 4 * 3 * 3);
        let powers: std::collections::BTreeSet<String> =
            plans.iter().map(|p| p.readings.power.to_string()).collect();
        assert_eq!(
            powers.len(),
            5,
            "mains, three battery charges and low power"
        );
        assert!(
            plans
                .iter()
                .any(|p| p.readings.thermal == Thermal::Critical)
        );
        assert!(
            plans
                .iter()
                .any(|p| p.readings.memory_pressure == MemoryPressure::Warn)
        );
        assert_eq!(Plan::default().readings, DeviceReadings::NOMINAL);
    }
}
