// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The process table.
//!
//! A pass reads `/proc/PID/stat` and `/proc/PID/status` for every process;
//! everything that rarely changes (command line, user name, cgroup) is
//! cached per [`ProcessIdentity`] and refreshed at a lower rate, and
//! `/proc/PID/io` is read only while subscribed. Expensive files (smaps,
//! fd tables, threads) belong to on-demand [`crate::inspect`].
//!
//! Processes are identified by PID *and* start time, so a recycled PID is a
//! new entity with fresh counter baselines, never a continuation.

use std::collections::HashMap;
use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker};
use rtop_core::{
    CollectorId, EdgeKind, EdgeUpdate, EntityId, EntityKind, Evidence, Metric, NodeUpdate,
    ProcessIdentity, Scope, Unavailable,
};
use rtop_linux::KernelError;
use rtop_linux::fs::KernelFsExt;
use rtop_linux::parse::{pid, system};

use crate::{
    Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext,
    Subscriptions,
};

pub const ID: CollectorId = CollectorId("processes");
/// Upper bound on processes tracked in one pass.
pub const MAX_PROCESSES: usize = 200_000;
const METADATA_REFRESH_PASSES: u32 = 10;
const STATUS_EVERY: u64 = 5;

#[derive(Debug, Clone)]
struct Cached {
    cmdline: String,
    cgroup: Option<String>,
    age: u32,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
enum Counter {
    Cpu(ProcessIdentity),
    MinFlt(ProcessIdentity),
    MajFlt(ProcessIdentity),
    ReadBytes(ProcessIdentity),
    WriteBytes(ProcessIdentity),
    Ctx(ProcessIdentity),
}

#[derive(Debug)]
pub struct ProcessCollector {
    env: Env,
    rates: RateTracker<Counter>,
    cache: HashMap<ProcessIdentity, Cached>,
    users: HashMap<u32, String>,
    users_loaded: bool,
    buf: Vec<u8>,
    status: HashMap<ProcessIdentity, pid::PidStatus>,
    pass: u64,
}

impl ProcessCollector {
    pub fn new(env: Env) -> Self {
        ProcessCollector {
            env,
            rates: RateTracker::new(),
            cache: HashMap::new(),
            users: HashMap::new(),
            users_loaded: false,
            buf: Vec::with_capacity(1024),
            status: HashMap::new(),
            pass: 0,
        }
    }

    fn user_name(&mut self, uid: u32) -> String {
        if !self.users_loaded || (!self.users.contains_key(&uid) && self.users.len() < 100_000) {
            if let Ok(passwd) = self.env.fs.read_string("/etc/passwd", 16 << 20) {
                for line in passwd.lines() {
                    let f: Vec<&str> = line.split(':').collect();
                    if let (Some(name), Some(id)) =
                        (f.first(), f.get(2).and_then(|v| v.parse().ok()))
                    {
                        self.users.insert(id, (*name).to_owned());
                    }
                }
            }
            self.users_loaded = true;
            // Remember unknown uids so /etc/passwd is not re-read every pass.
            self.users.entry(uid).or_insert_with(|| uid.to_string());
        }
        self.users
            .get(&uid)
            .cloned()
            .unwrap_or_else(|| uid.to_string())
    }
}

impl Collector for ProcessCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(5),
            isolation: Isolation::Expensive,
            essential: true,
            nice: 0,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let fs = self.env.fs.clone();
        let entries = fs
            .read_dir("/proc")
            .map_err(|e| CollectorError::from_kernel("/proc", &e))?;
        let uptime = fs
            .read_string("/proc/uptime", SMALL)
            .ok()
            .and_then(|u| system::uptime(&u))
            .map(|u| u.0);
        let ticks = self.env.clock_ticks.max(1) as f64;
        let page = self.env.page_size;
        let want_io = Subscriptions::get(&self.env.subscriptions.process_io);
        let now = ctx.now;

        let mut patch = ctx
            .patch(ID)
            .complete(Scope::Nodes(EntityKind::Process))
            .complete(Scope::Edges(EdgeKind::ParentOf))
            .complete(Scope::Edges(EdgeKind::MemberOf));
        let mut by_pid: HashMap<u32, ProcessIdentity> = HashMap::new();
        let mut parents: Vec<(ProcessIdentity, u32)> = Vec::new();
        let (mut running, mut blocked, mut zombies, mut threads_total, mut io_denied) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        self.rates.begin();

