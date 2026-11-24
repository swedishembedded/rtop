// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0
//
// Swedish Embedded AB implements solutions for low-overhead, failure-tolerant
// Linux telemetry collection for its clients. If your team needs expertise
// in Linux performance monitoring then you can procure our services by
// sending an email to info@swedishembedded.com.

//! rtop's telemetry collectors.
//!
//! Each [`Collector`] owns one slice of the system graph, keeps its own
//! counter baselines and, when sampled, returns a [`GraphPatch`] describing
//! everything it observed. Collectors never touch application state and
//! never talk to each other; relationships between their entities are
//! expressed as edges by stable [`rtop_core::EntityId`]s and reconciled by
//! the graph.
//!
//! All kernel access goes through [`Env`], so every collector runs
//! unchanged against the live machine, a captured fixture, or the
//! [`demo`] machine used for tests and screenshots.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rtop_core::{CollectorId, GraphPatch, ProcessIdentity, Timestamp, Unavailable};
use rtop_linux::sockdiag::SockDiag;
use rtop_linux::{KernelError, KernelFs};

pub mod cgroups;
pub mod cpu;
pub mod demo;
pub mod docker;
pub mod filesystems;
pub mod firewall;
pub mod gpu;
pub mod host;
pub mod inspect;
pub mod memory;
pub mod metrics;
pub mod monitor;
pub mod network;
pub mod pressure;
pub mod processes;
pub mod security;
pub mod sensors;
pub mod sockets;
pub mod storage;

/// How a collector must be isolated from the rest of the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// Cheap procfs reads.
    Fast,
    /// Proportional to system size (process tables, socket tables).
    Expensive,
    /// May block in the kernel indefinitely (statvfs on NFS, sensors,
    /// vendor libraries, sockets to daemons).
    MayBlock,
}

#[derive(Debug, Clone, Copy)]
pub struct Descriptor {
    pub id: CollectorId,
    pub interval: Duration,
    /// After this long without an answer the collector is marked blocked.
    pub timeout: Duration,
    pub isolation: Isolation,
    /// Kept running in emergency mode.
    pub essential: bool,
    /// Nice value for the worker thread running it.
    pub nice: i32,
}

/// Errors that make a whole sample unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectorError {
    pub reason: Unavailable,
    pub detail: String,
}

impl CollectorError {
    pub fn new(reason: Unavailable, detail: impl Into<String>) -> Self {
        CollectorError {
            reason,
            detail: detail.into(),
        }
    }

    pub fn from_kernel(what: &str, e: &KernelError) -> Self {
        CollectorError::new(unavailable_from(e), format!("{what}: {e}"))
    }
}

/// Maps a kernel error to the availability reason shown to the user.
pub fn unavailable_from(e: &KernelError) -> Unavailable {
    match e {
        KernelError::NotFound => Unavailable::Unsupported,
        KernelError::PermissionDenied => Unavailable::PermissionDenied,
        KernelError::Unsupported => Unavailable::Unsupported,
        KernelError::TooLarge { .. } | KernelError::Invalid(_) => Unavailable::InvalidData,
        KernelError::Os(_) => Unavailable::Unsupported,
    }
}

/// Result of one sample.
#[derive(Debug)]
pub struct Sample {
    pub patch: GraphPatch,
    /// Partial failure description; `None` when everything was read.
    pub degraded: Option<String>,
}

/// Inputs every sample receives.
#[derive(Debug, Clone, Copy)]
pub struct SampleContext {
    pub now: Timestamp,
    pub sequence: u64,
}

impl SampleContext {
    pub fn patch(&self, id: CollectorId) -> GraphPatch {
        GraphPatch::new(id, self.sequence, self.now)
    }
}

pub trait Collector: Send {
    fn descriptor(&self) -> Descriptor;
    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError>;
}

/// What the user is currently looking at; expensive collection follows it.
///
/// Shared between the UI thread (writer) and workers (readers); every field
/// is independently atomic so no lock is held across a kernel read.
#[derive(Debug, Default)]
pub struct Subscriptions {
    /// Per-process `/proc/PID/io` (sorting by I/O, process view visible).
    pub process_io: AtomicBool,
    /// Scan every process's descriptors to attribute sockets (network view,
    /// process inspector, `--stats`).
    pub socket_ownership: AtomicBool,
    /// Per-process DRM client accounting (GPU view).
    pub gpu_clients: AtomicBool,
    /// The process whose details are being inspected, if any.
    pub inspected: Mutex<Option<ProcessIdentity>>,
}

