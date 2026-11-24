// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Security-relevant activity: kernel log denials, OOM kills, firewall log
//! matches and connection-tracking failures.
//!
//! Strictly observational: rtop reads `/dev/kmsg` like `dmesg` does and
//! never reconfigures audit, logging or the firewall. Reading the kernel log
//! needs `CAP_SYSLOG` when `kernel.dmesg_restrict=1`; without it the kernel
//! log source is reported unavailable and the remaining sources continue.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker};
use rtop_core::{
    CollectorId, EntityId, EntityKind, EventCategory, Metric, NodeUpdate, ProcessIdentity,
    Severity, TelemetryEvent, Timestamp, Unavailable,
};
use rtop_linux::fs::KernelFsExt;
use rtop_linux::parse::{kmsg, pid, system};

use crate::{Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("security");
const MAX_RECORDS_PER_SAMPLE: usize = 4096;
const WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug)]
enum Source {
    Live(Option<OwnedFd>),
    /// Simulated systems: records in a file, consumed once.
    Snapshot {
        consumed: bool,
    },
}

#[derive(Debug)]
pub struct SecurityCollector {
    env: Env,
    source: Source,
    kmsg_error: Option<Unavailable>,
    boot_time: Option<u64>,
    recent: VecDeque<Timestamp>,
    totals: [u64; 3],
    rates: RateTracker<&'static str>,
}

impl SecurityCollector {
    pub fn new(env: Env) -> Self {
        let source = if env.fs.is_live() {
            Source::Live(None)
        } else {
            Source::Snapshot { consumed: false }
        };
        SecurityCollector {
            env,
            source,
            kmsg_error: None,
            boot_time: None,
            recent: VecDeque::new(),
            totals: [0; 3],
            rates: RateTracker::new(),
        }
    }

    fn read_records(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        match &mut self.source {
            Source::Snapshot { consumed } => {
                if !*consumed {
                    *consumed = true;
                    match self.env.fs.read_string("/dev/kmsg", 8 << 20) {
                        Ok(t) => {
                            out.extend(t.lines().filter(|l| !l.starts_with(' ')).map(str::to_owned))
                        }
                        Err(e) => self.kmsg_error = Some(crate::unavailable_from(&e)),
                    }
                }
            }
            Source::Live(fd) => {
                if fd.is_none() && self.kmsg_error.is_none() {
                    // SAFETY: open with a constant path.
                    let raw = unsafe {
                        libc::open(
                            c"/dev/kmsg".as_ptr(),
                            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
                        )
                    };
                    if raw < 0 {
                        let e = rtop_linux::KernelError::from_io(&std::io::Error::last_os_error());
                        self.kmsg_error = Some(crate::unavailable_from(&e));
                    } else {
                        // SAFETY: fresh descriptor we own.
                        *fd = Some(unsafe { OwnedFd::from_raw_fd(raw) });
                    }
                }
                if let Some(fd) = fd {
                    let mut buf = vec![0u8; 8192];
                    while out.len() < MAX_RECORDS_PER_SAMPLE {
                        // SAFETY: valid fd and buffer.
                        let n = unsafe {
                            libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
                        };
                        if n > 0 {
                            out.push(String::from_utf8_lossy(&buf[..n as usize]).into_owned());
                            continue;
                        }
                        // EPIPE: records were overwritten before we read them;
                        // the next read continues with the oldest available.
                        if n < 0
                            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPIPE)
                        {
                            continue;
                        }
                        break; // EAGAIN (caught up) or error
                    }
                }
            }
        }
        out
    }
}

