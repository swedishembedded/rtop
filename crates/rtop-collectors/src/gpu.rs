// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! GPUs and accelerators.
//!
//! Three providers feed one `gpu:<pci address>` entity per device:
//!
//! * DRM sysfs for identity and the driver-specific attributes AMD and Intel
//!   expose (busy percentages, VRAM, frequencies, hwmon power/temperature);
//! * DRM fdinfo for per-process engine time and memory (`uses_gpu` edges),
//!   deduplicated by `drm-client-id` because several descriptors may refer to
//!   one client;
//! * NVML (`rtop_linux::nvml`) for NVIDIA identity, limits, utilisation,
//!   memory, thermals, power, clocks, PCIe, error counters and processes.
//!
//! The DRM client scan walks every process's descriptors, so it runs only
//! when the GPU view asks for clients and a DRM device not covered by NVML
//! exists; NVIDIA processes come from the driver instead.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker};
use rtop_core::{
    CollectorId, EdgeKind, EdgeUpdate, EntityId, EntityKind, Evidence, Metric, NodeUpdate,
    ProcessIdentity, Scope, Unavailable,
};
use rtop_linux::fs::{KernelFs, KernelFsExt};
use rtop_linux::nvml::{NvidiaDevice, NvidiaSession, NvidiaSystem, clock_event_reasons};
use rtop_linux::parse::pid;

use crate::{
    Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext,
    Subscriptions,
};

pub const ID: CollectorId = CollectorId("gpu");

/// Samples between attempts to open NVML after it failed (the driver may
/// load, or GPU access may be granted, after rtop started).
pub const NVML_RETRY_SAMPLES: u32 = 30;

pub fn gpu_id(pci: &str) -> EntityId {
    EntityId::new(EntityKind::Gpu, pci)
}

fn vendor_name(v: &str) -> &'static str {
    match v {
        "0x1002" => "AMD",
        "0x8086" => "Intel",
        "0x10de" => "NVIDIA",
        "0x13b5" => "ARM",
        "0x5143" => "Qualcomm",
        _ => "Unknown",
    }
}

#[derive(Debug)]
pub struct GpuCollector {
    env: Env,
    /// Opened on the first sample (NVML initialisation is slow).
    nvml: Option<Result<Box<dyn NvidiaSession>, String>>,
    /// Samples since NVML last failed to open.
    nvml_failed_for: u32,
    rates: RateTracker<(String, String)>,
}

impl GpuCollector {
    pub fn new(env: Env) -> Self {
        GpuCollector {
            env,
            nvml: None,
            nvml_failed_for: 0,
            rates: RateTracker::new(),
        }
    }
}

/// Parsed DRM fdinfo of one client.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DrmClient {
    pub pdev: String,
    pub client_id: u64,
    pub driver: String,
    /// Busy time per engine in nanoseconds.
    pub engines: BTreeMap<String, u64>,
    /// Resident memory per region in bytes.
    pub memory: BTreeMap<String, u64>,
}

/// Parses `/proc/PID/fdinfo/N` for a DRM file (Documentation/gpu/drm-usage-stats.rst).
pub fn drm_fdinfo(text: &str) -> Option<DrmClient> {
    let mut c = DrmClient::default();
    let mut have_id = false;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        if k == "drm-client-id" {
            c.client_id = v.parse().ok()?;
            have_id = true;
        } else if k == "drm-pdev" {
            c.pdev = v.to_owned();
        } else if k == "drm-driver" {
            c.driver = v.to_owned();
        } else if let Some(engine) = k.strip_prefix("drm-engine-") {
            if engine.starts_with("capacity-") {
                continue;
            }
            if let Some(ns) = v.strip_suffix(" ns").and_then(|n| n.trim().parse().ok()) {
                c.engines.insert(engine.to_owned(), ns);
            }
        } else if let Some(region) = k
            .strip_prefix("drm-resident-")
            .or_else(|| k.strip_prefix("drm-memory-"))
        {
            let mut it = v.split_whitespace();
            let n: u64 = it.next().and_then(|n| n.parse().ok())?;
            let mult = match it.next() {
                Some("KiB") => 1024,
                Some("MiB") => 1 << 20,
                Some("GiB") => 1 << 30,
                _ => 1,
            };
            c.memory.insert(region.to_owned(), n.saturating_mul(mult));
        }
    }
    (have_id && !c.pdev.is_empty()).then_some(c)
}

