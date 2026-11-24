// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! A deterministic simulated machine.
//!
//! The demo machine is a complete fake Linux system expressed as procfs and
//! sysfs files in a [`MemoryKernelFs`]: a hybrid 16-CPU laptop running a
//! small web stack in Docker, a browser, a compiler and the usual daemons.
//! Its counters advance with [`DemoMachine::advance`], so every collector,
//! the TUI and the exporter run against it exactly as against a real kernel.
//!
//! It is used by tests, by `rtop --demo`, and to render README screenshots
//! that contain no property of any real system. Every name, address and
//! number here is invented; IP addresses come from documentation ranges.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use rtop_core::ProcessIdentity;
use rtop_linux::KernelError;
use rtop_linux::fs::FsUsage;
use rtop_linux::memfs::MemoryKernelFs;
use rtop_linux::sockdiag::{InetProto, InetSocket, SockDiag, TcpCounters, UnixSocket};
use serde_json::{Value, json};

use crate::docker::{DockerApi, DockerCollector};
use crate::{Collector, CollectorError, Env, Subscriptions};

const TICKS: f64 = 100.0;
const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// One simulated process.
#[derive(Debug, Clone)]
struct Proc {
    pid: u32,
    ppid: u32,
    name: &'static str,
    cmdline: &'static str,
    user: u32,
    /// Mean CPU in per-CPU percent and amplitude of its variation.
    cpu: f64,
    swing: f64,
    rss_mib: u64,
    threads: u64,
    state: char,
    cgroup: &'static str,
    io_read_bps: f64,
    io_write_bps: f64,
    // accumulated
    ticks: f64,
    read_bytes: f64,
    write_bytes: f64,
    faults: f64,
}

const DOCKER_PG: &str = "4c1d9e8f2a7b3c6d5e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d";
const DOCKER_REDIS: &str = "9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b9c8d7e6f5a4b3c2d1e0f9a8b";
const DOCKER_API: &str = "1f2e3d4c5b6a7980a1b2c3d4e5f60718293a4b5c6d7e8f9012a3b4c5d6e7f809";

fn scope(id: &str) -> String {
    format!("/system.slice/docker-{id}.scope")
}

fn procs() -> Vec<Proc> {
    let p =
        |pid, ppid, name, cmdline, user, cpu, swing, rss_mib, threads, cgroup: &'static str| Proc {
            pid,
            ppid,
            name,
            cmdline,
            user,
            cpu,
            swing,
            rss_mib,
            threads,
            state: 'S',
            cgroup,
            io_read_bps: 0.0,
            io_write_bps: 0.0,
            ticks: 0.0,
            read_bytes: 0.0,
            write_bytes: 0.0,
            faults: 0.0,
        };
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    let pg = leak(scope(DOCKER_PG));
    let redis = leak(scope(DOCKER_REDIS));
    let api = leak(scope(DOCKER_API));
    let user = "/user.slice/user-1000.slice/session-2.scope";
    let app = "/user.slice/user-1000.slice/user@1000.service/app.slice/browser.scope";
    let mut v = vec![
        p(
            1,
            0,
            "systemd",
            "/sbin/init splash",
            0,
            0.1,
            0.1,
            14,
            1,
            "/init.scope",
        ),
        p(2, 0, "kthreadd", "", 0, 0.0, 0.0, 0, 1, "/"),
        p(
            312,
            1,
            "systemd-journal",
            "/usr/lib/systemd/systemd-journald",
            0,
            0.3,
            0.2,
            48,
            1,
            "/system.slice/systemd-journald.service",
        ),
        p(
            640,
            1,
            "sshd",
            "sshd: /usr/sbin/sshd -D [listener] 0 of 10-100 startups",
            0,
            0.0,
            0.0,
            9,
            1,
            "/system.slice/ssh.service",
        ),
        p(
            702,
            1,
            "containerd",
            "/usr/bin/containerd",
            0,
            0.4,
            0.3,
            61,
            18,
            "/system.slice/containerd.service",
        ),
        p(
            731,
            1,
            "dockerd",
            "/usr/bin/dockerd -H fd:// --containerd=/run/containerd/containerd.sock",
            0,
            0.6,
            0.4,
            112,
            24,
            "/system.slice/docker.service",
        ),
        p(
            805,
            1,
            "nginx",
            "nginx: master process /usr/sbin/nginx -g daemon on; master_process on;",
            0,
            0.0,
            0.0,
            6,
            1,
            "/system.slice/nginx.service",
        ),
        p(
            806,
            805,
            "nginx",
            "nginx: worker process",
            33,
            1.8,
            1.2,
            12,
            1,
            "/system.slice/nginx.service",
        ),
        p(
            807,
            805,
            "nginx",
            "nginx: worker process",
            33,
            1.1,
            0.9,
            12,
            1,
            "/system.slice/nginx.service",
        ),
        p(
            1201,
            702,
            "containerd-shim",
            "/usr/bin/containerd-shim-runc-v2 -namespace moby -id 4c1d9e8f2a7b",
            0,
            0.1,
            0.1,
            13,
            11,
            "/system.slice/containerd.service",
        ),
        p(
            1230,
            1201,
            "postgres",
            "postgres -c shared_buffers=512MB",
            999,
            4.2,
            6.0,
            512,
            1,
            pg,
        ),
        p(
            1260,
            1230,
            "postgres",
            "postgres: checkpointer",
            999,
            0.3,
            0.3,
            140,
            1,
            pg,
        ),
        p(
            1261,
            1230,
            "postgres",
            "postgres: walwriter",
            999,
            0.6,
            0.5,
            18,
            1,
            pg,
        ),
        p(
            1262,
            1230,
            "postgres",
            "postgres: autovacuum launcher",
            999,
            0.1,
            0.1,
            9,
            1,
            pg,
        ),
        p(
            1263,
            1230,
            "postgres",
            "postgres: app appdb 172.18.0.4(41022) idle",
            999,
            7.5,
            9.0,
            96,
            1,
            pg,
        ),
        p(
            1301,
            702,
            "containerd-shim",
            "/usr/bin/containerd-shim-runc-v2 -namespace moby -id 9a8b7c6d5e4f",
            0,
            0.1,
            0.1,
            12,
            11,
            "/system.slice/containerd.service",
        ),
        p(
            1320,
            1301,
            "redis-server",
            "redis-server *:6379",
            999,
            1.2,
            1.0,
            128,
            5,
            redis,
        ),
        p(
            1401,
            702,
            "containerd-shim",
            "/usr/bin/containerd-shim-runc-v2 -namespace moby -id 1f2e3d4c5b6a",
            0,
            0.1,
            0.1,
            13,
            11,
            "/system.slice/containerd.service",
        ),
        p(
            1420,
            1401,
            "api-server",
            "/app/api-server --listen 0.0.0.0:8080 --db postgres://db:5432/appdb",
            1000,
            24.0,
            14.0,
            384,
            22,
            api,
        ),
        p(
            2010,
            1,
            "sddm",
            "/usr/bin/sddm",
            0,
            0.0,
            0.0,
            22,
            3,
            "/system.slice/sddm.service",
        ),
        p(2200, 1, "bash", "-bash", 1000, 0.0, 0.0, 6, 1, user),
        p(
            2240,
            2200,
            "tmux: server",
            "tmux new -s work",
            1000,
            0.2,
            0.2,
            7,
            1,
            user,
        ),
        p(2300, 2240, "zsh", "-zsh", 1000, 0.0, 0.0, 9, 1, user),
        p(
            2310,
            2300,
            "cargo",
            "cargo build --release --workspace",
            1000,
            2.0,
            1.5,
            210,
            9,
            user,
        ),
        p(
            2311,
            2310,
            "rustc",
            "rustc --crate-name engine --edition=2024 src/lib.rs -C opt-level=3",
            1000,
            360.0,
            120.0,
            1880,
            18,
            user,
        ),
        p(
            2312,
            2310,
            "rustc",
            "rustc --crate-name server --edition=2024 src/main.rs -C opt-level=3",
            1000,
            180.0,
            90.0,
            1120,
            14,
            user,
        ),
        p(2400, 2240, "zsh", "-zsh", 1000, 0.0, 0.0, 9, 1, user),
        p(
            2410,
            2400,
            "vim",
            "vim src/engine/scheduler.rs",
            1000,
            0.4,
            0.4,
            42,
            1,
            user,
        ),
        p(
            3000,
            1,
            "browser",
            "/opt/browser/browser --profile-directory=Default",
            1000,
            6.0,
            4.0,
            1080,
            48,
            app,
        ),
        p(
            3021,
            3000,
            "browser",
            "/opt/browser/browser --type=gpu-process",
            1000,
            4.0,
            3.0,
            420,
            16,
            app,
        ),
        p(
            3022,
            3000,
            "browser",
            "/opt/browser/browser --type=renderer --renderer-client-id=7",
            1000,
            5.1,
            6.0,
            749,
            21,
            app,
        ),
        p(
            3023,
            3000,
            "browser",
            "/opt/browser/browser --type=renderer --renderer-client-id=9",
            1000,
            1.2,
            1.5,
            312,
            19,
            app,
        ),
        p(
            3024,
            3000,
            "browser",
            "/opt/browser/browser --type=utility --utility-sub-type=network.mojom.NetworkService",
            1000,
            0.8,
            0.6,
            96,
            11,
            app,
        ),
        p(
            3100,
            1,
            "pipewire",
            "/usr/bin/pipewire",
            1000,
            0.5,
            0.3,
            21,
            3,
            user,
        ),
        p(
            3300,
            2240,
            "backup-job",
            "/usr/local/bin/backup-job --target /mnt/archive",
            0,
            1.5,
            1.0,
            64,
            2,
            user,
        ),
    ];
    for (i, w) in [
        (20u32, "kworker/u32:2-events_unbound"),
        (21, "kworker/3:1-mm_percpu_wq"),
        (22, "ksoftirqd/0"),
        (23, "rcu_preempt"),
        (24, "kswapd0"),
        (25, "jbd2/nvme0n1p2-8"),
    ]
    .iter()
    .enumerate()
    {
        v.push(p(
            w.0,
            2,
            w.1,
            "",
            0,
            [0.2, 0.1, 0.3, 0.4, 0.0, 0.1][i],
            0.2,
            0,
            1,
            "/",
        ));
    }
    for x in &mut v {
        match x.pid {
            1230 | 1263 => {
                x.io_read_bps = 2.4e6;
                x.io_write_bps = 6.0e5;
            }
            1261 => x.io_write_bps = 1.1e6,
            2311 | 2312 => {
                x.io_read_bps = 3.0e5;
                x.io_write_bps = 1.6e6;
                x.state = 'R';
            }
            1420 => x.state = 'R',
            3300 => {
                x.state = 'D';
                x.io_read_bps = 4.8e7;
            }
            _ => {}
        }
    }
    v
}