        for name in entries
            .iter()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        {
            if by_pid.len() >= MAX_PROCESSES {
                break;
            }
            let Ok(pid_num) = name.parse::<u32>() else {
                continue;
            };
            // A process may exit at any point during the pass: every read
            // failure simply drops it from this pass.
            if fs
                .read(&format!("/proc/{name}/stat"), &mut self.buf, SMALL)
                .is_err()
            {
                continue;
            }
            let Some(st) = pid::stat(&self.buf) else {
                continue;
            };
            let ident = ProcessIdentity {
                pid: pid_num,
                start_time_ticks: st.starttime,
            };
            // /proc/PID/status (credentials, swap, context switches) is
            // costly to format and changes slowly: read it for new
            // processes and then every STATUS_EVERY passes, staggered by PID.
            let fresh_status = !self.cache.contains_key(&ident)
                || (self.pass + u64::from(pid_num)).is_multiple_of(STATUS_EVERY);
            let status = if fresh_status {
                let st = fs
                    .read_string(&format!("/proc/{name}/status"), SMALL)
                    .map(|s| pid::status(&s))
                    .unwrap_or_default();
                self.status.insert(ident, st.clone());
                st
            } else {
                self.status.get(&ident).cloned().unwrap_or_default()
            };
            by_pid.insert(pid_num, ident);
            parents.push((ident, st.ppid));

            let cached = match self.cache.get(&ident) {
                Some(c) if c.age < METADATA_REFRESH_PASSES => {
                    let mut c = c.clone();
                    c.age += 1;
                    c
                }
                _ => {
                    let cmdline = fs
                        .read(&format!("/proc/{name}/cmdline"), &mut self.buf, SMALL)
                        .map(|_| pid::cmdline(&self.buf))
                        .unwrap_or_default();
                    let cgroup = fs
                        .read_string(&format!("/proc/{name}/cgroup"), SMALL)
                        .ok()
                        .and_then(|c| pid::cgroup_v2_path(&c).map(str::to_owned));
                    Cached {
                        cmdline,
                        cgroup,
                        age: 0,
                    }
                }
            };
            self.cache.insert(ident, cached.clone());

            let comm = String::from_utf8_lossy(&st.comm).into_owned();
            let id = ident.entity_id();
            let mut n = NodeUpdate::new(EntityKind::Process, id.clone())
                .attr("pid", pid_num)
                .attr("ppid", st.ppid)
                .text("name", &comm)
                .text("cmdline", &cached.cmdline)
                .attr("state", pid::state_name(st.state))
                .attr("state_code", (st.state as char).to_string())
                .attr("start_time_ticks", st.starttime);
            if st.ppid == 2 || pid_num == 2 {
                n = n.attr("kernel_thread", true);
            }
            if Some(pid_num) == self.env.self_pid {
                n = n.attr("is_self", true);
            }
            if let Some(uid) = status.real_uid {
                let user = self.user_name(uid);
                n = n.attr("uid", uid).text("user", &user);
            }
            if let Some(cg) = &cached.cgroup {
                n = n.text("cgroup", cg);
                patch.edge(EdgeUpdate::new(
                    EdgeKind::MemberOf,
                    id.clone(),
                    EntityId::new(EntityKind::Cgroup, cg),
                    Evidence::KernelConfirmed,
                ));
            }
            if let Some(cpu) = st.processor {
                n = n.attr("last_cpu", cpu);
            }
            match st.state {
                b'R' => running += 1,
                b'D' => blocked += 1,
                b'Z' => zombies += 1,
                _ => {}
            }
            threads_total += st.num_threads;

            let cpu_rate = self.rates.observe(
                Counter::Cpu(ident),
                st.utime + st.stime,
                now,
                CounterWidth::Bits64,
            );
            n.push_metric(
                "cpu_usage_percent",
                Metric::from_result(
                    cpu_rate
                        .map(|t| 100.0 * t / ticks)
                        .map_err(Unavailable::from),
                ),
            );
            n.push_metric(
                "cpu_time_seconds",
                Metric::f64((st.utime + st.stime) as f64 / ticks),
            );
            n.push_metric("rss_bytes", Metric::u64(st.rss_pages.saturating_mul(page)));
            n.push_metric("virtual_bytes", Metric::u64(st.vsize));
            n.push_metric(
                "swap_bytes",
                status
                    .vm_swap
                    .map_or(Metric::unavailable(Unavailable::Unsupported), Metric::u64),
            );
            n.push_metric("threads", Metric::u64(st.num_threads));
            n.push_metric("nice", Metric::i64(st.nice));
            n.push_metric("priority", Metric::i64(st.priority));
            n.push_metric(
                "minor_faults_per_second",
                self.rates
                    .metric(Counter::MinFlt(ident), st.minflt, now, CounterWidth::Bits64),
            );
            n.push_metric(
                "major_faults_per_second",
                self.rates
                    .metric(Counter::MajFlt(ident), st.majflt, now, CounterWidth::Bits64),
            );
            if let Some((v, nv)) = status
                .voluntary_ctxt_switches
                .zip(status.nonvoluntary_ctxt_switches)
            {
                n.push_metric(
                    "context_switches_per_second",
                    self.rates
                        .metric(Counter::Ctx(ident), v + nv, now, CounterWidth::Bits64),
                );
            }
            if let Some(up) = uptime {
                n.push_metric(
                    "age_seconds",
                    Metric::f64((up - st.starttime as f64 / ticks).max(0.0)),
                );
            }
            if want_io {
                match fs.read_string(&format!("/proc/{name}/io"), SMALL) {
                    Ok(t) => match pid::io(&t) {
                        Some(io) => {
                            n.push_metric(
                                "io_read_bytes_per_second",
                                self.rates.metric(
                                    Counter::ReadBytes(ident),
                                    io.read_bytes,
                                    now,
                                    CounterWidth::Bits64,
                                ),
                            );
                            n.push_metric(
                                "io_write_bytes_per_second",
                                self.rates.metric(
                                    Counter::WriteBytes(ident),
                                    io.write_bytes,
                                    now,
                                    CounterWidth::Bits64,
                                ),
                            );
                        }
                        None => {
                            n.push_metric(
                                "io_read_bytes_per_second",
                                Metric::unavailable(Unavailable::InvalidData),
                            );
                            n.push_metric(
                                "io_write_bytes_per_second",
                                Metric::unavailable(Unavailable::InvalidData),
                            );
                        }
                    },
                    Err(e) => {
                        if e == KernelError::PermissionDenied {
                            io_denied += 1;
                        }
                        let m = Metric::unavailable(crate::unavailable_from(&e));
                        n.push_metric("io_read_bytes_per_second", m);
                        n.push_metric("io_write_bytes_per_second", m);
                    }
                }
            } else {
                n.push_metric(
                    "io_read_bytes_per_second",
                    Metric::unavailable(Unavailable::NotCollected),
                );
                n.push_metric(
                    "io_write_bytes_per_second",
                    Metric::unavailable(Unavailable::NotCollected),
                );
            }
            patch.node(n);
        }
        self.rates.sweep();
        self.cache
            .retain(|ident, _| by_pid.get(&ident.pid) == Some(ident));
        self.status
            .retain(|ident, _| by_pid.get(&ident.pid) == Some(ident));
        self.pass += 1;

        for (child, ppid) in parents {
            if let Some(parent) = by_pid.get(&ppid) {
                patch.edge(EdgeUpdate::new(
                    EdgeKind::ParentOf,
                    parent.entity_id(),
                    child.entity_id(),
                    Evidence::KernelConfirmed,
                ));
            }
        }
        patch.node(
            NodeUpdate::new(EntityKind::Host, EntityId::host())
                .metric("processes_total", Metric::u64(by_pid.len() as u64))
                .metric("processes_running", Metric::u64(running))
                .metric("processes_blocked", Metric::u64(blocked))
                .metric("processes_zombie", Metric::u64(zombies))
                .metric("threads_total", Metric::u64(threads_total)),
        );
        let degraded = (io_denied > 0).then(|| {
            format!("I/O accounting denied for {io_denied} processes owned by other users")
        });
        Ok(Sample { patch, degraded })
    }
}