impl Subscriptions {
    /// Everything on: what `rtop --stats` collects.
    pub fn all() -> Arc<Self> {
        let s = Subscriptions::default();
        s.process_io.store(true, Ordering::Relaxed);
        s.socket_ownership.store(true, Ordering::Relaxed);
        s.gpu_clients.store(true, Ordering::Relaxed);
        Arc::new(s)
    }

    pub fn get(flag: &AtomicBool) -> bool {
        flag.load(Ordering::Relaxed)
    }
}

/// Access to the observed system.
#[derive(Debug, Clone)]
pub struct Env {
    pub fs: Arc<dyn KernelFs>,
    pub diag: Arc<dyn SockDiag>,
    pub firewall: Arc<dyn rtop_linux::nftables::FirewallSource>,
    pub subscriptions: Arc<Subscriptions>,
    /// `USER_HZ` of the observed system.
    pub clock_ticks: u64,
    pub page_size: u64,
    /// Path of the Docker Engine socket, if any should be tried.
    pub docker_socket: Option<String>,
    /// PID of rtop itself in the observed system (live only).
    pub self_pid: Option<u32>,
}

impl Env {
    /// The live machine rtop runs on.
    pub fn live(subscriptions: Arc<Subscriptions>) -> Self {
        let fs: Arc<dyn KernelFs> = Arc::new(rtop_linux::RealKernelFs::live());
        Env {
            diag: Arc::new(rtop_linux::sockdiag::NetlinkDiag::new(fs.clone())),
            firewall: Arc::new(rtop_linux::nftables::Nftables::default()),
            fs,
            subscriptions,
            clock_ticks: rtop_linux::sys::clock_ticks(),
            page_size: rtop_linux::sys::page_size(),
            docker_socket: Some(
                std::env::var("DOCKER_HOST")
                    .ok()
                    .and_then(|h| h.strip_prefix("unix://").map(str::to_owned))
                    .unwrap_or_else(|| "/var/run/docker.sock".into()),
            ),
            self_pid: Some(std::process::id()),
        }
    }

    /// A non-live system described entirely by `fs` (fixtures, demo).
    pub fn simulated(fs: Arc<dyn KernelFs>, subscriptions: Arc<Subscriptions>) -> Self {
        Env {
            diag: Arc::new(rtop_linux::sockdiag::ProcNetDiag::new(fs.clone())),
            firewall: Arc::new(rtop_linux::nftables::NoFirewall),
            fs,
            subscriptions,
            clock_ticks: 100,
            page_size: 4096,
            docker_socket: None,
            self_pid: None,
        }
    }
}

/// Every collector, in the order they appear in exports.
pub fn all(env: &Env) -> Vec<Box<dyn Collector>> {
    vec![
        Box::new(host::HostCollector::new(env.clone())),
        Box::new(cpu::CpuCollector::new(env.clone())),
        Box::new(memory::MemoryCollector::new(env.clone())),
        Box::new(pressure::PressureCollector::new(env.clone())),
        Box::new(processes::ProcessCollector::new(env.clone())),
        Box::new(storage::StorageCollector::new(env.clone())),
        Box::new(filesystems::FilesystemCollector::new(env.clone())),
        Box::new(network::NetworkCollector::new(env.clone())),
        Box::new(sockets::SocketCollector::new(env.clone())),
        Box::new(cgroups::CgroupCollector::new(env.clone())),
        Box::new(docker::DockerCollector::new(env.clone())),
        Box::new(sensors::SensorCollector::new(env.clone())),
        Box::new(gpu::GpuCollector::new(env.clone())),
        Box::new(security::SecurityCollector::new(env.clone())),
        Box::new(firewall::FirewallCollector::new(env.clone())),
        Box::new(monitor::MonitorCollector::new(env.clone())),
    ]
}

/// Read limit for ordinary kernel files.
pub(crate) const SMALL: usize = 256 * 1024;
/// Read limit for large tables (mountinfo, process status).
pub(crate) const LARGE: usize = 16 << 20;
