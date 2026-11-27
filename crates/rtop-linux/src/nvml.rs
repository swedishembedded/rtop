// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0
//
// Swedish Embedded AB implements solutions for low-overhead accelerator
// telemetry on Linux for its clients. If your team needs expertise in
// NVIDIA GPU monitoring and fleet observability then you can procure our
// services by sending an email to info@swedishembedded.com.

//! NVIDIA GPUs through the NVIDIA Management Library (NVML).
//!
//! `libnvidia-ml.so.1` is loaded at runtime, so rtop neither links against
//! nor requires the driver. Every NVML call costs a driver round trip, and
//! some cost milliseconds (measured on a 4 x GH200 node with driver 580:
//! total energy ~5.5 ms, maximum clocks ~2-5 ms, PCIe throughput ~20 ms
//! because it samples for 20 ms), so queries are tiered:
//!
//! * identity, limits and thresholds are read once per device;
//! * configuration (persistence, compute, MIG and ECC modes, active NVLinks)
//!   and the error counters (ECC, row remapping, PCIe replays; ~0.5 ms per
//!   GPU) are re-read every [`PERIODIC_REFRESH_SAMPLES`] samples;
//! * live values (utilisation, memory, temperatures, power, clocks, P-state,
//!   clock event reasons, encoder/decoder, PCIe link) are read every sample;
//!   at a 1 s interval each costs a few to 25 microseconds;
//! * processes and their SM utilisation (1.5-3 ms per busy GPU) are listed
//!   only when the caller asks for them;
//! * a query a device answers with "not supported" (or a symbol the driver
//!   lacks) is never issued again for that device;
//! * energy and PCIe throughput are not read at all: the first blocks the
//!   driver for milliseconds, the second for 20 ms per direction.
//!
//! Without processes a sample costs well under a millisecond per GPU.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;

/// Live samples between two reads of a device's configuration and error
/// counters.
pub const PERIODIC_REFRESH_SAMPLES: u32 = 30;

/// Identity, capabilities and limits of one GPU; read once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NvidiaStatic {
    pub index: u32,
    pub name: String,
    /// `domain:bus:device.function`, lower-case hex.
    pub pci: String,
    pub uuid: Option<String>,
    pub serial: Option<String>,
    pub vbios_version: Option<String>,
    pub board_part_number: Option<String>,
    pub architecture: Option<&'static str>,
    pub compute_capability: Option<String>,
    pub cuda_cores: Option<u32>,
    pub memory_bus_width_bits: Option<u32>,
    pub graphics_clock_max_mhz: Option<u32>,
    pub sm_clock_max_mhz: Option<u32>,
    pub memory_clock_max_mhz: Option<u32>,
    pub video_clock_max_mhz: Option<u32>,
    pub pcie_generation_max: Option<u32>,
    pub pcie_width_max: Option<u32>,
    pub temperature_shutdown_celsius: Option<u32>,
    pub temperature_slowdown_celsius: Option<u32>,
    pub temperature_max_operating_celsius: Option<u32>,
    pub memory_temperature_max_celsius: Option<u32>,
    pub power_limit_min_w: Option<f64>,
    pub power_limit_max_w: Option<f64>,
    pub power_limit_default_w: Option<f64>,
    pub nvlink_links: Option<u32>,
    /// Product brand, e.g. `NVIDIA`, `GeForce`, `Quadro RTX`.
    pub brand: Option<&'static str>,
    pub numa_node: Option<u32>,
    pub gsp_firmware_version: Option<String>,
    pub inforom_version: Option<String>,
    pub board_id: Option<u32>,
    /// PCI device and subsystem ids, e.g. `0x234210de`.
    pub pci_device_id: Option<String>,
    pub pci_subsystem_id: Option<String>,
    /// Highest boost clock the customer may run at.
    pub graphics_clock_boost_mhz: Option<u32>,
    /// Chip-to-chip (Grace–Hopper) links and per-link bandwidth.
    pub c2c_links: Option<u32>,
    pub c2c_link_bandwidth_bytes_per_second: Option<u64>,
    /// A display can be attached (graphics processes are only listed then).
    pub display_mode: Option<bool>,
}

/// Administrative state that changes rarely; re-read periodically.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NvidiaConfig {
    pub persistence_mode: Option<bool>,
    pub compute_mode: Option<&'static str>,
    pub mig_mode: Option<bool>,
    pub ecc_mode: Option<bool>,
    pub nvlink_links_active: Option<u32>,
}

/// Error counters; re-read periodically.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NvidiaHealth {
    pub pcie_replays_total: Option<u64>,
    /// `None` while ECC is disabled.
    pub ecc_corrected_volatile_total: Option<u64>,
    pub ecc_uncorrected_volatile_total: Option<u64>,
    pub ecc_corrected_aggregate_total: Option<u64>,
    pub ecc_uncorrected_aggregate_total: Option<u64>,
    pub remapped_rows_corrected_total: Option<u64>,
    pub remapped_rows_uncorrected_total: Option<u64>,
    pub remapping_pending: Option<bool>,
    pub remapping_failed: Option<bool>,
    /// Energy since the driver loaded, in millijoules.
    pub energy_mj_total: Option<u64>,
    pub c2c_active: Option<bool>,
}

/// A process with a context on the GPU.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NvidiaProcess {
    pub pid: u32,
    /// `None` when the driver withholds it (e.g. under MIG or without
    /// permission).
    pub used_memory_bytes: Option<u64>,
    pub graphics: bool,
    /// Mean SM utilisation over the samples the driver took since the
    /// previous call; 0 for a process with no sample in that window (the
    /// driver samples only processes that ran); `None` when the driver
    /// does not report per-process utilisation.
    pub sm_usage_percent: Option<f64>,
}

/// One sample of one GPU. `None` means the device or driver does not report
/// the value; it is never a stand-in for zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NvidiaDevice {
    pub info: Arc<NvidiaStatic>,
    pub config: NvidiaConfig,
    pub health: NvidiaHealth,
    pub gpu_usage_percent: Option<f64>,
    pub memory_busy_percent: Option<f64>,
    pub vram_total_bytes: Option<u64>,
    pub vram_used_bytes: Option<u64>,
    pub vram_reserved_bytes: Option<u64>,
    pub temperature_celsius: Option<f64>,
    pub memory_temperature_celsius: Option<f64>,
    pub power_w: Option<f64>,
    pub power_limit_w: Option<f64>,
    pub fan_speed_percent: Option<f64>,
    pub graphics_clock_mhz: Option<u32>,
    pub sm_clock_mhz: Option<u32>,
    pub memory_clock_mhz: Option<u32>,
    pub video_clock_mhz: Option<u32>,
    /// P-state, 0 (maximum performance) to 15.
    pub performance_state: Option<u32>,
    /// `nvmlClocksEventReason*` bit set; see [`clock_event_reasons`].
    pub clock_event_reasons: Option<u64>,
    pub encoder_usage_percent: Option<f64>,
    pub decoder_usage_percent: Option<f64>,
    pub pcie_generation: Option<u32>,
    pub pcie_width: Option<u32>,
    /// NVLink data sent and received over all links since the driver
    /// loaded, in KiB; read only for detailed samples.
    pub nvlink_tx_kib_total: Option<u64>,
    pub nvlink_rx_kib_total: Option<u64>,
    /// Listed only for detailed samples.
    pub processes: Vec<NvidiaProcess>,
    /// Set when the device failed in a way that is not "unsupported", e.g.
    /// it fell off the bus.
    pub error: Option<String>,
}

