// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The host entity: identity, uptime and load.

use std::time::Duration;

use rtop_core::{CollectorId, EntityId, EntityKind, Metric, NodeUpdate, Unavailable};
use rtop_linux::fs::KernelFsExt;
use rtop_linux::parse::system;

use crate::{Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("host");

#[derive(Debug)]
pub struct HostCollector {
    env: Env,
}

impl HostCollector {
    pub fn new(env: Env) -> Self {
        HostCollector { env }
    }
}

impl Collector for HostCollector {
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
        let fs = &self.env.fs;
        let load = fs
            .read_string("/proc/loadavg", SMALL)
            .map_err(|e| CollectorError::from_kernel("/proc/loadavg", &e))?;
        let load = system::loadavg(&load)
            .ok_or_else(|| CollectorError::new(Unavailable::InvalidData, "/proc/loadavg"))?;
        let mut n = NodeUpdate::new(EntityKind::Host, EntityId::host())
            .metric("load_1", Metric::f64(load.one))
            .metric("load_5", Metric::f64(load.five))
            .metric("load_15", Metric::f64(load.fifteen))
            .metric("tasks_runnable", Metric::u64(load.runnable))
            .metric("tasks_total", Metric::u64(load.tasks));
        let up = fs
            .read_string("/proc/uptime", SMALL)
            .ok()
            .and_then(|u| system::uptime(&u));
        n.push_metric(
            "uptime_seconds",
            up.map_or(Metric::unavailable(Unavailable::Unsupported), |u| {
                Metric::f64(u.0)
            }),
        );
        for (key, path) in [
            ("hostname", "/proc/sys/kernel/hostname"),
            ("kernel", "/proc/sys/kernel/osrelease"),
            ("boot_id", "/proc/sys/kernel/random/boot_id"),
        ] {
            if let Ok(v) = fs.read_attr(path) {
                n = n.text(key, v.trim());
            }
        }
        if let Ok(os) = fs.read_string("/etc/os-release", SMALL) {
            if let Some(pretty) = os.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")) {
                n = n.text("os", pretty.trim_matches('"'));
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
