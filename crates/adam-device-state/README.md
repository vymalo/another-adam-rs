# adam-device-state

The device-state port of adam-rs: the four readings a host pushes about its own machine, and the
in-process fake they are tested with.

[ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
decision 8, slice 5.

## What is in it

| Item | Purpose |
|---|---|
| `DeviceState` | The port: `fn readings(&self) -> DeviceReadings`. One call for the four readings, so the engine decides on one moment, not on four reads another thread may have changed between. `Send + Sync + 'static`. |
| `DynDeviceState` | `Arc<dyn DeviceState>`, what an engine is given. |
| `DeviceReadings` | The four readings together: `power`, `thermal`, `network`, `memory_pressure`. `DeviceReadings::NOMINAL` is on mains, cool, unmetered, memory normal. |
| `Power` | `OnMains` or `OnBattery { percent, low_power }`. `percent()`, `low_power()`, `is_charging()`, `is_low(threshold)` (on battery below the threshold, or in low-power mode). |
| `Thermal` | `Nominal`, `Fair`, `Serious`, `Critical`; `is_hot()` is `Serious` or `Critical`. |
| `Network` | `Unmetered`, `Metered`, `Offline`; `is_metered()`, `is_offline()`. |
| `MemoryPressure` | `Normal`, `Warn`, `Critical`; `is_critical()`. |
| `FakeDevice` | The in-process double: `new(readings)`, `set(readings)` and one setter per reading. It is the reference of the conformance suite. |

The value names and the two fields of `Power` (`percent`, `low_power`) are the ADR's, verbatim. The
four enums are `#[non_exhaustive]`, like every closed value a hosting platform may extend.

```rust
use std::sync::Arc;

use adam_device_state::{DeviceReadings, DeviceState, DynDeviceState, FakeDevice, Power};

let device = Arc::new(FakeDevice::new(DeviceReadings::NOMINAL));
device.set_power(Power::on_battery(12, false));
let state: DynDeviceState = device;
assert!(state.readings().power.is_low(20));
```

## Scope

The crate is pure: no platform API, no I/O, no clock of the wall. Reading the real battery, thermal
state, link cost or memory pressure needs the host's own platform calls and is a later slice;
`FakeDevice` is the device a test, and a host with no reader, uses. The rule table that lowers the
live budget by these readings is `adam_budget::DeviceRules`, and `Engine::set_device_state` applies it.

A server has no device state: it sets its own ceiling (ADR 0065 decision 1). A host that pushes
nothing reads as `DeviceReadings::NOMINAL`, so nothing is held back on a machine that cannot say.

## Environment

None.

## Tests

`cargo test -p adam-device-state` runs, in `tests/conformance.rs`, the suite
[`device_state_conformance!`](../adam-device-state-testkit/README.md) against `FakeDevice`, plus:

* `a_push_changes_one_reading_and_leaves_the_others`;
* `a_fake_on_battery_says_the_charge_is_low` and `a_hot_fake_says_the_device_is_hot`, the
  predicates `adam_budget::DeviceRules` reads;
* `a_push_on_one_thread_is_read_on_another`.

The unit tests cover the charge's clamp, `is_low` on mains and in low-power mode, the labels, and
the fake's setters.
