// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Bounded, tiered time series for recent graphs.
//!
//! Each series keeps a high-resolution ring of recent samples and a coarser
//! ring of buckets that preserve minimum, maximum, mean and gap counts, so
//! brief spikes stay visible after downsampling. Gaps (unavailable samples)
//! are stored explicitly rather than as zero.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

use crate::time::Timestamp;

/// Aggregate of several consecutive samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    /// Number of valid samples.
    pub count: u32,
    /// Number of unavailable samples.
    pub gaps: u32,
}

impl Bucket {
    fn from_samples(samples: impl Iterator<Item = Option<f64>>) -> Option<Bucket> {
        let (mut min, mut max, mut sum, mut count, mut gaps) =
            (f64::INFINITY, f64::NEG_INFINITY, 0.0, 0u32, 0u32);
        for s in samples {
            match s {
                Some(v) => {
                    min = min.min(v);
                    max = max.max(v);
                    sum += v;
                    count += 1;
                }
                None => gaps += 1,
            }
        }
        if count == 0 && gaps == 0 {
            return None;
        }
        if count == 0 {
            return Some(Bucket {
                min: f64::NAN,
                max: f64::NAN,
                mean: f64::NAN,
                count,
                gaps,
            });
        }
        // Clamp guards against float rounding putting the mean outside [min, max].
        let mean = (sum / f64::from(count)).clamp(min, max);
        Some(Bucket {
            min,
            max,
            mean,
            count,
            gaps,
        })
    }
}

/// Recent samples plus downsampled older buckets, both bounded.
#[derive(Debug, Clone)]
pub struct TieredSeries {
    live: VecDeque<Option<f64>>,
    live_capacity: usize,
    pending: Vec<Option<f64>>,
    factor: usize,
    coarse: VecDeque<Bucket>,
    coarse_capacity: usize,
    pub last_update: Timestamp,
}

impl TieredSeries {
    /// `live_capacity` samples at full resolution; every `factor` samples
    /// are folded into one of at most `coarse_capacity` buckets.
    pub fn new(live_capacity: usize, factor: usize, coarse_capacity: usize) -> Self {
        let live_capacity = live_capacity.max(1);
        TieredSeries {
            live: VecDeque::with_capacity(live_capacity),
            live_capacity,
            pending: Vec::with_capacity(factor.max(1)),
            factor: factor.max(1),
            coarse: VecDeque::new(),
            coarse_capacity,
            last_update: Timestamp::ZERO,
        }
    }

    pub fn push(&mut self, sample: Option<f64>, at: Timestamp) {
        let sample = sample.filter(|v| v.is_finite());
        if self.live.len() == self.live_capacity {
            self.live.pop_front();
        }
        self.live.push_back(sample);
        self.pending.push(sample);
        if self.pending.len() >= self.factor
            && let Some(b) = Bucket::from_samples(self.pending.drain(..))
            && self.coarse_capacity > 0
        {
            if self.coarse.len() == self.coarse_capacity {
                self.coarse.pop_front();
            }
            self.coarse.push_back(b);
        }
        self.last_update = at;
    }

    /// Full-resolution samples, oldest first.
    pub fn live(&self) -> impl ExactSizeIterator<Item = Option<f64>> + DoubleEndedIterator + '_ {
        self.live.iter().copied()
    }

    pub fn coarse(&self) -> impl ExactSizeIterator<Item = &Bucket> + '_ {
        self.coarse.iter()
    }

    pub fn latest(&self) -> Option<f64> {
        self.live.back().copied().flatten()
    }

    pub fn max_live(&self) -> Option<f64> {
        self.live
            .iter()
            .flatten()
            .copied()
            .fold(None, |a, v| Some(a.map_or(v, |a: f64| a.max(v))))
    }
}

/// History for many `(entity, metric)` series with a global series budget.
///
/// When the budget is exhausted, the least recently updated series is
/// evicted, so per-process history for exited processes cannot accumulate.
#[derive(Debug, Clone)]
pub struct HistoryStore<K> {
    series: HashMap<K, TieredSeries>,
    max_series: usize,
    live_capacity: usize,
    factor: usize,
    coarse_capacity: usize,
}

impl<K: Hash + Eq + Clone> HistoryStore<K> {
    pub fn new(
        max_series: usize,
        live_capacity: usize,
        factor: usize,
        coarse_capacity: usize,
    ) -> Self {
        HistoryStore {
            series: HashMap::new(),
            max_series: max_series.max(1),
            live_capacity,
            factor,
            coarse_capacity,
        }
    }

    pub fn push(&mut self, key: K, sample: Option<f64>, at: Timestamp) {
        if !self.series.contains_key(&key)
            && self.series.len() >= self.max_series
            && let Some(oldest) = self
                .series
                .iter()
                .min_by_key(|(_, s)| s.last_update)
                .map(|(k, _)| k.clone())
        {
            self.series.remove(&oldest);
        }
        self.series
            .entry(key)
            .or_insert_with(|| {
                TieredSeries::new(self.live_capacity, self.factor, self.coarse_capacity)
            })
            .push(sample, at);
    }

    pub fn get(&self, key: &K) -> Option<&TieredSeries> {
        self.series.get(key)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        self.series.retain(|k, _| keep(k));
    }

    pub fn len(&self) -> usize {
        self.series.len()
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn gaps_are_kept_and_not_zero() {
        let mut s = TieredSeries::new(4, 2, 4);
        s.push(Some(10.0), Timestamp(1));
        s.push(None, Timestamp(2));
        assert_eq!(s.live().collect::<Vec<_>>(), vec![Some(10.0), None]);
        let b = s.coarse().next().unwrap();
        assert_eq!((b.count, b.gaps, b.max), (1, 1, 10.0));
    }

    #[test]
    fn store_evicts_least_recent_series_at_budget() {
        let mut h = HistoryStore::new(2, 8, 4, 4);
        h.push("a", Some(1.0), Timestamp(1));
        h.push("b", Some(1.0), Timestamp(2));
        h.push("a", Some(1.0), Timestamp(3));
        h.push("c", Some(1.0), Timestamp(4));
        assert_eq!(h.len(), 2);
        assert!(h.get(&"b").is_none());
    }

    proptest! {
        #[test]
        fn capacity_never_exceeded_and_buckets_ordered(samples in proptest::collection::vec(proptest::option::of(-1e12f64..1e12), 0..300), cap in 1usize..20, factor in 1usize..10, ccap in 0usize..10) {
            let mut s = TieredSeries::new(cap, factor, ccap);
            for (i, v) in samples.iter().enumerate() {
                s.push(*v, Timestamp(i as u64));
                prop_assert!(s.live().len() <= cap);
                prop_assert!(s.coarse().len() <= ccap);
            }
            for b in s.coarse() {
                if b.count > 0 {
                    prop_assert!(b.min <= b.mean && b.mean <= b.max);
                }
            }
        }
    }
}