impl Collector for GpuCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(5),
            isolation: Isolation::MayBlock,
            essential: false,
            nice: 5,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let fs = self.env.fs.clone();
        let mut patch = ctx
            .patch(ID)
            .complete(Scope::Nodes(EntityKind::Gpu))
            .complete(Scope::Edges(EdgeKind::UsesGpu));
        let mut degraded = Vec::new();
        let mut found = 0;
        let mut nvidia_pci = Vec::new();

        self.rates.begin();
        let clients = Subscriptions::get(&self.env.subscriptions.gpu_clients);
        if matches!(self.nvml, Some(Err(_))) {
            self.nvml_failed_for += 1;
            if self.nvml_failed_for >= NVML_RETRY_SAMPLES {
                self.nvml = None;
            }
        }
        let nvml = self.nvml.get_or_insert_with(|| self.env.nvidia.open());
        if nvml.is_ok() {
            self.nvml_failed_for = 0;
        }
        // Why NVML could not report every GPU; explains procfs-only GPUs.
        let mut nvml_problem = None;
        if let Ok(session) = nvml {
            match session.sample(clients) {
                Ok(sample) => {
                    if !sample.problems.is_empty() {
                        nvml_problem = Some(sample.problems.join("; "));
                    }
                    for d in &sample.devices {
                        found += 1;
                        nvidia_pci.push(d.info.pci.clone());
                        if let Some(e) = &d.error {
                            degraded.push(format!("nvml {}: {e}", d.info.pci));
                        }
                        let id = gpu_id(&d.info.pci);
                        let mut n = nvidia_node(d, &sample.system);
                        for (key, kib) in [
                            ("nvlink_tx_bytes_per_second", d.nvlink_tx_kib_total),
                            ("nvlink_rx_bytes_per_second", d.nvlink_rx_kib_total),
                        ] {
                            if let Some(kib) = kib {
                                n.push_metric(
                                    key,
                                    self.rates.scaled(
                                        (d.info.pci.clone(), key.into()),
                                        kib,
                                        ctx.now,
                                        CounterWidth::Bits64,
                                        1024.0,
                                    ),
                                );
                            }
                        }
                        patch.node(n);
                        for p in &d.processes {
                            let Some(ident) = identity_of(fs.as_ref(), p.pid) else {
                                continue;
                            };
                            let o = |v: Option<f64>| {
                                v.map_or(Metric::unavailable(Unavailable::Unsupported), Metric::f64)
                            };
                            patch.edge(
                                EdgeUpdate::new(
                                    EdgeKind::UsesGpu,
                                    ident.entity_id(),
                                    id.clone(),
                                    Evidence::KernelConfirmed,
                                )
                                .metric("gpu_usage_percent", o(p.sm_usage_percent))
                                .metric(
                                    "gpu_memory_bytes",
                                    p.used_memory_bytes.map_or(
                                        Metric::unavailable(Unavailable::Unsupported),
                                        Metric::u64,
                                    ),
                                ),
                            );
                        }
                    }
                }
                Err(e) => nvml_problem = Some(e),
            }
        }

        // NVIDIA GPUs NVML did not report (library missing, no access to
        // the device nodes, driver mismatch) stay visible from the driver's
        // procfs, with the reason; nvidia-drm, and so a DRM card, is often
        // not loaded on compute nodes.
        let nvml_error = match &self.nvml {
            Some(Err(e)) => Some(e.clone()),
            _ => nvml_problem.clone(),
        };
        let mut procfs_only = Vec::new();
        let mut denied = Vec::new();
        for pci in fs.read_dir("/proc/driver/nvidia/gpus").unwrap_or_default() {
            if nvidia_pci.contains(&pci) {
                continue;
            }
            let Ok(info) = fs.read_string(
                &format!("/proc/driver/nvidia/gpus/{pci}/information"),
                SMALL,
            ) else {
                continue;
            };
            patch.node(nvidia_procfs_node(&pci, &info));
            if let Some(minor) = procfs_minor(&info)
                && let Err(e) = self.env.nvidia.device_access(minor)
            {
                denied.push(e);
            }
            procfs_only.push(pci.clone());
            nvidia_pci.push(pci);
            found += 1;
        }
        if !procfs_only.is_empty() {
            let total = nvidia_pci.len();
            let reason = if denied.is_empty() {
                nvml_error.unwrap_or_else(|| "NVML did not list every GPU".into())
            } else {
                format!(
                    "this process may open {} of {total} NVIDIA GPUs ({}){}",
                    total - denied.len(),
                    denied.join(", "),
                    crate::cluster::slurm_step(fs.as_ref(), self.env.self_pid).map_or_else(
                        || "; the device cgroup or container does not grant the others".into(),
                        |(job, step)| format!(
                            "; Slurm job {job} step {step} was granted fewer GPUs: \
                             run rtop in a step with --gres=gpu:{total}"
                        ),
                    )
                )
            };
            degraded.push(format!(
                "{reason}; NVIDIA GPU {} known only from /proc/driver/nvidia, without telemetry",
                procfs_only.join(", "),
            ));
        } else if let Some(p) = nvml_problem {
            degraded.push(format!("nvml: {p}"));
        }

        let mut cards = fs.read_dir("/sys/class/drm").unwrap_or_default();
        cards.retain(|c| {
            c.strip_prefix("card")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        });
        cards.sort();
        let mut drm_only = false;
        for card in cards {
            let dev = format!("/sys/class/drm/{card}/device");
            let Ok(link) = fs.read_link(&dev) else {
                continue;
            };
            let pci = link.rsplit('/').next().unwrap_or(&card).to_owned();
            if nvidia_pci.contains(&pci) {
                continue;
            }
            drm_only = true;
            let vendor_id = fs.read_attr(&format!("{dev}/vendor")).unwrap_or_default();
            let vendor = vendor_name(&vendor_id);
            let driver = fs
                .read_link(&format!("{dev}/driver"))
                .ok()
                .and_then(|l| l.rsplit('/').next().map(str::to_owned))
                .unwrap_or_default();
            let device_id = fs.read_attr(&format!("{dev}/device")).unwrap_or_default();
            let name = fs
                .read_attr(&format!("{dev}/product_name"))
                .unwrap_or_else(|_| format!("{vendor} GPU {device_id}"));
            let mut n = NodeUpdate::new(EntityKind::Gpu, gpu_id(&pci))
                .text("name", &name)
                .attr("vendor", vendor)
                .text("driver", &driver)
                .attr("provider", "drm")
                .attr("card", card.clone())
                .attr("pci_address", pci.clone());
            let pct = |f: &str| {
                fs.read_u64(&format!("{dev}/{f}"))
                    .map(|v| Metric::f64(v as f64))
            };
            n.push_metric(
                "gpu_usage_percent",
                pct("gpu_busy_percent").unwrap_or(Metric::unavailable(if vendor == "NVIDIA" {
                    Unavailable::Unsupported
                } else {
                    Unavailable::NotMeasurable
                })),
            );
            if let Ok(m) = pct("mem_busy_percent") {
                n.push_metric("memory_busy_percent", m);
            }
            for (key, f) in [
                ("vram_total_bytes", "mem_info_vram_total"),
                ("vram_used_bytes", "mem_info_vram_used"),
                ("gtt_total_bytes", "mem_info_gtt_total"),
                ("gtt_used_bytes", "mem_info_gtt_used"),
            ] {
                if let Ok(v) = fs.read_u64(&format!("{dev}/{f}")) {
                    n.push_metric(key, Metric::u64(v));
                }
            }
            for (key, f) in [
                ("frequency_mhz", "gt_act_freq_mhz"),
                ("frequency_max_mhz", "gt_max_freq_mhz"),
            ] {
                if let Ok(v) = fs.read_u64(&format!("/sys/class/drm/{card}/{f}")) {
                    n.push_metric(key, Metric::f64(v as f64));
                }
            }
            for hw in fs.read_dir(&format!("{dev}/hwmon")).unwrap_or_default() {
                let base = format!("{dev}/hwmon/{hw}");
                if let Ok(t) = fs.read_i64(&format!("{base}/temp1_input")) {
                    n.push_metric("temperature_celsius", Metric::f64(t as f64 / 1000.0));
                }
                if let Ok(p) = fs
                    .read_u64(&format!("{base}/power1_average"))
                    .or_else(|_| fs.read_u64(&format!("{base}/power1_input")))
                {
                    n.push_metric("power_w", Metric::f64(p as f64 * 1e-6));
                } else if let Ok(e) = fs.read_u64(&format!("{base}/energy1_input")) {
                    n.push_metric(
                        "power_w",
                        self.rates.scaled(
                            (pci.clone(), "energy".into()),
                            e,
                            ctx.now,
                            CounterWidth::Bits64,
                            1e-6,
                        ),
                    );
                }
            }
            if vendor == "NVIDIA"
                && let Some(Err(e)) = &self.nvml
            {
                degraded.push(format!("NVIDIA GPU present but NVML is not available: {e}"));
            }
            patch.node(n);
            found += 1;
        }

        if clients && drm_only {
            self.drm_clients(&mut patch, ctx, &nvidia_pci);
        }
        self.rates.sweep();
        if found == 0 {
            return Err(CollectorError::new(
                Unavailable::Unsupported,
                "no supported GPU or accelerator found",
            ));
        }
        let degraded = (!degraded.is_empty()).then(|| degraded.join("; "));
        Ok(Sample { patch, degraded })
    }
}

