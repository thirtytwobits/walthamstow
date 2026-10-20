//! Copyright (c) Scott A Dixon
//!
//! Monotonic time as the engine schedules by it. A [`Moment`] is a duration
//! since the process's monotonic origin on Tokio's clock, the clock the engine's
//! timers sleep on, so code that schedules by time takes a moment as an
//! argument and a test hands it any moment it likes, or pauses that clock.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};
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
    pub fn utc(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(self.now())
    }

    /// The time now as an RFC 3339 timestamp, as the engine records it.
    pub fn rfc3339(&self) -> String {
        self.utc().to_rfc3339()
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