#[derive(Debug, Clone)]
struct DemoSock {
    inode: u64,
    owner: u32,
    inet: Option<InetSocket>,
    unix: Option<UnixSocket>,
    /// (rx, tx) bytes per second when connected.
    rate: (f64, f64),
}

fn sockets() -> Vec<DemoSock> {
    let a = |s: &str| s.parse().expect("valid demo address");
    let tcp = |inode, owner, l: &str, r: &str, state, rate| DemoSock {
        inode,
        owner,
        inet: Some(InetSocket {
            proto: InetProto::Tcp,
            local: a(l),
            remote: a(r),
            state,
            rx_queue: 0,
            tx_queue: 0,
            uid: 0,
            inode,
            tcp: Some(TcpCounters {
                bytes_acked: Some(0),
                bytes_received: Some(0),
                total_retrans: Some(0),
                rtt_us: Some(120),
            }),
        }),
        unix: None,
        rate,
    };
    let unix = |inode, owner, path: &str, listening, peer: Option<u64>| DemoSock {
        inode,
        owner,
        inet: None,
        unix: Some(UnixSocket {
            inode,
            sock_type: 1,
            state: if listening { 10 } else { 3 },
            listening,
            path: path.into(),
            peer,
            rx_queue: Some(0),
            tx_queue: Some(0),
        }),
        rate: (0.0, 0.0),
    };
    vec![
        tcp(9001, 640, "0.0.0.0:22", "0.0.0.0:0", 10, (0.0, 0.0)),
        tcp(9002, 805, "0.0.0.0:443", "0.0.0.0:0", 10, (0.0, 0.0)),
        tcp(9003, 1230, "0.0.0.0:5432", "0.0.0.0:0", 10, (0.0, 0.0)),
        tcp(9004, 1320, "0.0.0.0:6379", "0.0.0.0:0", 10, (0.0, 0.0)),
        tcp(9005, 1420, "0.0.0.0:8080", "0.0.0.0:0", 10, (0.0, 0.0)),
        // nginx worker -> api
        tcp(
            9010,
            806,
            "127.0.0.1:41000",
            "127.0.0.1:8080",
            1,
            (131_000.0, 9_800.0),
        ),
        tcp(
            9011,
            1420,
            "127.0.0.1:8080",
            "127.0.0.1:41000",
            1,
            (9_800.0, 131_000.0),
        ),
        // api -> postgres
        tcp(
            9020,
            1420,
            "127.0.0.1:41022",
            "127.0.0.1:5432",
            1,
            (43_000.0, 6_100.0),
        ),
        tcp(
            9021,
            1263,
            "127.0.0.1:5432",
            "127.0.0.1:41022",
            1,
            (6_100.0, 43_000.0),
        ),
        // api -> redis
        tcp(
            9030,
            1420,
            "127.0.0.1:41040",
            "127.0.0.1:6379",
            1,
            (8_200.0, 2_300.0),
        ),
        tcp(
            9031,
            1320,
            "127.0.0.1:6379",
            "127.0.0.1:41040",
            1,
            (2_300.0, 8_200.0),
        ),
        // browser -> documentation-range remote hosts
        tcp(
            9040,
            3024,
            "192.0.2.15:52814",
            "203.0.113.10:443",
            1,
            (18_240.0, 4_096.0),
        ),
        tcp(
            9041,
            3024,
            "192.0.2.15:52820",
            "198.51.100.7:443",
            1,
            (2_900.0, 1_100.0),
        ),
        tcp(
            9042,
            2200,
            "192.0.2.15:22",
            "198.51.100.23:50110",
            1,
            (310.0, 1_900.0),
        ),
        unix(9100, 731, "/run/docker.sock", true, None),
        unix(9101, 702, "/run/containerd/containerd.sock", true, None),
        unix(9102, 731, "", false, Some(9103)),
        unix(9103, 702, "", false, Some(9102)),
        unix(9104, 3000, "", false, Some(9105)),
        unix(9105, 3021, "", false, Some(9104)),
        unix(9106, 3100, "/run/user/1000/pipewire-0", true, None),
    ]
}