/// Driver-wide versions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NvidiaSystem {
    pub driver_version: Option<String>,
    /// e.g. `13.0`.
    pub cuda_driver_version: Option<String>,
    pub nvml_version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NvidiaSample {
    pub system: Arc<NvidiaSystem>,
    pub devices: Vec<NvidiaDevice>,
    /// Why devices the driver has are missing from `devices` (e.g. a
    /// handle refused for lack of access to `/dev/nvidia*`), or that NVML
    /// reports no device at all.
    pub problems: Vec<String>,
}

/// Opens a connection to the NVIDIA driver. Opening may be slow (NVML
/// initialisation takes ~200 ms), so collectors open lazily on their worker.
pub trait NvidiaProvider: Send + Sync + std::fmt::Debug {
    fn open(&self) -> Result<Box<dyn NvidiaSession>, String>;

    /// Whether this process may open `/dev/nvidia<minor>`; NVML silently
    /// leaves out GPUs it may not open (e.g. a cgroup device constraint).
    fn device_access(&self, minor: u32) -> Result<(), String> {
        let _ = minor;
        Ok(())
    }
}

/// An open driver connection holding per-device caches.
pub trait NvidiaSession: Send + std::fmt::Debug {
    /// Samples every device; `detail` also lists the processes on each and
    /// reads NVLink traffic (milliseconds per GPU, so callers ask only while
    /// it is shown).
    fn sample(&mut self, detail: bool) -> Result<NvidiaSample, String>;
}

/// The real driver, through `libnvidia-ml.so.1`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Nvml;

impl NvidiaProvider for Nvml {
    fn open(&self) -> Result<Box<dyn NvidiaSession>, String> {
        Ok(Box::new(ffi::Session::open()?))
    }

    fn device_access(&self, minor: u32) -> Result<(), String> {
        let path = format!("/dev/nvidia{minor}");
        // Read-only, non-blocking, closed at once: the same open NVML does.
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .map(drop)
            .map_err(|e| match e.raw_os_error() {
                Some(libc::EPERM | libc::EACCES) => format!("{path}: permission denied"),
                Some(libc::ENOENT) => format!("{path}: missing"),
                _ => format!("{path}: {e}"),
            })
    }
}

/// No NVIDIA driver (captured or simulated systems).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoNvidia;

impl NvidiaProvider for NoNvidia {
    fn open(&self) -> Result<Box<dyn NvidiaSession>, String> {
        Err("NVML is not available for a captured or simulated system".into())
    }
}

/// Names of the set `nvmlClocksEventReason*` bits, in bit order. "gpu_idle"
/// is a reason too: clocks are low because nothing runs.
pub fn clock_event_reasons(bits: u64) -> Vec<&'static str> {
    const NAMES: [(u64, &str); 9] = [
        (0x1, "gpu_idle"),
        (0x2, "applications_clocks_setting"),
        (0x4, "sw_power_cap"),
        (0x8, "hw_slowdown"),
        (0x10, "sync_boost"),
        (0x20, "sw_thermal_slowdown"),
        (0x40, "hw_thermal_slowdown"),
        (0x80, "hw_power_brake_slowdown"),
        (0x100, "display_clock_setting"),
    ];
    let mut out: Vec<&str> = NAMES
        .iter()
        .filter(|(b, _)| bits & b != 0)
        .map(|(_, n)| *n)
        .collect();
    if bits & !0x1ff != 0 {
        out.push("other");
    }
    out
}

/// Human-readable `nvmlReturn_t`.
pub fn error_name(code: i32) -> &'static str {
    match code {
        1 => "NVML not initialised",
        2 => "invalid argument",
        3 => "not supported",
        4 => "no permission (no access to /dev/nvidia*?)",
        6 => "not found",
        9 => "NVIDIA driver not loaded",
        10 => "timeout",
        12 => "NVML library not found",
        13 => "function not found",
        14 => "corrupted infoROM",
        15 => "GPU fell off the bus",
        16 => "GPU reset required",
        17 => "blocked by the operating system or cgroups",
        18 => "driver/library version mismatch",
        28 => "no GPUs found",
        _ => "error",
    }
}

/// Where `libnvidia-ml.so.1` is looked for after the dynamic loader's own
/// search path: driver installs that are not on it (container images with
/// the NVIDIA runtime, distribution layouts, WSL).
pub const LIBRARY_PATHS: &[&str] = &[
    "libnvidia-ml.so.1",
    "/usr/lib64/libnvidia-ml.so.1",
    "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1",
    "/usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1",
    "/usr/lib/libnvidia-ml.so.1",
    "/usr/local/nvidia/lib64/libnvidia-ml.so.1",
    "/.singularity.d/libs/libnvidia-ml.so.1",
    "/usr/lib/wsl/lib/libnvidia-ml.so.1",
];

/// `nvmlBrandType_t` to a name.
pub fn brand_name(brand: u32) -> Option<&'static str> {
    Some(match brand {
        1 => "Quadro",
        2 => "Tesla",
        3 => "NVS",
        4 => "GRID",
        5 => "GeForce",
        6 => "Titan",
        7 => "NVIDIA Virtual Applications",
        8 => "NVIDIA Virtual PC",
        9 => "NVIDIA Virtual Compute Server",
        10 => "NVIDIA RTX Virtual Workstation",
        11 => "NVIDIA Cloud Gaming",
        12 => "Quadro RTX",
        13 => "NVIDIA RTX",
        14 => "NVIDIA",
        15 => "GeForce RTX",
        16 => "Titan RTX",
        _ => return None,
    })
}

/// `NVML_DEVICE_ARCH_*` to a name.
pub fn architecture_name(arch: u32) -> Option<&'static str> {
    Some(match arch {
        2 => "Kepler",
        3 => "Maxwell",
        4 => "Pascal",
        5 => "Volta",
        6 => "Turing",
        7 => "Ampere",
        8 => "Ada",
        9 => "Hopper",
        10 => "Blackwell",
        _ => return None,
    })
}

/// `nvmlComputeMode_t` to a name.
pub fn compute_mode_name(mode: u32) -> Option<&'static str> {
    Some(match mode {
        0 => "default",
        1 => "exclusive_thread",
        2 => "prohibited",
        3 => "exclusive_process",
        _ => return None,
    })
}

mod ffi {
    use super::*;

    type Device = *mut c_void;
    type Ret = c_int;

    const SUCCESS: Ret = 0;
    const ERROR_INVALID_ARGUMENT: Ret = 2;
    const ERROR_NOT_SUPPORTED: Ret = 3;
    const ERROR_NOT_FOUND: Ret = 6;
    const ERROR_INSUFFICIENT_SIZE: Ret = 7;
    const ERROR_FUNCTION_NOT_FOUND: Ret = 13;
    const ERROR_ARGUMENT_VERSION_MISMATCH: Ret = 25;

    const CLOCK_GRAPHICS: c_int = 0;
    const CLOCK_SM: c_int = 1;
    const CLOCK_MEM: c_int = 2;
    const CLOCK_VIDEO: c_int = 3;

