//! State-owned bounded wall-clock policy backed by monotonic elapsed time.
//!
//! Persisted timestamps are validated against wall time once when the policy is
//! anchored. In-process currentness advances only with the monotonic clock, so
//! wall-clock corrections cannot extend or resurrect admitted state.

use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) const MAX_DURABLE_COOLDOWN_SECONDS: i64 = 24 * 60 * 60;
pub(crate) const MAX_DURABLE_FUTURE_SKEW_SECONDS: i64 = 5 * 60;

#[derive(Clone, Copy)]
pub(crate) struct TemporalClockReading {
    pub(crate) wall: DateTime<Utc>,
    pub(crate) monotonic: Duration,
}

pub(crate) trait TemporalClock: Send + Sync {
    fn read(&self) -> TemporalClockReading;
}

struct SystemTemporalClock {
    started: Instant,
}

impl SystemTemporalClock {
    fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl TemporalClock for SystemTemporalClock {
    fn read(&self) -> TemporalClockReading {
        TemporalClockReading {
            wall: Utc::now(),
            monotonic: self.started.elapsed(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct BoundedTemporalRecord<'a> {
    pub(crate) first_observed_at: &'a str,
    pub(crate) last_observed_at: &'a str,
    pub(crate) suppression_until: Option<&'a str>,
    pub(crate) pending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BoundedTemporalDisposition {
    Current,
    Expired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BoundedTemporalViolation {
    MalformedTimestamp,
    ObservationTooFarInFuture,
    SuppressionWindowOutOfBounds,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct BoundedTemporalLoadIssueCounts {
    future_observation: usize,
    out_of_bounds_window: usize,
}

impl BoundedTemporalLoadIssueCounts {
    pub(crate) const fn new(future_observation: usize, out_of_bounds_window: usize) -> Self {
        Self {
            future_observation,
            out_of_bounds_window,
        }
    }

    pub(crate) fn record(&mut self, violation: BoundedTemporalViolation) {
        match violation {
            BoundedTemporalViolation::ObservationTooFarInFuture => {
                self.future_observation = self.future_observation.saturating_add(1);
            }
            BoundedTemporalViolation::SuppressionWindowOutOfBounds => {
                self.out_of_bounds_window = self.out_of_bounds_window.saturating_add(1);
            }
            BoundedTemporalViolation::MalformedTimestamp => {}
        }
    }

    pub(crate) const fn future_observation(self) -> usize {
        self.future_observation
    }

    pub(crate) const fn out_of_bounds_window(self) -> usize {
        self.out_of_bounds_window
    }

    pub(crate) const fn total(self) -> usize {
        self.future_observation
            .saturating_add(self.out_of_bounds_window)
    }
}

pub(crate) struct BoundedTemporalPolicy {
    clock: Arc<dyn TemporalClock>,
    anchor: TemporalClockReading,
    high_watermark: Mutex<DateTime<Utc>>,
}

impl BoundedTemporalPolicy {
    pub(crate) fn system() -> Self {
        Self::new(Arc::new(SystemTemporalClock::new()))
    }

    pub(crate) fn new(clock: Arc<dyn TemporalClock>) -> Self {
        let anchor = clock.read();
        Self {
            clock,
            anchor,
            high_watermark: Mutex::new(anchor.wall),
        }
    }

    #[cfg(test)]
    pub(crate) fn fixed(wall: DateTime<Utc>) -> Self {
        struct FixedClock(DateTime<Utc>);
        impl TemporalClock for FixedClock {
            fn read(&self) -> TemporalClockReading {
                TemporalClockReading {
                    wall: self.0,
                    monotonic: Duration::ZERO,
                }
            }
        }
        Self::new(Arc::new(FixedClock(wall)))
    }

    pub(crate) fn assess(
        &self,
        record: BoundedTemporalRecord<'_>,
    ) -> Result<BoundedTemporalDisposition, BoundedTemporalViolation> {
        let first_observed_at = parse_timestamp(record.first_observed_at)?;
        let last_observed_at = parse_timestamp(record.last_observed_at)?;
        let future_limit = self
            .now()
            .checked_add_signed(chrono::Duration::seconds(MAX_DURABLE_FUTURE_SKEW_SECONDS))
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        if first_observed_at > future_limit || last_observed_at > future_limit {
            return Err(BoundedTemporalViolation::ObservationTooFarInFuture);
        }
        if let Some(suppression_until) = record.suppression_until {
            parse_timestamp(suppression_until)?;
            if !suppression_window_is_bounded(record.last_observed_at, suppression_until) {
                return Err(BoundedTemporalViolation::SuppressionWindowOutOfBounds);
            }
        }
        if record.pending {
            return Ok(BoundedTemporalDisposition::Current);
        }
        Ok(match record.suppression_until {
            Some(until) if parse_timestamp(until)? <= self.now() => {
                BoundedTemporalDisposition::Expired
            }
            _ => BoundedTemporalDisposition::Current,
        })
    }

    pub(crate) fn suppression_active(&self, record: BoundedTemporalRecord<'_>) -> bool {
        self.assess(record)
            .is_ok_and(|disposition| disposition == BoundedTemporalDisposition::Current)
            && record
                .suppression_until
                .and_then(|until| parse_timestamp(until).ok())
                .is_some_and(|until| until > self.now())
    }

    pub(crate) fn now_timestamp(&self) -> String {
        self.now().to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    fn now(&self) -> DateTime<Utc> {
        let reading = self.clock.read();
        let elapsed = reading
            .monotonic
            .checked_sub(self.anchor.monotonic)
            .unwrap_or_default();
        let elapsed = chrono::Duration::from_std(elapsed).unwrap_or(chrono::Duration::MAX);
        let candidate = self
            .anchor
            .wall
            .checked_add_signed(elapsed)
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        let mut high_watermark = self
            .high_watermark
            .lock()
            .expect("State temporal policy lock poisoned");
        if candidate > *high_watermark {
            *high_watermark = candidate;
        }
        *high_watermark
    }
}

pub(crate) fn suppression_window_is_bounded(observed_at: &str, suppression_until: &str) -> bool {
    if observed_at != observed_at.trim() || suppression_until != suppression_until.trim() {
        return false;
    }
    let (Ok(observed_at), Ok(suppression_until)) = (
        DateTime::<FixedOffset>::parse_from_rfc3339(observed_at),
        DateTime::<FixedOffset>::parse_from_rfc3339(suppression_until),
    ) else {
        return false;
    };
    suppression_until > observed_at
        && suppression_until - observed_at
            <= chrono::Duration::seconds(MAX_DURABLE_COOLDOWN_SECONDS)
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, BoundedTemporalViolation> {
    if value != value.trim() {
        return Err(BoundedTemporalViolation::MalformedTimestamp);
    }
    DateTime::<FixedOffset>::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|_| BoundedTemporalViolation::MalformedTimestamp)
}