#[derive(Debug)]
struct State {
    t: f64,
    /// Logical CPUs; the first half are performance cores.
    cpus: u32,
    procs: Vec<Proc>,
    socks: Vec<DemoSock>,
    tcp_bytes: Vec<(f64, f64)>,
    cpu_ticks: Vec<[f64; 4]>,
    disk: [f64; 6],
    net: [[f64; 4]; 4],
    psi_total: [f64; 3],
    cg_usage: Vec<f64>,
    rapl_uj: f64,
    gpu_ns: f64,
    /// (packets, bytes) per demo firewall rule.
    fw: Vec<(f64, f64)>,
}

/// The simulated machine.
#[derive(Debug, Clone)]
pub struct DemoMachine {
    fs: Arc<MemoryKernelFs>,
    state: Arc<Mutex<State>>,
}

/// Socket table of the demo machine with netlink-grade detail (Unix peers,
/// TCP byte counters).
#[derive(Debug)]
pub struct DemoDiag {
    state: Arc<Mutex<State>>,
}

impl SockDiag for DemoDiag {
    fn inet(&self, proto: InetProto) -> Result<Vec<InetSocket>, KernelError> {
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        Ok(s.socks
            .iter()
            .zip(&s.tcp_bytes)
            .filter_map(|(d, b)| {
                let mut i = d.inet.clone()?;
                if i.proto != proto {
                    return None;
                }
                if let Some(t) = &mut i.tcp {
                    t.bytes_received = Some(b.0 as u64);
                    t.bytes_acked = Some(b.1 as u64);
                }
                Some(i)
            })
            .collect())
    }

    fn unix(&self) -> Result<Vec<UnixSocket>, KernelError> {
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        Ok(s.socks.iter().filter_map(|d| d.unix.clone()).collect())
    }

    fn backend(&self) -> &'static str {
        "demo"
    }
}

/// Demo firewall: an nftables-like ruleset with advancing counters.
/// (table, chain, handle, comment, verdict, has counter)
type DemoRule = (
    &'static str,
    &'static str,
    u64,
    Option<&'static str>,
    &'static str,
    bool,
);
const FW_RULES: [DemoRule; 6] = [
    (
        "filter",
        "input",
        4,
        Some("established,related"),
        "accept",
        true,
    ),
    ("filter", "input", 7, Some("ssh"), "accept", true),
    ("filter", "input", 9, Some("invalid"), "drop", true),
    ("filter", "input", 12, Some("default deny"), "drop", true),
    (
        "filter",
        "forward",
        21,
        Some("docker bridge"),
        "accept",
        true,
    ),
    ("filter", "output", 30, None, "accept", false),
];
const FW_RATES: [f64; 6] = [112.0, 0.4, 3.0, 18.0, 64.0, 0.0];

#[derive(Debug)]
pub struct DemoFirewall {
    state: Arc<Mutex<State>>,
}

impl rtop_linux::nftables::FirewallSource for DemoFirewall {
    fn rules(&self) -> Result<Vec<rtop_linux::nftables::Rule>, KernelError> {
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        Ok(FW_RULES
            .iter()
            .zip(&s.fw)
            .map(
                |((table, chain, handle, comment, verdict, counted), (p, b))| {
                    // Through the kernel encoding, so the real decoder is exercised.
                    let r = rtop_linux::nftables::Rule {
                        family: "inet",
                        table: (*table).into(),
                        chain: (*chain).into(),
                        handle: *handle,
                        counter: counted.then_some((*p as u64, *b as u64)),
                        verdict: Some((*verdict).into()),
                        comment: comment.map(Into::into),
                    };
                    rtop_linux::nftables::decode_rule(&rtop_linux::nftables::encode_rule(&r))
                        .expect("demo rule decodes")
                },
            )
            .collect())
    }
}

/// Docker Engine API answers for the demo containers.
#[derive(Debug)]
pub struct DemoDocker;

impl DockerApi for DemoDocker {
    fn get(&self, path: &str) -> Result<Value, CollectorError> {
        let containers = [
            (
                DOCKER_PG,
                "postgres-db",
                "postgres:17",
                1230,
                vec![
                    (
                        "volume",
                        "db-data",
                        "/var/lib/docker/volumes/db-data/_data",
                        "/var/lib/postgresql/data",
                    ),
                    ("bind", "", "/srv/backup", "/backup"),
                ],
                Some("healthy"),
            ),
            (
                DOCKER_REDIS,
                "redis-cache",
                "redis:7-alpine",
                1320,
                vec![(
                    "volume",
                    "redis-data",
                    "/var/lib/docker/volumes/redis-data/_data",
                    "/data",
                )],
                None,
            ),
            (
                DOCKER_API,
                "api",
                "example/api:1.4.2",
                1420,
                vec![("tmpfs", "", "", "/tmp")],
                Some("healthy"),
            ),
        ];
        if path.starts_with("/containers/json") {
            return Ok(Value::Array(
                containers
                    .iter()
                    .map(|(id, name, image, _, mounts, _)| {
                        json!({
                            "Id": id, "Names": [format!("/{name}")], "Image": image, "State": "running", "Status": "Up 3 hours",
                            "Mounts": mounts.iter().map(|(ty, n, src, dst)| json!({"Type": ty, "Name": n, "Source": src, "Destination": dst, "RW": true})).collect::<Vec<_>>()
                        })
                    })
                    .collect(),
            ));
        }
        for (id, _, _, pid, _, health) in &containers {
            if path == format!("/containers/{id}/json") {
                let mut state = json!({"Pid": pid, "Status": "running"});
                if let Some(h) = health {
                    state["Health"] = json!({"Status": h});
                }
                return Ok(json!({"State": state, "RestartCount": 0}));
            }
        }
        Err(CollectorError::new(
            rtop_core::Unavailable::Unsupported,
            format!("demo docker: {path}"),
        ))
    }
}

impl Default for DemoMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoMachine {
    pub fn new() -> Self {
        Self::with_cpus(16)
    }

    /// A demo machine with `cpus` logical CPUs (at least 2). The last CPU
    /// runs a pinned busy loop, so a saturated core is always present.
    pub fn with_cpus(cpus: u32) -> Self {
        let cpus = cpus.max(2);
        let procs = procs();
        let socks = sockets();
        let state = State {
            t: 0.0,
            cpus,
            tcp_bytes: vec![(1e6, 1e6); socks.len()],
            procs,
            socks,
            cpu_ticks: vec![[0.0; 4]; cpus as usize],
            disk: [0.0; 6],
            net: [[0.0; 4]; 4],
            psi_total: [0.0; 3],
            cg_usage: vec![0.0; 16],
            rapl_uj: 0.0,
            gpu_ns: 0.0,
            fw: vec![
                (2.8e5, 1.8e7),
                (1.2e3, 9.0e4),
                (310.0, 1.9e4),
                (9.6e3, 5.1e5),
                (4.4e4, 3.1e7),
                (0.0, 0.0),
            ],
        };
        let m = DemoMachine {
            fs: Arc::new(MemoryKernelFs::new()),
            state: Arc::new(Mutex::new(state)),
        };
        m.write_static();
        m.write_dynamic();
        m
    }

    pub fn fs(&self) -> Arc<MemoryKernelFs> {
        self.fs.clone()
    }

    pub fn env(&self, subscriptions: Arc<Subscriptions>) -> Env {
        let mut env = Env::simulated(self.fs.clone(), subscriptions);
        env.diag = Arc::new(DemoDiag {
            state: self.state.clone(),
        });
        env.firewall = Arc::new(DemoFirewall {
            state: self.state.clone(),
        });
        env
    }