fn identity_of(fs: &dyn KernelFs, p: u32) -> Option<ProcessIdentity> {
    let mut buf = Vec::new();
    fs.read(&format!("/proc/{p}/stat"), &mut buf, SMALL).ok()?;
    pid::stat(&buf).map(|s| ProcessIdentity {
        pid: p,
        start_time_ticks: s.starttime,
    })
}

impl GpuCollector {
    fn drm_clients(
        &mut self,
        patch: &mut rtop_core::GraphPatch,
        ctx: &SampleContext,
        skip_pci: &[String],
    ) {
        let fs = self.env.fs.clone();
        let Ok(procs) = fs.read_dir("/proc") else {
            return;
        };
        // (pid identity, pdev) -> (per-engine max busy %, memory bytes)
        let mut usage: HashMap<(ProcessIdentity, String), (Option<f64>, u64)> = HashMap::new();
        let mut seen_clients = std::collections::HashSet::new();
        for name in procs
            .iter()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        {
            let Ok(p) = name.parse::<u32>() else { continue };
            let Ok(fds) = fs.read_dir(&format!("/proc/{name}/fd")) else {
                continue;
            };
            let mut ident = None;
            for fd in fds {
                let Ok(link) = fs.read_link(&format!("/proc/{name}/fd/{fd}")) else {
                    continue;
                };
                if !link.starts_with("/dev/dri/") {
                    continue;
                }
                let Ok(info) = fs.read_string(&format!("/proc/{name}/fdinfo/{fd}"), SMALL) else {
                    continue;
                };
                let Some(c) = drm_fdinfo(&info) else { continue };
                if skip_pci.contains(&c.pdev) || !seen_clients.insert((c.pdev.clone(), c.client_id))
                {
                    continue;
                }
                let Some(id) = ident.or_else(|| identity_of(fs.as_ref(), p)) else {
                    break;
                };
                ident = Some(id);
                let mut busy: Option<f64> = None;
                for (engine, ns) in &c.engines {
                    let key = (format!("{}:{}:{}", p, c.pdev, c.client_id), engine.clone());
                    if let Ok(r) = self.rates.observe(key, *ns, ctx.now, CounterWidth::Bits64) {
                        let pct = (r / 1e7).min(100.0);
                        busy = Some(busy.map_or(pct, |b: f64| b.max(pct)));
                    }
                }
                let e = usage.entry((id, c.pdev.clone())).or_default();
                e.0 = match (e.0, busy) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
                e.1 = e.1.saturating_add(c.memory.values().sum());
            }
        }
        for ((p, pdev), (busy, mem)) in usage {
            patch.edge(
                EdgeUpdate::new(
                    EdgeKind::UsesGpu,
                    p.entity_id(),
                    gpu_id(&pdev),
                    Evidence::KernelConfirmed,
                )
                .metric(
                    "gpu_usage_percent",
                    busy.map_or(Metric::unavailable(Unavailable::Pending), Metric::f64),
                )
                .metric("gpu_memory_bytes", Metric::u64(mem)),
            );
        }
    }
}

