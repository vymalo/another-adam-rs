//! ADR 0065 decision 8 against `LocalAdmission`: a device on battery admits one run at a time, its
//! twin on mains as many as the budget allows, a hot device holds new steps, and a rule the person
//! turned off holds nothing.

use std::sync::Arc;
use std::time::Duration;

use adam_admission_local::LocalAdmission;
use adam_budget::{Admission, Ask, Budget, DeviceRules, Pool, Refused, Throttled};
use adam_device_state::{DeviceReadings, DynDeviceState, FakeDevice, Power, Thermal};
use adam_error::{Classify, ErrorClass};

fn rig(
    rules: DeviceRules,
    readings: DeviceReadings,
) -> (Throttled<LocalAdmission>, Arc<FakeDevice>) {
    let budget = Budget::builder()
        .runs(4)
        .tool_calls(4)
        .model_calls(4)
        .build();
    let device = Arc::new(FakeDevice::new(readings));
    let state: DynDeviceState = device.clone();
    (
        Throttled::new(LocalAdmission::new(budget), rules, state),
        device,
    )
}

fn on_battery(percent: u8, low_power: bool) -> DeviceReadings {
    DeviceReadings {
        power: Power::on_battery(percent, low_power),
        ..DeviceReadings::NOMINAL
    }
}

#[tokio::test]
async fn a_device_on_low_battery_admits_one_run_at_a_time() {
    for readings in [on_battery(15, false), on_battery(80, true)] {
        let (admission, _) = rig(DeviceRules::default(), readings);
        let first = admission
            .admit(Ask::new(Pool::Runs, "one"))
            .await
            .expect("a run on its own is always admitted");
        let second = admission
            .admit(Ask::new(Pool::Runs, "two"))
            .await
            .expect_err("the second run waits for the first");
        assert!(
            matches!(
                second,
                Refused::Busy {
                    pool: Pool::Runs,
                    ..
                }
            ),
            "{second:?}"
        );
        assert_eq!(second.class(), ErrorClass::RateLimited);
        assert!(second.retry_after().is_some());
        drop(first);
        let _ = admission
            .admit(Ask::new(Pool::Runs, "two"))
            .await
            .expect("the first run gave its seat back");
    }
}

#[tokio::test]
async fn its_twin_on_mains_admits_as_many_as_the_budget_allows() {
    let (admission, _) = rig(DeviceRules::default(), DeviceReadings::NOMINAL);
    let mut held = Vec::new();
    for i in 0..4 {
        held.push(
            admission
                .admit(Ask::new(Pool::Runs, format!("run-{i}")))
                .await
                .expect("within the budget"),
        );
    }
    admission
        .admit(Ask::new(Pool::Runs, "fifth"))
        .await
        .expect_err("the budget of four is the ceiling");
}

#[tokio::test]
async fn a_second_run_waits_and_goes_in_when_the_first_ends() {
    let (admission, _) = rig(DeviceRules::default(), on_battery(10, false));
    let first = admission.admit(Ask::new(Pool::Runs, "one")).await.unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        drop(first);
    });
    let _ = admission
        .admit(Ask::new(Pool::Runs, "two").wait(Duration::from_secs(5)))
        .await
        .expect("admitted once the first run is over");
    release.await.unwrap();
}

#[tokio::test]
async fn a_run_already_in_flight_counts_when_the_battery_runs_low() {
    let (admission, device) = rig(DeviceRules::default(), DeviceReadings::NOMINAL);
    let _first = admission.admit(Ask::new(Pool::Runs, "one")).await.unwrap();
    device.set_power(Power::on_battery(9, false));
    admission
        .admit(Ask::new(Pool::Runs, "two"))
        .await
        .expect_err("the run that started on mains is one run");
    device.set_power(Power::OnMains);
    let _ = admission
        .admit(Ask::new(Pool::Runs, "two"))
        .await
        .expect("plugged in again, the budget is whole");
}

#[tokio::test]
async fn the_person_turns_the_battery_rule_off() {
    let (admission, _) = rig(
        DeviceRules::default().low_battery(false),
        on_battery(5, true),
    );
    let _one = admission.admit(Ask::new(Pool::Runs, "one")).await.unwrap();
    let _two = admission
        .admit(Ask::new(Pool::Runs, "two"))
        .await
        .expect("with the rule off, a low battery holds nothing back");

    let (admission, _) = rig(DeviceRules::default(), on_battery(5, true));
    let _one = admission.admit(Ask::new(Pool::Runs, "one")).await.unwrap();
    admission.set_rules(DeviceRules::default().low_battery(false));
    let _two = admission
        .admit(Ask::new(Pool::Runs, "two"))
        .await
        .expect("a rule switched off takes effect on the next ask");
}

#[tokio::test]
async fn a_hot_device_holds_new_steps() {
    let hot = DeviceReadings {
        thermal: Thermal::Serious,
        ..DeviceReadings::NOMINAL
    };
    let (admission, device) = rig(DeviceRules::default(), hot);
    for pool in [Pool::ModelCalls, Pool::ToolCalls] {
        let held = admission
            .admit(Ask::new(pool, "c"))
            .await
            .expect_err("a hot device holds a new step");
        assert!(matches!(held, Refused::Busy { .. }), "{held:?}");
        assert_eq!(held.class(), ErrorClass::RateLimited);
    }
    let _ = admission
        .admit(Ask::new(Pool::Git, "c"))
        .await
        .expect("only steps are held, not every pool");

    let waiting = admission.admit(Ask::new(Pool::ToolCalls, "c").wait(Duration::from_secs(5)));
    let cool = async {
        tokio::time::sleep(Duration::from_millis(80)).await;
        device.set_thermal(Thermal::Fair);
    };
    let (permit, ()) = tokio::join!(waiting, cool);
    let _ = permit.expect("the step goes in when the device cools");

    device.set_thermal(Thermal::Critical);
    admission
        .admit(Ask::new(Pool::ModelCalls, "c"))
        .await
        .expect_err("critical holds too");
    admission.set_rules(DeviceRules::default().hot(false));
    let _ = admission
        .admit(Ask::new(Pool::ModelCalls, "c"))
        .await
        .expect("the person turned the rule off");
}
