//! When a schedule fires: the three forms of `schedules/<name>.md` (ADR 0060, decision 4), parsed
//! here and evaluated by `adam-schedule`. Nothing here reads a clock; a `timezone` is looked up by
//! name in jiff's bundled database, never the system's (decision 6).

use std::fmt;
use std::time::Duration;

/// The longest `after`: a one-shot further out than this belongs in `at`.
pub const AFTER_MAX: Duration = Duration::from_secs(366 * 24 * 60 * 60);

/// The zone `name` (`UTC`, `Europe/Berlin`) in jiff's bundled copy of the IANA database, so every
/// host reads the same rules whatever its `/usr/share/zoneinfo` holds.
pub fn time_zone(name: &str) -> Result<jiff::tz::TimeZone, WhenError> {
    let spelled = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'/' | b'_' | b'-' | b'+'));
    spelled
        .then(|| jiff::tz::TimeZoneDatabase::bundled().get(name).ok())
        .flatten()
        .ok_or_else(|| WhenError::new("is not an IANA time zone name such as `Europe/Berlin`"))
}

/// Why a `cron`, `at` or `after` value is refused; the text completes "`key: value` …".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct WhenError(String);

impl WhenError {
    pub(crate) fn new(why: impl Into<String>) -> Self {
        Self(why.into())
    }
}

/// When a schedule fires: exactly one of the three keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// `cron`: repeats.
    Cron(Cron),
    /// `at`: once, at an instant or a civil time in the schedule's `timezone`.
    At(At),
    /// `after`: once, this long after the scheduler starts.
    After(Duration),
}

/// The instant of an `at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At {
    /// An RFC 3339 time with its offset (`Z` or `±HH:MM`): the schedule's `timezone` is ignored.
    Instant(jiff::Timestamp),
    /// A date and time with no offset, read in the schedule's `timezone`.
    Civil(jiff::civil::DateTime),
}

/// Parse an `at`: `YYYY-MM-DDTHH:MM`, optionally `:SS`, then nothing, `Z` or `±HH:MM`. Stricter
/// than jiff's own parser, which also takes a bare date, an offset beyond a day and a `[zone]`
/// annotation it does not apply; jiff checks the calendar (`2026-02-30` is refused).
pub fn parse_at(text: &str) -> Result<At, WhenError> {
    const SHAPE: &str = "is not a time such as `2026-12-24T18:00` or `2026-12-24T18:00:00+01:00`";
    let b = text.as_bytes();
    let digits =
        |r: std::ops::Range<usize>| b.get(r).is_some_and(|s| s.iter().all(u8::is_ascii_digit));
    let date_time = digits(0..4)
        && b.get(4) == Some(&b'-')
        && digits(5..7)
        && b.get(7) == Some(&b'-')
        && digits(8..10)
        && b.get(10) == Some(&b'T')
        && digits(11..13)
        && b.get(13) == Some(&b':')
        && digits(14..16);
    if !date_time {
        return Err(WhenError::new(SHAPE));
    }
    let civil_end = if b.get(16) == Some(&b':') {
        if !digits(17..19) {
            return Err(WhenError::new(SHAPE));
        }
        19
    } else {
        16
    };
    if civil_end == 19 && b[17..19] == *b"60" {
        return Err(WhenError::new(
            "has the second 60: a schedule cannot name a leap second",
        ));
    }
    if &b[0..4] > b"9998".as_slice() {
        return Err(WhenError::new("is after the year 9998"));
    }
    let (civil, offset) = text.split_at(civil_end);
    let civil: jiff::civil::DateTime = civil
        .parse()
        .map_err(|e: jiff::Error| WhenError::new(format!("is not a valid date and time: {e}")))?;
    match offset {
        "" => Ok(At::Civil(civil)),
        "Z" => instant(civil, 0),
        _ => {
            let o = offset.as_bytes();
            let sign = match o.first() {
                Some(b'+') => 1,
                Some(b'-') => -1,
                _ => return Err(WhenError::new(SHAPE)),
            };
            if o.len() != 6
                || o[3] != b':'
                || !o[1..3].iter().chain(&o[4..6]).all(u8::is_ascii_digit)
            {
                return Err(WhenError::new(SHAPE));
            }
            if offset == "-00:00" {
                return Err(WhenError::new(
                    "has the offset `-00:00`, which RFC 3339 keeps for an unknown offset: write `Z`",
                ));
            }
            let (h, m) = (two(&o[1..3]), two(&o[4..6]));
            if h > 23 || m > 59 {
                return Err(WhenError::new(format!(
                    "has the offset `{offset}`, beyond a day"
                )));
            }
            instant(civil, sign * (h * 3600 + m * 60))
        }
    }
}

