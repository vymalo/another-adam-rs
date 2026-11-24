# adam-device-state-testkit

The conformance suite of [`adam_device_state::DeviceState`](../adam-device-state/README.md), as the
`device_state_conformance!` macro. Not published (`publish = false`): it is test code.

[ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
decision 8 names the four readings and their values; each case is a function of this crate,
documented where it lives, and the macro writes one `#[test]` per case into a
`mod device_state_conformance` of the crate that calls it, so the suite is compiled once and every
implementation runs the same code. The cases are synchronous: no runtime is needed.

## Using it

```rust
use std::sync::Arc;

use adam_device_state::{DynDeviceState, FakeDevice};
use adam_device_state_testkit::Plan;

fn make(plan: &Plan) -> DynDeviceState {
    // `FakeDevice` here; a host's own reader elsewhere.
    Arc::new(FakeDevice::new(plan.readings))
}

adam_device_state_testkit::device_state_conformance!(crate::make);
```

`$make` is a path to a `fn(&Plan) -> DynDeviceState`. In a file under `tests/`, that is `crate::make`;
keep the function out of the generated module.

## What the cases prove

| Case | What it proves |
|---|---|
| `reports_the_pushed_readings` | the four readings the host pushed are the four the port reports, field by field |
| `every_named_reading_is_reported` | every value ADR 0065 names — five power readings, four thermal states, three network states, three memory pressures, in every combination — is reported faithfully (a dropped field or one value mapped onto its neighbour fails) |
| `reading_twice_agrees` | an unchanged device reads the same again and again, so a fabricated or rotating value fails |
| `another_thread_reads_the_same` | the port is `Send + Sync`: a reader on another thread sees the pushed readings |

## `Plan`

`Plan::new(readings)` is one set of readings. `Plan::default()` is `DeviceReadings::NOMINAL`.
`Plan::every()` is the cross product of the values the ADR names, which is what the second case
walks. `Plan` is `#[non_exhaustive]`: a new field does not break a caller that reads one.

## The cases as functions

`reports_the_pushed_readings(device, plan)`, `reading_twice_agrees(device, plan)` and
`another_thread_reads_the_same(device, plan)` are public, so an implementation's own test file can
call one with a plan of its own beside the macro's defaults.
`every_named_reading_is_reported(make)` takes the factory, because each case builds a device for its
own plan.
## Tests

The suite has only its `Plan`'s own tests: the cases are the tests. `cargo test -p
adam-device-state` runs it against `FakeDevice`.