    /// Field ids from nvml.h (`NVML_FI_DEV_*`).
    const FI_ECC_SBE_VOL_TOTAL: u32 = 3;
    const FI_ECC_DBE_VOL_TOTAL: u32 = 4;
    const FI_ECC_SBE_AGG_TOTAL: u32 = 5;
    const FI_ECC_DBE_AGG_TOTAL: u32 = 6;
    const FI_MEMORY_TEMP: u32 = 82;
    const FI_NVLINK_LINK_COUNT: u32 = 91;
    const FI_PCIE_REPLAY_COUNTER: u32 = 94;
    const FI_REMAPPED_COR: u32 = 142;
    const FI_REMAPPED_UNC: u32 = 143;
    const FI_REMAPPED_PENDING: u32 = 144;
    const FI_REMAPPED_FAILURE: u32 = 145;
    const FI_NVLINK_THROUGHPUT_DATA_TX: u32 = 138;
    const FI_NVLINK_THROUGHPUT_DATA_RX: u32 = 139;
    const FI_C2C_LINK_COUNT: u32 = 170;
    const FI_C2C_LINK_GET_STATUS: u32 = 171;
    const FI_C2C_LINK_GET_MAX_BW: u32 = 172;
    /// `scopeId` meaning "all links".
    const SCOPE_ALL_LINKS: u32 = u32::MAX;
    /// Error counters, read in one batch per periodic refresh.
    const HEALTH_FIELDS: [u32; 10] = [
        FI_C2C_LINK_GET_STATUS,
        FI_PCIE_REPLAY_COUNTER,
        FI_ECC_SBE_VOL_TOTAL,
        FI_ECC_DBE_VOL_TOTAL,
        FI_ECC_SBE_AGG_TOTAL,
        FI_ECC_DBE_AGG_TOTAL,
        FI_REMAPPED_COR,
        FI_REMAPPED_UNC,
        FI_REMAPPED_PENDING,
        FI_REMAPPED_FAILURE,
    ];

    /// Upper bound on processes listed per GPU and kind.
    const MAX_PROCESSES: usize = 4096;

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

    /// `nvmlMemory_v2_t`; `used` excludes `reserved`.
    #[repr(C)]
    #[derive(Default)]
    struct MemoryV2 {
        version: c_uint,
        total: u64,
        reserved: u64,
        free: u64,
        used: u64,
    }

    /// `nvmlMemory_v2` = `sizeof(nvmlMemory_v2_t) | 2 << 24`.
    const MEMORY_V2: c_uint = std::mem::size_of::<MemoryV2>() as c_uint | (2 << 24);

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

