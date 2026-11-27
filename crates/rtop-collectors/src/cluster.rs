// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0
//
// Swedish Embedded AB implements solutions for observing GPU clusters and
// HPC jobs end to end for its clients. If your team needs expertise in
// Slurm, accelerator fleets and fabric telemetry then you can procure our
// services by sending an email to info@swedishembedded.com.

//! The other nodes of the Slurm job rtop runs in.
//!
//! When rtop's own cgroup places it in a Slurm job, this collector lists
//! the job's nodes with what Slurm knows about them (`scontrol show node`,
//! refreshed every [`SLURM_REFRESH_SAMPLES`] samples) and keeps one
//! `rtop --agent` per other node, started as an overlapping job step
//! (`srun --overlap --gres=gpu:<n>`, so it may open every GPU of its node).
//! An agent writes one JSON line per sample on stdout ([`encode_report`]);
//! the step ends when rtop closes the agent's stdin or exits. Remote GPUs
//! join the graph as `gpu:<node>/<pci address>`; each node becomes a
//! `cluster_node` with its CPU, memory, GPU and fabric summary.
//!
//! Only registered metrics ([`crate::metrics`]) and the attributes in
//! [`CARRIED_ATTRS`] cross the wire; a line is bounded to
//! [`MAX_LINE_BYTES`].

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rtop_core::{
    Attr, CollectorId, EntityId, EntityKind, Metric, NodeUpdate, Scope, SystemGraph, Unavailable,
};
use rtop_linux::fs::{KernelFs, KernelFsExt};
use serde_json::{Map, Value, json};

use crate::{Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("cluster");

/// Wire format version of agent lines.
pub const PROTOCOL: u64 = 1;
/// Samples between two `scontrol show node` queries.
pub const SLURM_REFRESH_SAMPLES: u32 = 15;
/// Longest accepted agent line.
pub const MAX_LINE_BYTES: usize = 4 << 20;
/// Wait before restarting an agent that exited or failed to start.
pub const AGENT_RETRY: Duration = Duration::from_secs(30);
/// How long a sample waits for freshly started agents to deliver two
/// reports (so their rates are real), e.g. for `rtop --stats`.
pub const AGENT_WARMUP: Duration = Duration::from_secs(8);
/// An agent silent for this long is shown as stale.
pub const AGENT_STALE: Duration = Duration::from_secs(5);

/// Attributes carried from agents (metrics are carried when registered).
pub const CARRIED_ATTRS: &[&str] = &[
    "name",
    "vendor",
    "driver",
    "provider",
    "pci_address",
    "index",
    "uuid",
    "serial",
    "vbios_version",
    "board_part_number",
    "compute_capability",
    "driver_version",
    "cuda_driver_version",
    "nvml_version",
    "gsp_firmware_version",
    "inforom_version",
    "pci_device_id",
    "pci_subsystem_id",
    "architecture",
    "brand",
    "board_id",
    "numa_node",
    "performance_state",
    "clock_event_reasons",
    "persistence_mode",
    "mig_mode",
    "ecc_mode",
    "compute_mode",
    "remapping_pending",
    "remapping_failed",
    "c2c_active",
    "card",
    "cpu_model",
    "kernel",
    "hostname",
];

/// Entity kinds an agent reports.
const REPORTED: [EntityKind; 2] = [EntityKind::Host, EntityKind::Gpu];

pub fn node_id(name: &str) -> EntityId {
    EntityId::new(EntityKind::ClusterNode, name)
}

// ----- wire format ----------------------------------------------------------

fn attr_json(a: &Attr) -> Value {
    match a {
        Attr::Text(s) => Value::String(s.clone()),
        Attr::Int(v) => json!(v),
        Attr::UInt(v) => json!(v),
        Attr::Bool(v) => json!(v),
    }
}

fn metric_json(m: &Metric) -> Value {
    match m {
        Metric::Valid(rtop_core::Value::U64(v)) => json!(v),
        Metric::Valid(rtop_core::Value::I64(v)) => json!(v),
        Metric::Unavailable(u) => json!({ "na": u.as_str() }),
        m => m.as_f64().map_or(Value::Null, |v| json!(v)),
    }
}

/// One agent line: the host and GPU entities of `g`.
pub fn encode_report(g: &SystemGraph, host: &str) -> String {
    let mut nodes = Vec::new();
    for kind in REPORTED {
        for n in g.nodes_of(kind) {
            let attrs: Map<String, Value> = n
                .attrs
                .iter()
                .filter(|(k, _)| CARRIED_ATTRS.contains(k))
                .map(|(k, a)| ((*k).to_owned(), attr_json(&a.value)))
                .collect();
            let metrics: Map<String, Value> = n
                .metrics
                .iter()
                .map(|(k, e)| ((*k).to_owned(), metric_json(&e.metric)))
                .collect();
            nodes.push(json!({
                "kind": kind.as_str(),
                "key": n.id.key(),
                "attrs": attrs,
                "metrics": metrics,
            }));
        }
    }
    json!({ "v": PROTOCOL, "host": host, "nodes": nodes }).to_string()
}

/// An entity reported by an agent, with keys resolved to the registry.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteNode {
    pub kind: EntityKind,
    pub key: String,
    pub attrs: Vec<(&'static str, Attr)>,
    pub metrics: Vec<(&'static str, Metric)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub host: String,
    pub nodes: Vec<RemoteNode>,
}

fn static_attr(k: &str) -> Option<&'static str> {
    CARRIED_ATTRS.iter().copied().find(|a| *a == k)
}

fn static_metric(k: &str) -> Option<&'static str> {
    crate::metrics::describe(k).map(|s| s.key)
}

