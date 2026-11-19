//! The instant adam-rs stores and sends: [`Timestamp`], a [`jiff::Timestamp`] with the serde
//! bytes the framework wrote before it moved off `chrono` (ADR 0053).
//!
//! `chrono::DateTime<Utc>` serialized as RFC 3339 with `Z` and chrono's `SecondsFormat::AutoSi`:
//! no fractional digits for a whole second, else 3, 6 or 9, whichever is the shortest exact one.
//! `jiff::Timestamp`'s own serde trims **every** trailing zero (`.120Z` becomes `.12Z`), which
//! would change the bytes of a stored run state and of a push cursor. This newtype writes the old
//! bytes and reads both, so a state written by either build decodes and re-encodes to itself.

use std::fmt;
use std::ops::{Add, AddAssign, Deref, Sub, SubAssign};
use std::str::FromStr;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp as JiffTimestamp};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An instant, at nanosecond precision, in UTC.
///
/// The field is public like [`RunId`](crate::RunId)'s: an adapter converts at its own edge (the
/// PostgreSQL store maps it to and from `TIMESTAMPTZ`), and nothing else needs to look inside.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub JiffTimestamp);

impl Timestamp {
    /// The instant at which the Unix epoch began.
    pub const UNIX_EPOCH: Timestamp = Timestamp(JiffTimestamp::UNIX_EPOCH);
    /// The earliest instant a [`Timestamp`] can hold.
    pub const MIN: Timestamp = Timestamp(JiffTimestamp::MIN);
    /// The latest instant a [`Timestamp`] can hold.
    pub const MAX: Timestamp = Timestamp(JiffTimestamp::MAX);

    /// The current time, at nanosecond precision (the stores truncate what they keep).
    pub fn now() -> Self {
        Self(JiffTimestamp::now())
    }

    /// The instant `millisecond` milliseconds after the Unix epoch, truncating toward zero for a
    /// negative value with a fractional millisecond.
    ///
    /// # Errors
    ///
    /// [`jiff::Error`] when the value is outside [`Timestamp::MIN`] and [`Timestamp::MAX`].
    pub fn from_millisecond(millisecond: i64) -> Result<Self, jiff::Error> {
        JiffTimestamp::from_millisecond(millisecond).map(Self)
    }

    /// The instant `second` seconds after the Unix epoch.
    ///
    /// # Errors
    ///
    /// [`jiff::Error`] when the value is outside [`Timestamp::MIN`] and [`Timestamp::MAX`].
    pub fn from_second(second: i64) -> Result<Self, jiff::Error> {
        JiffTimestamp::from_second(second).map(Self)
    }
}

impl Deref for Timestamp {
    type Target = JiffTimestamp;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<JiffTimestamp> for Timestamp {
    fn from(value: JiffTimestamp) -> Self {
        Self(value)
    }
}

impl From<Timestamp> for JiffTimestamp {
    fn from(value: Timestamp) -> Self {
        value.0
    }
}

impl FromStr for Timestamp {
    type Err = jiff::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<JiffTimestamp>().map(Self)
    }
}

/// Adding a duration saturates at [`Timestamp::MAX`] rather than panicking, as the framework's
/// arithmetic always did.
impl Add<SignedDuration> for Timestamp {
    type Output = Timestamp;

    fn add(self, rhs: SignedDuration) -> Timestamp {
        Timestamp(self.0.saturating_add(rhs).unwrap_or(JiffTimestamp::MAX))
    }
}

impl AddAssign<SignedDuration> for Timestamp {
    fn add_assign(&mut self, rhs: SignedDuration) {
        *self = *self + rhs;
    }
}

/// Subtracting a duration saturates at [`Timestamp::MIN`].
impl Sub<SignedDuration> for Timestamp {
    type Output = Timestamp;

    fn sub(self, rhs: SignedDuration) -> Timestamp {
        Timestamp(self.0.saturating_sub(rhs).unwrap_or(JiffTimestamp::MIN))
    }
}

impl SubAssign<SignedDuration> for Timestamp {
    fn sub_assign(&mut self, rhs: SignedDuration) {
        *self = *self - rhs;
    }
}

/// The elapsed time from `rhs` to `self`, negative when `self` is earlier.
impl Sub for Timestamp {
    type Output = SignedDuration;

    fn sub(self, rhs: Timestamp) -> SignedDuration {
        self.0.duration_since(rhs.0)
    }
}

/// The framework's std-duration arithmetic: a duration too large for [`SignedDuration`] saturates.
///
/// `chrono::Duration::from_std(..).unwrap_or(chrono::Duration::MAX)` was the same rule before
/// ADR 0053; use this instead of a per-crate copy.
pub fn span_of(duration: Duration) -> SignedDuration {
    SignedDuration::try_from(duration).unwrap_or(SignedDuration::MAX)
}

