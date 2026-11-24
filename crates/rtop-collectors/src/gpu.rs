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
//! * NVML, loaded at runtime from `libnvidia-ml.so.1` when present, for
//!   NVIDIA utilisation, memory, temperature, power and compute processes.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker};
use rtop_core::{
    CollectorId, EdgeKind, EdgeUpdate, EntityId, EntityKind, Evidence, Metric, NodeUpdate,
    ProcessIdentity, Scope, Unavailable,
};
use rtop_linux::fs::{KernelFs, KernelFsExt};
use rtop_linux::parse::pid;

use crate::{
    Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext,
    Subscriptions,
};

pub const ID: CollectorId = CollectorId("gpu");

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
    nvml: Option<Result<nvml::Nvml, String>>,
    rates: RateTracker<(String, String)>,
}

impl GpuCollector {
    pub fn new(env: Env) -> Self {
        GpuCollector {
            env,
            nvml: None,
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

        if self.env.fs.is_live() && self.nvml.is_none() {
            self.nvml = Some(nvml::Nvml::load());
        }
        if let Some(Ok(nv)) = &self.nvml {
            match nv.sample() {
                Ok(devs) => {
                    for d in devs {
                        found += 1;
                        nvidia_pci.push(d.pci.clone());
                        let id = gpu_id(&d.pci);
                        let o = |v: Option<f64>| {
                            v.map_or(Metric::unavailable(Unavailable::Unsupported), Metric::f64)
                        };
                        patch.node(
                            NodeUpdate::new(EntityKind::Gpu, id.clone())
                                .text("name", &d.name)
                                .attr("vendor", "NVIDIA")
                                .attr("driver", "nvidia")
                                .attr("provider", "nvml")
                                .attr("pci_address", d.pci.clone())
                                .metric("gpu_usage_percent", o(d.util_gpu))
                                .metric("memory_busy_percent", o(d.util_mem))
                                .metric("vram_total_bytes", o(d.mem_total))
                                .metric("vram_used_bytes", o(d.mem_used))
                                .metric("temperature_celsius", o(d.temp))
                                .metric("power_w", o(d.power_w)),
                        );
                        for (p, mem) in d.processes {
                            if let Some(ident) = identity_of(fs.as_ref(), p) {
                                patch.edge(
                                    EdgeUpdate::new(
                                        EdgeKind::UsesGpu,
                                        ident.entity_id(),
                                        id.clone(),
                                        Evidence::KernelConfirmed,
                                    )
                                    .metric(
                                        "gpu_memory_bytes",
                                        mem.map_or(
                                            Metric::unavailable(Unavailable::Unsupported),
                                            Metric::u64,
                                        ),
                                    ),
                                );
                            }
                        }
                    }
                }
                Err(e) => degraded.push(format!("nvml: {e}")),
            }
        }

        let mut cards = fs.read_dir("/sys/class/drm").unwrap_or_default();
        cards.retain(|c| {
            c.strip_prefix("card")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        });
        cards.sort();
        self.rates.begin();
        for card in cards {
            let dev = format!("/sys/class/drm/{card}/device");
            let Ok(link) = fs.read_link(&dev) else {
                continue;
            };
            let pci = link.rsplit('/').next().unwrap_or(&card).to_owned();
            if nvidia_pci.contains(&pci) {
                continue;
            }
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
            if vendor == "NVIDIA" && !matches!(self.nvml, Some(Ok(_))) {
                degraded.push(
                    "NVIDIA GPU present but NVML (libnvidia-ml.so.1) is not available".into(),
                );
            }
            patch.node(n);
            found += 1;
        }

        if Subscriptions::get(&self.env.subscriptions.gpu_clients) {
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

/// Runtime-loaded NVIDIA Management Library.
mod nvml {
    use super::*;

    type Device = *mut c_void;
    type Ret = c_int;

    #[repr(C)]
    #[derive(Default)]
    struct Utilization {
        gpu: c_uint,
        memory: c_uint,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Memory {
        total: u64,
        free: u64,
        used: u64,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct ProcessInfo {
        pid: c_uint,
        used_gpu_memory: u64,
        gpu_instance_id: c_uint,
        compute_instance_id: c_uint,
    }

    /// `nvmlPciInfo_t` (v3): legacy bus id, then domain/bus/device.
    #[repr(C)]
    struct PciInfo {
        bus_id_legacy: [c_char; 16],
        domain: c_uint,
        bus: c_uint,
        device: c_uint,
        pci_device_id: c_uint,
        pci_sub_system_id: c_uint,
        bus_id: [c_char; 32],
    }

    #[derive(Debug)]
    pub struct DeviceSample {
        pub name: String,
        pub pci: String,
        pub util_gpu: Option<f64>,
        pub util_mem: Option<f64>,
        pub mem_total: Option<f64>,
        pub mem_used: Option<f64>,
        pub temp: Option<f64>,
        pub power_w: Option<f64>,
        pub processes: Vec<(u32, Option<u64>)>,
    }

    struct Fns {
        count: unsafe extern "C" fn(*mut c_uint) -> Ret,
        handle: unsafe extern "C" fn(c_uint, *mut Device) -> Ret,
        name: unsafe extern "C" fn(Device, *mut c_char, c_uint) -> Ret,
        pci: unsafe extern "C" fn(Device, *mut PciInfo) -> Ret,
        util: unsafe extern "C" fn(Device, *mut Utilization) -> Ret,
        memory: unsafe extern "C" fn(Device, *mut Memory) -> Ret,
        temp: unsafe extern "C" fn(Device, c_int, *mut c_uint) -> Ret,
        power: unsafe extern "C" fn(Device, *mut c_uint) -> Ret,
        procs: Option<unsafe extern "C" fn(Device, *mut c_uint, *mut ProcessInfo) -> Ret>,
    }

    pub struct Nvml {
        fns: Fns,
        _lib: *mut c_void,
    }

    // SAFETY: NVML is documented as thread-safe; the library handle and
    // function pointers stay valid for the process lifetime (never dlclosed).
    unsafe impl Send for Nvml {}

    impl std::fmt::Debug for Nvml {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Nvml")
        }
    }

    unsafe fn sym<T: Copy>(lib: *mut c_void, name: &CStr) -> Option<T> {
        // SAFETY: lib is a valid dlopen handle; the caller supplies the
        // correct function type for `name` per nvml.h.
        let p = unsafe { libc::dlsym(lib, name.as_ptr()) };
        if p.is_null() {
            None
        } else {
            Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
        }
    }

    impl Nvml {
        pub fn load() -> Result<Nvml, String> {
            // SAFETY: dlopen with a constant library name.
            let lib = unsafe {
                libc::dlopen(
                    c"libnvidia-ml.so.1".as_ptr(),
                    libc::RTLD_NOW | libc::RTLD_LOCAL,
                )
            };
            if lib.is_null() {
                return Err("libnvidia-ml.so.1 not found".into());
            }
            // SAFETY: symbol types match nvml.h.
            unsafe {
                let init: unsafe extern "C" fn() -> Ret =
                    sym(lib, c"nvmlInit_v2").ok_or("nvmlInit_v2 missing")?;
                let r = init();
                if r != 0 {
                    return Err(format!("nvmlInit_v2 failed ({r})"));
                }
                Ok(Nvml {
                    fns: Fns {
                        count: sym(lib, c"nvmlDeviceGetCount_v2")
                            .ok_or("nvmlDeviceGetCount_v2 missing")?,
                        handle: sym(lib, c"nvmlDeviceGetHandleByIndex_v2")
                            .ok_or("handle missing")?,
                        name: sym(lib, c"nvmlDeviceGetName").ok_or("name missing")?,
                        pci: sym(lib, c"nvmlDeviceGetPciInfo_v3").ok_or("pci info missing")?,
                        util: sym(lib, c"nvmlDeviceGetUtilizationRates")
                            .ok_or("utilization missing")?,
                        memory: sym(lib, c"nvmlDeviceGetMemoryInfo").ok_or("memory missing")?,
                        temp: sym(lib, c"nvmlDeviceGetTemperature").ok_or("temperature missing")?,
                        power: sym(lib, c"nvmlDeviceGetPowerUsage").ok_or("power missing")?,
                        procs: sym(lib, c"nvmlDeviceGetComputeRunningProcesses_v3")
                            .or_else(|| sym(lib, c"nvmlDeviceGetComputeRunningProcesses_v2")),
                    },
                    _lib: lib,
                })
            }
        }

        pub fn sample(&self) -> Result<Vec<DeviceSample>, String> {
            let f = &self.fns;
            let mut n: c_uint = 0;
            // SAFETY: out-pointers to properly sized locals throughout.
            unsafe {
                if (f.count)(&mut n) != 0 {
                    return Err("device count failed".into());
                }
                let mut out = Vec::new();
                for i in 0..n.min(64) {
                    let mut d: Device = std::ptr::null_mut();
                    if (f.handle)(i, &mut d) != 0 {
                        continue;
                    }
                    let mut name = [0 as c_char; 96];
                    let name = if (f.name)(d, name.as_mut_ptr(), name.len() as c_uint) == 0 {
                        CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned()
                    } else {
                        format!("NVIDIA GPU {i}")
                    };
                    let mut pci: PciInfo = std::mem::zeroed();
                    let pci = if (f.pci)(d, &mut pci) == 0 {
                        format!("{:04x}:{:02x}:{:02x}.0", pci.domain, pci.bus, pci.device)
                    } else {
                        format!("nvml{i}")
                    };
                    let mut u = Utilization::default();
                    let ok_u = (f.util)(d, &mut u) == 0;
                    let mut m = Memory::default();
                    let ok_m = (f.memory)(d, &mut m) == 0;
                    let mut t: c_uint = 0;
                    let ok_t = (f.temp)(d, 0, &mut t) == 0;
                    let mut p: c_uint = 0;
                    let ok_p = (f.power)(d, &mut p) == 0;
                    let mut processes = Vec::new();
                    if let Some(procs) = f.procs {
                        let mut buf = [ProcessInfo::default(); 128];
                        let mut cnt = buf.len() as c_uint;
                        if procs(d, &mut cnt, buf.as_mut_ptr()) == 0 {
                            for pi in &buf[..(cnt as usize).min(buf.len())] {
                                // NVML_VALUE_NOT_AVAILABLE = u64::MAX
                                processes.push((
                                    pi.pid,
                                    (pi.used_gpu_memory != u64::MAX).then_some(pi.used_gpu_memory),
                                ));
                            }
                        }
                    }
                    out.push(DeviceSample {
                        name,
                        pci,
                        util_gpu: ok_u.then_some(f64::from(u.gpu)),
                        util_mem: ok_u.then_some(f64::from(u.memory)),
                        mem_total: ok_m.then_some(m.total as f64),
                        mem_used: ok_m.then_some(m.used as f64),
                        temp: ok_t.then_some(f64::from(t)),
                        power_w: ok_p.then_some(f64::from(p) / 1000.0),
                        processes,
                    });
                }
                Ok(out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
