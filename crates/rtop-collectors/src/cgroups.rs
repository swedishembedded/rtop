// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! cgroup v2 workloads: services, containers and their resource accounting.
//!
//! The unified hierarchy is walked (bounded in depth and count). Groups that
//! belong to a container runtime become `container` entities and systemd
//! `.service` groups become `service` entities, each `accounted_to` its
//! cgroup. Their member processes are linked by the process collector's
//! `member_of` edges, so a container's processes are a graph traversal away.
//!
//! Memory is reported raw (`memory.current`) and as a working-set estimate
//! (current minus inactive file cache, Docker's definition on cgroup v2),
//! which is explicitly marked estimated.

use std::collections::HashSet;
use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker};
use rtop_core::{
    CollectorId, EdgeKind, EdgeUpdate, EntityId, EntityKind, Evidence, Metric, NodeUpdate, Scope,
    Unavailable, Value,
};
use rtop_linux::fs::KernelFsExt;
use rtop_linux::parse::{cgroup, find_key, system};

use crate::{Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("cgroups");
const ROOT: &str = "/sys/fs/cgroup";
const MAX_DEPTH: usize = 8;
const MAX_GROUPS: usize = 8192;

#[derive(Debug)]
pub struct CgroupCollector {
    env: Env,
    rates: RateTracker<(String, &'static str)>,
}

impl CgroupCollector {
    pub fn new(env: Env) -> Self {
        CgroupCollector {
            env,
            rates: RateTracker::new(),
        }
    }
}

pub fn cgroup_id(path: &str) -> EntityId {
    EntityId::new(EntityKind::Cgroup, path)
}

pub fn container_id(id: &str) -> EntityId {
    EntityId::new(EntityKind::Container, &id[..id.len().min(12)])
}

impl Collector for CgroupCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(5),
            isolation: Isolation::Expensive,
            essential: false,
            nice: 5,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let fs = self.env.fs.clone();
        if fs.read_attr(&format!("{ROOT}/cgroup.controllers")).is_err() {
            return Err(CollectorError::new(
                Unavailable::Unsupported,
                "cgroup v2 unified hierarchy not mounted at /sys/fs/cgroup",
            ));
        }
        let mut patch = ctx
            .patch(ID)
            .complete(Scope::Nodes(EntityKind::Cgroup))
            .complete(Scope::Nodes(EntityKind::Container))
            .complete(Scope::Nodes(EntityKind::Service))
            .complete(Scope::Edges(EdgeKind::AccountedTo));
        let now = ctx.now;
        self.rates.begin();
        let mut stack = vec![(String::from("/"), 0usize)];
        let mut visited = 0usize;
        let mut seen_containers = HashSet::new();
        while let Some((path, depth)) = stack.pop() {
            if visited >= MAX_GROUPS {
                break;
            }
            visited += 1;
            let dir = if path == "/" {
                ROOT.to_owned()
            } else {
                format!("{ROOT}{path}")
            };
            let Ok(entries) = fs.read_dir(&dir) else {
                continue;
            };
            if depth < MAX_DEPTH {
                for e in &entries {
                    // Child groups are directories; interface files contain '.'
                    // except for a few names that never are directories.
                    let unit_like = [".slice", ".scope", ".service", ".mount", ".socket"]
                        .iter()
                        .any(|s| e.ends_with(s));
                    if (!e.contains('.') || unit_like) && fs.read_dir(&format!("{dir}/{e}")).is_ok()
                    {
                        let child = if path == "/" {
                            format!("/{e}")
                        } else {
                            format!("{path}/{e}")
                        };
                        stack.push((child, depth + 1));
                    }
                }
            }
            let read = |f: &str| fs.read_string(&format!("{dir}/{f}"), SMALL).ok();
            let id = cgroup_id(&path);
            let name = path
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("/");
            let mut n = NodeUpdate::new(EntityKind::Cgroup, id.clone())
                .text("name", name)
                .text("path", &path)
                .attr("depth", depth as u64);
            let key = path.clone();
            let mut rate = |k: &'static str, v: u64, scale: f64| {
                self.rates
                    .scaled((key.clone(), k), v, now, CounterWidth::Bits64, scale)
            };
            let mut metrics: Vec<(&'static str, Metric)> = Vec::new();
            if let Some(cpu) = read("cpu.stat") {
                if let Some(u) = find_key(&cpu, "usage_usec") {
                    // usec per second / 10_000 = percent of one CPU.
                    metrics.push(("cpu_usage_percent", rate("cpu_usage_percent", u, 1e-4)));
                    metrics.push(("cpu_time_seconds", Metric::f64(u as f64 / 1e6)));
                }
                if let Some(t) = find_key(&cpu, "throttled_usec") {
                    metrics.push((
                        "cpu_throttled_percent",
                        rate("cpu_throttled_percent", t, 1e-4),
                    ));
                }
                if let Some(t) = find_key(&cpu, "nr_throttled") {
                    metrics.push((
                        "cpu_throttle_events_per_second",
                        rate("cpu_throttle_events_per_second", t, 1.0),
                    ));
                }
            }
            if let Some(max) = read("cpu.max").and_then(|s| cgroup::cpu_max(&s)) {
                metrics.push((
                    "cpu_quota_cpus",
                    max.map_or(Metric::unavailable(Unavailable::NotMeasurable), Metric::f64),
                ));
            }
            let current = read("memory.current").and_then(|s| s.trim().parse::<u64>().ok());
            if let Some(cur) = current {
                metrics.push(("memory_current_bytes", Metric::u64(cur)));
                if let Some(stat) = read("memory.stat") {
                    let g = |k| find_key(&stat, k);
                    for (k, f) in [
                        ("memory_anon_bytes", "anon"),
                        ("memory_file_bytes", "file"),
                        ("memory_kernel_bytes", "kernel"),
                        ("memory_shmem_bytes", "shmem"),
                    ] {
                        if let Some(v) = g(f) {
                            metrics.push((k, Metric::u64(v)));
                        }
                    }
                    if let Some(inactive) = g("inactive_file") {
                        metrics.push((
                            "memory_working_set_bytes",
                            Metric::Estimated(Value::U64(cur.saturating_sub(inactive))),
                        ));
                    }
                }
            }
            for (k, f) in [
                ("memory_peak_bytes", "memory.peak"),
                ("swap_current_bytes", "memory.swap.current"),
                ("pids_current", "pids.current"),
            ] {
                if let Some(v) = read(f).and_then(|s| s.trim().parse::<u64>().ok()) {
                    metrics.push((k, Metric::u64(v)));
                }
            }
            if let Some(l) = read("memory.max").and_then(|s| cgroup::limit(&s)) {
                metrics.push((
                    "memory_limit_bytes",
                    match l {
                        cgroup::Limit::Bytes(b) => Metric::u64(b),
                        cgroup::Limit::Unlimited => Metric::unavailable(Unavailable::NotMeasurable),
                    },
                ));
            }
            if let Some(ev) = read("memory.events") {
                if let Some(v) = find_key(&ev, "oom_kill") {
                    metrics.push(("oom_kills_total", Metric::u64(v)));
                }
            }
            if let Some(io) = read("io.stat") {
                let (r, w) = cgroup::io_stat(&io).iter().fold((0u64, 0u64), |(r, w), s| {
                    (r.saturating_add(s.rbytes), w.saturating_add(s.wbytes))
                });
                metrics.push((
                    "io_read_bytes_per_second",
                    rate("io_read_bytes_per_second", r, 1.0),
                ));
                metrics.push((
                    "io_write_bytes_per_second",
                    rate("io_write_bytes_per_second", w, 1.0),
                ));
            }
            for (k, f) in [
                ("psi_cpu_some_avg10", "cpu.pressure"),
                ("psi_memory_some_avg10", "memory.pressure"),
                ("psi_io_some_avg10", "io.pressure"),
            ] {
                if let Some(p) = read(f).map(|s| system::pressure(&s)).and_then(|p| p.some) {
                    metrics.push((k, Metric::f64(p.avg10)));
                }
            }
            if let Some(procs) = read("cgroup.procs") {
                metrics.push((
                    "processes",
                    Metric::u64(procs.lines().filter(|l| !l.is_empty()).count() as u64),
                ));
            }
            for (k, m) in &metrics {
                n.push_metric(k, *m);
            }
            patch.node(n);

            if let Some((runtime, cid)) = cgroup::container_from_path(&path) {
                let c = container_id(&cid);
                if seen_containers.insert(c.clone()) {
                    let mut cn = NodeUpdate::new(EntityKind::Container, c.clone())
                        .attr("runtime", runtime)
                        .attr("container_id", cid.clone())
                        .attr("cgroup", path.clone());
                    for (k, m) in &metrics {
                        cn.push_metric(k, *m);
                    }
                    patch.node(cn);
                    patch.edge(EdgeUpdate::new(
                        EdgeKind::AccountedTo,
                        c,
                        id.clone(),
                        Evidence::KernelConfirmed,
                    ));
                }
            } else if let Some(unit) = name.strip_suffix(".service") {
                let s = EntityId::new(EntityKind::Service, name);
                let mut sn = NodeUpdate::new(EntityKind::Service, s.clone())
                    .text("name", unit)
                    .text("unit", name)
                    .attr("cgroup", path.clone());
                for (k, m) in &metrics {
                    sn.push_metric(k, *m);
                }
                patch.node(sn);
                patch.edge(EdgeUpdate::new(
                    EdgeKind::AccountedTo,
                    s,
                    id,
                    Evidence::KernelConfirmed,
                ));
            }
        }
        self.rates.sweep();
        let degraded = (visited >= MAX_GROUPS)
            .then(|| format!("cgroup walk truncated at {MAX_GROUPS} groups"));
        Ok(Sample { patch, degraded })
    }
}
