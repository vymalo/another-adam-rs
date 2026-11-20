//! When a schedule is due (ADR 0060, decisions 3, 4 and 6).
//!
//! A `cron` whose minute and hour are both restricted names times of day: each listed civil time
//! of a matching date becomes one instant, a time a DST change skips where jiff's `compatible`
//! puts it (02:30 in a one-hour gap is 03:30), one it repeats at the earlier offset. Any other
//! `cron` is an interval: the civil fields are matched on every minute of the instant line, so it
//! fires through a repeated hour and not in a skipped one, as Vixie cron does.

use adam_agent_fs::{At, Cron, When};
use adam_core::time::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp as Instant};

use crate::ScheduleSpec;

/// The most missed due times [`Due::missed`] counts; beyond it, it says this and means "at least".
pub const MISSED_MAX: u64 = 10_000;

/// How far ahead a search looks: a `cron` such as `0 0 29 2 *` fires once in four years, eight
/// across a century that is not a leap year.
const SEARCH_DAYS: i32 = 366 * 8 + 2;

/// The latest due time that has passed, and how many before it were skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Due {
    /// The due time to fire.
    pub at: Timestamp,
    /// The due times after the last firing and before [`at`](Self::at), which do not fire
    /// (decision 3); [`MISSED_MAX`] means at least that many.
    pub missed: u64,
}

impl ScheduleSpec {
    /// The first due time strictly after `after`, or `None` when there is none: a one-shot whose
    /// time is not after it, or a `cron` with no match in the next eight years (the loader refuses
    /// a `cron` that can never fire).
    ///
    /// `anchor` is the scheduler's anchor, the instant an `after` counts from (decision 4); the
    /// other forms ignore it.
    pub fn due_after(&self, after: Timestamp, anchor: Timestamp) -> Option<Timestamp> {
        let after = after.0;
        let due = match self.when() {
            When::At(At::Instant(t)) => Some(t),
            When::At(At::Civil(dt)) => civil_instant(self.zone(), dt),
            When::After(d) => anchor.0.checked_add(SignedDuration::try_from(d).ok()?).ok(),
            When::Cron(cron) if cron.is_time_of_day() => {
                return time_of_day_after(&cron, self.zone(), after).map(Timestamp);
            }
            When::Cron(cron) => return interval_after(&cron, self.zone(), after).map(Timestamp),
        };
        due.filter(|d| *d > after).map(Timestamp)
    }

    /// The latest due time in `(since, now]`, with the count of those before it, or `None` when
    /// none is due. `since` is the last due time fired, or the anchor before the first.
    pub fn latest_due(&self, since: Timestamp, now: Timestamp, anchor: Timestamp) -> Option<Due> {
        let mut latest = None::<Timestamp>;
        let mut seen: u64 = 0;
        let mut cursor = since;
        while let Some(due) = self.due_after(cursor, anchor) {
            // `due_after` moves forward; a zone database that broke that must not spin here.
            if due > now || due <= cursor {
                break;
            }
            seen += 1;
            latest = Some(due);
            cursor = due;
            if seen > MISSED_MAX {
                // Too many to walk: find the latest from just before `now`.
                // The 10 001st may itself be the last one due.
                let at = self.latest_before(cursor, now, anchor).unwrap_or(cursor);
                return Some(Due {
                    at,
                    missed: MISSED_MAX,
                });
            }
        }
        latest.map(|at| Due {
            at,
            missed: seen - 1,
        })
    }

    /// The latest due time in `(floor, now]`, searching back from `now` in widening windows.
    fn latest_before(
        &self,
        floor: Timestamp,
        now: Timestamp,
        anchor: Timestamp,
    ) -> Option<Timestamp> {
        for hours in [1, 24, 24 * 31, 24 * 366, 24 * i64::from(SEARCH_DAYS)] {
            let start = now.0.checked_sub(SignedDuration::from_hours(hours)).ok()?;
            let mut cursor = Timestamp(start).max(floor);
            let mut latest = None;
            while let Some(due) = self
                .due_after(cursor, anchor)
                .filter(|d| *d <= now && *d > cursor)
            {
                latest = Some(due);
                cursor = due;
            }
            if latest.is_some() {
                return latest;
            }
        }
        None
    }

    pub(crate) fn zone(&self) -> &TimeZone {
        &self.zone
    }
}

/// A civil time in `zone`: a skipped one where `compatible` puts it, a repeated one at the earlier
/// offset.
fn civil_instant(zone: &TimeZone, dt: DateTime) -> Option<Instant> {
    zone.to_ambiguous_timestamp(dt).compatible().ok()
}

fn date_matches(cron: &Cron, date: Date) -> bool {
    // Months, days and weekdays are small and positive: the casts cannot lose anything.
    #[allow(clippy::cast_sign_loss)]
    let (month, day, weekday) = (
        date.month() as u8,
        date.day() as u8,
        date.weekday().to_sunday_zero_offset() as u8,
    );
    cron.date(month, day, weekday)
}

fn time_of_day_after(cron: &Cron, zone: &TimeZone, after: Instant) -> Option<Instant> {
    // A day earlier: the first candidate may lie on the civil date before `after`'s.
    let mut date = after.to_zoned(zone.clone()).date().yesterday().ok()?;
    for _ in 0..=SEARCH_DAYS {
        if date_matches(cron, date) {
            let first = (0..24u8)
                .filter(|h| cron.hour(*h))
                .flat_map(|h| (0..60u8).filter(|m| cron.minute(*m)).map(move |m| (h, m)))
                .filter_map(|(h, m)| {
                    let dt = date.at(i8::try_from(h).ok()?, i8::try_from(m).ok()?, 0, 0);
                    civil_instant(zone, dt)
                })
                .filter(|t| *t > after)
                .min();
            if first.is_some() {
                return first;
            }
        }
        date = date.tomorrow().ok()?;
    }
    None
}

/// Where a skip from `t` towards `to` lands: never past the zone's next transition, whose new
/// offset can make minutes valid that the skip assumed away (Pacific/Chatham moves at 02:45), and
/// never back (a fall-back across midnight puts the next day's start before `t`).
fn jump(zone: &TimeZone, t: Instant, to: Instant) -> Option<Instant> {
    let to = match zone.following(t).next() {
        Some(transition) => to.min(transition.timestamp()),
        None => to,
    };
    let floor = t.checked_add(SignedDuration::from_mins(1)).ok()?;
    Some(to.max(floor))
}

fn interval_after(cron: &Cron, zone: &TimeZone, after: Instant) -> Option<Instant> {
    // The next whole minute of the instant line: every zone in use today has whole-minute offsets.
    let mut t = Instant::from_second((after.as_second().div_euclid(60) + 1) * 60).ok()?;
    let end = t
        .checked_add(SignedDuration::from_hours(24 * i64::from(SEARCH_DAYS)))
        .ok()?;
    while t <= end {
        let z = t.to_zoned(zone.clone());
        #[allow(clippy::cast_sign_loss)]
        let (hour, minute) = (z.hour() as u8, z.minute() as u8);
        if !date_matches(cron, z.date()) {
            // To the start of the next civil day.
            let next = z.date().tomorrow().ok()?.to_zoned(zone.clone()).ok()?;
            t = jump(zone, t, next.timestamp())?;
        } else if !cron.hour(hour) {
            let next_hour = t
                .checked_add(SignedDuration::from_mins(60 - i64::from(minute)))
                .ok()?;
            t = jump(zone, t, next_hour)?;
        } else if !cron.minute(minute) {
            t = t.checked_add(SignedDuration::from_mins(1)).ok()?;
        } else {
            return Some(t);
        }
    }
    None
}
