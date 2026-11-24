// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! rtop's own resource consumption, so its overhead is always visible.

use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker};
use rtop_core::{CollectorId, EntityId, EntityKind, Metric, NodeUpdate, Unavailable};
use rtop_linux::fs::KernelFsExt;
use rtop_linux::parse::pid;

use crate::{Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("monitor");

#[derive(Debug)]
pub struct MonitorCollector {
    env: Env,
    rates: RateTracker<()>,
}

impl MonitorCollector {
    pub fn new(env: Env) -> Self {
        MonitorCollector {
            env,
            rates: RateTracker::new(),
        }
    }
}

impl Collector for MonitorCollector {
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
        let Some(me) = self.env.self_pid else {
            return Err(CollectorError::new(
                Unavailable::NotMeasurable,
                "not observing the live system",
            ));
        };
        let mut buf = Vec::new();
        self.env
            .fs
            .read(&format!("/proc/{me}/stat"), &mut buf, SMALL)
            .map_err(|e| CollectorError::from_kernel("/proc/self/stat", &e))?;
        let st = pid::stat(&buf)
            .ok_or_else(|| CollectorError::new(Unavailable::InvalidData, "/proc/self/stat"))?;
        let status = self
            .env
            .fs
            .read_string(&format!("/proc/{me}/status"), SMALL)
            .map(|s| pid::status(&s))
            .unwrap_or_default();
        self.rates.begin();
        let cpu = self
            .rates
            .observe((), st.utime + st.stime, ctx.now, CounterWidth::Bits64);
        self.rates.sweep();
        let ticks = self.env.clock_ticks.max(1) as f64;
        let mut patch = ctx.patch(ID);
        patch.node(
            NodeUpdate::new(EntityKind::Host, EntityId::host())
                .metric(
                    "rtop_cpu_percent",
                    Metric::from_result(cpu.map(|t| 100.0 * t / ticks).map_err(Unavailable::from)),
                )
                .metric(
                    "rtop_rss_bytes",
                    status
                        .vm_rss
                        .map_or(Metric::unavailable(Unavailable::Unsupported), Metric::u64),
                )
                .metric("rtop_threads", Metric::u64(st.num_threads))
                .metric("rtop_nice", Metric::i64(st.nice)),
        );
        Ok(Sample {
            patch,
            degraded: None,
        })
    }
}
