//! The device-state port: what the host's machine can tell adam about itself.
//!
//! [ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
//! decision 8: the host pushes four readings — its **power**, its **thermal** state, whether its
//! **network** is metered and its **memory pressure** — and the engine lowers its live budget by a
//! fixed rule table. A server has no device state; a host that does not push one gets
//! [`DeviceReadings::NOMINAL`], so nothing is held back on a machine that cannot say.
//!
//! The port is [`DeviceState`], one call for the four readings so that a decision sees one moment.
//! The crate is pure: no platform API, no I/O, no clock of the wall. Reading the real device is a
//! later slice of ADR 0065 and belongs to the host; this crate carries the values, the port and
//! [`FakeDevice`], and its conformance is proven by
//! [`device_state_conformance!`](../adam-device-state-testkit/README.md).
//!
//! ```
//! use std::sync::Arc;
//!
//! use adam_device_state::{DeviceReadings, DeviceState, DynDeviceState, FakeDevice, Power};
//!
//! let device = Arc::new(FakeDevice::new(DeviceReadings::NOMINAL));
//! device.set_power(Power::on_battery(12, false));
//! let state: DynDeviceState = device;
//! assert!(state.readings().power.is_low(20));
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod fake;
mod reading;

pub use fake::FakeDevice;
pub use reading::{DeviceReadings, MemoryPressure, Network, Power, Thermal};

use std::fmt;
use std::sync::Arc;

/// Where the device's readings come from: the host's own knowledge, or a test's fake.
///
/// One call returns all four readings, so a decision is made on one moment rather than on four
/// reads another thread may have changed between. An implementation is cheap to call: an engine
/// asks it when it decides how much work to admit, not on a timer.
pub trait DeviceState: Send + Sync + 'static {
    /// The four readings now.
    fn readings(&self) -> DeviceReadings;
}

/// A [`DeviceState`] held behind an `Arc`: what an engine or a runtime is given.
pub type DynDeviceState = Arc<dyn DeviceState>;

impl<T: DeviceState + ?Sized> DeviceState for Arc<T> {
    fn readings(&self) -> DeviceReadings {
        (**self).readings()
    }
}

impl fmt::Debug for dyn DeviceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let readings = self.readings();
        f.debug_struct("DeviceState")
            .field("power", &readings.power)
            .field("thermal", &readings.thermal)
            .field("network", &readings.network)
            .field("memory_pressure", &readings.memory_pressure)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_arc_reads_what_it_holds() {
        let device: DynDeviceState = Arc::new(FakeDevice::new(DeviceReadings::new(
            Power::on_battery(4, true),
            Thermal::Serious,
            Network::Metered,
            MemoryPressure::Warn,
        )));
        let readings = device.readings();
        assert_eq!(readings.thermal, Thermal::Serious);
        assert!(readings.power.is_low(20));
        assert!(format!("{device:?}").contains("Serious"));
    }
}