/// `Device Minor` of a `/proc/driver/nvidia/gpus/*/information` file.
fn procfs_minor(info: &str) -> Option<u32> {
    info.lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.trim() == "Device Minor")
        })
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// An NVIDIA GPU known only from `/proc/driver/nvidia/gpus/*/information`:
/// identity, but no telemetry.
fn nvidia_procfs_node(pci: &str, info: &str) -> NodeUpdate {
    let mut n = NodeUpdate::new(EntityKind::Gpu, gpu_id(pci))
        .attr("vendor", "NVIDIA")
        .attr("driver", "nvidia")
        .attr("provider", "nvidia-procfs")
        .attr("pci_address", pci.to_owned())
        .metric(
            "gpu_usage_percent",
            Metric::unavailable(Unavailable::Unsupported),
        );
    let mut name = None;
    for line in info.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        match k.trim() {
            "Model" => name = Some(v.to_owned()),
            "GPU UUID" => n = n.text("uuid", v),
            "Video BIOS" => n = n.text("vbios_version", v),
            "Device Minor" => {
                if let Ok(m) = v.parse::<u32>() {
                    n.push_attr("index", m);
                }
            }
            _ => {}
        }
    }
    n.text("name", name.as_deref().unwrap_or("NVIDIA GPU"))
}