/// Parses an agent line; unknown keys are dropped, a different protocol
/// version is an error.
pub fn decode_report(line: &str) -> Result<Report, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("bad agent line: {e}"))?;
    if v["v"].as_u64() != Some(PROTOCOL) {
        return Err(format!(
            "agent speaks protocol {}, this rtop {PROTOCOL}: use the same rtop build",
            v["v"]
        ));
    }
    let host = v["host"].as_str().unwrap_or_default().to_owned();
    let mut nodes = Vec::new();
    for n in v["nodes"].as_array().into_iter().flatten() {
        let Some(kind) = n["kind"].as_str().and_then(EntityKind::parse) else {
            continue;
        };
        if !REPORTED.contains(&kind) {
            continue;
        }
        let key = n["key"].as_str().unwrap_or_default().to_owned();
        let attrs = n["attrs"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| {
                let a = match v {
                    Value::String(s) => Attr::Text(rtop_core::text::sanitize(s).into_owned()),
                    Value::Bool(b) => Attr::Bool(*b),
                    Value::Number(x) => match (x.as_u64(), x.as_i64()) {
                        (Some(u), _) => Attr::UInt(u),
                        (None, Some(i)) => Attr::Int(i),
                        _ => return None,
                    },
                    _ => return None,
                };
                Some((static_attr(k)?, a))
            })
            .collect();
        let metrics = n["metrics"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| {
                let m = match v {
                    Value::Number(x) => match (x.as_u64(), x.as_i64(), x.as_f64()) {
                        (Some(u), _, _) => Metric::u64(u),
                        (None, Some(i), _) => Metric::i64(i),
                        (_, _, Some(f)) => Metric::f64(f),
                        _ => return None,
                    },
                    Value::Object(o) => Metric::unavailable(
                        o.get("na")
                            .and_then(Value::as_str)
                            .and_then(Unavailable::parse)
                            .unwrap_or(Unavailable::InvalidData),
                    ),
                    _ => Metric::unavailable(Unavailable::InvalidData),
                };
                Some((static_metric(k)?, m))
            })
            .collect();
        nodes.push(RemoteNode {
            kind,
            key,
            attrs,
            metrics,
        });
    }
    Ok(Report { host, nodes })
}

// ----- Slurm ------------------------------------------------------------------

/// `(job, step)` of the Slurm step process `pid` runs in, from its cgroup
/// path (`…/job_<id>/step_<name>/…`).
pub fn slurm_step(fs: &dyn KernelFs, pid: Option<u32>) -> Option<(String, String)> {
    let cg = fs
        .read_string(&format!("/proc/{}/cgroup", pid?), SMALL)
        .ok()?;
    let mut job = None;
    for part in cg.lines().flat_map(|l| l.split('/')) {
        if let Some(j) = part.strip_prefix("job_") {
            job = Some(j.to_owned());
        } else if let (Some(j), Some(st)) = (&job, part.strip_prefix("step_")) {
            return Some((j.clone(), st.to_owned()));
        }
    }
    None
}

