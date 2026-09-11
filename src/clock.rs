// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! A minimal injectable time source (#308): `engine::stream`'s ring
//! autoscaling samples wall-clock ingestion rate, and testing an EWMA over
//! simulated minutes/hours without a `FakeClock` would mean real
//! `sleep`s. Deliberately small -- one method, no calendar/timezone
//! concerns (those stay in `storage::stream`'s per-line parsers).

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::SystemTime;

/// A source of the current time, nanoseconds since the Unix epoch.
pub trait Clock: Send + Sync {
    /// Nanoseconds since the Unix epoch.
    fn now_ns(&self) -> i64;
}

/// The real clock: a thin `SystemTime::now()` wrapper.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
}

/// A settable clock for deterministic tests: simulating a 42-minute ring
/// or a 1-day summary horizon by advancing this instead of sleeping.
/// Gated the same way as `storage::row::btree::test_minimal_db` (this
/// crate's own test-only-export convention) so an integration test can
/// enable it via `storage-test-support` without a real wall-clock wait.
#[cfg(any(test, feature = "storage-test-support"))]
#[derive(Debug)]
pub struct FakeClock(AtomicI64);

#[cfg(any(test, feature = "storage-test-support"))]
impl FakeClock {
    /// A clock starting at `now_ns`.
    #[must_use]
    pub const fn new(now_ns: i64) -> Self {
        Self(AtomicI64::new(now_ns))
    }

    /// Move the clock forward by `delta_ns` (may be negative).
    pub fn advance(&self, delta_ns: i64) {
        self.0.fetch_add(delta_ns, Ordering::SeqCst);
    }

    /// Set the clock to an absolute time.
    pub fn set(&self, now_ns: i64) {
        self.0.store(now_ns, Ordering::SeqCst);
    }
}

#[cfg(any(test, feature = "storage-test-support"))]
impl Clock for FakeClock {
    fn now_ns(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_advances_with_real_time() {
        let c = SystemClock;
        let a = c.now_ns();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = c.now_ns();
        assert!(b > a);
    }

    #[test]
    fn fake_clock_only_moves_when_told() {
        let c = FakeClock::new(1_000);
        assert_eq!(c.now_ns(), 1_000);
        c.advance(500);
        assert_eq!(c.now_ns(), 1_500);
        c.set(0);
        assert_eq!(c.now_ns(), 0);
    }
}
