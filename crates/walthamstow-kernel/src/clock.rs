//! Copyright (c) Scott A Dixon
//!
//! Monotonic time as the engine schedules by it. A [`Moment`] is a duration
//! since the process's monotonic origin on Tokio's clock, the clock the engine's
//! timers sleep on, so code that schedules by time takes a moment as an
//! argument and a test hands it any moment it likes, or pauses that clock.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use jiff::tz::Offset;
use jiff::Timestamp;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Moment(Duration);

impl Moment {
    /// The first moment, which tests build later moments from.
    pub const ORIGIN: Moment = Moment(Duration::ZERO);

    /// The moment the process is at now.
    pub fn now() -> Self {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        Moment(ORIGIN.get_or_init(Instant::now).elapsed())
    }
}

impl Moment {
    /// How long ago this moment was.
    pub fn elapsed(self) -> Duration {
        Moment::now().0.saturating_sub(self.0)
    }

    /// How long until this moment, or nothing when it has passed.
    pub fn remaining(self) -> Duration {
        self.0.saturating_sub(Moment::now().0)
    }

    /// How long after `earlier` this moment is, or nothing when it is not
    /// after it.
    pub fn duration_since(self, earlier: Moment) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

impl std::ops::Add<Duration> for Moment {
    type Output = Moment;

    fn add(self, duration: Duration) -> Moment {
        Moment(self.0 + duration)
    }
}

/// Where code that records or compares wall time reads it. The engine reads
/// one clock, given to it when it loads.
#[derive(Clone)]
pub struct WallClock(Arc<dyn Fn() -> SystemTime + Send + Sync>);

impl WallClock {
    /// A clock that reads the time from `now`.
    pub fn new(now: impl Fn() -> SystemTime + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    pub fn now(&self) -> SystemTime {
        (self.0)()
    }

    /// The time now in UTC.
    pub fn utc(&self) -> Timestamp {
        Timestamp::try_from(self.now()).expect("the wall clock reads a time between -9999 and 9999")
    }

    /// The time now as an RFC 3339 timestamp at `+00:00`, its fraction of a second in whole
    /// milli-, micro- or nanoseconds, as the engine records it.
    pub fn rfc3339(&self) -> String {
        let now = self.utc();
        format!(
            "{:.*}",
            fraction_digits(now),
            now.display_with_offset(Offset::UTC)
        )
    }

    /// The time now, in milliseconds since the UNIX epoch.
    pub fn epoch_ms(&self) -> i64 {
        self.now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
            })
    }
}

/// The digits of `timestamp`'s fraction of a second: none, or whole milli-, micro- or
/// nanoseconds.
fn fraction_digits(timestamp: Timestamp) -> usize {
    match timestamp.subsec_nanosecond().unsigned_abs() {
        0 => 0,
        nanos if nanos % 1_000_000 == 0 => 3,
        nanos if nanos % 1_000 == 0 => 6,
        _ => 9,
    }
}

/// Serde for a [`Timestamp`] as RFC 3339 in UTC, written with `Z` and its fraction of a second in
/// whole milli-, micro- or nanoseconds. It reads any RFC 3339 timestamp.
pub mod utc_rfc3339 {
    use jiff::Timestamp;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        timestamp: &Timestamp,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!(
            "{:.*}",
            super::fraction_digits(*timestamp),
            timestamp
        ))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Timestamp, D::Error> {
        Timestamp::deserialize(deserializer)
    }
}

impl Default for WallClock {
    fn default() -> Self {
        Self::new(SystemTime::now)
    }
}

impl std::fmt::Debug for WallClock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WallClock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A moment is as long after an earlier one as was added to it, and no
    /// time at all after a later one.
    #[test]
    fn a_moment_is_after_an_earlier_one_by_what_was_added() {
        let added = Duration::from_secs(5);
        let later = Moment::ORIGIN + added;

        assert_eq!(later.duration_since(Moment::ORIGIN), added);
        assert_eq!(Moment::ORIGIN.duration_since(later), Duration::ZERO);
    }

    /// Times across a second's fractions, at the clock's whole nanoseconds.
    fn moments() -> impl Iterator<Item = SystemTime> {
        [0, 120_000_000, 123_400_000, 123_456_789]
            .into_iter()
            .map(|nanos| SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, nanos))
    }

    /// The digits after the decimal point of an RFC 3339 `timestamp` ending in `offset`.
    fn fraction(timestamp: &str, offset: &str) -> usize {
        timestamp
            .strip_suffix(offset)
            .and_then(|rest| rest.split_once('.'))
            .map_or(0, |(_, digits)| digits.len())
    }

    /// The engine records a time at `+00:00` that reads back as that time, its fraction of a
    /// second in whole milli-, micro- or nanoseconds.
    #[test]
    fn the_engine_records_a_time_at_utc_that_reads_back() {
        for moment in moments() {
            let recorded = WallClock::new(move || moment).rfc3339();
            let read: Timestamp = recorded.parse().expect("an RFC 3339 timestamp");
            assert_eq!(SystemTime::try_from(read).unwrap(), moment, "{recorded}");
            assert!(recorded.ends_with("+00:00"), "{recorded}");
            assert!(
                [0, 3, 6, 9].contains(&fraction(&recorded, "+00:00")),
                "{recorded}"
            );
        }
    }

    /// A timestamp serialised in UTC reads back as itself, written with `Z` and its fraction of a
    /// second in whole milli-, micro- or nanoseconds.
    #[test]
    fn a_serialised_utc_timestamp_reads_back() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Recorded(#[serde(with = "utc_rfc3339")] Timestamp);

        for moment in moments() {
            let timestamp = WallClock::new(move || moment).utc();
            let json = serde_json::to_value(Recorded(timestamp)).unwrap();
            let written = json.as_str().expect("a string").to_owned();
            let Recorded(read) = serde_json::from_value(json).unwrap();
            assert_eq!(read, timestamp, "{written}");
            assert!(written.ends_with('Z'), "{written}");
            assert!([0, 3, 6, 9].contains(&fraction(&written, "Z")), "{written}");
        }
    }
}