/// Expands a Slurm host list (`n[1-3,7],gpu01`) into names; bounded to
/// `limit` names.
pub fn expand_hostlist(list: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut groups = Vec::new();
    let (mut depth, mut start) = (0, 0);
    for (i, c) in list.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ',' if depth == 0 => {
                groups.push(&list[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    groups.push(&list[start..]);
    for g in groups.into_iter().map(str::trim).filter(|g| !g.is_empty()) {
        let Some((prefix, rest)) = g.split_once('[') else {
            out.push(g.to_owned());
            continue;
        };
        let Some((ranges, suffix)) = rest.split_once(']') else {
            continue;
        };
        for r in ranges.split(',') {
            let (a, b) = r.split_once('-').unwrap_or((r, r));
            let (Ok(lo), Ok(hi)) = (a.parse::<u64>(), b.parse::<u64>()) else {
                continue;
            };
            for n in lo..=hi.min(lo.saturating_add(limit as u64)) {
                if out.len() >= limit {
                    return out;
                }
                out.push(format!("{prefix}{n:0w$}{suffix}", w = a.len()));
            }
        }
    }
    out.truncate(limit);
    out
}

/// What Slurm reports about a node (`scontrol show node -o`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlurmNode {
    pub name: String,
    pub state: String,
    pub partitions: String,
    pub cpus: Option<u64>,
    pub cpus_allocated: Option<u64>,
    /// Load average as Slurm sees it.
    pub cpu_load: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub memory_allocated_bytes: Option<u64>,
    pub memory_free_bytes: Option<u64>,
    pub gpus: Option<u64>,
    pub gpus_allocated: Option<u64>,
}

/// GPUs in a `gres/gpu=N` TRES list or a `gpu:<type>:N(...)` Gres string.
fn gpu_count(s: &str) -> Option<u64> {
    s.split(',').find_map(|t| {
        if let Some(n) = t.strip_prefix("gres/gpu=") {
            return n.parse().ok();
        }
        let t = t.strip_prefix("gpu:")?;
        let n = t.split('(').next()?.rsplit(':').next()?;
        n.parse().ok()
    })
}

/// Parses `scontrol show node -o` output, one node per line.
pub fn parse_nodes(text: &str) -> Vec<SlurmNode> {
    text.lines()
        .filter_map(|line| {
            let mut n = SlurmNode::default();
            for tok in line.split_whitespace() {
                let Some((k, v)) = tok.split_once('=') else {
                    continue;
                };
                let mib = |v: &str| v.parse::<u64>().ok().map(|m| m << 20);
                match k {
                    "NodeName" => n.name = v.to_owned(),
                    "State" => n.state = v.to_owned(),
                    "Partitions" => n.partitions = v.to_owned(),
                    "CPUTot" => n.cpus = v.parse().ok(),
                    "CPUAlloc" => n.cpus_allocated = v.parse().ok(),
                    "CPULoad" => n.cpu_load = v.parse().ok(),
                    "RealMemory" => n.memory_bytes = mib(v),
                    "AllocMem" => n.memory_allocated_bytes = mib(v),
                    "FreeMem" => n.memory_free_bytes = mib(v),
                    "Gres" => n.gpus = gpu_count(v),
                    "AllocTRES" => n.gpus_allocated = gpu_count(v).or(Some(0)),
                    _ => {}
                }
            }
            (!n.name.is_empty()).then_some(n)
        })
        .collect()
}

/// `NodeList=` of `scontrol show job -o`.
pub fn job_nodelist(text: &str) -> Option<&str> {
    text.split_whitespace()
        .find_map(|t| t.strip_prefix("NodeList="))
        .filter(|l| !l.is_empty() && *l != "(null)")
}

// ----- sources ----------------------------------------------------------------

/// A started agent: its line stream, and the process to stop on drop.
pub struct AgentProcess {
    pub lines: Box<dyn BufRead + Send>,
    pub child: Option<Child>,
    /// The agent's diagnostics (stderr), if captured.
    pub errors: Option<Box<dyn Read + Send>>,
}

impl std::fmt::Debug for AgentProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentProcess")
            .field("pid", &self.child.as_ref().map(Child::id))
            .finish()
    }
}

