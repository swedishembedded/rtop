// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! System memory, swap and paging activity.
//!
//! Swap *occupancy* says little about current pressure; the paging rates
//! published here (swap-in/out, major faults, direct reclaim) together with
//! PSI are what show whether the system is actively short of memory.

use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker};
use rtop_core::{CollectorId, EntityId, EntityKind, Metric, NodeUpdate, Unavailable};
use rtop_linux::fs::KernelFsExt;
use rtop_linux::parse::{find_key, key_values};

use crate::{Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("memory");

#[derive(Debug)]
pub struct MemoryCollector {
    env: Env,
    rates: RateTracker<&'static str>,
}

impl MemoryCollector {
    pub fn new(env: Env) -> Self {
        MemoryCollector {
            env,
            rates: RateTracker::new(),
        }
    }
}

impl Collector for MemoryCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(2),
            isolation: Isolation::Fast,
            essential: true,
            nice: 0,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let mi = self
            .env
            .fs
            .read_string("/proc/meminfo", SMALL)
            .map_err(|e| CollectorError::from_kernel("/proc/meminfo", &e))?;
        let get = |k: &str| find_key(&mi, k);
        let total = get("MemTotal")
            .ok_or_else(|| CollectorError::new(Unavailable::InvalidData, "MemTotal missing"))?;
        let opt =
            |v: Option<u64>| v.map_or(Metric::unavailable(Unavailable::Unsupported), Metric::u64);
        let mut n = NodeUpdate::new(EntityKind::Host, EntityId::host())
            .metric("memory_total_bytes", Metric::u64(total));
        // MemAvailable exists since 3.14; without it "used" cannot be derived honestly.
        let avail = get("MemAvailable");
        n.push_metric("memory_available_bytes", opt(avail));
        n.push_metric(
            "memory_used_bytes",
            opt(avail.map(|a| total.saturating_sub(a))),
        );
        n.push_metric(
            "memory_used_percent",
            avail.map_or(Metric::unavailable(Unavailable::Unsupported), |a| {
                Metric::f64(100.0 * total.saturating_sub(a) as f64 / total.max(1) as f64)
            }),
        );
        for (key, field) in [
            ("memory_free_bytes", "MemFree"),
            ("memory_buffers_bytes", "Buffers"),
            ("memory_cached_bytes", "Cached"),
            ("memory_shared_bytes", "Shmem"),
            ("memory_dirty_bytes", "Dirty"),
            ("memory_slab_reclaimable_bytes", "SReclaimable"),
            ("memory_slab_unreclaimable_bytes", "SUnreclaim"),
            ("memory_anon_bytes", "AnonPages"),
            ("swap_total_bytes", "SwapTotal"),
            ("swap_free_bytes", "SwapFree"),
            ("zswap_bytes", "Zswap"),
        ] {
            n.push_metric(key, opt(get(field)));
        }
        let swap_used = get("SwapTotal")
            .zip(get("SwapFree"))
            .map(|(t, f)| t.saturating_sub(f));
        n.push_metric("swap_used_bytes", opt(swap_used));

        match self.env.fs.read_string("/proc/vmstat", SMALL) {
            Ok(vm) => {
                let now = ctx.now;
                self.rates.begin();
                let mut scan_direct = 0u64;
                let mut have_scan = false;
                for (k, v) in key_values(&vm) {
                    let key = match k {
                        "pswpin" => "swap_in_pages_per_second",
                        "pswpout" => "swap_out_pages_per_second",
                        "pgmajfault" => "major_faults_per_second",
                        "pgfault" => "page_faults_per_second",
                        "oom_kill" => {
                            n.push_metric("oom_kills_total", Metric::u64(v));
                            "oom_kills_per_second"
                        }
                        k if k.starts_with("pgscan_direct") => {
                            scan_direct = scan_direct.saturating_add(v);
                            have_scan = true;
                            continue;
                        }
                        _ => continue,
                    };
                    let m = self.rates.metric(key, v, now, CounterWidth::Bits64);
                    n.push_metric(key, m);
                }
                if have_scan {
                    let m = self.rates.metric(
                        "direct_reclaim_pages_per_second",
                        scan_direct,
                        now,
                        CounterWidth::Bits64,
                    );
                    n.push_metric("direct_reclaim_pages_per_second", m);
                }
                self.rates.sweep();
            }
            Err(e) => {
                let m = Metric::unavailable(crate::unavailable_from(&e));
                for k in [
                    "swap_in_pages_per_second",
                    "swap_out_pages_per_second",
                    "major_faults_per_second",
                ] {
                    n.push_metric(k, m);
                }
            }
        }
        let mut patch = ctx.patch(ID);
        patch.node(n);
        Ok(Sample {
            patch,
            degraded: None,
        })
    }
}