impl Collector for SecurityCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(3),
            isolation: Isolation::Fast,
            essential: false,
            nice: 5,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let fs = self.env.fs.clone();
        if self.boot_time.is_none() {
            self.boot_time = fs
                .read_string("/proc/stat", SMALL)
                .ok()
                .and_then(|s| system::proc_stat(&s).boot_time);
        }
        let mut patch = ctx.patch(ID);
        let mut buf = Vec::new();
        for raw in self.read_records() {
            let Some(rec) = kmsg::record(&raw) else {
                continue;
            };
            let Some(kind) = kmsg::classify(&rec.message) else {
                continue;
            };
            let severity = match kind {
                kmsg::KernelEventKind::OomKill | kmsg::KernelEventKind::HardwareError => {
                    Severity::Critical
                }
                kmsg::KernelEventKind::Segfault
                | kmsg::KernelEventKind::FirewallLog
                | kmsg::KernelEventKind::AuditOther => Severity::Info,
                _ => Severity::Warning,
            };
            let category = if kind.is_security() {
                EventCategory::Security
            } else if kind == kmsg::KernelEventKind::OomKill {
                EventCategory::Oom
            } else {
                EventCategory::Kernel
            };
            let mut ev = TelemetryEvent::new(
                ctx.now,
                severity,
                category,
                ID,
                &format!("{}: {}", kind.describe(), rec.message),
            );
            ev.wall_time = self
                .boot_time
                .map(|b| b as f64 + rec.timestamp_us as f64 / 1e6);
            if let Some(p) = kmsg::subject_pid(&rec.message) {
                if fs.read(&format!("/proc/{p}/stat"), &mut buf, SMALL).is_ok() {
                    if let Some(st) = pid::stat(&buf) {
                        ev.subjects.push(
                            ProcessIdentity {
                                pid: p,
                                start_time_ticks: st.starttime,
                            }
                            .entity_id(),
                        );
                    }
                }
            }
            self.totals[match category {
                EventCategory::Security => 0,
                EventCategory::Oom => 1,
                _ => 2,
            }] += 1;
            if category == EventCategory::Security {
                self.recent.push_back(ctx.now);
            }
            patch.events.push(ev);
        }
        while self
            .recent
            .front()
            .is_some_and(|t| ctx.now.saturating_since(*t) > WINDOW)
        {
            self.recent.pop_front();
        }

        let mut h = NodeUpdate::new(EntityKind::Host, EntityId::host());
        match self.kmsg_error {
            Some(reason) => {
                for k in [
                    "security_events_total",
                    "security_events_per_minute",
                    "kernel_oom_events_total",
                ] {
                    h.push_metric(k, Metric::unavailable(reason));
                }
            }
            None => {
                h.push_metric("security_events_total", Metric::u64(self.totals[0]));
                h.push_metric(
                    "security_events_per_minute",
                    Metric::u64(self.recent.len() as u64),
                );
                h.push_metric("kernel_oom_events_total", Metric::u64(self.totals[1]));
            }
        }
        self.rates.begin();
        match (
            fs.read_u64("/proc/sys/net/netfilter/nf_conntrack_count"),
            fs.read_u64("/proc/sys/net/netfilter/nf_conntrack_max"),
        ) {
            (Ok(c), Ok(m)) => {
                h.push_metric("conntrack_entries", Metric::u64(c));
                h.push_metric("conntrack_max", Metric::u64(m));
            }
            (Err(e), _) | (_, Err(e)) => h.push_metric(
                "conntrack_entries",
                Metric::unavailable(crate::unavailable_from(&e)),
            ),
        }
        if let Ok(t) = fs.read_string("/proc/net/stat/nf_conntrack", SMALL) {
            let mut lines = t.lines();
            let header: Vec<&str> = lines.next().unwrap_or("").split_whitespace().collect();
            let mut sums = vec![0u64; header.len()];
            for l in lines {
                for (i, v) in l.split_whitespace().enumerate().take(sums.len()) {
                    sums[i] = sums[i].saturating_add(u64::from_str_radix(v, 16).unwrap_or(0));
                }
            }
            for (field, key) in [
                ("drop", "conntrack_drops_per_second"),
                ("early_drop", "conntrack_early_drops_per_second"),
                ("insert_failed", "conntrack_insert_failed_per_second"),
                ("invalid", "conntrack_invalid_per_second"),
            ] {
                if let Some(i) = header.iter().position(|h| *h == field) {
                    let m = self
                        .rates
                        .metric(key, sums[i], ctx.now, CounterWidth::Bits64);
                    h.push_metric(key, m);
                }
            }
        }
        self.rates.sweep();
        h.push_metric(
            "firewall_rule_counters",
            Metric::unavailable(Unavailable::Unsupported),
        );
        patch.node(h);
        let degraded = self.kmsg_error.map(|r| {
            format!("kernel log unavailable ({r}); needs CAP_SYSLOG when dmesg_restrict=1")
        });
        Ok(Sample { patch, degraded })
    }
}
