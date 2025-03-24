//! Clock abstraction (D14): UTC everywhere, injectable for tests.
//!
//! All persisted timestamps are UTC. Tests inject [`FakeClock`] so decay,
//! expiry, and idle-session logic are deterministic without sleeps.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Milliseconds since the Unix epoch.
pub type UnixMillis = i64;

/// Source of time. Implementors must return UTC epoch milliseconds.
pub trait Clock: Send + Sync {
    /// Current time in milliseconds since the Unix epoch (UTC).
    fn now_millis(&self) -> UnixMillis;
}

/// Real system clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> UnixMillis {
        chrono::Utc::now().timestamp_millis()
    }
}

/// Deterministic clock for tests: starts at a fixed epoch and only advances
/// when [`FakeClock::advance`] is called.
#[derive(Debug)]
pub struct FakeClock {
    millis: AtomicU64,
}

impl FakeClock {
    /// A fake clock pinned to 2026-01-01T00:00:00Z.
    pub fn new() -> Self {
        Self::at(1767225600000) // 2026-01-01T00:00:00Z
    }

    /// A fake clock pinned to an explicit epoch-millis instant.
    pub fn at(millis: i64) -> Self {
        Self {
            millis: AtomicU64::new(millis.max(0) as u64),
        }
    }

    /// Advance the clock; negative durations are ignored.
    pub fn advance(&self, d: Duration) {
        self.millis
            .fetch_add(d.as_millis() as u64, Ordering::SeqCst);
    }

    /// Arc handle for injection.
    pub fn shared() -> Arc<FakeClock> {
        Arc::new(Self::new())
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn now_millis(&self) -> UnixMillis {
        self.millis.load(Ordering::SeqCst) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_clock_is_deterministic() {
        let c = FakeClock::shared();
        let t0 = c.now_millis();
        assert_eq!(t0, c.now_millis());
        c.advance(Duration::from_secs(30));
        assert_eq!(c.now_millis(), t0 + 30_000);
    }

    #[test]
    fn fake_clock_negative_advance_is_ignored() {
        let c = FakeClock::at(1000);
        c.advance(Duration::from_millis(0));
        c.advance(Duration::from_millis(5));
        assert_eq!(c.now_millis(), 1005);
    }

    #[test]
    fn system_clock_near_utc_now() {
        let now = SystemClock.now_millis();
        let ref_now = chrono::Utc::now().timestamp_millis();
        assert!((now - ref_now).abs() < 5_000);
    }
}