    /// Every collector, with the Docker collector answered by [`DemoDocker`].
    pub fn collectors(&self, env: &Env) -> Vec<Box<dyn Collector>> {
        crate::all(env)
            .into_iter()
            .map(|c| {
                if c.descriptor().id == crate::docker::ID {
                    Box::new(DockerCollector::with_api(env.clone(), Box::new(DemoDocker)))
                        as Box<dyn Collector>
                } else {
                    c
                }
            })
            .collect()
    }

    /// Identity of a demo process by PID (for tests).
    pub fn identity(&self, pid: u32) -> Option<ProcessIdentity> {
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        s.procs
            .iter()
            .find(|p| p.pid == pid)
            .map(|p| ProcessIdentity {
                pid: p.pid,
                start_time_ticks: start_ticks(p.pid),
            })
    }

    /// Removes a process, as if it exited.
    pub fn kill(&self, pid: u32) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .procs
            .retain(|p| p.pid != pid);
        self.fs.remove(&format!("/proc/{pid}"));
    }

    /// Advances simulated time by `dt` seconds and rewrites every counter.
    pub fn advance(&self, dt: f64) {
        {
            let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            s.t += dt;
            let t = s.t;
            for p in &mut s.procs {
                let cpu = cpu_at(p, t);
                p.ticks += cpu / 100.0 * TICKS * dt;
                p.read_bytes += p.io_read_bps * dt * (1.0 + 0.3 * (t * 0.7 + p.pid as f64).sin());
                p.write_bytes += p.io_write_bps * dt * (1.0 + 0.3 * (t * 0.5 + p.pid as f64).cos());
                p.faults += (cpu * 3.0 + 1.0) * dt;
            }
            let total_cpu: f64 = s.procs.iter().map(|p| cpu_at(p, t)).sum();
            let n = s.cpus as usize;
            #[allow(clippy::needless_range_loop)]
            for c in 0..n {
                // Spread process load over CPUs; P-cores (first half) take
                // more; the last CPU is pinned at saturation.
                let share = if c < n / 2 {
                    1.36 / n as f64
                } else {
                    0.64 / n as f64
                };
                let busy = if c == n - 1 {
                    0.995
                } else {
                    ((total_cpu * share * (1.0 + 0.25 * (t * 0.9 + c as f64).sin())) / 100.0)
                        .clamp(0.01, 0.98)
                };
                let ticks = TICKS * dt;
                let tk = &mut s.cpu_ticks[c];
                tk[0] += ticks * busy * 0.75;
                tk[1] += ticks * busy * 0.25;
                tk[2] += ticks * (1.0 - busy) * 0.97;
                tk[3] += ticks * (1.0 - busy) * 0.03;
            }
            let disk_rates = [52e6, 4.1e6, 380.0, 210.0, 410.0, 0.0];
            for (d, r) in s.disk.iter_mut().zip(disk_rates) {
                *d += r * dt * (1.0 + 0.35 * (t * 1.3).sin());
            }
            let net_rates = [
                [1.05e5, 2.1e4, 150.0, 90.0],
                [3.2e3, 2.8e3, 20.0, 18.0],
                [1.9e5, 1.9e5, 700.0, 700.0],
                [1.4e5, 1.6e5, 300.0, 280.0],
            ];
            for (n, r) in s.net.iter_mut().zip(net_rates) {
                for i in 0..4 {
                    n[i] += r[i] * dt * (1.0 + 0.4 * (t * 0.8 + i as f64).sin());
                }
            }
            for (i, w) in [0.18, 0.06, 0.12].iter().enumerate() {
                s.psi_total[i] += w * 1e6 * dt;
            }
            let cg = [24.0, 12.0, 1.2];
            for (i, c) in cg.iter().enumerate() {
                s.cg_usage[i] += c / 100.0 * 1e6 * dt * (1.0 + 0.3 * (t + i as f64).sin());
            }
            s.rapl_uj += 18.5e6 * dt * (1.0 + 0.2 * (t * 0.6).sin());
            s.gpu_ns += 0.34e9 * dt;
            for (i, rate) in FW_RATES.iter().enumerate() {
                let r = rate * (1.0 + 0.4 * (t * 0.7 + i as f64).sin());
                s.fw[i].0 += r * dt;
                s.fw[i].1 += r * 620.0 * dt;
            }
            let socks: Vec<(f64, f64)> = s.socks.iter().map(|d| d.rate).collect();
            for (b, r) in s.tcp_bytes.iter_mut().zip(socks) {
                b.0 += r.0 * dt * (1.0 + 0.3 * (t * 1.1).sin());
                b.1 += r.1 * dt * (1.0 + 0.3 * (t * 0.9).cos());
            }
        }
        self.write_dynamic();
    }

    fn write_static(&self) {
        let fs = &self.fs;
        fs.file("/proc/sys/kernel/hostname", "demo-host\n");
        fs.file("/proc/sys/kernel/osrelease", "6.12.0-demo\n");
        fs.file(
            "/proc/sys/kernel/random/boot_id",
            "00000000-0000-4000-8000-000000000000\n",
        );
        fs.file(
            "/etc/os-release",
            "NAME=\"Demo Linux\"\nPRETTY_NAME=\"Demo Linux 26.04\"\n",
        );
        fs.file("/etc/passwd", "root:x:0:0:root:/root:/bin/bash\nwww-data:x:33:33:www-data:/var/www:/usr/sbin/nologin\ndbuser:x:999:999::/home/dbuser:/bin/sh\ndemo:x:1000:1000:Demo User:/home/demo:/bin/zsh\n");
        let ncpu = self.state.lock().unwrap_or_else(|p| p.into_inner()).cpus;
        fs.file(
            "/proc/cpuinfo",
            format!("processor\t: 0\nmodel name\t: Demo Hybrid CPU {ncpu}-Core @ 4.8GHz\n"),
        );
        fs.link("/proc/self/ns/net", "net:[4026531840]");
        // CPU topology: 8 P-cores (no SMT here) + 8 E-cores in two clusters.
        let cpus = self.state.lock().unwrap_or_else(|p| p.into_inner()).cpus;
        fs.file(
            "/sys/devices/cpu_core/cpus",
            format!("0-{}\n", cpus / 2 - 1),
        );
        fs.file(
            "/sys/devices/cpu_atom/cpus",
            format!("{}-{}\n", cpus / 2, cpus - 1),
        );
        for c in 0..cpus {
            let base = format!("/sys/devices/system/cpu/cpu{c}");
            fs.file(&format!("{base}/topology/physical_package_id"), "0\n");
            fs.file(&format!("{base}/topology/core_id"), format!("{c}\n"));
        }
        // Storage.
        for (name, parent, size_gib) in [
            ("nvme0n1", None, 954u64),
            ("nvme0n1p1", Some("nvme0n1"), 1),
            ("nvme0n1p2", Some("nvme0n1"), 953),
            ("sda", None, 3726),
            ("sda1", Some("sda"), 3726),
        ] {
            let base = format!("/sys/class/block/{name}");
            fs.file(
                &format!("{base}/size"),
                format!("{}\n", size_gib * GIB / 512),
            );
            match parent {
                Some(p) => {
                    fs.file(&format!("{base}/partition"), "1\n");
                    fs.link(
                        &base,
                        &format!("../../devices/pci0000:00/0000:00:06.0/block/{p}/{name}"),
                    );
                }
                None => {
                    fs.file(
                        &format!("/sys/block/{name}/queue/rotational"),
                        if name == "sda" { "1\n" } else { "0\n" },
                    );
                    fs.file(
                        &format!("/sys/block/{name}/device/model"),
                        if name == "sda" {
                            "Demo HDD 4TB\n"
                        } else {
                            "Demo NVMe SSD 1TB\n"
                        },
                    );
                }
            }
        }
        fs.link(
            "/sys/dev/block/259:2",
            "../../devices/pci0000:00/0000:00:06.0/nvme/nvme0/nvme0n1/nvme0n1p2",
        );
        fs.link(
            "/sys/dev/block/259:1",
            "../../devices/pci0000:00/0000:00:06.0/nvme/nvme0/nvme0n1/nvme0n1p1",
        );
        fs.link(
            "/sys/dev/block/8:1",
            "../../devices/pci0000:00/0000:00:17.0/ata1/block/sda/sda1",
        );
        fs.file(
            "/proc/self/mountinfo",
            "22 1 259:2 / / rw,relatime - ext4 /dev/nvme0n1p2 rw\n\
             23 22 0:21 / /proc rw - proc proc rw\n\
             24 22 0:25 / /run rw,nosuid - tmpfs tmpfs rw,size=3200000k\n\
             25 22 259:1 / /boot/efi rw - vfat /dev/nvme0n1p1 rw\n\
             26 22 8:1 / /mnt/archive rw,noatime - ext4 /dev/sda1 rw\n\
             27 22 0:42 / /srv/backup rw - nfs4 nas.example:/backup rw\n\
             28 22 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n",
        );
        fs.set_usage(
            "/",
            Ok(FsUsage {
                total_bytes: 937 * GIB,
                free_bytes: 172 * GIB,
                available_bytes: 125 * GIB,
                total_inodes: 61_000_000,
                free_inodes: 52_000_000,
            }),
        );
        fs.set_usage(
            "/run",
            Ok(FsUsage {
                total_bytes: 3 * GIB,
                free_bytes: 3 * GIB - 4 * MIB,
                available_bytes: 3 * GIB - 4 * MIB,
                total_inodes: 800_000,
                free_inodes: 799_000,
            }),
        );
        fs.set_usage(
            "/boot/efi",
            Ok(FsUsage {
                total_bytes: 511 * MIB,
                free_bytes: 440 * MIB,
                available_bytes: 440 * MIB,
                total_inodes: 0,
                free_inodes: 0,
            }),
        );
        fs.set_usage(
            "/mnt/archive",
            Ok(FsUsage {
                total_bytes: 3667 * GIB,
                free_bytes: 1210 * GIB,
                available_bytes: 1024 * GIB,
                total_inodes: 240_000_000,
                free_inodes: 238_000_000,
            }),
        );
        fs.set_usage("/srv/backup", Err(KernelError::Os(libc::EIO)));
        // Network.
        for (name, physical, speed, wireless) in [
            ("lo", false, -1, false),
            ("wlan0", true, 1200, true),
            ("eth0", true, 1000, false),
            ("docker0", false, -1, false),
        ] {
            let base = format!("/sys/class/net/{name}");
            fs.file(
                &format!("{base}/operstate"),
                if name == "eth0" { "down\n" } else { "up\n" },
            );
            fs.file(&format!("{base}/speed"), format!("{speed}\n"));
            fs.file(
                &format!("{base}/mtu"),
                if name == "lo" { "65536\n" } else { "1500\n" },
            );
            if physical {
                fs.link(&format!("{base}/device"), "../../../0000:00:14.3");
            }
            if wireless {
                fs.dir(&format!("{base}/wireless"));
            }
        }
        fs.file("/proc/net/snmp", "Tcp: RtoAlgorithm RtoMin RtoMax MaxConn ActiveOpens PassiveOpens AttemptFails EstabResets CurrEstab InSegs OutSegs RetransSegs InErrs OutRsts\nTcp: 1 200 120000 -1 1500 900 12 30 11 900000 880000 140 0 60\nUdp: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors\nUdp: 40000 12 0 39000 0 0\n");
        // cgroups.
        fs.file(
            "/sys/fs/cgroup/cgroup.controllers",
            "cpuset cpu io memory hugetlb pids rdma misc\n",
        );
        // Sensors.
        fs.file("/sys/class/hwmon/hwmon0/name", "coretemp\n");
        fs.file("/sys/class/hwmon/hwmon0/temp1_label", "Package id 0\n");
        fs.file("/sys/class/hwmon/hwmon0/temp1_crit", "100000\n");
        fs.file("/sys/class/hwmon/hwmon0/temp1_max", "90000\n");
        for c in 0..4 {
            fs.file(
                &format!("/sys/class/hwmon/hwmon0/temp{}_label", c + 2),
                format!("Core {}\n", c * 4),
            );
        }
        fs.file("/sys/class/hwmon/hwmon1/name", "nvme\n");
        fs.file("/sys/class/hwmon/hwmon1/temp1_label", "Composite\n");
        fs.file("/sys/class/hwmon/hwmon2/name", "demo_ec\n");
        fs.file("/sys/class/hwmon/hwmon2/fan1_label", "CPU Fan\n");
        fs.file("/sys/class/thermal/thermal_zone0/type", "acpitz\n");
        fs.file("/sys/class/powercap/intel-rapl:0/name", "package-0\n");
        fs.file(
            "/sys/class/powercap/intel-rapl:0/max_energy_range_uj",
            "262143328850\n",
        );
        fs.file("/sys/class/power_supply/BAT0/type", "Battery\n");
        fs.file("/sys/class/power_supply/BAT0/manufacturer", "DemoCell\n");
        fs.file("/sys/class/power_supply/BAT0/model_name", "DC-5X\n");
        fs.file("/sys/class/power_supply/BAT0/technology", "Li-poly\n");
        fs.file("/sys/class/power_supply/BAT0/energy_full", "52800000\n");
        fs.file(
            "/sys/class/power_supply/BAT0/energy_full_design",
            "57000000\n",
        );
        fs.file("/sys/class/power_supply/BAT0/cycle_count", "212\n");
        fs.file("/sys/class/power_supply/AC/type", "Mains\n");
        fs.file("/sys/class/power_supply/AC/online", "0\n");
        // GPUs: integrated Intel + discrete AMD.
        for (card, pci, vendor, device, driver) in [
            ("card0", "0000:00:02.0", "0x8086", "0x7d55", "i915"),
            ("card1", "0000:03:00.0", "0x1002", "0x73df", "amdgpu"),
        ] {
            let dev = format!("/sys/class/drm/{card}/device");
            fs.link(&dev, &format!("../../../{pci}"));
            fs.file(&format!("{dev}/vendor"), format!("{vendor}\n"));
            fs.file(&format!("{dev}/device"), format!("{device}\n"));
            fs.link(
                &format!("{dev}/driver"),
                &format!("../../../bus/pci/drivers/{driver}"),
            );
        }
        fs.file(
            "/sys/class/drm/card0/device/product_name",
            "Demo Integrated Graphics\n",
        );
        fs.file(
            "/sys/class/drm/card1/device/product_name",
            "Demo Radeon 6700M\n",
        );
        fs.file("/sys/class/drm/card0/gt_max_freq_mhz", "2250\n");
        fs.file(
            "/sys/class/drm/card1/device/mem_info_vram_total",
            format!("{}\n", 10 * GIB),
        );
        fs.file(
            "/dev/kmsg",
            "4,1001,8200000,-;audit: type=1400 apparmor=\"DENIED\" operation=\"open\" profile=\"browser\" name=\"/proc/3000/mountinfo\" pid=3000 comm=\"browser\"\n\
             4,1002,9100000,-;[UFW BLOCK] IN=wlan0 OUT= MAC=00:00:00:00:00:00 SRC=198.51.100.99 DST=192.0.2.15 PROTO=TCP SPT=40404 DPT=23\n\
             3,1003,9600000,-;Out of memory: Killed process 4100 (test-runner) total-vm:9000000kB\n\
             4,1004,9900000,-;[UFW BLOCK] IN=wlan0 OUT= MAC=00:00:00:00:00:00 SRC=198.51.100.99 DST=192.0.2.15 PROTO=TCP SPT=40405 DPT=3389\n",
        );
        fs.file("/proc/sys/net/netfilter/nf_conntrack_max", "262144\n");
    }

    fn write_dynamic(&self) {
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let fs = &self.fs;
        let t = s.t;
        let wave = |f: f64, ph: f64| (t * f + ph).sin();
        let uptime = 37_000.0 + t;
        fs.file(
            "/proc/uptime",
            format!("{uptime:.2} {:.2}\n", uptime * 12.0),
        );
        let load = 4.02 + 0.6 * wave(0.1, 0.0);
        fs.file(
            "/proc/loadavg",
            format!(
                "{load:.2} {:.2} {:.2} 7/{} 9{:04}\n",
                load * 0.9,
                load * 0.7,
                900 + s.procs.len(),
                (t as u64) % 10_000
            ),
        );

        let mut stat = String::new();
        let sum: [f64; 4] = s.cpu_ticks.iter().fold([0.0; 4], |a, c| {
            [a[0] + c[0], a[1] + c[1], a[2] + c[2], a[3] + c[3]]
        });
        let base = 1_000_000.0;
        let _ = writeln!(
            stat,
            "cpu  {:.0} 0 {:.0} {:.0} {:.0} 0 0 0 0 0",
            base + sum[0],
            base + sum[1],
            base * 4.0 + sum[2],
            1000.0 + sum[3]
        );
        for (c, tk) in s.cpu_ticks.iter().enumerate() {
            let _ = writeln!(
                stat,
                "cpu{c} {:.0} 0 {:.0} {:.0} {:.0} 0 0 0 0 0",
                60_000.0 + tk[0],
                60_000.0 + tk[1],
                250_000.0 + tk[2],
                60.0 + tk[3]
            );
        }
        let _ = writeln!(
            stat,
            "intr {:.0}\nctxt {:.0}\nbtime 1767225600\nprocesses {:.0}\nprocs_running 7\nprocs_blocked 1",
            9e7 + t * 41_000.0,
            2e8 + t * 98_000.0,
            400_000.0 + t * 35.0
        );
        fs.file("/proc/stat", stat);
        for c in 0..s.cpus {
            let khz = if c < s.cpus / 2 {
                3_900_000.0 + 800_000.0 * wave(0.3, c as f64)
            } else {
                2_600_000.0 + 400_000.0 * wave(0.3, c as f64)
            };
            fs.file(
                &format!("/sys/devices/system/cpu/cpu{c}/cpufreq/scaling_cur_freq"),
                format!("{khz:.0}\n"),
            );
        }

        let total = 31_744u64 * MIB;
        let avail = (16.3 * GIB as f64 + 0.2 * GIB as f64 * wave(0.05, 0.0)) as u64;
        let swap_total = 8 * GIB - 8 * MIB;
        let swap_free = 112 * MIB;
        fs.file(
            "/proc/meminfo",
            format!(
                "MemTotal: {} kB\nMemFree: {} kB\nMemAvailable: {} kB\nBuffers: {} kB\nCached: {} kB\nSwapTotal: {} kB\nSwapFree: {} kB\nZswap: {} kB\nDirty: {} kB\nAnonPages: {} kB\nShmem: {} kB\nSReclaimable: {} kB\nSUnreclaim: {} kB\n",
                total / 1024, 2 * GIB / 1024, avail / 1024, 420 * MIB / 1024, 11 * GIB / 1024, swap_total / 1024, swap_free / 1024, 1210 * MIB / 1024, 180 * MIB / 1024, 12 * GIB / 1024, 900 * MIB / 1024, 780 * MIB / 1024, 310 * MIB / 1024
            ),
        );
        fs.file("/proc/vmstat", format!("pgfault {:.0}\npgmajfault {:.0}\npswpin {:.0}\npswpout {:.0}\npgscan_direct {:.0}\noom_kill 1\n", 9e8 + t * 41_000.0, 22_000.0 + t * 3.0, 410_000.0 + t * 0.4, 2_100_000.0 + t * 0.2, 31_000.0));
        let psi = |avg: f64, total: f64| {
            format!(
                "some avg10={avg:.2} avg60={:.2} avg300={:.2} total={total:.0}\nfull avg10={:.2} avg60={:.2} avg300={:.2} total={:.0}\n",
                avg * 0.8,
                avg * 0.6,
                avg / 3.0,
                avg / 4.0,
                avg / 5.0,
                total / 3.0
            )
        };
        fs.file(
            "/proc/pressure/cpu",
            psi(18.0 + 4.0 * wave(0.2, 0.0), s.psi_total[0]),
        );
        fs.file(
            "/proc/pressure/memory",
            psi(0.4 + 0.3 * wave(0.15, 1.0).abs(), s.psi_total[1]),
        );
        fs.file(
            "/proc/pressure/io",
            psi(12.0 + 3.0 * wave(0.25, 2.0), s.psi_total[2]),
        );

        // Processes.
        for p in &s.procs {
            let base = format!("/proc/{}", p.pid);
            let rss_pages = p.rss_mib * MIB / 4096;
            let (ut, st) = (p.ticks * 0.8, p.ticks * 0.2);
            let start = start_ticks(p.pid);
            fs.file(
                &format!("{base}/stat"),
                format!("{} ({}) {} {} {} {} 0 -1 4194560 {:.0} 0 {:.0} 0 {:.0} {:.0} 0 0 20 0 {} 0 {start} {} {rss_pages} 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 {} 0 0 0 0 0\n", p.pid, p.name, p.state, p.ppid, p.pid, p.pid, p.faults, p.faults / 500.0, ut, st, p.threads, p.rss_mib * MIB * 3 + 120 * MIB, p.pid % s.cpus),
            );
            fs.file(&format!("{base}/status"), format!("Name:\t{}\nUid:\t{u}\t{u}\t{u}\t{u}\nThreads:\t{}\nVmRSS:\t{} kB\nVmSwap:\t{} kB\nvoluntary_ctxt_switches:\t{:.0}\nnonvoluntary_ctxt_switches:\t{:.0}\n", p.name, p.threads, p.rss_mib * 1024, if p.pid == 3000 { 412 * 1024 } else { 0 }, p.ticks * 9.0 + 100.0, p.ticks + 10.0, u = p.user));
            fs.file(&format!("{base}/cmdline"), p.cmdline.replace(' ', "\0"));
            fs.file(&format!("{base}/cgroup"), format!("0::{}\n", p.cgroup));
            fs.file(&format!("{base}/io"), format!("rchar: {:.0}\nwchar: {:.0}\nsyscr: 1\nsyscw: 1\nread_bytes: {:.0}\nwrite_bytes: {:.0}\ncancelled_write_bytes: 0\n", p.read_bytes * 1.2, p.write_bytes * 1.1, p.read_bytes, p.write_bytes));
            fs.file(&format!("{base}/net/dev"), net_dev_text(&s, p.pid));
            if s.t == 0.0 {
                write_process_detail(fs, p, start);
            }
        }
        for (i, d) in s.socks.iter().enumerate() {
            if s.procs.iter().any(|p| p.pid == d.owner) {
                fs.link(
                    &format!("/proc/{}/fd/{}", d.owner, 10 + i),
                    &format!("socket:[{}]", d.inode),
                );
            }
            // fork-inherited listener shared by both nginx workers
            if d.inode == 9002 {
                for w in [806, 807] {
                    fs.link(&format!("/proc/{w}/fd/6"), "socket:[9002]");
                }
            }
        }
        fs.file("/proc/net/dev", net_dev_text(&s, 0));

        // Disks: sectors read/written etc.
        let d = &s.disk;
        let sec = |b: f64| b / 512.0;
        fs.file(
            "/proc/diskstats",
            format!(
                " 259 0 nvme0n1 {r:.0} 0 {sr:.0} {rms:.0} {w:.0} 0 {sw:.0} {wms:.0} 2 {io:.0} {wio:.0} 900 0 1800 30 1200 60\n 259 1 nvme0n1p1 300 0 2400 40 2 0 16 1 0 60 41 0 0 0 0 0 0\n 259 2 nvme0n1p2 {r:.0} 0 {sr:.0} {rms:.0} {w:.0} 0 {sw:.0} {wms:.0} 2 {io:.0} {wio:.0} 900 0 1800 30 0 0\n 8 0 sda {hr:.0} 0 {hsr:.0} {hms:.0} 40 0 320 90 1 {hio:.0} {hms:.0}\n 8 1 sda1 {hr:.0} 0 {hsr:.0} {hms:.0} 40 0 320 90 1 {hio:.0} {hms:.0}\n 7 0 loop0 12 0 24 1 0 0 0 0 0 4 1\n",
                r = 1e6 + d[2] * 1.0,
                sr = 4e7 + sec(d[1] * 3.0),
                rms = 2e5 + d[2] * 0.12,
                w = 2e6 + d[3],
                sw = 9e7 + sec(d[1]),
                wms = 4e5 + d[3] * 0.4,
                io = 3e6 + d[4] * 1.1,
                wio = 6e6 + d[4] * 2.0,
                hr = 5e4 + d[2] * 0.9,
                hsr = 1e7 + sec(d[0]),
                hms = 1e5 + d[2] * 7.5,
                hio = 2e5 + d[4] * 2.3
            ),
        );

        // cgroups.
        let cgroups: Vec<(String, f64, u64, u64)> = vec![
            (
                "/system.slice".into(),
                s.cg_usage.iter().sum::<f64>(),
                1500 * MIB,
                0,
            ),
            (scope(DOCKER_PG), s.cg_usage[1], 512 * MIB, 6),
            (scope(DOCKER_REDIS), s.cg_usage[2], 128 * MIB, 1),
            (scope(DOCKER_API), s.cg_usage[0], 384 * MIB, 1),
            (
                "/system.slice/nginx.service".into(),
                s.cg_usage[0] * 0.12,
                30 * MIB,
                3,
            ),
            (
                "/system.slice/docker.service".into(),
                s.cg_usage[2] * 0.5,
                112 * MIB,
                1,
            ),
            (
                "/system.slice/containerd.service".into(),
                s.cg_usage[2] * 0.4,
                99 * MIB,
                4,
            ),
            ("/system.slice/ssh.service".into(), 1000.0, 9 * MIB, 1),
            ("/user.slice".into(), s.cg_usage[0] * 22.0, 6 * GIB, 0),
            (
                "/user.slice/user-1000.slice".into(),
                s.cg_usage[0] * 22.0,
                6 * GIB,
                0,
            ),
        ];
        for (path, usage, mem, procs) in &cgroups {
            let base = format!("/sys/fs/cgroup{path}");
            fs.file(&format!("{base}/cpu.stat"), format!("usage_usec {usage:.0}\nuser_usec {:.0}\nsystem_usec {:.0}\nnr_periods 100\nnr_throttled {}\nthrottled_usec {:.0}\n", usage * 0.8, usage * 0.2, if path.contains(DOCKER_API) { (t * 8.0) as u64 } else { 0 }, if path.contains(DOCKER_API) { t * 41_000.0 } else { 0.0 }));
            fs.file(&format!("{base}/memory.current"), format!("{mem}\n"));
            fs.file(
                &format!("{base}/memory.stat"),
                format!(
                    "anon {}\nfile {}\nkernel {}\nshmem 0\ninactive_file {}\n",
                    mem * 7 / 10,
                    mem * 2 / 10,
                    mem / 10,
                    mem / 4
                ),
            );
            fs.file(
                &format!("{base}/memory.peak"),
                format!("{}\n", mem + mem / 5),
            );
            fs.file(
                &format!("{base}/memory.max"),
                if path.contains(DOCKER_PG) {
                    format!("{}\n", 2 * GIB)
                } else {
                    "max\n".into()
                },
            );
            fs.file(
                &format!("{base}/cpu.max"),
                if path.contains(DOCKER_API) {
                    "200000 100000\n".into()
                } else {
                    "max 100000\n".to_string()
                },
            );
            fs.file(
                &format!("{base}/pids.current"),
                format!("{}\n", procs.max(&1)),
            );
            fs.file(
                &format!("{base}/io.stat"),
                format!(
                    "259:0 rbytes={:.0} wbytes={:.0} rios=10 wios=10\n",
                    usage * 3.0,
                    usage * 1.5
                ),
            );
            fs.file(&format!("{base}/memory.pressure"), "some avg10=0.10 avg60=0.05 avg300=0.01 total=100\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n");
            let members: String = s
                .procs
                .iter()
                .filter(|p| p.cgroup == path)
                .map(|p| format!("{}\n", p.pid))
                .collect();
            fs.file(&format!("{base}/cgroup.procs"), members);
        }

        // Sensors.
        let pkg = 71_000.0 + 6_000.0 * wave(0.2, 0.0);
        fs.file("/sys/class/hwmon/hwmon0/temp1_input", format!("{pkg:.0}\n"));
        for c in 0..4 {
            fs.file(
                &format!("/sys/class/hwmon/hwmon0/temp{}_input", c + 2),
                format!("{:.0}\n", pkg - 3000.0 + 2500.0 * wave(0.4, c as f64)),
            );
        }
        fs.file(
            "/sys/class/hwmon/hwmon1/temp1_input",
            format!("{:.0}\n", 48_850.0 + 900.0 * wave(0.1, 1.0)),
        );
        fs.file(
            "/sys/class/hwmon/hwmon2/fan1_input",
            format!("{:.0}\n", 3_400.0 + 500.0 * wave(0.2, 0.5)),
        );
        fs.file(
            "/sys/class/thermal/thermal_zone0/temp",
            format!("{:.0}\n", 54_000.0 + 1_000.0 * wave(0.1, 0.0)),
        );
        fs.file(
            "/sys/class/powercap/intel-rapl:0/energy_uj",
            format!("{:.0}\n", 1e9 + s.rapl_uj),
        );
        let energy_now = 37_600_000.0 - t * 3_900.0;
        fs.file(
            "/sys/class/power_supply/BAT0/energy_now",
            format!("{energy_now:.0}\n"),
        );
        fs.file(
            "/sys/class/power_supply/BAT0/capacity",
            format!("{:.0}\n", 100.0 * energy_now / 52_800_000.0),
        );
        fs.file("/sys/class/power_supply/BAT0/status", "Discharging\n");
        fs.file(
            "/sys/class/power_supply/BAT0/power_now",
            format!("{:.0}\n", 14_100_000.0 + 1_500_000.0 * wave(0.3, 0.0)),
        );
        fs.file("/sys/class/power_supply/BAT0/voltage_now", "16420000\n");
        fs.file(
            "/sys/class/drm/card0/gt_act_freq_mhz",
            format!("{:.0}\n", 900.0 + 300.0 * wave(0.3, 0.0)),
        );
        fs.file(
            "/sys/class/drm/card1/device/gpu_busy_percent",
            format!("{:.0}\n", 34.0 + 12.0 * wave(0.35, 0.0)),
        );
        fs.file(
            "/sys/class/drm/card1/device/mem_busy_percent",
            format!("{:.0}\n", 11.0 + 5.0 * wave(0.3, 1.0)),
        );
        fs.file(
            "/sys/class/drm/card1/device/mem_info_vram_used",
            format!("{}\n", 3 * GIB + 200 * MIB),
        );
        fs.file(
            "/sys/class/drm/card1/device/hwmon/hwmon5/temp1_input",
            format!("{:.0}\n", 58_000.0 + 2000.0 * wave(0.2, 0.0)),
        );
        fs.file(
            "/sys/class/drm/card1/device/hwmon/hwmon5/power1_average",
            format!("{:.0}\n", 31_000_000.0 + 6_000_000.0 * wave(0.3, 0.0)),
        );
        // Browser GPU process is a DRM client of the discrete GPU.
        if s.procs.iter().any(|p| p.pid == 3021) {
            fs.link("/proc/3021/fd/40", "/dev/dri/renderD129");
            fs.file("/proc/3021/fdinfo/40", format!("pos:\t0\ndrm-driver:\tamdgpu\ndrm-pdev:\t0000:03:00.0\ndrm-client-id:\t17\ndrm-engine-gfx:\t{:.0} ns\ndrm-memory-vram:\t{} KiB\n", s.gpu_ns, 1_536 * 1024));
        }
        let ct = 1_900 + (40.0 * wave(0.2, 0.0)) as i64;
        fs.file(
            "/proc/sys/net/netfilter/nf_conntrack_count",
            format!("{ct}\n"),
        );
        fs.file("/proc/net/stat/nf_conntrack", format!("entries clashres found new invalid ignore delete delete_list insert insert_failed drop early_drop\n{:08x} 0 0 0 {:08x} 0 0 0 0 0 {:08x} 0\n", ct, (t * 3.0) as u64, (t * 18.0) as u64));
    }
}

