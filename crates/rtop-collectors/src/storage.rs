// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Block devices from `/proc/diskstats` (all counters in 512-byte sectors).
//!
//! Latency and utilisation are block-layer accounting, not
//! application-observed latency; on devices that serve requests in parallel
//! (NVMe) 100% utilisation does not mean saturation.

use std::collections::HashMap;
use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker, counter_delta};
use rtop_core::{
    CollectorId, EdgeKind, EdgeUpdate, EntityId, EntityKind, Evidence, Metric, NodeUpdate, Scope,
    Timestamp, Unavailable,
};
use rtop_linux::fs::KernelFsExt;
use rtop_linux::parse::disk::{self, DiskStat};

use crate::{Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("storage");

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
enum C {
    ReadSectors,
    WriteSectors,
    Reads,
    Writes,
    Discards,
    IoMs,
    WeightedMs,
}

#[derive(Debug)]
pub struct StorageCollector {
    env: Env,
    rates: RateTracker<(String, C)>,
    prev: HashMap<String, (DiskStat, Timestamp)>,
    devices: HashMap<String, (DeviceInfo, u32)>,
}

impl StorageCollector {
    pub fn new(env: Env) -> Self {
        StorageCollector {
            env,
            rates: RateTracker::new(),
            prev: HashMap::new(),
            devices: HashMap::new(),
        }
    }
}

const DEVICE_REFRESH: u32 = 30;

#[derive(Debug, Clone, Default)]
struct DeviceInfo {
    size_bytes: Option<u64>,
    rotational: Option<bool>,
    model: Option<String>,
    partition: bool,
    /// Parent disk of a partition, when sysfs names it.
    parent: Option<String>,
}

fn device_info(fs: &dyn rtop_linux::KernelFs, name: &str) -> DeviceInfo {
    let sys = format!("/sys/class/block/{name}");
    let mut info = DeviceInfo {
        size_bytes: fs
            .read_u64(&format!("{sys}/size"))
            .ok()
            .map(|s| s.saturating_mul(512)),
        ..Default::default()
    };
    if fs.read_attr(&format!("{sys}/partition")).is_ok() {
        info.partition = true;
        // .../block/<parent>/<partition>
        info.parent = fs.read_link(&sys).ok().and_then(|l| {
            let mut parts = l.rsplit('/');
            parts.next();
            parts.next().map(str::to_owned)
        });
    } else {
        info.rotational = fs
            .read_u64(&format!("/sys/block/{name}/queue/rotational"))
            .ok()
            .map(|r| r == 1);
        info.model = ["device/model", "device/name"].iter().find_map(|f| {
            fs.read_attr(&format!("/sys/block/{name}/{f}"))
                .ok()
                .map(|m| m.trim().to_owned())
        });
    }
    info
}

pub fn block_device_id(name: &str) -> EntityId {
    EntityId::new(EntityKind::BlockDevice, name)
}

impl Collector for StorageCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(2),
            isolation: Isolation::Fast,
            essential: false,
            nice: 0,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let fs = &self.env.fs;
        let text = fs
            .read_string("/proc/diskstats", SMALL)
            .map_err(|e| CollectorError::from_kernel("/proc/diskstats", &e))?;
        let now = ctx.now;
        let mut patch = ctx
            .patch(ID)
            .complete(Scope::Nodes(EntityKind::BlockDevice))
            .complete(Scope::Edges(EdgeKind::Contains));
        let (mut total_r, mut total_w, mut have_total) = (0.0, 0.0, false);
        self.rates.begin();
        let mut seen = Vec::new();
        for d in disk::diskstats(&text) {
            // ram disks and unused loop devices are noise.
            if d.name.starts_with("ram")
                || (d.name.starts_with("loop") && d.reads == 0 && d.writes == 0)
            {
                continue;
            }
            // Static device attributes are cached and re-read only for new
            // devices and every DEVICE_REFRESH passes.
            let info = match self.devices.get(&d.name) {
                Some((info, age)) if *age < DEVICE_REFRESH => info.clone(),
                _ => device_info(fs.as_ref(), &d.name),
            };
            let age = self
                .devices
                .get(&d.name)
                .map_or(0, |(_, a)| if *a < DEVICE_REFRESH { a + 1 } else { 0 });
            self.devices.insert(d.name.clone(), (info.clone(), age));
            let id = block_device_id(&d.name);
            let mut n = NodeUpdate::new(EntityKind::BlockDevice, id.clone())
                .text("name", &d.name)
                .attr("major", d.major)
                .attr("minor", d.minor)
                .attr("partition", info.partition);
            if let Some(b) = info.size_bytes {
                n.push_metric("size_bytes", Metric::u64(b));
            }
            if let Some(r) = info.rotational {
                n = n.attr("rotational", r);
            }
            if let Some(m) = &info.model {
                n = n.text("model", m);
            }
            if let Some(parent) = &info.parent {
                patch.edge(EdgeUpdate::new(
                    EdgeKind::Contains,
                    block_device_id(parent),
                    id.clone(),
                    Evidence::KernelConfirmed,
                ));
            }
            let is_partition = info.partition;
            let key = d.name.clone();
            let mut rate = |c: C, v: u64, scale: f64| {
                self.rates
                    .scaled((key.clone(), c), v, now, CounterWidth::Bits64, scale)
            };
            let rb = rate(C::ReadSectors, d.sectors_read, 512.0);
            let wb = rate(C::WriteSectors, d.sectors_written, 512.0);
            let ri = rate(C::Reads, d.reads, 1.0);
            let wi = rate(C::Writes, d.writes, 1.0);
            let util = rate(C::IoMs, d.io_ms, 0.1); // ms per s -> percent
            let depth = rate(C::WeightedMs, d.weighted_io_ms, 0.001);
            let disc = d
                .discards
                .map_or(Metric::unavailable(Unavailable::Unsupported), |v| {
                    rate(C::Discards, v, 1.0)
                });
            if !is_partition
                && !d.name.starts_with("loop")
                && !d.name.starts_with("dm-")
                && !d.name.starts_with("md")
                && let (Some(r), Some(w)) = (rb.as_f64(), wb.as_f64())
            {
                total_r += r;
                total_w += w;
                have_total = true;
            }
            n.push_metric("read_bytes_per_second", rb);
            n.push_metric("write_bytes_per_second", wb);
            n.push_metric("read_iops", ri);
            n.push_metric("write_iops", wi);
            n.push_metric("discard_iops", disc);
            n.push_metric(
                "utilization_percent",
                Metric::from_result(
                    util.as_f64()
                        .map(|u| u.min(100.0))
                        .ok_or(Unavailable::Pending),
                ),
            );
            n.push_metric("queue_depth", depth);
            n.push_metric("in_flight", Metric::u64(d.in_flight));
            // Average completion time: delta time / delta completed requests.
            let (rl, wl) = match self.prev.get(&d.name) {
                Some((p, _)) => {
                    let lat = |ms0, ms1, n0, n1| match (
                        counter_delta(ms0, ms1, CounterWidth::Bits64),
                        counter_delta(n0, n1, CounterWidth::Bits64),
                    ) {
                        (Ok(_), Ok(0)) => Metric::f64(0.0),
                        (Ok(ms), Ok(n)) => Metric::f64(ms as f64 / n as f64),
                        _ => Metric::unavailable(Unavailable::CounterReset),
                    };
                    (
                        lat(p.read_ms, d.read_ms, p.reads, d.reads),
                        lat(p.write_ms, d.write_ms, p.writes, d.writes),
                    )
                }
                None => (
                    Metric::unavailable(Unavailable::Pending),
                    Metric::unavailable(Unavailable::Pending),
                ),
            };
            n.push_metric("read_latency_ms", rl);
            n.push_metric("write_latency_ms", wl);
            seen.push(d.name.clone());
            self.prev.insert(d.name.clone(), (d, now));
            patch.node(n);
        }
        self.rates.sweep();
        self.prev.retain(|k, _| seen.contains(k));
        self.devices.retain(|k, _| seen.contains(k));
        let host_metric = |v: f64| {
            if have_total {
                Metric::f64(v)
            } else {
                Metric::unavailable(Unavailable::Pending)
            }
        };
        patch.node(
            NodeUpdate::new(EntityKind::Host, EntityId::host())
                .metric("disk_read_bytes_per_second", host_metric(total_r))
                .metric("disk_write_bytes_per_second", host_metric(total_w)),
        );
        Ok(Sample {
            patch,
            degraded: None,
        })
    }
}
