// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Monotonic time used for every rate computation.
//!
//! Wall-clock time can jump; rates are always derived from [`Timestamp`]s
//! produced by a [`Clock`]. Tests inject [`VirtualClock`] so schedules and
//! rates are deterministic.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Nanoseconds on a monotonic clock with an arbitrary origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(pub u64);

impl Timestamp {
    pub const ZERO: Timestamp = Timestamp(0);

    pub fn from_millis(ms: u64) -> Self {
        Timestamp(ms.saturating_mul(1_000_000))
    }

    pub fn as_nanos(self) -> u64 {
        self.0
    }

    /// Elapsed time since `earlier`, or `None` when `earlier` is not earlier.
    pub fn since(self, earlier: Timestamp) -> Option<Duration> {
        self.0
            .checked_sub(earlier.0)
            .filter(|d| *d > 0)
            .map(Duration::from_nanos)
    }

    /// Saturating elapsed time (zero when `earlier` is in the future).
    pub fn saturating_since(self, earlier: Timestamp) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }

    pub fn plus(self, d: Duration) -> Timestamp {
        Timestamp(
            self.0
                .saturating_add(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)),
        )
    }
}

/// Source of monotonic timestamps.
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> Timestamp;
}

/// Real monotonic clock (CLOCK_MONOTONIC via [`Instant`]).
#[derive(Debug, Clone)]
pub struct MonotonicClock {
    origin: Instant,
}

impl MonotonicClock {
    pub fn new() -> Self {
        MonotonicClock {
            origin: Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Timestamp {
        // +1 so that the first reading is never Timestamp::ZERO, which
        // callers use as "never observed".
        Timestamp(u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX - 1) + 1)
    }
}

/// Manually advanced clock for deterministic tests.
#[derive(Debug, Clone, Default)]
pub struct VirtualClock {
    nanos: Arc<AtomicU64>,
}

impl VirtualClock {
    pub fn new(start: Timestamp) -> Self {
        VirtualClock {
            nanos: Arc::new(AtomicU64::new(start.0)),
        }
    }

    pub fn advance(&self, d: Duration) {
        let n = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.nanos.fetch_add(n, Ordering::SeqCst);
    }

    pub fn set(&self, t: Timestamp) {
        self.nanos.store(t.0, Ordering::SeqCst);
    }
}

impl Clock for VirtualClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.nanos.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_rejects_non_positive_elapsed() {
        assert_eq!(Timestamp(5).since(Timestamp(5)), None);
        assert_eq!(Timestamp(4).since(Timestamp(5)), None);
        assert_eq!(
            Timestamp(7).since(Timestamp(5)),
            Some(Duration::from_nanos(2))
        );
    }

    #[test]
    fn virtual_clock_is_shared_between_clones() {
        let c = VirtualClock::new(Timestamp::from_millis(1));
        let d = c.clone();
        c.advance(Duration::from_millis(9));
        assert_eq!(d.now(), Timestamp::from_millis(10));
    }
}
