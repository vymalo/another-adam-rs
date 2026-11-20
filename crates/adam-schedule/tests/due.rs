//! When schedules are due: DST in `Europe/Berlin` for times of day and intervals, the day rule,
//! the one-shots and missed due times (ADR 0060, decisions 3, 4 and 6).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::PathBuf;

use adam_agent_fs::Schedule;
use adam_core::time::Timestamp;
use adam_schedule::{Due, MISSED_MAX, ScheduleSpec};

fn at(text: &str) -> Timestamp {
    Timestamp(text.parse().unwrap())
}

fn schedule(cron: &str, at: Option<&str>, after: Option<&str>, timezone: &str) -> Schedule {
    Schedule {
        name: "daily".into(),
        cron: cron.into(),
        at: at.map(Into::into),
        after: after.map(Into::into),
        timezone: timezone.into(),
        agent: "coder".into(),
        prompt: "Go.".into(),
        path: PathBuf::from("agent/schedules/daily.md"),
    }
}

fn cron(expr: &str, timezone: &str) -> ScheduleSpec {
    ScheduleSpec::from_schedule(&schedule(expr, None, None, timezone)).unwrap()
}

/// The due times in `(from, to]`.
fn dues(spec: &ScheduleSpec, from: &str, to: &str) -> Vec<String> {
    let (mut cursor, to) = (at(from), at(to));
    let mut out = Vec::new();
    while let Some(due) = spec.due_after(cursor, Timestamp::UNIX_EPOCH) {
        if due > to {
            break;
        }
        out.push(due.0.to_string());
        cursor = due;
    }
    out
}

// Europe/Berlin in 2026: 02:00 CET becomes 03:00 CEST on 29 March (01:00Z); 03:00 CEST becomes
// 02:00 CET on 25 October (01:00Z).

#[test]
fn a_time_of_day_skipped_by_dst_fires_where_compatible_puts_it() {
    let spec = cron("30 2 * * *", "Europe/Berlin");
    assert_eq!(
        dues(&spec, "2026-03-28T00:00:00Z", "2026-03-30T23:00:00Z"),
        [
            "2026-03-28T01:30:00Z", // 02:30 CET
            "2026-03-29T01:30:00Z", // 02:30 does not exist: 03:30 CEST
            "2026-03-30T00:30:00Z", // 02:30 CEST
        ]
    );
}

#[test]
fn a_time_of_day_repeated_by_dst_fires_once_at_the_earlier_offset() {
    let spec = cron("30 2 * * *", "Europe/Berlin");
    assert_eq!(
        dues(&spec, "2026-10-24T00:00:00Z", "2026-10-26T23:00:00Z"),
        [
            "2026-10-24T00:30:00Z", // 02:30 CEST
            "2026-10-25T00:30:00Z", // 02:30 CEST, not again at 02:30 CET
            "2026-10-26T01:30:00Z", // 02:30 CET
        ]
    );
}

#[test]
fn an_interval_fires_through_a_repeated_hour() {
    let spec = cron("*/15 2 * * *", "Europe/Berlin");
    assert_eq!(
        dues(&spec, "2026-10-24T23:00:00Z", "2026-10-25T23:00:00Z"),
        [
            "2026-10-25T00:00:00Z",
            "2026-10-25T00:15:00Z",
            "2026-10-25T00:30:00Z",
            "2026-10-25T00:45:00Z", // 02:00-02:45 CEST
            "2026-10-25T01:00:00Z",
            "2026-10-25T01:15:00Z",
            "2026-10-25T01:30:00Z",
            "2026-10-25T01:45:00Z", // 02:00-02:45 CET
        ]
    );
}

#[test]
fn an_interval_does_not_fire_in_a_skipped_hour() {
    let spec = cron("*/15 2 * * *", "Europe/Berlin");
    // 29 March has no hour 2: the next is 30 March, 02:00 CEST.
    assert_eq!(
        spec.due_after(at("2026-03-28T23:00:00Z"), Timestamp::UNIX_EPOCH),
        Some(at("2026-03-30T00:00:00Z"))
    );
    let hourly = cron("0 * * * *", "Europe/Berlin");
    assert_eq!(
        dues(&hourly, "2026-03-28T23:30:00Z", "2026-03-29T02:00:00Z"),
        [
            "2026-03-29T00:00:00Z", // 01:00 CET
            "2026-03-29T01:00:00Z", // 03:00 CEST: 02:00 never was
            "2026-03-29T02:00:00Z",
        ]
    );
}