/// One NVML device as a graph node. Values a device does not report are
/// omitted, except the headline ones, which are explicitly unsupported.
fn nvidia_node(d: &NvidiaDevice, sys: &NvidiaSystem) -> NodeUpdate {
    let i = &d.info;
    let headline =
        |v: Option<f64>| v.map_or(Metric::unavailable(Unavailable::Unsupported), Metric::f64);
    let mut n = NodeUpdate::new(EntityKind::Gpu, gpu_id(&i.pci))
        .text("name", &i.name)
        .attr("vendor", "NVIDIA")
        .attr("driver", "nvidia")
        .attr("provider", "nvml")
        .attr("pci_address", i.pci.clone())
        .attr("index", i.index)
        .metric("gpu_usage_percent", headline(d.gpu_usage_percent))
        .metric("memory_busy_percent", headline(d.memory_busy_percent))
        .metric(
            "vram_total_bytes",
            d.vram_total_bytes
                .map_or(Metric::unavailable(Unavailable::Unsupported), Metric::u64),
        )
        .metric(
            "vram_used_bytes",
            d.vram_used_bytes
                .map_or(Metric::unavailable(Unavailable::Unsupported), Metric::u64),
        )
        .metric("temperature_celsius", headline(d.temperature_celsius))
        .metric("power_w", headline(d.power_w));
    let texts = [
        ("gsp_firmware_version", &i.gsp_firmware_version),
        ("inforom_version", &i.inforom_version),
        ("pci_device_id", &i.pci_device_id),
        ("pci_subsystem_id", &i.pci_subsystem_id),
        ("uuid", &i.uuid),
        ("serial", &i.serial),
        ("vbios_version", &i.vbios_version),
        ("board_part_number", &i.board_part_number),
        ("compute_capability", &i.compute_capability),
        ("driver_version", &sys.driver_version),
        ("cuda_driver_version", &sys.cuda_driver_version),
        ("nvml_version", &sys.nvml_version),
    ];
    for (k, v) in texts {
        if let Some(v) = v {
            n = n.text(k, v);
        }
    }
    if let Some(a) = i.architecture {
        n.push_attr("architecture", a);
    }
    if let Some(b) = i.brand {
        n.push_attr("brand", b);
    }
    if let Some(b) = i.board_id {
        n.push_attr("board_id", format!("0x{b:x}"));
    }
    if let Some(numa) = i.numa_node {
        n.push_attr("numa_node", numa);
    }
    if let Some(p) = d.performance_state {
        n.push_attr("performance_state", format!("P{p}"));
    }
    if let Some(bits) = d.clock_event_reasons {
        let r = clock_event_reasons(bits);
        n.push_attr(
            "clock_event_reasons",
            if r.is_empty() {
                "none".into()
            } else {
                r.join(",")
            },
        );
    }
    let c = &d.config;
    for (k, v) in [
        ("persistence_mode", c.persistence_mode),
        ("mig_mode", c.mig_mode),
        ("ecc_mode", c.ecc_mode),
        ("remapping_pending", d.health.remapping_pending),
        ("remapping_failed", d.health.remapping_failed),
        ("c2c_active", d.health.c2c_active),
    ] {
        if let Some(v) = v {
            n.push_attr(k, v);
        }
    }
    if let Some(m) = c.compute_mode {
        n.push_attr("compute_mode", m);
    }
    let w = |v: Option<u32>| v.map(f64::from);
    let h = &d.health;
    let optional = [
        (
            "vram_reserved_bytes",
            d.vram_reserved_bytes.map(|v| v as f64),
        ),
        ("memory_temperature_celsius", d.memory_temperature_celsius),
        ("power_limit_w", d.power_limit_w),
        ("power_limit_default_w", i.power_limit_default_w),
        ("power_limit_min_w", i.power_limit_min_w),
        ("power_limit_max_w", i.power_limit_max_w),
        ("fan_speed_percent", d.fan_speed_percent),
        ("frequency_mhz", w(d.graphics_clock_mhz)),
        ("frequency_max_mhz", w(i.graphics_clock_max_mhz)),
        ("frequency_boost_mhz", w(i.graphics_clock_boost_mhz)),
        (
            "energy_consumed_wh",
            d.health.energy_mj_total.map(|mj| mj as f64 / 3.6e6),
        ),
        ("sm_clock_mhz", w(d.sm_clock_mhz)),
        ("sm_clock_max_mhz", w(i.sm_clock_max_mhz)),
        ("memory_clock_mhz", w(d.memory_clock_mhz)),
        ("memory_clock_max_mhz", w(i.memory_clock_max_mhz)),
        ("video_clock_mhz", w(d.video_clock_mhz)),
        ("video_clock_max_mhz", w(i.video_clock_max_mhz)),
        ("encoder_usage_percent", d.encoder_usage_percent),
        ("decoder_usage_percent", d.decoder_usage_percent),
        (
            "temperature_slowdown_celsius",
            w(i.temperature_slowdown_celsius),
        ),
        (
            "temperature_shutdown_celsius",
            w(i.temperature_shutdown_celsius),
        ),
        (
            "temperature_max_operating_celsius",
            w(i.temperature_max_operating_celsius),
        ),
        (
            "memory_temperature_max_celsius",
            w(i.memory_temperature_max_celsius),
        ),
    ];
    for (k, v) in optional {
        if let Some(v) = v {
            n.push_metric(k, Metric::f64(v));
        }
    }
    let counts = [
        ("cuda_cores", i.cuda_cores.map(u64::from)),
        (
            "memory_bus_width_bits",
            i.memory_bus_width_bits.map(u64::from),
        ),
        ("pcie_link_generation", d.pcie_generation.map(u64::from)),
        (
            "pcie_link_generation_max",
            i.pcie_generation_max.map(u64::from),
        ),
        ("pcie_link_width", d.pcie_width.map(u64::from)),
        ("pcie_link_width_max", i.pcie_width_max.map(u64::from)),
        ("nvlink_links", i.nvlink_links.map(u64::from)),
        ("nvlink_links_active", c.nvlink_links_active.map(u64::from)),
        ("c2c_links", i.c2c_links.map(u64::from)),
        (
            "c2c_link_bandwidth_bytes_per_second",
            i.c2c_link_bandwidth_bytes_per_second,
        ),
        ("pcie_replays_total", h.pcie_replays_total),
        (
            "ecc_corrected_volatile_total",
            h.ecc_corrected_volatile_total,
        ),
        (
            "ecc_uncorrected_volatile_total",
            h.ecc_uncorrected_volatile_total,
        ),
        (
            "ecc_corrected_aggregate_total",
            h.ecc_corrected_aggregate_total,
        ),
        (
            "ecc_uncorrected_aggregate_total",
            h.ecc_uncorrected_aggregate_total,
        ),
        (
            "remapped_rows_corrected_total",
            h.remapped_rows_corrected_total,
        ),
        (
            "remapped_rows_uncorrected_total",
            h.remapped_rows_uncorrected_total,
        ),
    ];
    for (k, v) in counts {
        if let Some(v) = v {
            n.push_metric(k, Metric::u64(v));
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use rtop_core::Timestamp;
    use rtop_linux::MemoryKernelFs;
    use rtop_linux::nvml::{
        NvidiaProcess, NvidiaProvider, NvidiaSample, NvidiaStatic, NvidiaSystem,
    };

    /// Records whether processes were requested; answers with one GPU and
    /// one process.
    #[derive(Debug, Default)]
    struct FakeNvml {
        process_requests: Arc<AtomicU32>,
        fail: Option<String>,
    }

    #[derive(Debug)]
    struct FakeSession(Arc<AtomicU32>);

    impl NvidiaProvider for FakeNvml {
        fn open(&self) -> Result<Box<dyn NvidiaSession>, String> {
            match &self.fail {
                Some(e) => Err(e.clone()),
                None => Ok(Box::new(FakeSession(self.process_requests.clone()))),
            }
        }
    }

    impl NvidiaSession for FakeSession {
        fn sample(&mut self, processes: bool) -> Result<NvidiaSample, String> {
            if processes {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            Ok(NvidiaSample {
                system: Arc::new(NvidiaSystem {
                    driver_version: Some("999.1".into()),
                    ..Default::default()
                }),
                problems: Vec::new(),
                devices: vec![NvidiaDevice {
                    info: Arc::new(NvidiaStatic {
                        name: "Fake GPU\x1b[31m".into(),
                        pci: "0000:65:00.0".into(),
                        temperature_slowdown_celsius: Some(90),
                        ..Default::default()
                    }),
                    gpu_usage_percent: Some(40.0),
                    vram_total_bytes: Some(8 << 30),
                    vram_used_bytes: Some(1 << 30),
                    clock_event_reasons: Some(0x4),
                    processes: if processes {
                        vec![NvidiaProcess {
                            pid: 7,
                            used_memory_bytes: Some(1 << 20),
                            graphics: false,
                            sm_usage_percent: Some(12.5),
                        }]
                    } else {
                        Vec::new()
                    },
                    ..Default::default()
                }],
            })
        }
    }

    fn machine() -> Arc<MemoryKernelFs> {
        let fs = MemoryKernelFs::new();
        fs.file(
            "/proc/7/stat",
            "7 (trainer) S 1 7 7 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 500 0 0\n",
        );
        // A DRM client of a device NVML does not cover and sysfs does not
        // list: only a /proc descriptor scan could find it.
        fs.link("/proc/7/fd/3", "/dev/dri/renderD128");
        fs.file(
            "/proc/7/fdinfo/3",
            "drm-driver:\tnouveau\ndrm-pdev:\t0000:99:00.0\ndrm-client-id:\t1\n",
        );
        Arc::new(fs)
    }

    fn sample(nvml: FakeNvml, clients: bool) -> (Result<Sample, CollectorError>, Arc<AtomicU32>) {
        let requests = nvml.process_requests.clone();
        let subs = Arc::new(crate::Subscriptions::default());
        subs.gpu_clients.store(clients, Ordering::Relaxed);
        let mut env = Env::simulated(machine(), subs);
        env.nvidia = Arc::new(nvml);
        let mut c = GpuCollector::new(env);
        let ctx = SampleContext {
            now: Timestamp::from_millis(1000),
            sequence: 1,
        };
        (c.sample(&ctx), requests)
    }

    #[test]
    fn nvml_devices_become_detailed_nodes_with_process_edges() {
        let (s, requests) = sample(FakeNvml::default(), true);
        let mut g = rtop_core::SystemGraph::default();
        g.apply(s.unwrap().patch);
        let n = g.node(&gpu_id("0000:65:00.0")).unwrap();
        assert!(
            !n.text("name").unwrap().contains('\x1b'),
            "names are sanitised"
        );
        assert_eq!(n.text("driver_version"), Some("999.1"));
        assert_eq!(n.text("clock_event_reasons"), Some("sw_power_cap"));
        assert_eq!(n.f64("temperature_slowdown_celsius"), Some(90.0));
        assert_eq!(n.f64("vram_used_bytes"), Some((1u64 << 30) as f64));
        // Values the device did not report are absent or explicitly N/A.
        assert!(n.metric("fan_speed_percent").is_none());
        assert_eq!(
            n.metric("power_w"),
            Some(&Metric::Unavailable(Unavailable::Unsupported))
        );
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        let e = g.edges_of(EdgeKind::UsesGpu).collect::<Vec<_>>();
        assert_eq!(e.len(), 1, "only the NVML process; no DRM scan: {e:?}");
        assert_eq!(e[0].key.target, n.id);
        assert_eq!(
            e[0].metric("gpu_usage_percent").and_then(|m| m.as_f64()),
            Some(12.5)
        );
    }

    #[test]
    fn processes_are_not_queried_unless_shown() {
        let (s, requests) = sample(FakeNvml::default(), false);
        let mut g = rtop_core::SystemGraph::default();
        g.apply(s.unwrap().patch);
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        assert_eq!(g.edges_of(EdgeKind::UsesGpu).count(), 0);
        assert!(g.node(&gpu_id("0000:65:00.0")).is_some());
    }

    #[test]
    fn without_nvml_nvidia_gpus_come_from_procfs_with_the_reason() {
        // nvidia-drm not loaded: no DRM card, only the driver's procfs.
        let fs = machine();
        fs.file(
            "/proc/driver/nvidia/gpus/0009:01:00.0/information",
            "Model: \t\t Demo NVIDIA GPU\nIRQ:   \t\t 7\nGPU UUID: \t GPU-0000\nVideo BIOS: \t 96.00.00.00.01\nBus Location: \t 0009:01:00.0\nDevice Minor: \t 0\n",
        );
        let mut env = Env::simulated(fs, Arc::new(crate::Subscriptions::default()));
        env.nvidia = Arc::new(FakeNvml {
            fail: Some("libnvidia-ml.so.1 not found".into()),
            ..Default::default()
        });
        let s = GpuCollector::new(env)
            .sample(&SampleContext {
                now: Timestamp::from_millis(1000),
                sequence: 1,
            })
            .unwrap();
        assert!(s.degraded.unwrap().contains("libnvidia-ml.so.1 not found"));
        let mut g = rtop_core::SystemGraph::default();
        g.apply(s.patch);
        let n = g.node(&gpu_id("0009:01:00.0")).expect("GPU from procfs");
        assert_eq!(n.text("name"), Some("Demo NVIDIA GPU"));
        assert_eq!(n.text("uuid"), Some("GPU-0000"));
        assert_eq!(n.text("provider"), Some("nvidia-procfs"));
        assert_eq!(
            n.metric("gpu_usage_percent"),
            Some(&Metric::Unavailable(Unavailable::Unsupported))
        );
    }

    #[test]
    fn gpus_outside_the_slurm_step_are_explained() {
        #[derive(Debug)]
        struct OneOfTwo;
        impl NvidiaProvider for OneOfTwo {
            fn open(&self) -> Result<Box<dyn NvidiaSession>, String> {
                FakeNvml::default().open()
            }
            fn device_access(&self, minor: u32) -> Result<(), String> {
                Err(format!("/dev/nvidia{minor}: permission denied"))
            }
        }
        let fs = machine();
        fs.file(
            "/proc/driver/nvidia/gpus/0000:66:00.0/information",
            "Model: \t\t Demo NVIDIA GPU\nDevice Minor: \t 1\n",
        );
        fs.file(
            "/proc/4242/cgroup",
            "0::/system.slice/slurmstepd.scope/job_77/step_249/user/task_0\n",
        );
        let mut env = Env::simulated(fs, Arc::new(crate::Subscriptions::default()));
        env.nvidia = Arc::new(OneOfTwo);
        env.self_pid = Some(4242);
        let s = GpuCollector::new(env)
            .sample(&SampleContext {
                now: Timestamp::from_millis(1000),
                sequence: 1,
            })
            .unwrap();
        let d = s.degraded.unwrap();
        assert!(
            d.contains("may open 1 of 2 NVIDIA GPUs (/dev/nvidia1: permission denied)")
                && d.contains("Slurm job 77 step 249")
                && d.contains("--gres=gpu:2"),
            "{d}"
        );
    }

    #[test]
    fn nvml_listing_no_gpu_says_why() {
        #[derive(Debug)]
        struct Empty;
        impl NvidiaSession for Empty {
            fn sample(&mut self, _: bool) -> Result<NvidiaSample, String> {
                Ok(NvidiaSample {
                    problems: vec!["NVML cannot open GPU 0: no permission (4)".into()],
                    ..Default::default()
                })
            }
        }
        #[derive(Debug)]
        struct EmptyNvml;
        impl NvidiaProvider for EmptyNvml {
            fn open(&self) -> Result<Box<dyn NvidiaSession>, String> {
                Ok(Box::new(Empty))
            }
        }
        let fs = machine();
        fs.file(
            "/proc/driver/nvidia/gpus/0009:01:00.0/information",
            "Model: \t\t Demo NVIDIA GPU\n",
        );
        let mut env = Env::simulated(fs, Arc::new(crate::Subscriptions::default()));
        env.nvidia = Arc::new(EmptyNvml);
        let s = GpuCollector::new(env)
            .sample(&SampleContext {
                now: Timestamp::from_millis(1000),
                sequence: 1,
            })
            .unwrap();
        let d = s.degraded.unwrap();
        assert!(d.contains("NVML cannot open GPU 0: no permission"), "{d}");
    }

    #[test]
    fn nvml_is_retried_after_a_failed_open() {
        #[derive(Debug)]
        struct FlakyNvml(Arc<AtomicU32>);
        impl NvidiaProvider for FlakyNvml {
            fn open(&self) -> Result<Box<dyn NvidiaSession>, String> {
                if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                    Err("nvmlInit_v2 failed: driver not loaded".into())
                } else {
                    FakeNvml::default().open()
                }
            }
        }
        let opens = Arc::new(AtomicU32::new(0));
        let mut env = Env::simulated(machine(), Arc::new(crate::Subscriptions::default()));
        env.nvidia = Arc::new(FlakyNvml(opens.clone()));
        let mut c = GpuCollector::new(env);
        let mut ok = false;
        for i in 1..=2 * u64::from(NVML_RETRY_SAMPLES) {
            ok = c
                .sample(&SampleContext {
                    now: Timestamp::from_millis(i * 1000),
                    sequence: i,
                })
                .is_ok();
        }
        assert!(ok, "GPU found once NVML opens");
        assert_eq!(
            opens.load(Ordering::Relaxed),
            2,
            "one retry, not one per sample"
        );
    }

    #[test]
    fn missing_nvml_is_reported_only_with_an_nvidia_card() {
        let fail = || FakeNvml {
            fail: Some("libnvidia-ml.so.1 not found".into()),
            ..Default::default()
        };
        let (s, _) = sample(fail(), true);
        assert!(s.is_err(), "no GPU at all");
        let fs = machine();
        fs.link("/sys/class/drm/card0/device", "../../../0000:65:00.0");
        fs.file("/sys/class/drm/card0/device/vendor", "0x10de\n");
        let mut env = Env::simulated(fs, Arc::new(crate::Subscriptions::default()));
        env.nvidia = Arc::new(fail());
        let s = GpuCollector::new(env)
            .sample(&SampleContext {
                now: Timestamp::from_millis(1000),
                sequence: 1,
            })
            .unwrap();
        assert!(s.degraded.unwrap().contains("libnvidia-ml.so.1 not found"));
    }

    #[test]
    fn fdinfo_parses_engines_and_memory() {
        let t = "pos:\t0\ndrm-driver:\tamdgpu\ndrm-pdev:\t0000:03:00.0\ndrm-client-id:\t42\ndrm-engine-gfx:\t1000 ns\ndrm-engine-capacity-gfx:\t2\ndrm-memory-vram:\t4 MiB\ndrm-resident-system:\t1024\n";
        let c = drm_fdinfo(t).unwrap();
        assert_eq!((c.client_id, c.pdev.as_str()), (42, "0000:03:00.0"));
        assert_eq!(c.engines.get("gfx"), Some(&1000));
        assert!(!c.engines.contains_key("capacity-gfx"));
        assert_eq!(c.memory.get("vram"), Some(&(4 << 20)));
        assert!(drm_fdinfo("pos: 0\nflags: 2\n").is_none());
    }
}