fn two(d: &[u8]) -> i32 {
    i32::from(d[0] - b'0') * 10 + i32::from(d[1] - b'0')
}

fn instant(civil: jiff::civil::DateTime, offset_seconds: i32) -> Result<At, WhenError> {
    let offset = jiff::tz::Offset::from_seconds(offset_seconds)
        .map_err(|e| WhenError::new(format!("has an invalid offset: {e}")))?;
    offset
        .to_timestamp(civil)
        .map(At::Instant)
        .map_err(|_| WhenError::new("is out of range"))
}

/// Parse an `after`: whole numbers with a unit, largest first, each unit once: `90s`, `30m`, `2h`,
/// `1h30m`, `1d 12h`. A day is 24 hours. Zero and more than [`AFTER_MAX`] are refused.
pub fn parse_after(text: &str) -> Result<Duration, WhenError> {
    const SHAPE: &str =
        "is not a duration such as `30m`, `2h` or `1h30m` (units `d`, `h`, `m`, `s`)";
    let units: [(u8, u64); 4] = [(b'd', 86_400), (b'h', 3_600), (b'm', 60), (b's', 1)];
    let mut next_unit = 0;
    let mut total: u64 = 0;
    let mut rest = text.trim_ascii_start();
    if rest.is_empty() {
        return Err(WhenError::new(SHAPE));
    }
    while !rest.is_empty() {
        let n_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        let unit = rest.as_bytes().get(n_len).copied();
        let (Some(unit), true) = (unit, n_len > 0) else {
            return Err(WhenError::new(SHAPE));
        };
        let Some(i) = units[next_unit..].iter().position(|(u, _)| *u == unit) else {
            return Err(WhenError::new(if units.iter().any(|(u, _)| *u == unit) {
                "names its units out of order or twice: largest first, each once (`1h30m`)"
                    .to_owned()
            } else {
                SHAPE.to_owned()
            }));
        };
        let n: u64 = rest[..n_len]
            .parse()
            .map_err(|_| WhenError::new("is too long"))?;
        total = n
            .checked_mul(units[next_unit + i].1)
            .and_then(|s| total.checked_add(s))
            .ok_or_else(|| WhenError::new("is too long"))?;
        next_unit += i + 1;
        rest = rest[n_len + 1..].trim_ascii_start();
    }
    let after = Duration::from_secs(total);
    if after.is_zero() {
        Err(WhenError::new("is zero: a schedule fires after some time"))
    } else if after > AFTER_MAX {
        Err(WhenError::new("is longer than 366 days: use `at`"))
    } else {
        Ok(after)
    }
}

/// A five-field cron expression: minute, hour, day of month, month, day of week (ADR 0060,
/// decision 7). `*`, lists, ranges, steps (`*/15`, `1-30/2`, and `5/15` for `5-max/15`), `JAN`-`DEC`,
/// `SUN`-`SAT` (any case), `0` and `7` for Sunday, and `?` as `*`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days_of_month: u32,
    months: u16,
    days_of_week: u8,
    minute_star: bool,
    hour_star: bool,
    day_of_month_star: bool,
    day_of_week_star: bool,
}

const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const DAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

struct Field {
    name: &'static str,
    min: u32,
    max: u32,
    /// Names for `min..`, in order.
    names: &'static [&'static str],
}

const FIELDS: [Field; 5] = [
    Field {
        name: "minute",
        min: 0,
        max: 59,
        names: &[],
    },
    Field {
        name: "hour",
        min: 0,
        max: 23,
        names: &[],
    },
    Field {
        name: "day-of-month",
        min: 1,
        max: 31,
        names: &[],
    },
    Field {
        name: "month",
        min: 1,
        max: 12,
        names: &MONTHS,
    },
    Field {
        name: "day-of-week",
        min: 0,
        max: 7,
        names: &DAYS,
    },
];