#[test]
fn utc_and_half_hour_zones() {
    assert_eq!(
        dues(
            &cron("0 9 * * MON-FRI", "UTC"),
            "2026-11-20T09:00:00Z",
            "2026-11-24T12:00:00Z"
        ),
        ["2026-11-23T09:00:00Z", "2026-11-24T09:00:00Z"]
    );
    // India is UTC+05:30: 09:00 there is 03:30Z.
    assert_eq!(
        cron("0 9 * * *", "Asia/Kolkata")
            .due_after(at("2026-11-20T00:00:00Z"), Timestamp::UNIX_EPOCH),
        Some(at("2026-11-20T03:30:00Z"))
    );
    assert_eq!(
        cron("*/30 * * * *", "Asia/Kolkata")
            .due_after(at("2026-11-20T00:00:00Z"), Timestamp::UNIX_EPOCH),
        Some(at("2026-11-20T00:30:00Z")) // 00:00Z is 05:30 there, and not after itself
    );
}

#[test]
fn the_day_rule_matches_either_restricted_field() {
    // November 2026: Fridays are the 6th, 13th, 20th and 27th; 13 December is a Sunday.
    assert_eq!(
        dues(
            &cron("0 9 13 * FRI", "UTC"),
            "2026-11-01T00:00:00Z",
            "2026-12-14T00:00:00Z"
        ),
        [
            "2026-11-06T09:00:00Z",
            "2026-11-13T09:00:00Z",
            "2026-11-20T09:00:00Z",
            "2026-11-27T09:00:00Z",
            "2026-12-04T09:00:00Z",
            "2026-12-11T09:00:00Z",
            "2026-12-13T09:00:00Z",
        ]
    );
    // A starred day of the month: both must match, so Fridays that are odd days.
    assert_eq!(
        dues(
            &cron("0 9 */2 * FRI", "UTC"),
            "2026-11-01T00:00:00Z",
            "2026-11-30T00:00:00Z"
        ),
        ["2026-11-13T09:00:00Z", "2026-11-27T09:00:00Z"]
    );
}

#[test]
fn a_rare_date_is_found_years_ahead() {
    // 2100 is not a leap year: after 2096, 29 February comes in 2104.
    assert_eq!(
        cron("0 0 29 2 *", "UTC").due_after(at("2096-03-01T00:00:00Z"), Timestamp::UNIX_EPOCH),
        Some(at("2104-02-29T00:00:00Z"))
    );
}

#[test]
fn one_shots_fire_once() {
    let civil = ScheduleSpec::from_schedule(&schedule(
        "",
        Some("2026-12-24T18:00"),
        None,
        "Europe/Berlin",
    ))
    .unwrap();
    let epoch = Timestamp::UNIX_EPOCH;
    assert_eq!(
        civil.due_after(at("2026-01-01T00:00:00Z"), epoch),
        Some(at("2026-12-24T17:00:00Z"))
    );
    assert_eq!(civil.due_after(at("2026-12-24T17:00:00Z"), epoch), None);
    assert!(civil.is_one_shot());

    let instant = ScheduleSpec::from_schedule(&schedule(
        "",
        Some("2026-12-24T18:00:00+01:00"),
        None,
        "Asia/Tokyo",
    ))
    .unwrap();
    assert_eq!(
        instant.due_after(at("2026-01-01T00:00:00Z"), epoch),
        Some(at("2026-12-24T17:00:00Z"))
    );

    // A civil time a DST change skips: 02:30 on 29 March in Berlin is 03:30 CEST.
    let gap = ScheduleSpec::from_schedule(&schedule(
        "",
        Some("2026-03-29T02:30"),
        None,
        "Europe/Berlin",
    ))
    .unwrap();
    assert_eq!(
        gap.due_after(epoch, epoch),
        Some(at("2026-03-29T01:30:00Z"))
    );

    let after = ScheduleSpec::from_schedule(&schedule("", None, Some("1h30m"), "UTC")).unwrap();
    let anchor = at("2026-11-20T10:00:00Z");
    assert_eq!(
        after.due_after(anchor, anchor),
        Some(at("2026-11-20T11:30:00Z"))
    );
    assert_eq!(after.due_after(at("2026-11-20T11:30:00Z"), anchor), None);
    // It counts from the anchor, not from the instant asked about.
    assert_eq!(
        after.due_after(at("2026-11-20T11:00:00Z"), anchor),
        Some(at("2026-11-20T11:30:00Z"))
    );
    assert!(!cron("0 9 * * *", "UTC").is_one_shot());
}

