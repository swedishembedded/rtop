// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Collectors under injected kernel faults: they degrade to explicit
//! unavailability, never to zeros, panics or hangs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rtop_collectors::filesystems::FilesystemCollector;
use rtop_collectors::pressure::PressureCollector;
use rtop_collectors::processes::ProcessCollector;
use rtop_collectors::storage::StorageCollector;
use rtop_collectors::{Collector, Env, SampleContext, Subscriptions};
use rtop_core::{EntityId, EntityKind, Metric, SystemGraph, Timestamp, Unavailable};
use rtop_linux::KernelError;
use rtop_linux::fs::FsUsage;
use rtop_linux::memfs::{Gate, MemoryKernelFs};

fn ctx(n: u64) -> SampleContext {
    SampleContext {
        now: Timestamp::from_millis(n * 1000),
        sequence: n,
    }
}

fn stat_line(pid: u32, start: u64, ticks: u64) -> String {
    format!(
        "{pid} (p{pid}) S 1 {pid} {pid} 0 -1 0 0 0 0 0 {ticks} 0 0 0 20 0 1 0 {start} 1000 10 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0\n"
    )
}

#[test]
fn hung_remote_filesystem_times_out_without_blocking_the_sample() {
    let fs = Arc::new(MemoryKernelFs::new());
    fs.file(
        "/proc/self/mountinfo",
        "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 0:50 / /mnt/nfs rw - nfs4 server:/x rw\n",
    );
    fs.set_usage(
        "/",
        Ok(FsUsage {
            total_bytes: 100,
            free_bytes: 50,
            available_bytes: 40,
            total_inodes: 10,
            free_inodes: 5,
        }),
    );
    let gate = Gate::new();
    fs.block_statvfs("/mnt/nfs", gate.clone());
    let mut c = FilesystemCollector::new(Env::simulated(fs, Arc::new(Subscriptions::default())));
    for n in 1..=3 {
        let t = Instant::now();
        let s = c.sample(&ctx(n)).unwrap();
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "sample {n} took {:?}",
            t.elapsed()
        );
        let mut g = SystemGraph::default();
        g.apply(s.patch);
        let nfs = g
            .node(&EntityId::new(EntityKind::Mount, "/mnt/nfs"))
            .unwrap();
        assert_eq!(
            nfs.metric("capacity_bytes"),
            Some(&Metric::Unavailable(Unavailable::TimedOut))
        );
        assert!(
            g.node(&EntityId::new(EntityKind::Mount, "/"))
                .unwrap()
                .f64("used_percent")
                .is_some()
        );
        assert!(s.degraded.is_some());
    }
    gate.open();
}

#[test]
fn permission_denied_io_and_vanishing_processes_are_explicit() {
    let fs = Arc::new(MemoryKernelFs::new());
    fs.file("/proc/uptime", "100.0 50.0\n");
    for (pid, start) in [(10u32, 5u64), (11, 6), (12, 7)] {
        fs.file(&format!("/proc/{pid}/stat"), stat_line(pid, start, 100));
        fs.file(
            &format!("/proc/{pid}/status"),
            "Uid:\t0\t0\t0\t0\nVmRSS:\t40 kB\n",
        );
    }
    fs.file(
        "/proc/10/io",
        "rchar: 1\nwchar: 1\nsyscr: 1\nsyscw: 1\nread_bytes: 0\nwrite_bytes: 0\n",
    );
    fs.fail("/proc/11/io", KernelError::PermissionDenied);
    // 12 disappears between listing and reading.
    fs.fail("/proc/12/stat", KernelError::NotFound);
    let mut c = ProcessCollector::new(Env::simulated(fs, Subscriptions::all()));
    let s = c.sample(&ctx(1)).unwrap();
    let mut g = SystemGraph::default();
    g.apply(s.patch);
    assert_eq!(g.nodes_of(EntityKind::Process).count(), 2);
    let p11 = g
        .nodes_of(EntityKind::Process)
        .find(|n| n.attr("pid").and_then(|a| a.as_u64()) == Some(11))
        .unwrap();
    assert_eq!(
        p11.metric("io_read_bytes_per_second"),
        Some(&Metric::Unavailable(Unavailable::PermissionDenied))
    );
    assert!(s.degraded.unwrap().contains("I/O accounting denied"));
}

#[test]
fn pid_reuse_gets_a_fresh_baseline_not_a_spike() {
    let fs = Arc::new(MemoryKernelFs::new());
    fs.file("/proc/uptime", "100.0 50.0\n");
    fs.file("/proc/20/stat", stat_line(20, 5, 1_000_000));
    fs.file("/proc/20/status", "Uid:\t0\t0\t0\t0\n");
    let mut c = ProcessCollector::new(Env::simulated(
        fs.clone(),
        Arc::new(Subscriptions::default()),
    ));
    c.sample(&ctx(1)).unwrap();
    // Same PID, new process (different start time), small CPU counter.
    fs.file("/proc/20/stat", stat_line(20, 999, 3));
    let s = c.sample(&ctx(2)).unwrap();
    let n = s
        .patch
        .nodes
        .iter()
        .find(|n| n.kind == EntityKind::Process)
        .unwrap();
    assert!(n.id.as_str().ends_with(":999"));
    let cpu = n
        .metrics
        .iter()
        .find(|(k, _)| *k == "cpu_usage_percent")
        .unwrap()
        .1;
    assert_eq!(cpu, Metric::Unavailable(Unavailable::Pending));
}

#[test]
fn missing_psi_is_unsupported_and_device_counter_reset_is_not_a_spike() {
    let fs = Arc::new(MemoryKernelFs::new());
    let env = Env::simulated(fs.clone(), Arc::new(Subscriptions::default()));
    let err = PressureCollector::new(env.clone())
        .sample(&ctx(1))
        .unwrap_err();
    assert_eq!(err.reason, Unavailable::Unsupported);

    let line = |sectors: u64| format!("8 0 sda 10 0 {sectors} 5 10 0 {sectors} 5 0 10 10\n");
    fs.file("/proc/diskstats", line(1_000_000));
    let mut c = StorageCollector::new(env);
    c.sample(&ctx(1)).unwrap();
    fs.file("/proc/diskstats", line(10)); // driver reload / device reset
    let s = c.sample(&ctx(2)).unwrap();
    let dev = s
        .patch
        .nodes
        .iter()
        .find(|n| n.kind == EntityKind::BlockDevice)
        .unwrap();
    let rb = dev
        .metrics
        .iter()
        .find(|(k, _)| *k == "read_bytes_per_second")
        .unwrap()
        .1;
    assert_eq!(rb, Metric::Unavailable(Unavailable::CounterReset));
}