impl fmt::Display for Timestamp {
    /// RFC 3339, `Z`, and chrono's `AutoSi` digits (see the module docs).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let nanos = self.0.subsec_nanosecond().unsigned_abs();
        let precision = if nanos == 0 {
            0
        } else if nanos % 1_000_000 == 0 {
            3
        } else if nanos % 1_000 == 0 {
            6
        } else {
            9
        };
        write!(f, "{:.precision$}", self.0)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        JiffTimestamp::deserialize(deserializer).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes chrono's `DateTime<Utc>` wrote, shape by shape: a whole second, 3 digits (with
    /// the trailing zeros that jiff's own serde would trim), 6, and 9; a fraction whose last
    /// digits are zeros keeps the shortest exact width.
    #[test]
    fn every_fractional_width_serializes_as_chrono_wrote_it() {
        let cases = [
            ("2026-01-02T03:04:05Z", "2026-01-02T03:04:05Z"),
            ("2026-01-02T03:04:05.120Z", "2026-01-02T03:04:05.120Z"),
            ("2026-01-02T03:04:05.100Z", "2026-01-02T03:04:05.100Z"),
            ("2026-01-02T03:04:05.123Z", "2026-01-02T03:04:05.123Z"),
            ("2026-01-02T03:04:05.123456Z", "2026-01-02T03:04:05.123456Z"),
            (
                "2026-01-02T03:04:05.123456000Z",
                "2026-01-02T03:04:05.123456Z",
            ),
            (
                "2026-01-02T03:04:05.123456700Z",
                "2026-01-02T03:04:05.123456700Z",
            ),
            (
                "2026-01-02T03:04:05.123456789Z",
                "2026-01-02T03:04:05.123456789Z",
            ),
            // Before the epoch, where jiff keeps the fraction negative and the wall clock does not.
            ("1969-12-31T23:59:59.500Z", "1969-12-31T23:59:59.500Z"),
        ];
        for (input, want) in cases {
            let at: Timestamp = input.parse().expect("a timestamp");
            assert_eq!(
                serde_json::to_string(&at).expect("serialises"),
                format!("\"{want}\""),
                "{input}"
            );
        }
    }

    #[test]
    fn a_timestamp_round_trips_through_serde() {
        for input in [
            "2026-01-02T03:04:05Z",
            "2026-01-02T03:04:05.120Z",
            "2026-01-02T03:04:05.123456Z",
            "2026-01-02T03:04:05.123456789Z",
            "1969-12-31T23:59:59.500Z",
        ] {
            let at: Timestamp = input.parse().expect("a timestamp");
            let bytes = serde_json::to_string(&at).expect("serialises");
            let back: Timestamp = serde_json::from_str(&bytes).expect("decodes");
            assert_eq!(back, at, "{input}");
            assert_eq!(serde_json::to_string(&back).expect("serialises"), bytes);
        }
    }

    /// An offset is accepted on the way in and written as `Z`, as chrono's serde did.
    #[test]
    fn an_offset_decodes_and_is_written_as_utc() {
        let at: Timestamp = "2026-01-02T04:04:05.120+01:00"
            .parse()
            .expect("a timestamp");
        assert_eq!(
            serde_json::to_string(&at).expect("serialises"),
            "\"2026-01-02T03:04:05.120Z\""
        );
    }

    #[test]
    fn arithmetic_saturates_instead_of_panicking() {
        assert_eq!(Timestamp::MAX + SignedDuration::from_secs(1), Timestamp::MAX);
        assert_eq!(Timestamp::MIN - SignedDuration::from_secs(1), Timestamp::MIN);
        let start = Timestamp::from_second(1_700_000_000).expect("in range");
        assert_eq!(
            start + SignedDuration::from_millis(1_500),
            Timestamp::from_millisecond(1_700_000_001_500).expect("in range")
        );
        assert_eq!(
            (start + SignedDuration::from_secs(5)) - start,
            SignedDuration::from_secs(5)
        );
        assert_eq!(
            start - (start + SignedDuration::from_secs(5)),
            SignedDuration::from_secs(-5)
        );
    }

    #[test]
    fn a_duration_that_does_not_fit_saturates() {
        let at = Timestamp::from_second(1_700_000_000).expect("in range");
        assert_eq!(at + span_of(Duration::MAX), Timestamp::MAX);
        assert_eq!(span_of(Duration::from_millis(1_500)), SignedDuration::from_millis(1_500));
    }
}