/// Static per-process files read only by deep inspection.
fn write_process_detail(fs: &MemoryKernelFs, p: &Proc, start: u64) {
    let base = format!("/proc/{}", p.pid);
    let exe = p
        .cmdline
        .split(' ')
        .next()
        .filter(|e| e.starts_with('/'))
        .map_or_else(|| format!("/usr/bin/{}", p.name), str::to_owned);
    fs.link(&format!("{base}/exe"), &exe);
    fs.link(
        &format!("{base}/cwd"),
        if p.user == 1000 {
            "/home/demo/src/engine"
        } else {
            "/"
        },
    );
    for (fd, target) in [
        (0, "/dev/null"),
        (1, "/dev/null"),
        (2, "/dev/null"),
        (3, "anon_inode:[eventpoll]"),
        (4, "pipe:[7700]"),
    ] {
        fs.link(&format!("{base}/fd/{fd}"), target);
    }
    if p.threads > 1 || p.name == "postgres" {
        fs.link(&format!("{base}/fd/5"), &format!("/var/log/{}.log", p.name));
    }
    let rss = p.rss_mib * 1024;
    fs.file(&format!("{base}/smaps_rollup"), format!("Rss: {rss} kB\nPss: {} kB\nPss_Anon: {} kB\nPss_File: {} kB\nShared_Clean: {} kB\nShared_Dirty: 0 kB\nPrivate_Clean: {} kB\nPrivate_Dirty: {} kB\nSwap: 0 kB\nSwapPss: 0 kB\n", rss * 8 / 10, rss * 6 / 10, rss / 5, rss / 5, rss / 10, rss * 7 / 10));
    fs.file(
        &format!("{base}/oom_score"),
        format!("{}\n", 600 + p.rss_mib / 10),
    );
    fs.file(
        &format!("{base}/wchan"),
        if p.state == 'D' {
            "io_schedule"
        } else {
            "do_epoll_wait"
        },
    );
    fs.file(&format!("{base}/limits"), "Limit                     Soft Limit           Hard Limit           Units     \nMax stack size            8388608              unlimited            bytes     \nMax processes             127422               127422               processes \nMax open files            1024                 524288               files     \nMax locked memory         8388608              8388608              bytes     \nMax address space         unlimited            unlimited            bytes     \n");
    for (ns, inode) in [
        ("cgroup", 4026531835u64),
        ("ipc", 4026531839),
        ("mnt", 4026531841),
        ("net", 4026531840),
        ("pid", 4026531836),
        ("user", 4026531837),
        ("uts", 4026531838),
    ] {
        let inode = if p.cgroup.contains("docker-") && ns != "user" {
            inode + 1000
        } else {
            inode
        };
        fs.link(&format!("{base}/ns/{ns}"), &format!("{ns}:[{inode}]"));
    }
    let names = ["worker", "io", "timer", "gc", "net", "pool"];
    for t in 0..p.threads.min(12) {
        let tid = if t == 0 { p.pid } else { p.pid * 10 + t as u32 };
        let tname = if t == 0 {
            p.name.to_owned()
        } else {
            format!("{}-{}", names[t as usize % names.len()], t)
        };
        let ticks = (t + 1) * 37 + u64::from(p.pid % 97);
        fs.file(&format!("{base}/task/{tid}/stat"), format!("{tid} ({tname}) S {} {} {} 0 -1 0 0 0 0 0 {ticks} {} 0 0 20 0 1 0 {start} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 {} 0 0 0 0 0\n", p.ppid, p.pid, p.pid, ticks / 4, (tid as u64) % 16));
    }
}

