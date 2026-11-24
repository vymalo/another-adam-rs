//! `FakeDevice` against the conformance suite, and the cases the fake adds: a push changes one
//! reading and leaves the others, and the predicates the rule table will read.

use std::sync::Arc;

use adam_device_state::{
    DeviceReadings, DynDeviceState, FakeDevice, MemoryPressure, Network, Power, Thermal,
};
use adam_device_state_testkit::Plan;

/// The device `plan` describes.
fn make(plan: &Plan) -> DynDeviceState {
    Arc::new(FakeDevice::new(plan.readings))
}

adam_device_state_testkit::device_state_conformance!(crate::make);

#[test]
fn a_push_changes_one_reading_and_leaves_the_others() {
    let device = FakeDevice::default();
    device.set_power(Power::on_battery(12, false));
    assert_eq!(device.readings().power.percent(), Some(12));
    assert_eq!(device.readings().thermal, Thermal::Nominal);
    assert_eq!(device.readings().network, Network::Unmetered);
    assert_eq!(device.readings().memory_pressure, MemoryPressure::Normal);
}

#[test]
fn a_fake_on_battery_says_the_charge_is_low() {
    let plan = Plan::new(DeviceReadings::new(
        Power::on_battery(12, false),
        Thermal::Nominal,
        Network::Metered,
        MemoryPressure::Normal,
    ));
    let device = make(&plan);
    let readings = device.readings();
    assert!(readings.power.is_low(20), "12 % is below 20 %");
    assert!(!readings.power.is_charging());
    assert!(readings.network.is_metered());
}

#[test]
fn a_hot_fake_says_the_device_is_hot() {
    let readings = DeviceReadings::new(
        Power::OnMains,
        Thermal::Serious,
        Network::Unmetered,
        MemoryPressure::Normal,
    );
    let device = FakeDevice::new(readings);
    assert!(device.readings().thermal.is_hot());
    device.set_thermal(Thermal::Fair);
    assert!(!device.readings().thermal.is_hot());
}

#[test]
fn a_push_on_one_thread_is_read_on_another() {
    let device = Arc::new(FakeDevice::default());
    let reader = Arc::clone(&device);
    let pushed = std::thread::spawn(move || {
        device.set_memory_pressure(MemoryPressure::Critical);
        device.readings()
    })
    .join()
    .expect("the pushing thread does not panic");
    assert_eq!(pushed.memory_pressure, MemoryPressure::Critical);
    assert_eq!(reader.readings(), pushed);
}
