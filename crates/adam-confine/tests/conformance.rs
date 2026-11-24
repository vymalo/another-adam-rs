//! The two backends against the conformance suite, and the memory-hog proof the card needs: the same
//! hog that a confined command cannot finish runs to its end under `NoConfine`.

use std::sync::Arc;

use adam_confine::{Rlimits, SystemdScope};
use adam_workspace::{Confine, DynConfine, Enforcement, NoConfine, ProcessLimits};

mod rlimits {
    use super::*;

    fn make() -> DynConfine {
        Arc::new(Rlimits::new())
    }

    adam_confine_testkit::confine_conformance!(crate::rlimits::make);
}

/// The scope backend where the system has one. `ADAM_TEST_REQUIRE_SYSTEMD=1` (CI sets it) makes a
/// system without a full scope a failure, not a skip.
mod scope {
    use super::*;

    fn required() -> bool {
        std::env::var("ADAM_TEST_REQUIRE_SYSTEMD").is_ok_and(|v| v == "1")
    }

    fn make() -> DynConfine {
        match SystemdScope::detect() {
            Some(scope) => Arc::new(scope),
            None => {
                assert!(
                    !required(),
                    "ADAM_TEST_REQUIRE_SYSTEMD=1 but no systemd user manager makes a scope here"
                );
                eprintln!("skipping: no systemd user manager here");
                Arc::new(NoConfine)
            }
        }
    }

    adam_confine_testkit::confine_conformance!(crate::scope::make);

    #[test]
    fn ci_has_the_full_scope() {
        if required() {
            assert_eq!(
                make().describe(),
                Enforcement::Full,
                "the CI job must delegate memory, cpu and pids to the user manager"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_rlimit_backend_says_what_it_does_not_enforce() {
    let described = Rlimits::new().describe();
    let Enforcement::Partial(missing) = described else {
        panic!("rlimits are partial: {described:?}");
    };
    assert!(missing.contains(&"process count"));
    assert!(missing.contains(&"cpu share"));
}

/// The card's memory-hog proof, and the control that shows it can fail: under `NoConfine` the hog
/// (700 MB) finishes; confined to 256 MB, it does not.
#[tokio::test(flavor = "multi_thread")]
async fn the_memory_hog_finishes_unconfined_and_is_stopped_confined() {
    let dir = tempfile::tempdir().unwrap();
    let hog = "x=$(head -c 700000000 /dev/zero | tr '\\0' a); echo hog-finished";
    let limits = ProcessLimits::new().with_memory(256 * 1024 * 1024);

    let (free, _) = adam_confine_testkit::run(&NoConfine, &limits, dir.path(), hog).await;
    assert!(
        free.stdout.contains("hog-finished"),
        "control: with no confinement the hog finishes: {free:?}"
    );

    let (held, _) = adam_confine_testkit::run(&Rlimits::new(), &limits, dir.path(), hog).await;
    assert!(
        !held.stdout.contains("hog-finished"),
        "under rlimits the hog is stopped: {held:?}"
    );
}