impl Cron {
    /// Parse five fields separated by ASCII spaces or tabs. A day-of-month that no listed month has (`30 2`) is
    /// refused, since the schedule could never fire.
    pub fn parse(text: &str) -> Result<Self, WhenError> {
        let fields: Vec<&str> = text.split_ascii_whitespace().collect();
        if fields.len() != 5 {
            return Err(WhenError::new(format!(
                "has {} fields; a schedule takes five (minute hour day-of-month month day-of-week)",
                fields.len()
            )));
        }
        let mut bits = [0u64; 5];
        let mut star = [false; 5];
        for (i, (text, field)) in fields.iter().zip(&FIELDS).enumerate() {
            (bits[i], star[i]) = field.parse(text)?;
        }
        // Sunday is 0 and 7.
        let days_of_week = (bits[4] | (bits[4] >> 7)) & 0x7f;
        let cron = Self {
            minutes: bits[0],
            hours: u32::try_from(bits[1]).unwrap_or_default(),
            days_of_month: u32::try_from(bits[2]).unwrap_or_default(),
            months: u16::try_from(bits[3]).unwrap_or_default(),
            days_of_week: u8::try_from(days_of_week).unwrap_or_default(),
            minute_star: star[0],
            hour_star: star[1],
            day_of_month_star: star[2],
            day_of_week_star: star[4],
        };
        // With the day of week restricted too, a day matches either field, so some day always does.
        if !cron.day_of_month_star && cron.day_of_week_star {
            let possible = (1..=12u8)
                .filter(|m| cron.month(*m))
                .any(|m| (1..=days_in(m)).any(|d| cron.day_of_month(d)));
            if !possible {
                return Err(WhenError::new(
                    "names a day of the month that none of its months has, so it never fires",
                ));
            }
        }
        Ok(cron)
    }

    /// Whether the minute (0-59) is listed.
    pub fn minute(&self, minute: u8) -> bool {
        minute < 60 && self.minutes & (1 << minute) != 0
    }

    /// Whether the hour (0-23) is listed.
    pub fn hour(&self, hour: u8) -> bool {
        hour < 24 && self.hours & (1 << hour) != 0
    }

    /// Whether the day of the month (1-31) is listed, ignoring the day of the week.
    pub fn day_of_month(&self, day: u8) -> bool {
        (1..=31).contains(&day) && self.days_of_month & (1 << day) != 0
    }

    /// Whether the month (1-12) is listed.
    pub fn month(&self, month: u8) -> bool {
        (1..=12).contains(&month) && self.months & (1 << month) != 0
    }

    /// Whether the day of the week (0 Sunday to 6 Saturday) is listed, ignoring the day of the month.
    pub fn day_of_week(&self, weekday: u8) -> bool {
        weekday < 7 && self.days_of_week & (1 << weekday) != 0
    }

    /// Whether a date matches: its month is listed and, when both day fields are restricted (do
    /// not start with `*` or `?`), either day field; otherwise both (Vixie cron's rule).
    pub fn date(&self, month: u8, day: u8, weekday: u8) -> bool {
        let (dom, dow) = (self.day_of_month(day), self.day_of_week(weekday));
        self.month(month)
            && if self.day_of_month_star || self.day_of_week_star {
                dom && dow
            } else {
                dom || dow
            }
    }

    /// Whether the minute and hour fields are both restricted, so the expression names times of
    /// day, evaluated on civil time; otherwise it is an interval, evaluated on instants (ADR 0060,
    /// decision 6).
    pub fn is_time_of_day(&self) -> bool {
        !self.minute_star && !self.hour_star
    }
}

impl fmt::Debug for Cron {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |n: std::ops::RangeInclusive<u8>, has: &dyn Fn(u8) -> bool| -> Vec<u8> {
            n.filter(|v| has(*v)).collect()
        };
        f.debug_struct("Cron")
            .field("minutes", &list(0..=59, &|v| self.minute(v)))
            .field("hours", &list(0..=23, &|v| self.hour(v)))
            .field("days_of_month", &list(1..=31, &|v| self.day_of_month(v)))
            .field("months", &list(1..=12, &|v| self.month(v)))
            .field("days_of_week", &list(0..=6, &|v| self.day_of_week(v)))
            .field("time_of_day", &self.is_time_of_day())
            .finish()
    }
}

