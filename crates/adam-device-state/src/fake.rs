//! The in-process device state a test and a host without a platform reader use.

use std::sync::{Mutex, PoisonError};

use crate::DeviceState;
use crate::reading::{DeviceReadings, MemoryPressure, Network, Power, Thermal};

/// A [`DeviceState`] whose readings are pushed by hand.
///
/// It is the double of [`device_state_conformance!`](crate::DeviceState) and the device a test
/// drives: `set_power` before a call, `set_thermal` before the next, so a rule can be read from the
/// outside without a battery, a thermometer or a network. A host with no platform reader of its own
/// starts from [`DeviceReadings::NOMINAL`] and pushes what it learns.
#[derive(Debug, Default)]
pub struct FakeDevice {
    readings: Mutex<DeviceReadings>,
}

impl FakeDevice {
    /// A device that reports `readings` until it is pushed another set.
    #[must_use]
    pub fn new(readings: DeviceReadings) -> Self {
        Self {
            readings: Mutex::new(readings),
        }
    }

    /// Push a whole set of readings.
    pub fn set(&self, readings: DeviceReadings) {
        *self.lock() = readings;
    }

    /// Push the power reading, leaving the other three as they were.
    pub fn set_power(&self, power: Power) {
        self.lock().power = power;
    }

    /// Push the thermal reading, leaving the other three as they were.
    pub fn set_thermal(&self, thermal: Thermal) {
        self.lock().thermal = thermal;
    }

    /// Push the network reading, leaving the other three as they were.
    pub fn set_network(&self, network: Network) {
        self.lock().network = network;
    }

    /// Push the memory-pressure reading, leaving the other three as they were.
    pub fn set_memory_pressure(&self, memory_pressure: MemoryPressure) {
        self.lock().memory_pressure = memory_pressure;
    }

    /// The four readings now.
    #[must_use]
    pub fn readings(&self) -> DeviceReadings {
        *self.lock()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DeviceReadings> {
        self.readings.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl DeviceState for FakeDevice {
    fn readings(&self) -> DeviceReadings {
        Self::readings(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_fake_reports_what_it_was_given() {
        let readings = DeviceReadings::new(
            Power::on_battery(12, false),
            Thermal::Serious,
            Network::Metered,
            MemoryPressure::Warn,
        );
        assert_eq!(FakeDevice::new(readings).readings(), readings);
        assert_eq!(FakeDevice::default().readings(), DeviceReadings::NOMINAL);
    }

    #[test]
    fn one_reading_is_pushed_at_a_time() {
        let device = FakeDevice::default();
        device.set_power(Power::on_battery(8, true));
        device.set_thermal(Thermal::Critical);
        device.set_network(Network::Offline);
        device.set_memory_pressure(MemoryPressure::Critical);
        assert_eq!(
            device.readings(),
            DeviceReadings::new(
                Power::on_battery(8, true),
                Thermal::Critical,
                Network::Offline,
                MemoryPressure::Critical,
            )
        );
        device.set(DeviceReadings::NOMINAL);
        assert!(device.readings().is_nominal());
    }

    #[test]
    fn every_reader_sees_a_push() {
        let device = std::sync::Arc::new(FakeDevice::default());
        let reader = std::sync::Arc::clone(&device);
        device.set_thermal(Thermal::Fair);
        assert_eq!(reader.readings().thermal, Thermal::Fair);
    }
}
