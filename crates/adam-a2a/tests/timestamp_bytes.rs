//! The bytes of an adam timestamp against the bytes `chrono` wrote, for every fractional width.
//!
//! `adam_core::time::Timestamp` replaced `chrono::DateTime<Utc>` in adam's own values (ADR 0057),
//! and `adam-a2a` keeps chrono for the A2A SDK's `TaskStatus.timestamp`. The two types meet at the
//! boundary, so this is where the claim "the serialized bytes do not change" is checked against a
//! real chrono value rather than against a string copied into the test.

use adam_core::time::Timestamp;
use chrono::{DateTime, Duration, SecondsFormat, TimeZone, Utc};
use jiff::SignedDuration;

/// Every `(instant, what chrono's AutoSi writes)` pair the stored state can carry.
fn cases() -> Vec<(DateTime<Utc>, &'static str)> {
    let base = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
    let before = Utc.with_ymd_and_hms(1969, 12, 31, 23, 59, 59).unwrap();
    vec![
        (base, "2026-01-02T03:04:05Z"),
        (
            base + Duration::milliseconds(120),
            "2026-01-02T03:04:05.120Z",
        ),
        (
            base + Duration::milliseconds(100),
            "2026-01-02T03:04:05.100Z",
        ),
        (
            base + Duration::milliseconds(123),
            "2026-01-02T03:04:05.123Z",
        ),
        (
            base + Duration::microseconds(123_400),
            "2026-01-02T03:04:05.123400Z",
        ),
        (
            base + Duration::microseconds(123_456),
            "2026-01-02T03:04:05.123456Z",
        ),
        (
            base + Duration::nanoseconds(123_456_700),
            "2026-01-02T03:04:05.123456700Z",
        ),
        (
            base + Duration::nanoseconds(123_456_789),
            "2026-01-02T03:04:05.123456789Z",
        ),
        // chrono's wall clock is non-negative before the epoch: the fraction is `.500`, not `-.500`.
        (
            before + Duration::milliseconds(500),
            "1969-12-31T23:59:59.500Z",
        ),
        (
            before + Duration::nanoseconds(999_999_999),
            "1969-12-31T23:59:59.999999999Z",
        ),
        // Half a millisecond before the epoch: chrono's `timestamp_millis` floors it to -1.
        (
            before + Duration::microseconds(999_500),
            "1969-12-31T23:59:59.999500Z",
        ),
        (before, "1969-12-31T23:59:59Z"),
        // The epoch itself, and one nanosecond after it.
        (Utc.timestamp_opt(0, 0).unwrap(), "1970-01-01T00:00:00Z"),
        (
            Utc.timestamp_opt(0, 1).unwrap(),
            "1970-01-01T00:00:00.000000001Z",
        ),
    ]
}

#[test]
fn chrono_and_adam_write_the_same_bytes() {
    for (at, want) in cases() {
        // The claim is about chrono's rule, so check the reference first: if chrono ever wrote
        // something else, the expectation below is what is wrong.
        assert_eq!(
            at.to_rfc3339_opts(SecondsFormat::AutoSi, true),
            want,
            "chrono's own AutoSi output changed"
        );
        assert_eq!(
            serde_json::to_string(&at).unwrap(),
            format!("\"{want}\""),
            "chrono's serde output changed"
        );

        let ours: Timestamp = at
            .to_rfc3339_opts(SecondsFormat::AutoSi, true)
            .parse()
            .expect("adam parses what chrono wrote");
        assert_eq!(
            serde_json::to_string(&ours).unwrap(),
            serde_json::to_string(&at).unwrap(),
            "adam writes different bytes for {want}"
        );
    }
}

/// A string chrono wrote decodes into the same instant chrono holds, and re-encodes to itself.
#[test]
fn every_chrono_instant_round_trips_through_adam() {
    for (at, _) in cases() {
        let bytes = serde_json::to_string(&at).unwrap();
        let ours: Timestamp = serde_json::from_str(&bytes).unwrap();
        // jiff keeps the fraction's sign and rounds `as_second` toward zero, chrono keeps a
        // non-negative fraction and floors the second: compare the instant, not its parts.
        assert_eq!(
            ours.as_duration(),
            SignedDuration::new(
                at.timestamp(),
                i32::try_from(at.timestamp_subsec_nanos()).expect("under one second"),
            ),
            "the instant differs for {bytes}"
        );
        assert_eq!(serde_json::to_string(&ours).unwrap(), bytes);
    }
}

/// Old data still reads: every digit width chrono wrote, a `+00:00` offset, and the lowercase
/// forms RFC 3339 allows (`t`/`z`), through both `FromStr` and serde.
#[test]
fn old_strings_still_decode() {
    for (text, want) in [
        ("2026-01-02T03:04:05Z", "2026-01-02T03:04:05Z"),
        ("2026-01-02T03:04:05.120Z", "2026-01-02T03:04:05.120Z"),
        ("2026-01-02T03:04:05.123456Z", "2026-01-02T03:04:05.123456Z"),
        (
            "2026-01-02T03:04:05.123456789Z",
            "2026-01-02T03:04:05.123456789Z",
        ),
        ("2026-01-02T03:04:05+00:00", "2026-01-02T03:04:05Z"),
        ("2026-01-02T03:04:05.120+00:00", "2026-01-02T03:04:05.120Z"),
        ("2026-01-02T04:04:05.120+01:00", "2026-01-02T03:04:05.120Z"),
        ("2026-01-02t03:04:05.120z", "2026-01-02T03:04:05.120Z"),
    ] {
        let parsed: Timestamp = text.parse().unwrap_or_else(|e| panic!("{text}: {e}"));
        let from_serde: Timestamp = serde_json::from_str(&format!("\"{text}\""))
            .unwrap_or_else(|e| panic!("{text} (serde): {e}"));
        assert_eq!(parsed, from_serde, "{text}");
        assert_eq!(
            serde_json::to_string(&parsed).unwrap(),
            format!("\"{want}\""),
            "{text}"
        );
    }
}

/// `truncate_ms` floors, exactly as chrono's `timestamp_millis` did, before the epoch too.
#[test]
fn truncate_ms_floors_like_chrono() {
    for (at, want) in cases() {
        let ours: Timestamp = serde_json::to_string(&at)
            .unwrap()
            .trim_matches('"')
            .parse()
            .unwrap();
        assert_eq!(
            adam_core::store::truncate_ms(ours).as_millisecond(),
            at.timestamp_millis(),
            "truncate_ms disagrees with chrono at {want}"
        );
    }
}

/// A leap second, which chrono can hold and jiff clamps: `:60` reads as `:59`, one second earlier.
/// Nothing adam's own clock writes can carry a leap second (Unix time has none), so no stored
/// state is affected; the A2A types keep chrono, so an SDK value still round-trips through chrono.
#[test]
fn a_leap_second_is_chrono_only() {
    let leap = DateTime::from_timestamp(1_483_228_799, 1_000_000_000).expect("a leap second");
    assert_eq!(
        leap.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        "2016-12-31T23:59:60Z"
    );
    let ours: Timestamp = "2016-12-31T23:59:60Z"
        .parse()
        .expect("jiff accepts a leap second and clamps it");
    assert_eq!(
        serde_json::to_string(&ours).unwrap(),
        "\"2016-12-31T23:59:59Z\"",
        "jiff clamps :60 to :59 (one second earlier than chrono's leap-second instant)"
    );
    // chrono's own serialization of the leap second, for the record: the A2A types keep it.
    assert_eq!(
        serde_json::to_string(&leap).unwrap(),
        "\"2016-12-31T23:59:60Z\""
    );
}