/// The days of a month in a leap year: the most it can have.
fn days_in(month: u8) -> u8 {
    match month {
        2 => 29,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

impl Field {
    /// The listed values as bits, and whether the field starts with `*` or `?`.
    fn parse(&self, text: &str) -> Result<(u64, bool), WhenError> {
        let star = text.starts_with('*') || text.starts_with('?');
        let mut bits = 0u64;
        for item in text.split(',') {
            let (range, step) = match item.split_once('/') {
                Some((range, step)) => (range, Some(self.step(item, step)?)),
                None => (item, None),
            };
            let (lo, hi) = if range == "*" || range == "?" {
                (self.min, self.max)
            } else if let Some((a, b)) = range.split_once('-') {
                (self.value(item, a)?, self.value(item, b)?)
            } else {
                let v = self.value(item, range)?;
                (v, if step.is_some() { self.max } else { v })
            };
            if lo > hi {
                return Err(self.refuse(item, "runs backwards"));
            }
            for v in (lo..=hi).step_by(step.unwrap_or(1) as usize) {
                bits |= 1u64 << v;
            }
        }
        Ok((bits, star))
    }

    fn value(&self, item: &str, text: &str) -> Result<u32, WhenError> {
        if let Some(i) = self.names.iter().position(|n| n.eq_ignore_ascii_case(text)) {
            return Ok(self.min + i as u32);
        }
        if text.is_empty() || !text.bytes().all(|c| c.is_ascii_digit()) {
            return Err(self.refuse(item, "is not a number or a name"));
        }
        match text.parse::<u32>() {
            Ok(v) if (self.min..=self.max).contains(&v) => Ok(v),
            _ => Err(self.refuse(item, &format!("is outside {}-{}", self.min, self.max))),
        }
    }

    fn step(&self, item: &str, text: &str) -> Result<u32, WhenError> {
        match text.parse::<u32>() {
            Ok(s) if s >= 1 && s <= self.max && text.bytes().all(|c| c.is_ascii_digit()) => Ok(s),
            _ => Err(self.refuse(item, &format!("has a step that is not 1-{}", self.max))),
        }
    }

    fn refuse(&self, item: &str, why: &str) -> WhenError {
        WhenError::new(format!("has the {} item `{item}`, which {why}", self.name))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn cron(text: &str) -> Cron {
        Cron::parse(text).unwrap()
    }

    fn listed(c: &Cron, field: usize) -> Vec<u8> {
        match field {
            0 => (0..=59).filter(|v| c.minute(*v)).collect(),
            1 => (0..=23).filter(|v| c.hour(*v)).collect(),
            2 => (1..=31).filter(|v| c.day_of_month(*v)).collect(),
            3 => (1..=12).filter(|v| c.month(*v)).collect(),
            _ => (0..=6).filter(|v| c.day_of_week(*v)).collect(),
        }
    }

    #[test]
    fn fields_list_what_they_name() {
        let c = cron("*/15 9-17 1,15 JAN-mar,Dec MON-FRI");
        assert_eq!(listed(&c, 0), [0, 15, 30, 45]);
        assert_eq!(listed(&c, 1), (9..=17).collect::<Vec<_>>());
        assert_eq!(listed(&c, 2), [1, 15]);
        assert_eq!(listed(&c, 3), [1, 2, 3, 12]);
        assert_eq!(listed(&c, 4), [1, 2, 3, 4, 5]);
        assert_eq!(listed(&cron("5/20 1-10/3 * * *"), 0), [5, 25, 45]);
        assert_eq!(listed(&cron("5/20 1-10/3 * * *"), 1), [1, 4, 7, 10]);
        assert_eq!(listed(&cron("? ? ? ? ?"), 0).len(), 60);
        // Bounds: a one-value range, a step equal to the field's largest value, names with steps.
        assert_eq!(listed(&cron("7-7 * * * *"), 0), [7]);
        assert_eq!(listed(&cron("*/59 * * * *"), 0), [0, 59]);
        assert_eq!(listed(&cron("0 */23 * * *"), 1), [0, 23]);
        assert_eq!(listed(&cron("0 0 */31 * *"), 2), [1]);
        assert_eq!(listed(&cron("0 0 * JAN-DEC/3 *"), 3), [1, 4, 7, 10]);
        assert_eq!(listed(&cron("0 0 * FEB/5 *"), 3), [2, 7, 12]);
        assert_eq!(listed(&cron("0 0 * * MON-FRI/2"), 4), [1, 3, 5]);
    }

    #[test]
    fn sunday_is_zero_and_seven() {
        assert_eq!(listed(&cron("0 0 * * 7"), 4), [0]);
        assert_eq!(listed(&cron("0 0 * * 5-7"), 4), [0, 5, 6]);
        assert_eq!(listed(&cron("0 0 * * sun"), 4), [0]);
        assert_eq!(listed(&cron("0 0 * * *"), 4), [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(listed(&cron("0 0 * * */7"), 4), [0]);
        assert_eq!(listed(&cron("0 0 * * 0-7/7"), 4), [0]);
        assert_eq!(listed(&cron("0 0 * * SUN-SAT"), 4), [0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn the_day_rule_is_vixies() {
        // Both restricted: either matches.
        let either = cron("0 0 13 * FRI");
        assert!(either.date(6, 13, 2) && either.date(6, 7, 5) && !either.date(6, 7, 2));
        // One starred, even with a step: both must match.
        let both = cron("0 0 */2 * FRI");
        assert!(both.date(6, 1, 5) && !both.date(6, 2, 5) && !both.date(6, 1, 4));
        let dom_only = cron("0 0 13 * *");
        assert!(dom_only.date(6, 13, 2) && !dom_only.date(6, 14, 2));
        // `?` is a star too.
        let qmark = cron("0 0 13 * ?");
        assert!(qmark.date(6, 13, 2) && !qmark.date(6, 7, 5));
        let qmark = cron("0 0 ? * FRI");
        assert!(qmark.date(6, 7, 5) && !qmark.date(6, 13, 2));
        let month = cron("0 0 * JUN *");
        assert!(month.date(6, 1, 0) && !month.date(7, 1, 0));
    }

    #[test]
    fn times_of_day_and_intervals() {
        assert!(cron("30 2 * * *").is_time_of_day());
        assert!(cron("0,30 9-17 * * *").is_time_of_day());
        assert!(!cron("*/15 * * * *").is_time_of_day());
        assert!(!cron("0 * * * *").is_time_of_day());
        assert!(!cron("0 */2 * * *").is_time_of_day());
        assert!(!cron("0 ? * * *").is_time_of_day());
        assert!(!cron("? 9 * * *").is_time_of_day());
    }

    #[test]
    fn refusals_name_the_item() {
        for (text, want) in [
            ("0 9 * *", "has 4 fields"),
            ("0 9 * * * *", "has 6 fields"),
            ("60 9 * * *", "minute item `60`, which is outside 0-59"),
            ("0 24 * * *", "hour item `24`"),
            ("0 9 0 * *", "day-of-month item `0`"),
            ("0 9 * 13 *", "month item `13`"),
            ("0 9 * * 8", "day-of-week item `8`"),
            (
                "0 9 * * MONDAY",
                "`MONDAY`, which is not a number or a name",
            ),
            ("0 9 * * $", "`$`, which is not a number or a name"),
            ("0 9 * * L", "`L`"),
            ("0 9 * * 5-1", "`5-1`, which runs backwards"),
            ("*/0 9 * * *", "step that is not 1-59"),
            ("*/x 9 * * *", "step that is not 1-59"),
            ("*/60 9 * * *", "step that is not 1-59"),
            ("1,,2 9 * * *", "item ``"),
            ("0 9 -1 * *", "`-1`"),
            ("0 9 +1 * *", "`+1`"),
            ("0 9 30 2 *", "never fires"),
            ("0 9 31 4,6 *", "never fires"),
            ("0 9 31 4,6 ?", "never fires"),
            ("0\u{a0}9 * * *", "has 4 fields"),
            ("0 9 * * *\u{3000}", "`*\u{3000}`"),
            ("0 9 * * 7-6", "runs backwards"),
            ("*/0 9 * * *", "step that is not 1-59"),
            ("0 */24 * * *", "step that is not 1-23"),
            ("0 9 * * */8", "step that is not 1-7"),
            ("*-5 9 * * *", "`*-5`"),
            ("** 9 * * *", "`**`"),
            ("0 9 * * 1,", "item ``"),
        ] {
            let err = Cron::parse(text).unwrap_err().to_string();
            assert!(err.contains(want), "{text}: {err}");
        }
    }

    #[test]
    fn a_day_some_month_has_is_fine() {
        assert!(Cron::parse("0 9 29 2 *").is_ok());
        assert!(Cron::parse("0 9 31 1-3 *").is_ok());
        // The day of week makes it fire, by the rule above.
        assert!(Cron::parse("0 9 30 2 MON").is_ok());
    }

    #[test]
    fn at_takes_an_instant_or_a_civil_time() {
        let civil = |s: &str| match parse_at(s).unwrap() {
            At::Civil(d) => d.to_string(),
            At::Instant(t) => panic!("{s} is an instant {t}"),
        };
        let instant = |s: &str| match parse_at(s).unwrap() {
            At::Instant(t) => t.to_string(),
            At::Civil(d) => panic!("{s} is civil {d}"),
        };
        assert_eq!(civil("2026-12-24T18:00"), "2026-12-24T18:00:00");
        assert_eq!(civil("2026-12-24T18:00:30"), "2026-12-24T18:00:30");
        assert_eq!(instant("2026-12-24T18:00Z"), "2026-12-24T18:00:00Z");
        assert_eq!(instant("2026-12-24T18:00:00+01:00"), "2026-12-24T17:00:00Z");
        assert_eq!(instant("2026-12-24T18:00-05:30"), "2026-12-24T23:30:00Z");
    }

    #[test]
    fn at_refuses_what_jiff_would_bend() {
        for (text, want) in [
            ("2026-12-24", "is not a time"),
            ("2026-12-24 18:00", "is not a time"),
            ("2026-12-24t18:00", "is not a time"),
            ("2026-12-24T18:00z", "is not a time"),
            ("2026-12-24T18:00[Europe/Berlin]", "is not a time"),
            ("2026-12-24T18:00+01:00[Europe/Berlin]", "is not a time"),
            ("2026-12-24T18:00:00.5", "is not a time"),
            ("2026-12-24T18:00+0100", "is not a time"),
            ("2026-12-24T18:00+25:00", "beyond a day"),
            ("2026-02-30T18:00", "not a valid date"),
            ("2026-12-24T24:00", "not a valid date"),
            ("tomorrow", "is not a time"),
            ("", "is not a time"),
            ("2026-12-31T23:59:60Z", "leap second"),
            ("2026-12-24T18:00-00:00", "unknown offset"),
            ("9999-01-01T00:00", "after the year 9998"),
            ("9999-12-31T23:59Z", "after the year 9998"),
            ("2026-12-24T18:00\u{a0}", "is not a time"),
            ("２026-12-24T18:00", "is not a time"),
        ] {
            let err = parse_at(text).unwrap_err().to_string();
            assert!(err.contains(want), "{text}: {err}");
        }
    }

    #[test]
    fn time_zones_come_from_the_bundled_database() {
        assert!(time_zone("UTC").is_ok());
        assert!(time_zone("Europe/Berlin").is_ok());
        assert!(time_zone("America/Argentina/Buenos_Aires").is_ok());
        assert!(time_zone("Etc/GMT+1").is_ok());
        for bad in [
            "",
            "Mars/Olympus",
            "Mars Base",
            "Europe/Berlin\u{a0}",
            "../etc/passwd",
            "UTC\n",
        ] {
            let err = time_zone(bad).unwrap_err().to_string();
            assert!(err.contains("is not an IANA time zone"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn after_counts_units() {
        let secs = |s: &str| parse_after(s).unwrap().as_secs();
        assert_eq!(secs("90s"), 90);
        assert_eq!(secs("30m"), 1_800);
        assert_eq!(secs("2h"), 7_200);
        assert_eq!(secs("1h30m"), 5_400);
        assert_eq!(secs("1d 12h"), 129_600);
        assert_eq!(secs("366d"), AFTER_MAX.as_secs());
        assert_eq!(secs(" 1h\t30m "), 5_400);
    }

    #[test]
    fn after_refusals() {
        for (text, want) in [
            ("", "is not a duration"),
            ("30", "is not a duration"),
            ("m", "is not a duration"),
            ("30x", "is not a duration"),
            ("1.5h", "is not a duration"),
            ("-30m", "is not a duration"),
            ("30m1h", "out of order or twice"),
            ("1h1h", "out of order or twice"),
            ("0s", "is zero"),
            ("0h0m", "is zero"),
            ("367d", "longer than 366 days"),
            ("99999999999999999999s", "too long"),
            ("999999999999999999d", "too long"),
            ("1d\u{3000}1h", "is not a duration"),
            ("1h\u{a0}", "is not a duration"),
            ("1ｈ", "is not a duration"),
        ] {
            let err = parse_after(text).unwrap_err().to_string();
            assert!(err.contains(want), "{text}: {err}");
        }
    }
}