#[test]
fn missed_due_times_fire_once_with_a_count() {
    let hourly = cron("0 * * * *", "UTC");
    let epoch = Timestamp::UNIX_EPOCH;
    // Last fired at midnight, down until 05:30: fire 05:00, skip 01:00-04:00.
    assert_eq!(
        hourly.latest_due(
            at("2026-11-20T00:00:00Z"),
            at("2026-11-20T05:30:00Z"),
            epoch
        ),
        Some(Due {
            at: at("2026-11-20T05:00:00Z"),
            missed: 4
        })
    );
    assert_eq!(
        hourly.latest_due(
            at("2026-11-20T00:00:00Z"),
            at("2026-11-20T01:00:00Z"),
            epoch
        ),
        Some(Due {
            at: at("2026-11-20T01:00:00Z"),
            missed: 0
        })
    );
    assert_eq!(
        hourly.latest_due(
            at("2026-11-20T00:00:00Z"),
            at("2026-11-20T00:59:59Z"),
            epoch
        ),
        None
    );
    assert_eq!(
        hourly.latest_due(
            at("2026-11-20T05:00:00Z"),
            at("2026-11-20T05:00:00Z"),
            epoch
        ),
        None
    );

    let after = ScheduleSpec::from_schedule(&schedule("", None, Some("30m"), "UTC")).unwrap();
    let anchor = at("2026-11-20T10:00:00Z");
    assert_eq!(
        after.latest_due(anchor, at("2026-11-21T00:00:00Z"), anchor),
        Some(Due {
            at: at("2026-11-20T10:30:00Z"),
            missed: 0
        })
    );
}

#[test]
fn a_long_outage_counts_up_to_the_cap() {
    let minutely = cron("* * * * *", "UTC");
    let due = minutely
        .latest_due(
            at("2026-01-01T00:00:00Z"),
            at("2026-03-01T12:34:56Z"),
            Timestamp::UNIX_EPOCH,
        )
        .unwrap();
    assert_eq!(
        due,
        Due {
            at: at("2026-03-01T12:34:00Z"),
            missed: MISSED_MAX
        }
    );
    // Just under the cap is counted exactly.
    let n = MISSED_MAX;
    let start = at("2026-01-01T00:00:00Z");
    let now = Timestamp(
        start
            .0
            .checked_add(jiff::SignedDuration::from_mins(i64::try_from(n).unwrap()))
            .unwrap(),
    );
    assert_eq!(
        minutely.latest_due(start, now, Timestamp::UNIX_EPOCH),
        Some(Due {
            at: now,
            missed: n - 1
        })
    );
}

#[test]
fn a_spec_refuses_what_the_loader_would() {
    for (s, want) in [
        (
            schedule("0 9 * * *", None, None, "Mars/Olympus"),
            "is not an IANA time zone",
        ),
        (
            schedule("0 9 * * *", Some("2026-12-24T18:00"), None, "UTC"),
            "exactly one",
        ),
        (schedule("", None, None, "UTC"), "exactly one"),
        (schedule("0 25 * * *", None, None, "UTC"), "hour item `25`"),
    ] {
        let err = ScheduleSpec::from_schedule(&s).unwrap_err();
        assert_eq!((err.agent.as_str(), err.name.as_str()), ("coder", "daily"));
        assert!(err.to_string().contains(want), "{err}");
    }
}

#[test]
fn the_hash_covers_every_field_of_the_file() {
    let base = schedule("0 9 * * *", None, None, "UTC");
    let hash = |s: &Schedule| ScheduleSpec::from_schedule(s).unwrap().hash().to_owned();
    let h = hash(&base);
    assert!(h.starts_with("sha256:") && h.len() == 7 + 64, "{h}");
    assert_eq!(h, hash(&base.clone()));
    let mut changed = Vec::new();
    for edit in [
        |s: &mut Schedule| s.cron = "0 10 * * *".into(),
        |s: &mut Schedule| s.timezone = "Europe/Berlin".into(),
        |s: &mut Schedule| s.prompt = "Go now.".into(),
        |s: &mut Schedule| s.name = "nightly".into(),
        |s: &mut Schedule| s.agent = "chat".into(),
        |s: &mut Schedule| {
            s.cron = String::new();
            s.after = Some("30m".into());
        },
        |s: &mut Schedule| {
            s.cron = String::new();
            s.at = Some("2026-12-24T18:00".into());
        },
    ] {
        let mut s = base.clone();
        edit(&mut s);
        changed.push(ScheduleSpec::from_schedule(&s).map(|spec| spec.hash().to_owned()));
    }
    for (i, h2) in changed.iter().enumerate() {
        assert_ne!(*h2.as_ref().unwrap(), h, "edit {i}");
    }
    // The path is not part of the file's content: moving nothing else keeps the hash.
    let mut moved = base.clone();
    moved.path = PathBuf::from("elsewhere.md");
    assert_eq!(hash(&moved), h);
}