fn start_ticks(pid: u32) -> u64 {
    // Deterministic, ordered by PID like a real boot.
    500 + u64::from(pid) * 37
}

fn cpu_at(p: &Proc, t: f64) -> f64 {
    (p.cpu + p.swing * (t * 0.37 + f64::from(p.pid) * 0.61).sin()).max(0.0)
}

fn net_dev_text(s: &State, pid: u32) -> String {
    let mut out = String::from(
        "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n",
    );
    let names = if pid == 0 {
        vec!["wlan0", "eth0", "lo", "docker0"]
    } else if [1230, 1320, 1420].contains(&pid) {
        vec!["lo", "eth0"]
    } else {
        return out;
    };
    for (i, name) in names.into_iter().enumerate() {
        let n = s.net[i.min(3)];
        let scale = if pid == 0 {
            1.0
        } else {
            0.1 + f64::from(pid % 7) * 0.05
        };
        let _ = writeln!(
            out,
            "{name:>6}: {:.0} {:.0} 0 {} 0 0 0 0 {:.0} {:.0} 0 0 0 0 0 0",
            9e9 + n[0] * scale,
            6e6 + n[2] * scale,
            if name == "wlan0" { 12 } else { 0 },
            2e9 + n[1] * scale,
            3e6 + n[3] * scale
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_counters_advance_monotonically() {
        let m = DemoMachine::new();
        let read = |m: &DemoMachine| {
            let mut b = Vec::new();
            rtop_linux::KernelFs::read(m.fs.as_ref(), "/proc/2311/stat", &mut b, 4096).unwrap();
            rtop_linux::parse::pid::stat(&b).unwrap().utime
        };
        let a = read(&m);
        m.advance(1.0);
        assert!(read(&m) > a);
    }
}
