// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! CPU utilisation and topology.
//!
//! Topology is read from sysfs and never assumed: CPU numbering may be
//! sparse, CPUs may go offline, cores may have one or several threads and
//! hybrid parts expose distinct performance/efficiency core types.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker, counter_delta};
use rtop_core::{
    CollectorId, EdgeKind, EdgeUpdate, EntityId, EntityKind, Evidence, Metric, NodeUpdate, Scope,
    Unavailable,
};
use rtop_linux::fs::KernelFsExt;
use rtop_linux::parse::system::{self, CpuTimes};

use crate::{Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("cpu");

#[derive(Debug, Clone, Default)]
struct Topology {
    package: Option<u32>,
    core: Option<u32>,
    core_type: Option<&'static str>,
}

#[derive(Debug)]
pub struct CpuCollector {
    env: Env,
    prev: HashMap<Option<u32>, CpuTimes>,
    rates: RateTracker<&'static str>,
    topology: BTreeMap<u32, Topology>,
    topology_age: u32,
}

impl CpuCollector {
    pub fn new(env: Env) -> Self {
        CpuCollector {
            env,
            prev: HashMap::new(),
            rates: RateTracker::new(),
            topology: BTreeMap::new(),
            topology_age: u32::MAX,
        }
    }

    fn refresh_topology(&mut self, cpus: &[u32]) {
        let fs = &self.env.fs;
        let hybrid: Vec<(&'static str, Vec<u32>)> = [
            ("performance", "/sys/devices/cpu_core/cpus"),
            ("efficiency", "/sys/devices/cpu_atom/cpus"),
        ]
        .into_iter()
        .filter_map(|(t, p)| {
            fs.read_attr(p)
                .ok()
                .and_then(|s| system::cpu_list(&s))
                .map(|l| (t, l))
        })
        .collect();
        self.topology.clear();
        for &cpu in cpus {
            let base = format!("/sys/devices/system/cpu/cpu{cpu}/topology");
            let read = |f: &str| {
                fs.read_u64(&format!("{base}/{f}"))
                    .ok()
                    .and_then(|v| u32::try_from(v).ok())
            };
            let core_type = hybrid
                .iter()
                .find(|(_, l)| l.contains(&cpu))
                .map(|(t, _)| *t);
            self.topology.insert(
                cpu,
                Topology {
                    package: read("physical_package_id"),
                    core: read("core_id"),
                    core_type,
                },
            );
        }
        self.topology_age = 0;
    }

    /// Utilisation percentages between two readings; `None` without baseline
    /// or when counters went backwards (CPU hotplug, suspend glitches).
    fn usage(prev: Option<&CpuTimes>, cur: &CpuTimes) -> Option<[f64; 5]> {
        let p = prev?;
        let total = counter_delta(p.total(), cur.total(), CounterWidth::Bits64).ok()?;
        if total == 0 {
            return None;
        }
        let pct = |a: u64, b: u64| {
            counter_delta(a, b, CounterWidth::Bits64)
                .ok()
                .map(|d| 100.0 * d as f64 / total as f64)
        };
        Some([
            pct(p.busy(), cur.busy())?,
            pct(p.user + p.nice, cur.user + cur.nice)?,
            pct(
                p.system + p.irq + p.softirq,
                cur.system + cur.irq + cur.softirq,
            )?,
            pct(p.iowait, cur.iowait)?,
            pct(p.steal, cur.steal)?,
        ])
    }
}

impl Collector for CpuCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_millis(1000),
            timeout: Duration::from_secs(2),
            isolation: Isolation::Fast,
            essential: true,
            nice: 0,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let text = self
            .env
            .fs
            .read_string("/proc/stat", SMALL)
            .map_err(|e| CollectorError::from_kernel("/proc/stat", &e))?;
        let stat = system::proc_stat(&text);
        let total = stat.total.ok_or_else(|| {
            CollectorError::new(Unavailable::InvalidData, "/proc/stat has no cpu line")
        })?;
        let cpus: Vec<u32> = stat.cpus.iter().map(|c| c.0).collect();
        // Rediscover topology periodically and whenever the online set changes.
        if self.topology_age > 30 || self.topology.keys().ne(cpus.iter()) {
            self.refresh_topology(&cpus);
        }
        self.topology_age += 1;

        let mut patch = ctx
            .patch(ID)
            .complete(Scope::Nodes(EntityKind::LogicalCpu))
            .complete(Scope::Nodes(EntityKind::CpuCore))
            .complete(Scope::Nodes(EntityKind::CpuPackage))
            .complete(Scope::Edges(EdgeKind::Contains));
        let host = EntityId::host();
        let mut h = NodeUpdate::new(EntityKind::Host, host.clone())
            .metric("logical_cpus", Metric::u64(cpus.len() as u64));
        let pending = Metric::unavailable(Unavailable::Pending);
        let u = Self::usage(self.prev.get(&None), &total);
        for (i, key) in [
            "cpu_usage_percent",
            "cpu_user_percent",
            "cpu_system_percent",
            "cpu_iowait_percent",
            "cpu_steal_percent",
        ]
        .into_iter()
        .enumerate()
        {
            h.push_metric(key, u.map_or(pending, |u| Metric::f64(u[i])));
        }
        self.prev.insert(None, total);
        self.rates.begin();
        let now = ctx.now;
        for (key, v) in [
            ("context_switches_per_second", stat.context_switches),
            ("interrupts_per_second", stat.interrupts),
            ("forks_per_second", stat.forks),
        ] {
            h.push_metric(
                key,
                v.map_or(Metric::unavailable(Unavailable::Unsupported), |v| {
                    self.rates.metric(key, v, now, CounterWidth::Bits64)
                }),
            );
        }
        self.rates.sweep();
        if let Some(b) = stat.procs_blocked {
            h.push_metric("procs_blocked", Metric::u64(b));
        }
        if let Ok(model) = self.env.fs.read_string("/proc/cpuinfo", 4 << 20)
            && let Some(m) = model.lines().find_map(|l| {
                l.strip_prefix("model name")
                    .and_then(|r| r.split_once(':'))
                    .map(|(_, v)| v.trim().to_owned())
            })
        {
            h = h.text("cpu_model", &m);
        }
        patch.node(h);

        let mut packages = BTreeMap::new();
        for (cpu, times) in &stat.cpus {
            let id = EntityId::new(EntityKind::LogicalCpu, cpu);
            let mut n = NodeUpdate::new(EntityKind::LogicalCpu, id.clone()).attr("index", *cpu);
            n.push_metric(
                "cpu_usage_percent",
                Self::usage(self.prev.get(&Some(*cpu)), times)
                    .map_or(pending, |u| Metric::f64(u[0])),
            );
            let freq = self.env.fs.read_u64(&format!(
                "/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_cur_freq"
            ));
            n.push_metric(
                "frequency_mhz",
                freq.map_or(Metric::unavailable(Unavailable::Unsupported), |khz| {
                    Metric::f64(khz as f64 / 1000.0)
                }),
            );
            let topo = self.topology.get(cpu).cloned().unwrap_or_default();
            if let Some(t) = topo.core_type {
                n = n.attr("core_type", t);
            }
            patch.node(n);
            if let (Some(pkg), Some(core)) = (topo.package, topo.core) {
                let pkg_id = EntityId::new(EntityKind::CpuPackage, pkg);
                let core_id = EntityId::new(EntityKind::CpuCore, format_args!("{pkg}.{core}"));
                packages.entry(pkg).or_insert_with(|| pkg_id.clone());
                let mut c = NodeUpdate::new(EntityKind::CpuCore, core_id.clone())
                    .attr("package", pkg)
                    .attr("core", core);
                if let Some(t) = topo.core_type {
                    c = c.attr("core_type", t);
                }
                patch.node(c);
                patch.edge(EdgeUpdate::new(
                    EdgeKind::Contains,
                    pkg_id,
                    core_id.clone(),
                    Evidence::KernelConfirmed,
                ));
                patch.edge(EdgeUpdate::new(
                    EdgeKind::Contains,
                    core_id,
                    id,
                    Evidence::KernelConfirmed,
                ));
            }
            self.prev.insert(Some(*cpu), *times);
        }
        self.prev.retain(|k, _| k.is_none_or(|c| cpus.contains(&c)));
        for (pkg, id) in packages {
            patch.node(NodeUpdate::new(EntityKind::CpuPackage, id.clone()).attr("index", pkg));
            patch.edge(EdgeUpdate::new(
                EdgeKind::Contains,
                host.clone(),
                id,
                Evidence::KernelConfirmed,
            ));
        }
        Ok(Sample {
            patch,
            degraded: None,
        })
    }
}
