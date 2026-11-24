// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Reset-safe differentiation of monotonically increasing kernel counters.
//!
//! Never subtract kernel counters blindly: devices reset, drivers reload,
//! 32-bit counters wrap and processes are replaced. A rate is produced only
//! when two observations of the *same* entity are ordered in time and the
//! delta is explainable; otherwise the caller gets a [`RateError`] and a new
//! baseline is kept, so no false spike is ever published.

use std::collections::HashMap;
use std::hash::Hash;

use crate::metric::{Metric, Unavailable};
use crate::time::Timestamp;

/// Bit width of a counter, when known. Only known-width counters may be
/// treated as wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterWidth {
    Bits32,
    Bits64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterSample {
    pub value: u64,
    pub at: Timestamp,
}

impl CounterSample {
    pub fn new(value: u64, at: Timestamp) -> Self {
        CounterSample { value, at }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateError {
    /// No previous observation.
    NoBaseline,
    /// The counter went backwards in a way wraparound cannot explain.
    Reset,
    /// The two observations are not ordered in time.
    NonPositiveElapsed,
}

impl From<RateError> for Unavailable {
    fn from(e: RateError) -> Self {
        match e {
            RateError::NoBaseline | RateError::NonPositiveElapsed => Unavailable::Pending,
            RateError::Reset => Unavailable::CounterReset,
        }
    }
}

/// Delta between two observations, honouring wraparound for 32-bit counters.
///
/// A decrease of a 32-bit counter is accepted as one wrap only when the
/// previous value was in the upper half of the range and the implied delta is
/// below half the range; anything else is a reset. 64-bit counters never wrap
/// in practice, so a decrease is always a reset.
pub fn counter_delta(prev: u64, cur: u64, width: CounterWidth) -> Result<u64, RateError> {
    if cur >= prev {
        return Ok(cur - prev);
    }
    match width {
        CounterWidth::Bits64 => Err(RateError::Reset),
        CounterWidth::Bits32 => {
            const RANGE: u64 = 1 << 32;
            if prev >= RANGE || cur >= RANGE || prev < RANGE / 2 {
                return Err(RateError::Reset);
            }
            let delta = RANGE - prev + cur;
            if delta < RANGE / 2 {
                Ok(delta)
            } else {
                Err(RateError::Reset)
            }
        }
    }
}

/// Per-second rate between two samples.
pub fn derive_rate(
    prev: CounterSample,
    cur: CounterSample,
    width: CounterWidth,
) -> Result<f64, RateError> {
    let elapsed = cur.at.since(prev.at).ok_or(RateError::NonPositiveElapsed)?;
    let delta = counter_delta(prev.value, cur.value, width)?;
    Ok(delta as f64 / elapsed.as_secs_f64())
}

/// Keeps the previous sample of many counters and turns new observations
/// into rates.
///
/// Call [`RateTracker::begin`] at the start of a sampling pass and
/// [`RateTracker::sweep`] at its end to forget counters whose entity was not
/// observed again, so baselines never outlive their entity and memory stays
/// bounded by the live entity count.
#[derive(Debug)]
pub struct RateTracker<K> {
    entries: HashMap<K, (CounterSample, u64)>,
    pass: u64,
}

impl<K: Hash + Eq + Clone> Default for RateTracker<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Hash + Eq + Clone> RateTracker<K> {
    pub fn new() -> Self {
        RateTracker {
            entries: HashMap::new(),
            pass: 0,
        }
    }

    pub fn begin(&mut self) {
        self.pass += 1;
    }

    /// Records `value` for `key` and returns the rate since the previous
    /// observation. On a reset the new value becomes the baseline.
    pub fn observe(
        &mut self,
        key: K,
        value: u64,
        at: Timestamp,
        width: CounterWidth,
    ) -> Result<f64, RateError> {
        let cur = CounterSample::new(value, at);
        let pass = self.pass;
        match self.entries.insert(key, (cur, pass)) {
            None => Err(RateError::NoBaseline),
            Some((prev, _)) => derive_rate(prev, cur, width),
        }
    }

    /// Like [`observe`](Self::observe) but returns a ready-to-publish metric.
    pub fn metric(&mut self, key: K, value: u64, at: Timestamp, width: CounterWidth) -> Metric {
        Metric::from_result(
            self.observe(key, value, at, width)
                .map_err(Unavailable::from),
        )
    }

    /// Like [`metric`](Self::metric) with a scale applied (e.g. sectors→bytes).
    pub fn scaled(
        &mut self,
        key: K,
        value: u64,
        at: Timestamp,
        width: CounterWidth,
        scale: f64,
    ) -> Metric {
        Metric::from_result(
            self.observe(key, value, at, width)
                .map(|r| r * scale)
                .map_err(Unavailable::from),
        )
    }

    /// Drops every counter not observed since the last [`begin`](Self::begin).
    pub fn sweep(&mut self) {
        let pass = self.pass;
        self.entries.retain(|_, (_, p)| *p == pass);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn s(v: u64, ms: u64) -> CounterSample {
        CounterSample::new(v, Timestamp::from_millis(ms))
    }

    #[test]
    fn counter_reset_does_not_create_throughput_spike() {
        assert_eq!(
            derive_rate(s(1_000_000, 10_000), s(100, 11_000), CounterWidth::Bits64),
            Err(RateError::Reset)
        );
    }

    #[test]
    fn rate_uses_actual_elapsed_time() {
        assert_eq!(
            derive_rate(s(0, 1_000), s(500, 1_500), CounterWidth::Bits64),
            Ok(1000.0)
        );
    }

    #[test]
    fn zero_or_negative_elapsed_is_rejected() {
        assert_eq!(
            derive_rate(s(0, 5), s(9, 5), CounterWidth::Bits64),
            Err(RateError::NonPositiveElapsed)
        );
        assert_eq!(
            derive_rate(s(0, 6), s(9, 5), CounterWidth::Bits64),
            Err(RateError::NonPositiveElapsed)
        );
    }

    #[test]
    fn thirty_two_bit_wrap_is_explained() {
        let prev = u64::from(u32::MAX) - 9;
        assert_eq!(counter_delta(prev, 5, CounterWidth::Bits32), Ok(15));
        // A drop from a low value is a reset, not a wrap.
        assert_eq!(
            counter_delta(1000, 5, CounterWidth::Bits32),
            Err(RateError::Reset)
        );
    }

    #[test]
    fn tracker_first_sample_is_pending_and_sweep_forgets_missing() {
        let mut t = RateTracker::new();
        t.begin();
        assert_eq!(
            t.observe("a", 10, Timestamp::from_millis(1), CounterWidth::Bits64),
            Err(RateError::NoBaseline)
        );
        t.observe("b", 10, Timestamp::from_millis(1), CounterWidth::Bits64)
            .ok();
        t.begin();
        assert_eq!(
            t.observe("a", 20, Timestamp::from_millis(1001), CounterWidth::Bits64),
            Ok(10.0)
        );
        t.sweep();
        assert_eq!(t.len(), 1);
        // "b" reappears: no stale baseline, so no rate.
        t.begin();
        assert_eq!(
            t.observe("b", 99, Timestamp::from_millis(2001), CounterWidth::Bits64),
            Err(RateError::NoBaseline)
        );
    }

    proptest! {
        #[test]
        fn rates_are_never_negative_or_non_finite(a in any::<u64>(), b in any::<u64>(), t0 in 0u64..1<<40, dt in 0u64..1<<40, w32 in any::<bool>()) {
            let width = if w32 { CounterWidth::Bits32 } else { CounterWidth::Bits64 };
            if let Ok(r) = derive_rate(CounterSample::new(a, Timestamp(t0)), CounterSample::new(b, Timestamp(t0 + dt)), width) {
                prop_assert!(r.is_finite() && r >= 0.0);
            }
        }

        #[test]
        fn decreasing_64bit_counter_never_yields_rate(a in 1u64.., d in 1u64..) {
            let b = a.saturating_sub(d);
            prop_assume!(b < a);
            prop_assert_eq!(counter_delta(a, b, CounterWidth::Bits64), Err(RateError::Reset));
        }
    }
}