/// Access to Slurm and to agents on other nodes.
pub trait ClusterSource: Send + Sync + std::fmt::Debug {
    /// Runs `scontrol` with `args`; its standard output.
    fn scontrol(&self, args: &[&str]) -> Result<String, String>;
    /// Starts an agent on `node` of `job` with `gpus` GPUs.
    fn launch(&self, job: &str, node: &str, gpus: u64) -> Result<AgentProcess, String>;
}

/// No cluster (simulated systems).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCluster;

impl ClusterSource for NoCluster {
    fn scontrol(&self, _: &[&str]) -> Result<String, String> {
        Err("no Slurm on a captured or simulated system".into())
    }
    fn launch(&self, _: &str, _: &str, _: u64) -> Result<AgentProcess, String> {
        Err("no Slurm on a captured or simulated system".into())
    }
}

/// The real Slurm commands on `PATH`.
#[derive(Debug, Clone)]
pub struct SlurmCli {
    /// rtop binary the agents run (on a shared file system).
    pub exe: std::path::PathBuf,
    /// Words placed between `srun …` and the agent, e.g.
    /// `apptainer exec --nv image.sif`.
    pub wrapper: Vec<String>,
    pub interval: Duration,
}

impl ClusterSource for SlurmCli {
    fn scontrol(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new("scontrol")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("scontrol: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "scontrol {}: {}",
                args.join(" "),
                err.lines().last().unwrap_or("failed")
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn launch(&self, job: &str, node: &str, gpus: u64) -> Result<AgentProcess, String> {
        let mut cmd = Command::new("srun");
        cmd.args([
            &format!("--jobid={job}"),
            "--overlap",
            "--nodes=1",
            "--ntasks=1",
            "--quiet",
            "--job-name=rtop-agent",
            &format!("--nodelist={node}"),
        ]);
        if gpus > 0 {
            cmd.arg(format!("--gres=gpu:{gpus}"));
        }
        cmd.args(&self.wrapper)
            .arg(&self.exe)
            .arg("--agent")
            .arg(format!("--interval={}ms", self.interval.as_millis()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: prctl is async-signal-safe; it asks the kernel to stop
        // srun (and so the step) if rtop dies without cleaning up.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("srun: {e}"))?;
        let stdout = child.stdout.take().ok_or("srun: no stdout")?;
        let errors = child
            .stderr
            .take()
            .map(|e| Box::new(e) as Box<dyn Read + Send>);
        Ok(AgentProcess {
            lines: Box::new(std::io::BufReader::new(stdout)),
            child: Some(child),
            errors,
        })
    }
}

// ----- agents ---------------------------------------------------------------

#[derive(Debug, Default)]
struct Shared {
    latest: Option<(Report, Instant)>,
    reports: u64,
    /// Why the agent stopped, when it has.
    ended: Option<String>,
}

#[derive(Debug)]
struct Agent {
    shared: Arc<Mutex<Shared>>,
    child: Option<Child>,
    started: Instant,
}

impl Drop for Agent {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            // Closing stdin ends the agent; the kill covers a stuck srun.
            drop(c.stdin.take());
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Reads lines into `shared` until the stream ends or misbehaves.
fn pump(
    mut lines: Box<dyn BufRead + Send>,
    shared: Arc<Mutex<Shared>>,
    errors: Option<Box<dyn Read + Send>>,
) {
    let mut buf = Vec::new();
    let end = loop {
        buf.clear();
        match (&mut lines)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut buf)
        {
            Ok(0) => break "agent exited".to_owned(),
            Ok(_) if buf.len() > MAX_LINE_BYTES => break "agent line too long".to_owned(),
            Ok(_) => match decode_report(&String::from_utf8_lossy(&buf)) {
                Ok(r) => {
                    let mut s = shared.lock().unwrap_or_else(|p| p.into_inner());
                    s.latest = Some((r, Instant::now()));
                    s.reports += 1;
                }
                Err(e) => break e,
            },
            Err(e) => break format!("reading agent: {e}"),
        }
    };
    // The last diagnostic line explains most failures (e.g. srun refusing
    // the GRES request).
    let mut detail = String::new();
    if let Some(mut e) = errors {
        let _ = e.by_ref().take(64 << 10).read_to_string(&mut detail);
    }
    let last = detail.lines().rev().find(|l| !l.trim().is_empty());
    let mut s = shared.lock().unwrap_or_else(|p| p.into_inner());
    s.ended = Some(match last {
        Some(l) => format!("{end}: {}", rtop_core::text::sanitize(l.trim())),
        None => end,
    });
}

fn start(src: &dyn ClusterSource, job: &str, node: &str, gpus: u64) -> Agent {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let child = match src.launch(job, node, gpus) {
        Ok(p) => {
            let sh = shared.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("rtop-agent-{node}"))
                .spawn(move || pump(p.lines, sh, p.errors));
            if let Err(e) = spawned {
                shared.lock().unwrap_or_else(|p| p.into_inner()).ended =
                    Some(format!("cannot start reader thread: {e}"));
            }
            p.child
        }
        Err(e) => {
            shared.lock().unwrap_or_else(|p| p.into_inner()).ended = Some(e);
            None
        }
    };
    Agent {
        shared,
        child,
        started: Instant::now(),
    }
}

// ----- collector --------------------------------------------------------------

#[derive(Debug)]
pub struct ClusterCollector {
    env: Env,
    /// Job id and its nodes, found once.
    job: Option<Result<(String, Vec<String>), String>>,
    slurm: Vec<SlurmNode>,
    slurm_error: Option<String>,
    samples: u32,
    agents: HashMap<String, Agent>,
}

impl ClusterCollector {
    pub fn new(env: Env) -> Self {
        ClusterCollector {
            env,
            job: None,
            slurm: Vec::new(),
            slurm_error: None,
            samples: 0,
            agents: HashMap::new(),
        }
    }

    fn discover(&self) -> Result<(String, Vec<String>), String> {
        let (job, _) = slurm_step(self.env.fs.as_ref(), self.env.self_pid)
            .ok_or("not running in a Slurm job")?;
        let text = self
            .env
            .cluster
            .scontrol(&["show", "job", "--oneliner", &job])?;
        let list = job_nodelist(&text).ok_or("the job lists no nodes")?;
        Ok((job, expand_hostlist(list, 1024)))
    }
}

/// Mean of the values present.
fn mean(v: impl Iterator<Item = f64>) -> Option<f64> {
    let (s, n) = v.fold((0.0, 0u32), |(s, n), x| (s + x, n + 1));
    (n > 0).then(|| s / f64::from(n))
}

impl Collector for ClusterCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(10),
            isolation: Isolation::MayBlock,
            essential: false,
            nice: 5,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        if self.job.is_none() {
            self.job = Some(self.discover());
        }
        let (job, nodes) = match &self.job {
            Some(Ok(j)) => j.clone(),
            Some(Err(e)) => {
                return Err(CollectorError::new(Unavailable::Unsupported, e.clone()));
            }
            None => unreachable!("discovered above"),
        };
        let host = self
            .env
            .fs
            .read_string("/proc/sys/kernel/hostname", SMALL)
            .map(|h| h.trim().to_owned())
            .unwrap_or_default();
        if self.samples.is_multiple_of(SLURM_REFRESH_SAMPLES) {
            let list = nodes.join(",");
            match self
                .env
                .cluster
                .scontrol(&["show", "node", "--oneliner", &list])
            {
                Ok(t) => {
                    self.slurm = parse_nodes(&t);
                    self.slurm_error = None;
                }
                Err(e) => self.slurm_error = Some(e),
            }
        }
        self.samples = self.samples.wrapping_add(1);

        // One agent per other node; a failed one is retried after a pause.
        let src = self.env.cluster.clone();
        for node in nodes.iter().filter(|n| **n != host) {
            let gpus = self
                .slurm
                .iter()
                .find(|s| &s.name == node)
                .and_then(|s| s.gpus)
                .unwrap_or(0);
            let restart = self.agents.get(node).is_none_or(|a| {
                let ended = a
                    .shared
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .ended
                    .is_some();
                ended && a.started.elapsed() >= AGENT_RETRY
            });
            if restart {
                self.agents.remove(node);
                self.agents
                    .insert(node.clone(), start(src.as_ref(), &job, node, gpus));
            }
        }
        self.agents.retain(|n, _| nodes.contains(n));
        // Freshly started agents get a bounded moment to report twice.
        loop {
            let warming = self.agents.values().any(|a| {
                let s = a.shared.lock().unwrap_or_else(|p| p.into_inner());
                s.ended.is_none() && s.reports < 2 && a.started.elapsed() < AGENT_WARMUP
            });
            if !warming {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let mut patch = ctx
            .patch(ID)
            .complete(Scope::Nodes(EntityKind::ClusterNode))
            .complete(Scope::Nodes(EntityKind::Gpu));
        let mut problems = Vec::new();
        if let Some(e) = &self.slurm_error {
            problems.push(e.clone());
        }
        for name in &nodes {
            let mut n = NodeUpdate::new(EntityKind::ClusterNode, node_id(name))
                .text("name", name)
                .attr("job", job.clone())
                .attr("local", *name == host);
            if let Some(s) = self.slurm.iter().find(|s| &s.name == name) {
                n = n
                    .text("slurm_state", &s.state)
                    .text("partitions", &s.partitions);
                for (k, v) in [
                    ("slurm_cpus", s.cpus),
                    ("slurm_cpus_allocated", s.cpus_allocated),
                    ("slurm_memory_bytes", s.memory_bytes),
                    ("slurm_memory_allocated_bytes", s.memory_allocated_bytes),
                    ("slurm_memory_free_bytes", s.memory_free_bytes),
                    ("slurm_gpus", s.gpus),
                    ("slurm_gpus_allocated", s.gpus_allocated),
                ] {
                    if let Some(v) = v {
                        n.push_metric(k, Metric::u64(v));
                    }
                }
                if let Some(l) = s.cpu_load {
                    n.push_metric("slurm_cpu_load", Metric::f64(l));
                }
            }
            if *name == host {
                n.push_attr("agent", "local");
                patch.node(n);
                continue;
            }
            let Some(agent) = self.agents.get(name) else {
                continue;
            };
            let s = agent.shared.lock().unwrap_or_else(|p| p.into_inner());
            let fresh = s
                .latest
                .as_ref()
                .filter(|(_, at)| at.elapsed() < AGENT_STALE);
            let status = match (&s.ended, &s.latest, fresh) {
                (Some(e), _, _) => {
                    problems.push(format!("{name}: {e}"));
                    format!("stopped: {e}")
                }
                (None, None, _) => "starting".to_owned(),
                (None, Some(_), None) => "stale".to_owned(),
                (None, Some(_), Some(_)) => "streaming".to_owned(),
            };
            n.push_attr("agent", status);
            if let Some((report, _)) = &s.latest {
                let mut gpu_busy = Vec::new();
                let mut gpu_power = Vec::new();
                for r in &report.nodes {
                    match r.kind {
                        EntityKind::Host => {
                            for (k, m) in &r.metrics {
                                if matches!(
                                    *k,
                                    "cpu_usage_percent"
                                        | "memory_used_bytes"
                                        | "memory_total_bytes"
                                        | "load_1"
                                        | "fabric_rx_bytes_per_second"
                                        | "fabric_tx_bytes_per_second"
                                ) {
                                    n.push_metric(
                                        k,
                                        if fresh.is_some() {
                                            *m
                                        } else {
                                            Metric::unavailable(Unavailable::TimedOut)
                                        },
                                    );
                                }
                            }
                        }
                        EntityKind::Gpu => {
                            let mut g = NodeUpdate::new(
                                EntityKind::Gpu,
                                EntityId::new(EntityKind::Gpu, format!("{name}/{}", r.key)),
                            )
                            .attr("node", name.clone());
                            for (k, a) in &r.attrs {
                                g.push_attr(k, a.clone());
                            }
                            for (k, m) in &r.metrics {
                                g.push_metric(
                                    k,
                                    if fresh.is_some() {
                                        *m
                                    } else {
                                        Metric::unavailable(Unavailable::TimedOut)
                                    },
                                );
                                match (*k, m.as_f64()) {
                                    ("gpu_usage_percent", Some(v)) => gpu_busy.push(v),
                                    ("power_w", Some(v)) => gpu_power.push(v),
                                    _ => {}
                                }
                            }
                            patch.node(g);
                        }
                        _ => {}
                    }
                }
                n.push_metric("gpus", Metric::u64(gpu_busy.len() as u64));
                if let Some(m) = mean(gpu_busy.into_iter()) {
                    n.push_metric("gpu_usage_percent", Metric::f64(m));
                }
                if !gpu_power.is_empty() {
                    n.push_metric("gpu_power_w", Metric::f64(gpu_power.iter().sum()));
                }
            }
            drop(s);
            patch.node(n);
        }
        let degraded = (!problems.is_empty()).then(|| problems.join("; "));
        Ok(Sample { patch, degraded })
    }
}

/// Writes agent lines for `graph` snapshots until the reader goes away.
pub fn write_report(out: &mut impl Write, g: &SystemGraph, host: &str) -> std::io::Result<()> {
    out.write_all(encode_report(g, host).as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slurm with three nodes; agents answer from canned lines.
    #[derive(Debug, Default)]
    struct FakeSlurm {
        launches: Mutex<Vec<(String, u64)>>,
    }

    impl ClusterSource for FakeSlurm {
        fn scontrol(&self, args: &[&str]) -> Result<String, String> {
            Ok(match args {
                ["show", "job", ..] => "JobId=77 NodeList=n[1-3] BatchHost=n1".into(),
                ["show", "node", ..] => "NodeName=n1 CPUTot=8 Gres=gpu:a:2(S:0) State=MIXED AllocTRES=cpu=8,gres/gpu=2\nNodeName=n2 CPUTot=8 Gres=gpu:a:1 State=ALLOCATED\nNodeName=n3 CPUTot=8 Gres=(null) State=IDLE\n".into(),
                _ => return Err("unexpected".into()),
            })
        }

        fn launch(&self, _: &str, node: &str, gpus: u64) -> Result<AgentProcess, String> {
            self.launches.lock().unwrap().push((node.to_owned(), gpus));
            if node == "n3" {
                return Err("srun: error: Unable to create step".into());
            }
            let mut g = SystemGraph::default();
            let mut p = rtop_core::GraphPatch::new(
                CollectorId("t"),
                1,
                rtop_core::Timestamp::from_millis(1),
            );
            p.node(
                NodeUpdate::new(
                    EntityKind::Gpu,
                    EntityId::new(EntityKind::Gpu, "0000:01:00.0"),
                )
                .text("name", "Remote GPU")
                .metric("gpu_usage_percent", Metric::f64(80.0))
                .metric("power_w", Metric::f64(300.0)),
            );
            p.node(
                NodeUpdate::new(EntityKind::Host, EntityId::host())
                    .metric("cpu_usage_percent", Metric::f64(12.5)),
            );
            g.apply(p);
            let line = encode_report(&g, node) + "\n";
            Ok(AgentProcess {
                lines: Box::new(std::io::Cursor::new(line.repeat(2).into_bytes())),
                child: None,
                errors: None,
            })
        }
    }

    #[test]
    fn job_nodes_and_remote_gpus_join_the_graph() {
        let fs = rtop_linux::MemoryKernelFs::new();
        fs.file(
            "/proc/9/cgroup",
            "0::/system.slice/slurmstepd.scope/job_77/step_3/user\n",
        );
        fs.file("/proc/sys/kernel/hostname", "n1\n");
        let mut env = Env::simulated(Arc::new(fs), Arc::new(crate::Subscriptions::default()));
        env.self_pid = Some(9);
        let slurm = Arc::new(FakeSlurm::default());
        env.cluster = slurm.clone();
        let mut c = ClusterCollector::new(env);
        let s = c
            .sample(&SampleContext {
                now: rtop_core::Timestamp::from_millis(1000),
                sequence: 1,
            })
            .unwrap();
        // Agents on the other nodes only, each asking for its node's GPUs.
        let mut launched = slurm.launches.lock().unwrap().clone();
        launched.sort();
        assert_eq!(launched, [("n2".to_owned(), 1), ("n3".to_owned(), 0)]);
        assert!(
            s.degraded
                .unwrap()
                .contains("n3: srun: error: Unable to create step")
        );
        let mut g = SystemGraph::default();
        g.apply(s.patch);
        assert_eq!(g.nodes_of(EntityKind::ClusterNode).count(), 3);
        let n1 = g.node(&node_id("n1")).unwrap();
        assert_eq!(n1.text("agent"), Some("local"));
        assert_eq!(n1.f64("slurm_gpus_allocated"), Some(2.0));
        let n2 = g.node(&node_id("n2")).unwrap();
        assert_eq!(n2.text("slurm_state"), Some("ALLOCATED"));
        assert_eq!(n2.f64("gpu_usage_percent"), Some(80.0));
        assert_eq!(n2.f64("cpu_usage_percent"), Some(12.5));
        let gpu = g
            .node(&EntityId::new(EntityKind::Gpu, "n2/0000:01:00.0"))
            .expect("remote GPU");
        assert_eq!(
            (gpu.text("node"), gpu.text("name")),
            (Some("n2"), Some("Remote GPU"))
        );
        assert!(
            g.node(&node_id("n3"))
                .unwrap()
                .text("agent")
                .unwrap()
                .starts_with("stopped")
        );
    }

    #[test]
    fn outside_slurm_the_collector_is_unsupported() {
        let fs = rtop_linux::MemoryKernelFs::new();
        fs.file("/proc/9/cgroup", "0::/user.slice/session-1.scope\n");
        let mut env = Env::simulated(Arc::new(fs), Arc::new(crate::Subscriptions::default()));
        env.self_pid = Some(9);
        let e = ClusterCollector::new(env)
            .sample(&SampleContext {
                now: rtop_core::Timestamp::from_millis(1000),
                sequence: 1,
            })
            .unwrap_err();
        assert!(format!("{e:?}").contains("not running in a Slurm job"));
    }

    #[test]
    fn hostlists_expand() {
        assert_eq!(
            expand_hostlist("n[457,466,566-567],gpu01", 100),
            ["n457", "n466", "n566", "n567", "gpu01"]
        );
        assert_eq!(expand_hostlist("a[001-003]", 100), ["a001", "a002", "a003"]);
        assert_eq!(expand_hostlist("x[1-1000000]", 3).len(), 3);
    }

    #[test]
    fn scontrol_nodes_parse() {
        let t = "NodeName=n466 Arch=aarch64 CPUAlloc=288 CPUEfctv=288 CPUTot=288 CPULoad=5.27 Gres=gpu:nvidia_gh200_120gb:4(S:0-3) RealMemory=485854 AllocMem=420192 FreeMem=258452 State=ALLOCATED Partitions=gpu AllocTRES=cpu=288,mem=420192M,gres/gpu=4,gres/gpu:nvidia_gh200_120gb=4\n";
        let n = &parse_nodes(t)[0];
        assert_eq!(n.name, "n466");
        assert_eq!(
            (n.cpus, n.cpus_allocated, n.gpus, n.gpus_allocated),
            (Some(288), Some(288), Some(4), Some(4))
        );
        assert_eq!(n.memory_bytes, Some(485_854 << 20));
        assert_eq!(n.cpu_load, Some(5.27));
        assert_eq!(n.state, "ALLOCATED");
        assert_eq!(
            job_nodelist("JobId=5 NodeList=n[1-2] BatchHost=n1"),
            Some("n[1-2]")
        );
    }

    #[test]
    fn reports_round_trip_registered_keys_only() {
        let mut g = SystemGraph::default();
        let mut p =
            rtop_core::GraphPatch::new(CollectorId("t"), 1, rtop_core::Timestamp::from_millis(1));
        p.node(
            NodeUpdate::new(
                EntityKind::Gpu,
                EntityId::new(EntityKind::Gpu, "0000:01:00.0"),
            )
            .text("name", "GPU")
            .attr("index", 3u32)
            .metric("nvlink_links", Metric::u64(18))
            .attr("secret_thing", "x")
            .metric("gpu_usage_percent", Metric::f64(42.0))
            .metric("power_w", Metric::unavailable(Unavailable::Unsupported)),
        );
        g.apply(p);
        let r = decode_report(&encode_report(&g, "n9")).unwrap();
        assert_eq!(r.host, "n9");
        let gpu = &r.nodes[0];
        assert_eq!(
            (gpu.kind, gpu.key.as_str()),
            (EntityKind::Gpu, "0000:01:00.0")
        );
        assert!(gpu.attrs.contains(&("index", Attr::UInt(3))));
        assert!(!gpu.attrs.iter().any(|(k, _)| *k == "secret_thing"));
        assert!(
            gpu.metrics
                .contains(&("gpu_usage_percent", Metric::f64(42.0)))
        );
        assert!(gpu.metrics.contains(&("nvlink_links", Metric::u64(18))));
        assert!(
            gpu.metrics
                .contains(&("power_w", Metric::unavailable(Unavailable::Unsupported)))
        );
        assert!(decode_report(r#"{"v":99,"host":"x","nodes":[]}"#).is_err());
    }
}
