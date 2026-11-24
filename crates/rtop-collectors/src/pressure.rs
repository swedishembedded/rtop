// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Pressure Stall Information: whether tasks are actually *waiting* for CPU,
//! memory or I/O, as opposed to merely using them.

use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker};
use rtop_core::{CollectorId, EntityId, EntityKind, Metric, NodeUpdate, Unavailable};
use rtop_linux::fs::KernelFsExt;
use rtop_linux::parse::system::{self, PressureLine};

use crate::{Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("pressure");

#[derive(Debug)]
pub struct PressureCollector {
    env: Env,
    rates: RateTracker<&'static str>,
}

impl PressureCollector {
    pub fn new(env: Env) -> Self {
        PressureCollector {
            env,
            rates: RateTracker::new(),
        }
    }
}

const KEYS: [[&str; 4]; 6] = [
    [
        "psi_cpu_some_avg10",
        "psi_cpu_some_avg60",
        "psi_cpu_some_avg300",
        "psi_cpu_some_stall_percent",
    ],
    [
        "psi_cpu_full_avg10",
        "psi_cpu_full_avg60",
        "psi_cpu_full_avg300",
        "psi_cpu_full_stall_percent",
    ],
    [
        "psi_memory_some_avg10",
        "psi_memory_some_avg60",
        "psi_memory_some_avg300",
        "psi_memory_some_stall_percent",
    ],
    [
        "psi_memory_full_avg10",
        "psi_memory_full_avg60",
        "psi_memory_full_avg300",
        "psi_memory_full_stall_percent",
    ],
    [
        "psi_io_some_avg10",
        "psi_io_some_avg60",
        "psi_io_some_avg300",
        "psi_io_some_stall_percent",
    ],
    [
        "psi_io_full_avg10",
        "psi_io_full_avg60",
        "psi_io_full_avg300",
        "psi_io_full_stall_percent",
    ],
];

impl Collector for PressureCollector {
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
        let mut n = NodeUpdate::new(EntityKind::Host, EntityId::host());
        let mut any = false;
        let mut first_err = None;
        self.rates.begin();
        for (r, resource) in ["cpu", "memory", "io"].iter().enumerate() {
            match self
                .env
                .fs
                .read_string(&format!("/proc/pressure/{resource}"), SMALL)
            {
                Ok(t) => {
                    any = true;
                    let p = system::pressure(&t);
                    for (half, line) in [(0, p.some), (1, p.full)] {
                        let keys = KEYS[r * 2 + half];
                        self.push(&mut n, keys, line, ctx);
                    }
                }
                Err(e) => {
                    let m = Metric::unavailable(crate::unavailable_from(&e));
                    first_err.get_or_insert(e);
                    for keys in [KEYS[r * 2], KEYS[r * 2 + 1]] {
                        for k in keys {
                            n.push_metric(k, m);
                        }
                    }
                }
            }
        }
        self.rates.sweep();
        if !any {
            let e = first_err.expect("at least one error");
            return Err(CollectorError::from_kernel(
                "/proc/pressure (kernel without CONFIG_PSI or psi=0)",
                &e,
            ));
        }
        let mut patch = ctx.patch(ID);
        patch.node(n);
        Ok(Sample {
            patch,
            degraded: None,
        })
    }
}

impl PressureCollector {
    fn push(
        &mut self,
        n: &mut NodeUpdate,
        keys: [&'static str; 4],
        line: Option<PressureLine>,
        ctx: &SampleContext,
    ) {
        let Some(l) = line else {
            for k in keys {
                n.push_metric(k, Metric::unavailable(Unavailable::Unsupported));
            }
            return;
        };
        n.push_metric(keys[0], Metric::f64(l.avg10));
        n.push_metric(keys[1], Metric::f64(l.avg60));
        n.push_metric(keys[2], Metric::f64(l.avg300));
        // Stall time per wall time over the last interval: reacts faster
        // than avg10 and is exact between our two samples.
        let r = self
            .rates
            .observe(keys[3], l.total_us, ctx.now, CounterWidth::Bits64);
        n.push_metric(
            keys[3],
            Metric::from_result(
                r.map(|us_per_s| (us_per_s / 10_000.0).min(100.0))
                    .map_err(Unavailable::from),
            ),
        );
    }
}