    /// `nvmlProcessUtilizationSample_t`.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct ProcessUtilization {
        pid: c_uint,
        timestamp_us: u64,
        sm: c_uint,
        memory: c_uint,
        encoder: c_uint,
        decoder: c_uint,
    }

    /// `nvmlFieldValue_t`.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FieldValue {
        field_id: c_uint,
        scope_id: c_uint,
        timestamp: i64,
        latency_usec: i64,
        value_type: c_uint,
        nvml_return: Ret,
        /// `nvmlValue_t` union; interpreted through `value_type`.
        value: u64,
    }

    impl FieldValue {
        fn new(id: u32) -> Self {
            FieldValue {
                field_id: id,
                ..Default::default()
            }
        }

        fn as_u64(&self) -> Option<u64> {
            let v = self.value;
            match self.value_type {
                // double
                0 => {
                    let d = f64::from_bits(v);
                    (d.is_finite() && d >= 0.0).then_some(d as u64)
                }
                // unsigned int
                1 => Some(v & 0xffff_ffff),
                // unsigned long / unsigned long long
                2 | 3 => Some(v),
                // signed long long
                4 => u64::try_from(v as i64).ok(),
                // signed int
                5 => u64::try_from(v as u32 as i32).ok(),
                // unsigned short
                6 => Some(v & 0xffff),
                _ => None,
            }
        }
    }

    type Fn0<A> = unsafe extern "C" fn(Device, *mut A) -> Ret;
    type Fn2<A, B> = unsafe extern "C" fn(Device, *mut A, *mut B) -> Ret;
    type FnArg<A> = unsafe extern "C" fn(Device, c_int, *mut A) -> Ret;
    type FnStr = unsafe extern "C" fn(Device, *mut c_char, c_uint) -> Ret;
    type FnProcs = unsafe extern "C" fn(Device, *mut c_uint, *mut ProcessInfo) -> Ret;

    /// Resolved entry points. Only identity is mandatory; everything else
    /// is optional so an older driver still yields what it supports.
    struct Fns {
        count: unsafe extern "C" fn(*mut c_uint) -> Ret,
        handle: unsafe extern "C" fn(c_uint, *mut Device) -> Ret,
        name: FnStr,
        pci: Fn0<PciInfo>,
        driver_version: Option<unsafe extern "C" fn(*mut c_char, c_uint) -> Ret>,
        nvml_version: Option<unsafe extern "C" fn(*mut c_char, c_uint) -> Ret>,
        cuda_version: Option<unsafe extern "C" fn(*mut c_int) -> Ret>,
        uuid: Option<FnStr>,
        serial: Option<FnStr>,
        vbios: Option<FnStr>,
        part_number: Option<FnStr>,
        architecture: Option<Fn0<c_uint>>,
        compute_capability: Option<Fn2<c_int, c_int>>,
        cores: Option<Fn0<c_uint>>,
        bus_width: Option<Fn0<c_uint>>,
        max_clock: Option<FnArg<c_uint>>,
        max_pcie_gen: Option<Fn0<c_uint>>,
        max_pcie_width: Option<Fn0<c_uint>>,
        temp_threshold: Option<FnArg<c_uint>>,
        power_constraints: Option<Fn2<c_uint, c_uint>>,
        power_default: Option<Fn0<c_uint>>,
        display_mode: Option<Fn0<c_int>>,
        persistence: Option<Fn0<c_int>>,
        compute_mode: Option<Fn0<c_int>>,
        mig_mode: Option<Fn2<c_uint, c_uint>>,
        ecc_mode: Option<Fn2<c_int, c_int>>,
        nvlink_state: Option<unsafe extern "C" fn(Device, c_uint, *mut c_int) -> Ret>,
        util: Option<Fn0<Utilization>>,
        memory_v2: Option<Fn0<MemoryV2>>,
        memory: Option<Fn0<Memory>>,
        temperature: Option<FnArg<c_uint>>,
        power: Option<Fn0<c_uint>>,
        power_limit: Option<Fn0<c_uint>>,
        fan: Option<Fn0<c_uint>>,
        clock: Option<FnArg<c_uint>>,
        pstate: Option<Fn0<c_int>>,
        clock_events: Option<Fn0<u64>>,
        encoder: Option<Fn2<c_uint, c_uint>>,
        decoder: Option<Fn2<c_uint, c_uint>>,
        pcie_gen: Option<Fn0<c_uint>>,
        pcie_width: Option<Fn0<c_uint>>,
        fields: Option<unsafe extern "C" fn(Device, c_int, *mut FieldValue) -> Ret>,
        brand: Option<Fn0<c_uint>>,
        numa_node: Option<Fn0<c_uint>>,
        gsp_version: Option<unsafe extern "C" fn(Device, *mut c_char) -> Ret>,
        inforom: Option<FnStr>,
        board_id: Option<Fn0<c_uint>>,
        boost_clock: Option<FnArg<c_uint>>,
        energy: Option<Fn0<u64>>,
        compute_procs: Option<FnProcs>,
        graphics_procs: Option<FnProcs>,
        process_util:
            Option<unsafe extern "C" fn(Device, *mut ProcessUtilization, *mut c_uint, u64) -> Ret>,
    }

    /// Live queries a device still answers; cleared on "not supported".
    #[derive(Debug, Clone, Copy)]
    struct Caps {
        util: bool,
        memory_v2: bool,
        memory: bool,
        temperature: bool,
        power: bool,
        power_limit: bool,
        fan: bool,
        clocks: [bool; 4],
        pstate: bool,
        clock_events: bool,
        encoder: bool,
        decoder: bool,
        pcie_gen: bool,
        pcie_width: bool,
        memory_temperature: bool,
        compute_procs: bool,
        graphics_procs: bool,
        process_util: bool,
        nvlink_traffic: bool,
    }

    impl Caps {
        fn from(f: &Fns, display: bool) -> Caps {
            Caps {
                util: f.util.is_some(),
                memory_v2: f.memory_v2.is_some(),
                memory: f.memory.is_some(),
                temperature: f.temperature.is_some(),
                power: f.power.is_some(),
                power_limit: f.power_limit.is_some(),
                fan: f.fan.is_some(),
                clocks: [f.clock.is_some(); 4],
                pstate: f.pstate.is_some(),
                clock_events: f.clock_events.is_some(),
                encoder: f.encoder.is_some(),
                decoder: f.decoder.is_some(),
                pcie_gen: f.pcie_gen.is_some(),
                pcie_width: f.pcie_width.is_some(),
                memory_temperature: f.fields.is_some(),
                compute_procs: f.compute_procs.is_some(),
                graphics_procs: f.graphics_procs.is_some() && display,
                process_util: f.process_util.is_some(),
                nvlink_traffic: f.fields.is_some(),
            }
        }
    }

    /// Records the outcome of an optional query: success yields the value;
    /// "not supported" disables the query for good.
    fn keep<T>(cap: &mut bool, r: Ret, v: T) -> Option<T> {
        match r {
            SUCCESS => Some(v),
            ERROR_NOT_SUPPORTED | ERROR_FUNCTION_NOT_FOUND => {
                *cap = false;
                None
            }
            _ => None,
        }
    }

    struct DeviceState {
        handle: Device,
        info: Arc<NvidiaStatic>,
        caps: Caps,
        config: NvidiaConfig,
        health: NvidiaHealth,
        samples: u32,
        /// Health fields the device still answers.
        health_fields: Vec<FieldValue>,
        procs: Vec<ProcessInfo>,
        utils: Vec<ProcessUtilization>,
        /// Newest process utilisation sample seen (driver timestamp, us).
        util_seen_us: u64,
    }

    pub(super) struct Session {
        fns: Fns,
        system: Arc<NvidiaSystem>,
        devices: Vec<DeviceState>,
        /// Device count the device list was built for.
        enumerated: Option<u32>,
        problems: Vec<String>,
    }

    // SAFETY: NVML is thread-safe; the library handle, function pointers and
    // device handles stay valid for the process lifetime (never dlclosed or
    // shut down). The session is used by one thread at a time (`&mut`).
    unsafe impl Send for Session {}

    impl std::fmt::Debug for Session {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("NvmlSession")
                .field("devices", &self.devices.len())
                .finish()
        }
    }

    unsafe fn sym<T: Copy>(lib: *mut c_void, name: &CStr) -> Option<T> {
        // SAFETY: lib is a valid dlopen handle; the caller supplies the
        // correct function type for `name` per nvml.h.
        let p = unsafe { libc::dlsym(lib, name.as_ptr()) };
        if p.is_null() {
            None
        } else {
            // SAFETY: `T` is a function pointer type of the same size.
            Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
        }
    }

    /// Reads a NUL-terminated string through `f`; `None` unless it succeeds
    /// with a non-empty result.
    fn string(f: impl FnOnce(*mut c_char, c_uint) -> Ret) -> Option<String> {
        let mut buf = [0 as c_char; 96];
        if f(buf.as_mut_ptr(), buf.len() as c_uint) != SUCCESS {
            return None;
        }
        buf[buf.len() - 1] = 0;
        // SAFETY: buf is NUL-terminated (forced above).
        let s = unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .trim()
            .to_owned();
        (!s.is_empty()).then_some(s)
    }

    fn mw_to_w(mw: c_uint) -> f64 {
        f64::from(mw) / 1000.0
    }

    impl Session {
        pub(super) fn open() -> Result<Session, String> {
            let lib = LIBRARY_PATHS
                .iter()
                .filter_map(|p| std::ffi::CString::new(*p).ok())
                .map(|p| {
                    // SAFETY: dlopen with a constant, NUL-terminated path.
                    unsafe { libc::dlopen(p.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) }
                })
                .find(|l| !l.is_null());
            let Some(lib) = lib else {
                return Err(
                    "libnvidia-ml.so.1 not found (NVIDIA driver libraries not installed or \
                     not mounted into this container)"
                        .into(),
                );
            };
            // SAFETY: every symbol type matches its nvml.h prototype.
            let fns = unsafe {
                let init: unsafe extern "C" fn() -> Ret =
                    sym(lib, c"nvmlInit_v2").ok_or("nvmlInit_v2 missing")?;
                let r = init();
                if r != SUCCESS {
                    return Err(format!("nvmlInit_v2 failed: {} ({r})", error_name(r)));
                }
                Fns {
                    count: sym(lib, c"nvmlDeviceGetCount_v2")
                        .ok_or("nvmlDeviceGetCount_v2 missing")?,
                    handle: sym(lib, c"nvmlDeviceGetHandleByIndex_v2")
                        .ok_or("nvmlDeviceGetHandleByIndex_v2 missing")?,
                    name: sym(lib, c"nvmlDeviceGetName").ok_or("nvmlDeviceGetName missing")?,
                    pci: sym(lib, c"nvmlDeviceGetPciInfo_v3")
                        .ok_or("nvmlDeviceGetPciInfo_v3 missing")?,
                    driver_version: sym(lib, c"nvmlSystemGetDriverVersion"),
                    nvml_version: sym(lib, c"nvmlSystemGetNVMLVersion"),
                    cuda_version: sym(lib, c"nvmlSystemGetCudaDriverVersion_v2"),
                    uuid: sym(lib, c"nvmlDeviceGetUUID"),
                    serial: sym(lib, c"nvmlDeviceGetSerial"),
                    vbios: sym(lib, c"nvmlDeviceGetVbiosVersion"),
                    part_number: sym(lib, c"nvmlDeviceGetBoardPartNumber"),
                    architecture: sym(lib, c"nvmlDeviceGetArchitecture"),
                    compute_capability: sym(lib, c"nvmlDeviceGetCudaComputeCapability"),
                    cores: sym(lib, c"nvmlDeviceGetNumGpuCores"),
                    bus_width: sym(lib, c"nvmlDeviceGetMemoryBusWidth"),
                    max_clock: sym(lib, c"nvmlDeviceGetMaxClockInfo"),
                    max_pcie_gen: sym(lib, c"nvmlDeviceGetMaxPcieLinkGeneration"),
                    max_pcie_width: sym(lib, c"nvmlDeviceGetMaxPcieLinkWidth"),
                    temp_threshold: sym(lib, c"nvmlDeviceGetTemperatureThreshold"),
                    power_constraints: sym(lib, c"nvmlDeviceGetPowerManagementLimitConstraints"),
                    power_default: sym(lib, c"nvmlDeviceGetPowerManagementDefaultLimit"),
                    display_mode: sym(lib, c"nvmlDeviceGetDisplayMode"),
                    persistence: sym(lib, c"nvmlDeviceGetPersistenceMode"),
                    compute_mode: sym(lib, c"nvmlDeviceGetComputeMode"),
                    mig_mode: sym(lib, c"nvmlDeviceGetMigMode"),
                    ecc_mode: sym(lib, c"nvmlDeviceGetEccMode"),
                    nvlink_state: sym(lib, c"nvmlDeviceGetNvLinkState"),
                    util: sym(lib, c"nvmlDeviceGetUtilizationRates"),
                    memory_v2: sym(lib, c"nvmlDeviceGetMemoryInfo_v2"),
                    memory: sym(lib, c"nvmlDeviceGetMemoryInfo"),
                    temperature: sym(lib, c"nvmlDeviceGetTemperature"),
                    power: sym(lib, c"nvmlDeviceGetPowerUsage"),
                    power_limit: sym(lib, c"nvmlDeviceGetEnforcedPowerLimit"),
                    fan: sym(lib, c"nvmlDeviceGetFanSpeed"),
                    clock: sym(lib, c"nvmlDeviceGetClockInfo"),
                    pstate: sym(lib, c"nvmlDeviceGetPerformanceState"),
                    clock_events: sym(lib, c"nvmlDeviceGetCurrentClocksEventReasons")
                        .or_else(|| sym(lib, c"nvmlDeviceGetCurrentClocksThrottleReasons")),
                    encoder: sym(lib, c"nvmlDeviceGetEncoderUtilization"),
                    decoder: sym(lib, c"nvmlDeviceGetDecoderUtilization"),
                    pcie_gen: sym(lib, c"nvmlDeviceGetCurrPcieLinkGeneration"),
                    pcie_width: sym(lib, c"nvmlDeviceGetCurrPcieLinkWidth"),
                    fields: sym(lib, c"nvmlDeviceGetFieldValues"),
                    brand: sym(lib, c"nvmlDeviceGetBrand"),
                    numa_node: sym(lib, c"nvmlDeviceGetNumaNodeId"),
                    gsp_version: sym(lib, c"nvmlDeviceGetGspFirmwareVersion"),
                    inforom: sym(lib, c"nvmlDeviceGetInforomImageVersion"),
                    board_id: sym(lib, c"nvmlDeviceGetBoardId"),
                    boost_clock: sym(lib, c"nvmlDeviceGetMaxCustomerBoostClock"),
                    energy: sym(lib, c"nvmlDeviceGetTotalEnergyConsumption"),
                    compute_procs: sym(lib, c"nvmlDeviceGetComputeRunningProcesses_v3")
                        .or_else(|| sym(lib, c"nvmlDeviceGetComputeRunningProcesses_v2")),
                    graphics_procs: sym(lib, c"nvmlDeviceGetGraphicsRunningProcesses_v3")
                        .or_else(|| sym(lib, c"nvmlDeviceGetGraphicsRunningProcesses_v2")),
                    process_util: sym(lib, c"nvmlDeviceGetProcessUtilization"),
                }
            };
            let f = &fns;
            let system = NvidiaSystem {
                // SAFETY (all three): out-pointers to sized locals.
                driver_version: f
                    .driver_version
                    .and_then(|g| string(|p, n| unsafe { g(p, n) })),
                nvml_version: f
                    .nvml_version
                    .and_then(|g| string(|p, n| unsafe { g(p, n) })),
                cuda_driver_version: f.cuda_version.and_then(|g| {
                    let mut v: c_int = 0;
                    (unsafe { g(&mut v) } == SUCCESS && v > 0)
                        .then(|| format!("{}.{}", v / 1000, v % 1000 / 10))
                }),
            };
            Ok(Session {
                fns,
                system: Arc::new(system),
                devices: Vec::new(),
                enumerated: None,
                problems: Vec::new(),
            })
        }

        /// Re-enumerates when the device count changes (hot plug, reset).
        fn enumerate(&mut self) -> Result<(), String> {
            let mut n: c_uint = 0;
            // SAFETY: out-pointer to a local.
            let r = unsafe { (self.fns.count)(&mut n) };
            if r != SUCCESS {
                return Err(format!(
                    "nvmlDeviceGetCount failed: {} ({r})",
                    error_name(r)
                ));
            }
            let n = n.min(64);
            if self.enumerated == Some(n) {
                return Ok(());
            }
            self.enumerated = Some(n);
            self.devices.clear();
            self.problems.clear();
            if n == 0 {
                self.problems.push(
                    "NVML reports no GPU (this container or cgroup may not grant access to \
                     /dev/nvidia*)"
                        .into(),
                );
            }
            for i in 0..n {
                let mut d: Device = std::ptr::null_mut();
                // SAFETY: out-pointer to a local.
                let r = unsafe { (self.fns.handle)(i, &mut d) };
                if r != SUCCESS {
                    self.problems
                        .push(format!("NVML cannot open GPU {i}: {} ({r})", error_name(r)));
                    continue;
                }
                let info = self.identify(i, d);
                let caps = Caps::from(&self.fns, info.display_mode == Some(true));
                self.devices.push(DeviceState {
                    handle: d,
                    info: Arc::new(info),
                    caps,
                    config: NvidiaConfig::default(),
                    health: NvidiaHealth::default(),
                    samples: 0,
                    health_fields: HEALTH_FIELDS
                        .iter()
                        .map(|&id| FieldValue::new(id))
                        .collect(),
                    procs: vec![ProcessInfo::default(); 64],
                    utils: vec![ProcessUtilization::default(); 64],
                    util_seen_us: 0,
                });
            }
            Ok(())
        }

        /// Static identity and limits of device `i`.
        fn identify(&self, i: u32, d: Device) -> NvidiaStatic {
            let f = &self.fns;
            // SAFETY (whole function): `d` is a valid handle and every call
            // writes only to properly sized locals.
            unsafe {
                let u32_of = |g: Option<Fn0<c_uint>>| {
                    g.and_then(|g| {
                        let mut v: c_uint = 0;
                        (g(d, &mut v) == SUCCESS).then_some(v)
                    })
                };
                let with_arg = |g: Option<FnArg<c_uint>>, a: c_int| {
                    g.and_then(|g| {
                        let mut v: c_uint = 0;
                        (g(d, a, &mut v) == SUCCESS && v > 0).then_some(v)
                    })
                };
                let text = |g: Option<FnStr>| g.and_then(|g| string(|p, n| g(d, p, n)));
                let mut pci: PciInfo = std::mem::zeroed();
                let pci_ok = (f.pci)(d, &mut pci) == SUCCESS;
                let (pci_device_id, pci_subsystem_id) = if pci_ok {
                    (
                        Some(format!("0x{:08x}", pci.pci_device_id)),
                        Some(format!("0x{:08x}", pci.pci_sub_system_id)),
                    )
                } else {
                    (None, None)
                };
                let pci = if pci_ok {
                    format!("{:04x}:{:02x}:{:02x}.0", pci.domain, pci.bus, pci.device)
                } else {
                    format!("nvml{i}")
                };
                let mut c2c = [
                    FieldValue::new(FI_C2C_LINK_COUNT),
                    FieldValue::new(FI_C2C_LINK_GET_MAX_BW),
                ];
                let c2c_ok = f
                    .fields
                    .is_some_and(|g| g(d, 2, c2c.as_mut_ptr()) == SUCCESS);
                let c2c_value = |fv: &FieldValue| {
                    (c2c_ok && fv.nvml_return == SUCCESS)
                        .then(|| fv.as_u64())
                        .flatten()
                };
                let c2c_links = c2c_value(&c2c[0])
                    .and_then(|v| u32::try_from(v).ok())
                    .filter(|n| *n > 0);
                let (pl_min, pl_max) = f
                    .power_constraints
                    .and_then(|g| {
                        let (mut a, mut b): (c_uint, c_uint) = (0, 0);
                        (g(d, &mut a, &mut b) == SUCCESS).then(|| (mw_to_w(a), mw_to_w(b)))
                    })
                    .unzip();
                let mut nvlink = FieldValue::new(FI_NVLINK_LINK_COUNT);
                let nvlink_links = f.fields.and_then(|g| {
                    (g(d, 1, &mut nvlink) == SUCCESS && nvlink.nvml_return == SUCCESS)
                        .then(|| nvlink.as_u64())
                        .flatten()
                        .and_then(|v| u32::try_from(v).ok())
                });
                NvidiaStatic {
                    index: i,
                    name: text(Some(f.name)).unwrap_or_else(|| format!("NVIDIA GPU {i}")),
                    pci,
                    uuid: text(f.uuid),
                    serial: text(f.serial),
                    vbios_version: text(f.vbios),
                    board_part_number: text(f.part_number),
                    architecture: u32_of(f.architecture).and_then(architecture_name),
                    compute_capability: f.compute_capability.and_then(|g| {
                        let (mut a, mut b): (c_int, c_int) = (0, 0);
                        (g(d, &mut a, &mut b) == SUCCESS).then(|| format!("{a}.{b}"))
                    }),
                    cuda_cores: u32_of(f.cores),
                    memory_bus_width_bits: u32_of(f.bus_width),
                    graphics_clock_max_mhz: with_arg(f.max_clock, CLOCK_GRAPHICS),
                    sm_clock_max_mhz: with_arg(f.max_clock, CLOCK_SM),
                    memory_clock_max_mhz: with_arg(f.max_clock, CLOCK_MEM),
                    video_clock_max_mhz: with_arg(f.max_clock, CLOCK_VIDEO),
                    pcie_generation_max: u32_of(f.max_pcie_gen),
                    pcie_width_max: u32_of(f.max_pcie_width),
                    temperature_shutdown_celsius: with_arg(f.temp_threshold, 0),
                    temperature_slowdown_celsius: with_arg(f.temp_threshold, 1),
                    memory_temperature_max_celsius: with_arg(f.temp_threshold, 2),
                    temperature_max_operating_celsius: with_arg(f.temp_threshold, 3),
                    power_limit_min_w: pl_min,
                    power_limit_max_w: pl_max,
                    power_limit_default_w: u32_of(f.power_default).map(mw_to_w),
                    nvlink_links,
                    brand: u32_of(f.brand).and_then(brand_name),
                    numa_node: u32_of(f.numa_node),
                    gsp_firmware_version: f.gsp_version.and_then(|g| {
                        string(|p, n| {
                            if n >= 64 {
                                g(d, p)
                            } else {
                                ERROR_INSUFFICIENT_SIZE
                            }
                        })
                    }),
                    inforom_version: text(f.inforom),
                    board_id: u32_of(f.board_id),
                    pci_device_id,
                    pci_subsystem_id,
                    graphics_clock_boost_mhz: with_arg(f.boost_clock, CLOCK_GRAPHICS),
                    c2c_link_bandwidth_bytes_per_second: c2c_links
                        .and(c2c_value(&c2c[1]))
                        .map(|mbps| mbps.saturating_mul(1_000_000)),
                    c2c_links,
                    display_mode: f.display_mode.and_then(|g| {
                        let mut v: c_int = 0;
                        (g(d, &mut v) == SUCCESS).then_some(v != 0)
                    }),
                }
            }
        }
    }

    impl NvidiaSession for Session {
        fn sample(&mut self, detail: bool) -> Result<NvidiaSample, String> {
            self.enumerate()?;
            let f = &self.fns;
            let devices = self
                .devices
                .iter_mut()
                .map(|dev| {
                    if dev.samples % PERIODIC_REFRESH_SAMPLES == 0 {
                        dev.config = config(f, dev.handle, &dev.info);
                        dev.health = health(f, dev);
                    }
                    dev.samples = dev.samples.wrapping_add(1);
                    let mut out = live(f, dev);
                    if detail {
                        list_processes(f, dev, &mut out.processes);
                        nvlink_traffic(f, dev, &mut out);
                    }
                    out
                })
                .collect();
            Ok(NvidiaSample {
                system: self.system.clone(),
                devices,
                problems: self.problems.clone(),
            })
        }
    }

    fn config(f: &Fns, d: Device, info: &NvidiaStatic) -> NvidiaConfig {
        // SAFETY (whole function): valid handle, sized out-pointers.
        unsafe {
            let flag = |g: Option<Fn0<c_int>>| {
                g.and_then(|g| {
                    let mut v: c_int = 0;
                    (g(d, &mut v) == SUCCESS).then_some(v)
                })
            };
            let nvlink_links_active = f.nvlink_state.zip(info.nvlink_links).and_then(|(g, n)| {
                let mut active = 0;
                for link in 0..n.min(64) {
                    let mut v: c_int = 0;
                    match g(d, link, &mut v) {
                        SUCCESS => active += u32::from(v != 0),
                        ERROR_INVALID_ARGUMENT => break,
                        _ => return None,
                    }
                }
                Some(active)
            });
            NvidiaConfig {
                persistence_mode: flag(f.persistence).map(|v| v != 0),
                compute_mode: flag(f.compute_mode)
                    .and_then(|v| u32::try_from(v).ok())
                    .and_then(compute_mode_name),
                mig_mode: f.mig_mode.and_then(|g| {
                    let (mut cur, mut pend): (c_uint, c_uint) = (0, 0);
                    (g(d, &mut cur, &mut pend) == SUCCESS).then_some(cur != 0)
                }),
                ecc_mode: f.ecc_mode.and_then(|g| {
                    let (mut cur, mut pend): (c_int, c_int) = (0, 0);
                    (g(d, &mut cur, &mut pend) == SUCCESS).then_some(cur != 0)
                }),
                nvlink_links_active,
            }
        }
    }

    fn health(f: &Fns, dev: &mut DeviceState) -> NvidiaHealth {
        let mut h = NvidiaHealth::default();
        let Some(g) = f.fields else {
            return h;
        };
        if dev.health_fields.is_empty() {
            return h;
        }
        // Energy costs ~5 ms per call: periodic only.
        for fv in &mut dev.health_fields {
            *fv = FieldValue::new(fv.field_id);
        }
        // SAFETY: valid handle; the buffer holds `len` initialised fields.
        let r = unsafe {
            g(
                dev.handle,
                dev.health_fields.len() as c_int,
                dev.health_fields.as_mut_ptr(),
            )
        };
        if r != SUCCESS {
            return h;
        }
        if let Some(g) = f.energy {
            let mut mj = 0u64;
            // SAFETY: valid handle, sized out-pointer.
            h.energy_mj_total = (unsafe { g(dev.handle, &mut mj) } == SUCCESS).then_some(mj);
        }
        for fv in &dev.health_fields {
            let v = (fv.nvml_return == SUCCESS).then(|| fv.as_u64()).flatten();
            match fv.field_id {
                FI_C2C_LINK_GET_STATUS if dev.info.c2c_links.is_some() => {
                    h.c2c_active = v.map(|v| v != 0)
                }
                FI_PCIE_REPLAY_COUNTER => h.pcie_replays_total = v,
                FI_ECC_SBE_VOL_TOTAL => h.ecc_corrected_volatile_total = v,
                FI_ECC_DBE_VOL_TOTAL => h.ecc_uncorrected_volatile_total = v,
                FI_ECC_SBE_AGG_TOTAL => h.ecc_corrected_aggregate_total = v,
                FI_ECC_DBE_AGG_TOTAL => h.ecc_uncorrected_aggregate_total = v,
                FI_REMAPPED_COR => h.remapped_rows_corrected_total = v,
                FI_REMAPPED_UNC => h.remapped_rows_uncorrected_total = v,
                FI_REMAPPED_PENDING => h.remapping_pending = v.map(|v| v != 0),
                FI_REMAPPED_FAILURE => h.remapping_failed = v.map(|v| v != 0),
                _ => {}
            }
        }
        // ECC counters are meaningless while ECC is off.
        if dev.config.ecc_mode != Some(true) {
            h.ecc_corrected_volatile_total = None;
            h.ecc_uncorrected_volatile_total = None;
            h.ecc_corrected_aggregate_total = None;
            h.ecc_uncorrected_aggregate_total = None;
        }
        // Fields the device does not support are not asked again.
        dev.health_fields.retain(|fv| {
            !matches!(
                fv.nvml_return,
                ERROR_NOT_SUPPORTED | ERROR_FUNCTION_NOT_FOUND
            )
        });
        h
    }

    /// NVLink data counters summed over all links (one call, ~2 ms).
    fn nvlink_traffic(f: &Fns, dev: &mut DeviceState, out: &mut NvidiaDevice) {
        let (Some(g), Some(n)) = (f.fields, dev.info.nvlink_links) else {
            return;
        };
        if n == 0 || !dev.caps.nvlink_traffic {
            return;
        }
        let mut fv = [
            FieldValue::new(FI_NVLINK_THROUGHPUT_DATA_TX),
            FieldValue::new(FI_NVLINK_THROUGHPUT_DATA_RX),
        ];
        for v in &mut fv {
            v.scope_id = SCOPE_ALL_LINKS;
        }
        // SAFETY: valid handle; two initialised fields.
        let r = unsafe { g(dev.handle, 2, fv.as_mut_ptr()) };
        let r = if r == SUCCESS { fv[0].nvml_return } else { r };
        if keep(&mut dev.caps.nvlink_traffic, r, ()).is_some() {
            out.nvlink_tx_kib_total = fv[0].as_u64();
            out.nvlink_rx_kib_total = (fv[1].nvml_return == SUCCESS)
                .then(|| fv[1].as_u64())
                .flatten();
        }
    }

    /// Compute and graphics processes with their SM utilisation; a PID in
    /// both lists is reported once, as compute, with memory summed.
    fn list_processes(f: &Fns, dev: &mut DeviceState, out: &mut Vec<NvidiaProcess>) {
        let d = dev.handle;
        let c = &mut dev.caps;
        // SAFETY: valid handle; functions are called only when resolved and
        // write into the device's own buffers of the stated length.
        unsafe {
            if c.compute_procs
                && let Some(g) = f.compute_procs
            {
                processes(g, d, &mut dev.procs, &mut c.compute_procs, false, out);
            }
            if c.graphics_procs
                && let Some(g) = f.graphics_procs
            {
                let first_graphics = out.len();
                processes(g, d, &mut dev.procs, &mut c.graphics_procs, true, out);
                let mut i = first_graphics;
                while i < out.len() {
                    let p = out[i];
                    if let Some(c) = out[..first_graphics].iter_mut().find(|c| c.pid == p.pid) {
                        c.used_memory_bytes = match (c.used_memory_bytes, p.used_memory_bytes) {
                            (Some(a), Some(b)) => Some(a.saturating_add(b)),
                            (a, b) => a.or(b),
                        };
                        out.swap_remove(i);
                    } else {
                        i += 1;
                    }
                }
            }
            if out.is_empty() || !c.process_util {
                return;
            }
            let Some(g) = f.process_util else {
                return;
            };
            for _ in 0..2 {
                let mut n = dev.utils.len() as c_uint;
                let r = g(d, dev.utils.as_mut_ptr(), &mut n, dev.util_seen_us);
                if r == ERROR_INSUFFICIENT_SIZE && (n as usize) > dev.utils.len() {
                    let want = (n as usize + 16).min(MAX_PROCESSES);
                    if want <= dev.utils.len() {
                        return;
                    }
                    dev.utils.resize(want, ProcessUtilization::default());
                    continue;
                }
                // NOT_FOUND: no samples since the last call, so nothing ran.
                if r == ERROR_NOT_FOUND {
                    out.iter_mut().for_each(|p| p.sm_usage_percent = Some(0.0));
                    return;
                }
                if keep(&mut c.process_util, r, ()).is_none() {
                    return;
                }
                let samples = &dev.utils[..(n as usize).min(dev.utils.len())];
                for p in out.iter_mut() {
                    let (sum, k) = samples
                        .iter()
                        .filter(|s| s.pid == p.pid)
                        .fold((0u64, 0u64), |(s, k), x| {
                            (s + u64::from(x.sm.min(100)), k + 1)
                        });
                    p.sm_usage_percent = Some(if k > 0 { sum as f64 / k as f64 } else { 0.0 });
                }
                if let Some(t) = samples.iter().map(|s| s.timestamp_us).max() {
                    dev.util_seen_us = dev.util_seen_us.max(t);
                }
                return;
            }
        }
    }

    /// Lists processes through `g`, growing the reused buffer when the
    /// driver reports more than it holds (bounded by `MAX_PROCESSES`).
    unsafe fn processes(
        g: FnProcs,
        d: Device,
        buf: &mut Vec<ProcessInfo>,
        cap: &mut bool,
        graphics: bool,
        out: &mut Vec<NvidiaProcess>,
    ) {
        for _ in 0..2 {
            let mut n = buf.len() as c_uint;
            // SAFETY: `buf` holds `n` writable entries.
            let r = unsafe { g(d, &mut n, buf.as_mut_ptr()) };
            if r == ERROR_INSUFFICIENT_SIZE && (n as usize) > buf.len() {
                let want = (n as usize + 16).min(MAX_PROCESSES);
                if want <= buf.len() {
                    return;
                }
                buf.resize(want, ProcessInfo::default());
                continue;
            }
            if keep(cap, r, ()).is_some() {
                out.extend(
                    buf[..(n as usize).min(buf.len())]
                        .iter()
                        .map(|p| NvidiaProcess {
                            pid: p.pid,
                            // NVML_VALUE_NOT_AVAILABLE = u64::MAX
                            used_memory_bytes: (p.used_gpu_memory != u64::MAX)
                                .then_some(p.used_gpu_memory),
                            graphics,
                            sm_usage_percent: None,
                        }),
                );
            }
            return;
        }
    }

    fn live(f: &Fns, dev: &mut DeviceState) -> NvidiaDevice {
        let d = dev.handle;
        let c = &mut dev.caps;
        let mut out = NvidiaDevice {
            info: dev.info.clone(),
            config: dev.config.clone(),
            health: dev.health.clone(),
            ..Default::default()
        };
        // SAFETY (whole function): `d` is a valid handle; each call writes
        // only into properly sized locals or the device's own buffers; an
        // optional function is called only when resolved (`Some`).
        unsafe {
            if c.util
                && let Some(g) = f.util
            {
                let mut u = Utilization::default();
                let r = g(d, &mut u);
                if r != SUCCESS && r != ERROR_NOT_SUPPORTED {
                    out.error = Some(format!("utilisation query failed: {} ({r})", error_name(r)));
                }
                if let Some(u) = keep(&mut c.util, r, u) {
                    out.gpu_usage_percent = Some(f64::from(u.gpu));
                    out.memory_busy_percent = Some(f64::from(u.memory));
                }
            }
            let mut have_mem = false;
            if c.memory_v2
                && let Some(g) = f.memory_v2
            {
                let mut m = MemoryV2 {
                    version: MEMORY_V2,
                    ..Default::default()
                };
                let r = g(d, &mut m);
                // An old driver rejects the struct version: use v1.
                if r == ERROR_ARGUMENT_VERSION_MISMATCH {
                    c.memory_v2 = false;
                }
                if let Some(m) = keep(&mut c.memory_v2, r, m) {
                    have_mem = true;
                    out.vram_total_bytes = Some(m.total);
                    out.vram_used_bytes = Some(m.used);
                    out.vram_reserved_bytes = Some(m.reserved);
                }
            }
            if !have_mem
                && c.memory
                && let Some(g) = f.memory
            {
                let mut m = Memory::default();
                let r = g(d, &mut m);
                if let Some(m) = keep(&mut c.memory, r, m) {
                    out.vram_total_bytes = Some(m.total);
                    out.vram_used_bytes = Some(m.used);
                }
            }
            let mut v: c_uint = 0;
            if c.temperature
                && let Some(g) = f.temperature
            {
                out.temperature_celsius =
                    keep(&mut c.temperature, g(d, 0, &mut v), v).map(f64::from);
            }
            if c.power
                && let Some(g) = f.power
            {
                out.power_w = keep(&mut c.power, g(d, &mut v), v).map(mw_to_w);
            }
            if c.power_limit
                && let Some(g) = f.power_limit
            {
                out.power_limit_w = keep(&mut c.power_limit, g(d, &mut v), v).map(mw_to_w);
            }
            if c.fan
                && let Some(g) = f.fan
            {
                out.fan_speed_percent = keep(&mut c.fan, g(d, &mut v), v).map(f64::from);
            }
            if let Some(g) = f.clock {
                let mut clock = |i: usize, kind: c_int| {
                    if !c.clocks[i] {
                        return None;
                    }
                    let mut v: c_uint = 0;
                    keep(&mut c.clocks[i], g(d, kind, &mut v), v)
                };
                out.graphics_clock_mhz = clock(0, CLOCK_GRAPHICS);
                out.sm_clock_mhz = clock(1, CLOCK_SM);
                out.memory_clock_mhz = clock(2, CLOCK_MEM);
                out.video_clock_mhz = clock(3, CLOCK_VIDEO);
            }
            if c.pstate
                && let Some(g) = f.pstate
            {
                let mut p: c_int = 0;
                out.performance_state = keep(&mut c.pstate, g(d, &mut p), p)
                    .and_then(|p| u32::try_from(p).ok())
                    .filter(|p| *p <= 15);
            }
            if c.clock_events
                && let Some(g) = f.clock_events
            {
                let mut bits = 0u64;
                out.clock_event_reasons = keep(&mut c.clock_events, g(d, &mut bits), bits);
            }
            let mut period: c_uint = 0;
            if c.encoder
                && let Some(g) = f.encoder
            {
                out.encoder_usage_percent =
                    keep(&mut c.encoder, g(d, &mut v, &mut period), v).map(f64::from);
            }
            if c.decoder
                && let Some(g) = f.decoder
            {
                out.decoder_usage_percent =
                    keep(&mut c.decoder, g(d, &mut v, &mut period), v).map(f64::from);
            }
            if c.pcie_gen
                && let Some(g) = f.pcie_gen
            {
                out.pcie_generation = keep(&mut c.pcie_gen, g(d, &mut v), v);
            }
            if c.pcie_width
                && let Some(g) = f.pcie_width
            {
                out.pcie_width = keep(&mut c.pcie_width, g(d, &mut v), v);
            }
            if c.memory_temperature
                && let Some(g) = f.fields
            {
                let mut fv = FieldValue::new(FI_MEMORY_TEMP);
                let r = g(d, 1, &mut fv);
                let r = if r == SUCCESS { fv.nvml_return } else { r };
                out.memory_temperature_celsius = keep(&mut c.memory_temperature, r, ())
                    .and_then(|()| fv.as_u64())
                    .filter(|t| *t > 0)
                    .map(|t| t as f64);
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn struct_layouts_match_nvml_h() {
            assert_eq!(std::mem::size_of::<FieldValue>(), 40);
            assert_eq!(std::mem::size_of::<MemoryV2>(), 40);
            assert_eq!(MEMORY_V2, 40 | (2 << 24));
            assert_eq!(std::mem::size_of::<ProcessInfo>(), 24);
            assert_eq!(std::mem::size_of::<PciInfo>(), 68);
            assert_eq!(std::mem::size_of::<ProcessUtilization>(), 32);
        }

        #[test]
        fn field_values_decode_by_type() {
            let fv = |value_type, value| FieldValue {
                value_type,
                value,
                ..FieldValue::new(1)
            };
            assert_eq!(fv(1, 0xdead_beef_0000_0007).as_u64(), Some(7));
            assert_eq!(fv(3, u64::MAX - 1).as_u64(), Some(u64::MAX - 1));
            assert_eq!(fv(0, 42.0f64.to_bits()).as_u64(), Some(42));
            assert_eq!(fv(4, (-1i64) as u64).as_u64(), None);
            assert_eq!(fv(9, 1).as_u64(), None);
        }

        #[test]
        fn unsupported_queries_are_dropped_for_good() {
            let mut cap = true;
            assert_eq!(keep(&mut cap, 4, 1), None, "no permission is transient");
            assert!(cap);
            assert_eq!(keep(&mut cap, SUCCESS, 1), Some(1));
            assert_eq!(keep(&mut cap, ERROR_NOT_SUPPORTED, 1), None);
            assert!(!cap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_event_reasons_name_every_bit() {
        assert!(clock_event_reasons(0).is_empty());
        assert_eq!(
            clock_event_reasons(0x4 | 0x40),
            ["sw_power_cap", "hw_thermal_slowdown"]
        );
        assert_eq!(clock_event_reasons(1 << 40), ["other"]);
    }

    #[test]
    fn enum_names() {
        assert_eq!(architecture_name(9), Some("Hopper"));
        assert_eq!(architecture_name(0xffff_ffff), None);
        assert_eq!(compute_mode_name(3), Some("exclusive_process"));
        assert_eq!(compute_mode_name(7), None);
    }
}
