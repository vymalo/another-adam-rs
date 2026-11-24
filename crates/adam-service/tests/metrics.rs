//! ADR 0065 decision 9: the server's metrics carry the pools. No person, run or conversation is a
//! label.

use adam_budget::{Limit, Pool, PoolUsage, TokenUsage, Usage};
use adam_service::metrics;

/// What a process with two ceilings and a token bucket holds.
fn usage() -> Usage {
    Usage::new(
        [
            PoolUsage::new(Pool::Runs, Limit::AtMost(2), 1, 0),
            PoolUsage::new(Pool::ToolCalls, Limit::AtMost(4), 3, 2),
            PoolUsage::new(Pool::Processes, Limit::Unlimited, 9, 0),
        ],
        Some(TokenUsage::new(1_000, 900)),
    )
}

/// Every pool is one series per metric, and a pool with no ceiling says `+Inf`, never a zero a
/// reader would take for a limit.
#[test]
fn every_pool_is_one_series_and_an_unlimited_pool_is_infinite() {
    assert_eq!(
        metrics::render(&usage()),
        "\
# HELP adam_admission_in_use The share of a pool held now, in the pool's unit.
# TYPE adam_admission_in_use gauge
adam_admission_in_use{pool=\"runs\"} 1
adam_admission_in_use{pool=\"tool_calls\"} 3
adam_admission_in_use{pool=\"processes\"} 9
# HELP adam_admission_limit The ceiling of a pool, in its unit.
# TYPE adam_admission_limit gauge
adam_admission_limit{pool=\"runs\"} 2
adam_admission_limit{pool=\"tool_calls\"} 4
adam_admission_limit{pool=\"processes\"} +Inf
# HELP adam_admission_waiting The asks waiting for room in a pool now.
# TYPE adam_admission_waiting gauge
adam_admission_waiting{pool=\"runs\"} 0
adam_admission_waiting{pool=\"tool_calls\"} 2
adam_admission_waiting{pool=\"processes\"} 0
# HELP adam_admission_tokens_available The tokens available in the per-minute bucket now.
# TYPE adam_admission_tokens_available gauge
adam_admission_tokens_available 900
# HELP adam_admission_tokens_limit The bucket's size, in tokens per minute.
# TYPE adam_admission_tokens_limit gauge
adam_admission_tokens_limit 1000
"
    );
    assert!(
        !metrics::render(&usage()).contains("adam_admission_limit{pool=\"processes\"} 0"),
        "an unlimited pool is never reported as a zero limit"
    );
}

/// No token rate is a series the process does not have, never a zero rate.
#[test]
fn a_process_that_meters_no_tokens_reports_no_bucket() {
    let text = metrics::render(&Usage::new(
        [PoolUsage::new(Pool::Runs, Limit::AtMost(1), 0, 0)],
        None,
    ));
    assert!(text.contains("adam_admission_limit{pool=\"runs\"} 1"));
    assert!(
        !text.contains("adam_admission_tokens_"),
        "no bucket, no series: {text}"
    );
}
