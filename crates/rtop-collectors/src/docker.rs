// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Docker Engine metadata: names, images, lifecycle, mounts and network.
//!
//! Resource accounting comes from cgroups (see [`crate::cgroups`]); this
//! collector adds what only the daemon knows. It talks HTTP/1.0 over the
//! Engine's Unix socket with strict timeouts and response size limits, and
//! never changes permissions on, or escalates access to, that socket: when
//! it is not readable, containers stay visible from cgroups alone.
//!
//! Volumes report the backing filesystem's capacity, never as if it were
//! the volume's own usage: exclusive per-volume usage is not cheaply
//! measurable and is reported as such.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker};
use rtop_core::{
    CollectorId, EdgeKind, EdgeUpdate, EntityId, EntityKind, Evidence, Metric, NodeUpdate, Scope,
    Unavailable,
};
use rtop_linux::fs::KernelFsExt;
use rtop_linux::parse::{disk, net};
use serde_json::Value;

use crate::cgroups::container_id;
use crate::{
    Collector, CollectorError, Descriptor, Env, Isolation, LARGE, SMALL, Sample, SampleContext,
};

pub const ID: CollectorId = CollectorId("docker");
const MAX_RESPONSE: usize = 32 << 20;
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// Minimal Docker Engine API client.
pub trait DockerApi: Send + std::fmt::Debug {
    fn get(&self, path: &str) -> Result<Value, CollectorError>;
}

#[derive(Debug)]
pub struct UnixSocketApi {
    socket: String,
}

impl UnixSocketApi {
    pub fn new(socket: impl Into<String>) -> Self {
        UnixSocketApi {
            socket: socket.into(),
        }
    }
}

fn io_err(e: &std::io::Error) -> CollectorError {
    let reason = match e.kind() {
        std::io::ErrorKind::PermissionDenied => Unavailable::PermissionDenied,
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
            Unavailable::Unsupported
        }
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Unavailable::TimedOut,
        _ => Unavailable::Unsupported,
    };
    CollectorError::new(reason, format!("docker socket: {e}"))
}

impl DockerApi for UnixSocketApi {
    fn get(&self, path: &str) -> Result<Value, CollectorError> {
        let mut s = UnixStream::connect(&self.socket).map_err(|e| io_err(&e))?;
        s.set_read_timeout(Some(IO_TIMEOUT))
            .map_err(|e| io_err(&e))?;
        s.set_write_timeout(Some(IO_TIMEOUT))
            .map_err(|e| io_err(&e))?;
        // HTTP/1.0: the daemon closes after the body and never chunks it.
        write!(
            s,
            "GET {path} HTTP/1.0\r\nHost: docker\r\nUser-Agent: rtop\r\n\r\n"
        )
        .map_err(|e| io_err(&e))?;
        let mut body = Vec::new();
        s.take(MAX_RESPONSE as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|e| io_err(&e))?;
        if body.len() > MAX_RESPONSE {
            return Err(CollectorError::new(
                Unavailable::InvalidData,
                "docker response too large",
            ));
        }
        parse_http_json(&body)
    }
}

/// Splits an HTTP/1.x response and decodes its JSON body.
pub fn parse_http_json(resp: &[u8]) -> Result<Value, CollectorError> {
    let bad = |what: &str| CollectorError::new(Unavailable::InvalidData, format!("docker: {what}"));
    let split = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| bad("no header terminator"))?;
    let head = std::str::from_utf8(&resp[..split]).map_err(|_| bad("non-UTF-8 headers"))?;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("no status"))?;
    if status != 200 {
        return Err(CollectorError::new(
            if status == 403 {
                Unavailable::PermissionDenied
            } else {
                Unavailable::InvalidData
            },
            format!("docker: HTTP {status}"),
        ));
    }
    serde_json::from_slice(&resp[split + 4..]).map_err(|_| bad("invalid JSON"))
}

#[derive(Debug, Clone)]
struct Inspected {
    pid: Option<u32>,
    health: Option<String>,
    restarts: Option<u64>,
}

#[derive(Debug)]
pub struct DockerCollector {
    env: Env,
    api: Option<Box<dyn DockerApi>>,
    inspected: HashMap<String, Inspected>,
    rates: RateTracker<(String, bool)>,
}

impl DockerCollector {
    pub fn new(env: Env) -> Self {
        let api = env
            .docker_socket
            .clone()
            .map(|s| Box::new(UnixSocketApi::new(s)) as Box<dyn DockerApi>);
        DockerCollector {
            env,
            api,
            inspected: HashMap::new(),
            rates: RateTracker::new(),
        }
    }

    pub fn with_api(env: Env, api: Box<dyn DockerApi>) -> Self {
        DockerCollector {
            env,
            api: Some(api),
            inspected: HashMap::new(),
            rates: RateTracker::new(),
        }
    }
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

impl Collector for DockerCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(5),
            isolation: Isolation::MayBlock,
            essential: false,
            nice: 5,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let api = self.api.as_ref().ok_or_else(|| {
            CollectorError::new(Unavailable::Unsupported, "no Docker socket configured")
        })?;
        let list = api.get("/containers/json?all=1")?;
        let containers = list.as_array().ok_or_else(|| {
            CollectorError::new(Unavailable::InvalidData, "container list is not an array")
        })?;
        let mounts_text = self
            .env
            .fs
            .read_string("/proc/self/mountinfo", LARGE)
            .unwrap_or_default();
        let mut host_mounts: Vec<String> = disk::mountinfo(&mounts_text)
            .into_iter()
            .filter(|m| !disk::is_pseudo_fs(&m.fs_type))
            .map(|m| m.mount_point)
            .collect();
        host_mounts.sort_by_key(|m| std::cmp::Reverse(m.len()));

        let mut patch = ctx
            .patch(ID)
            .complete(Scope::Nodes(EntityKind::Container))
            .complete(Scope::Nodes(EntityKind::Volume))
            .complete(Scope::Edges(EdgeKind::Mounts))
            .complete(Scope::Edges(EdgeKind::BackedBy));
        let mut degraded = Vec::new();
        let mut live = Vec::new();
        self.rates.begin();
        for c in containers {
            let Some(full_id) = s(c, "Id") else { continue };
            live.push(full_id.to_owned());
            let id = container_id(full_id);
            let name = c
                .get("Names")
                .and_then(|n| n.get(0))
                .and_then(Value::as_str)
                .unwrap_or(full_id)
                .trim_start_matches('/');
            let state = s(c, "State").unwrap_or("unknown");
            let mut n = NodeUpdate::new(EntityKind::Container, id.clone())
                .text("name", name)
                .text("image", s(c, "Image").unwrap_or(""))
                .attr("state", state)
                .text("status", s(c, "Status").unwrap_or(""))
                .attr("runtime", "docker")
                .attr("container_id", full_id.to_owned());
            if state == "running" && !self.inspected.contains_key(full_id) {
                match api.get(&format!("/containers/{full_id}/json")) {
                    Ok(i) => {
                        let st = i.get("State");
                        self.inspected.insert(
                            full_id.to_owned(),
                            Inspected {
                                pid: st
                                    .and_then(|s| s.get("Pid"))
                                    .and_then(Value::as_u64)
                                    .and_then(|p| u32::try_from(p).ok())
                                    .filter(|p| *p > 0),
                                health: st
                                    .and_then(|s| s.get("Health"))
                                    .and_then(|h| s_owned(h, "Status")),
                                restarts: i.get("RestartCount").and_then(Value::as_u64),
                            },
                        );
                    }
                    Err(e) => degraded.push(e.detail),
                }
            }
            if state != "running" {
                self.inspected.remove(full_id);
            }
            if let Some(info) = self.inspected.get(full_id) {
                if let Some(h) = &info.health {
                    n = n.text("health", h);
                }
                if let Some(r) = info.restarts {
                    n.push_metric("restarts_total", Metric::u64(r));
                }
                // The container's own network namespace, seen through its init process.
                if let Some(pid) = info.pid {
                    if let Ok(dev) = self
                        .env
                        .fs
                        .read_string(&format!("/proc/{pid}/net/dev"), SMALL)
                    {
                        let (rx, tx) = net::net_dev(&dev)
                            .iter()
                            .filter(|d| d.name != "lo")
                            .fold((0u64, 0u64), |a, d| (a.0 + d.rx_bytes, a.1 + d.tx_bytes));
                        n.push_metric(
                            "network_rx_bytes_per_second",
                            self.rates.metric(
                                (full_id.to_owned(), false),
                                rx,
                                ctx.now,
                                CounterWidth::Bits64,
                            ),
                        );
                        n.push_metric(
                            "network_tx_bytes_per_second",
                            self.rates.metric(
                                (full_id.to_owned(), true),
                                tx,
                                ctx.now,
                                CounterWidth::Bits64,
                            ),
                        );
                    }
                    n = n.attr("init_pid", pid);
                }
            }
            patch.node(n);

            for m in c
                .get("Mounts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let ty = s(m, "Type").unwrap_or("unknown");
                let src = s(m, "Source").unwrap_or("");
                let dest = s(m, "Destination").unwrap_or("");
                let vname = s(m, "Name").filter(|n| !n.is_empty()).unwrap_or(src);
                if vname.is_empty() {
                    continue;
                }
                let vid = EntityId::new(EntityKind::Volume, vname);
                patch.node(
                    NodeUpdate::new(EntityKind::Volume, vid.clone())
                        .text("name", vname)
                        .attr("mount_type", ty)
                        .text("source", src)
                        .metric(
                            "exclusive_bytes",
                            Metric::unavailable(Unavailable::NotMeasurable),
                        ),
                );
                patch.edge(
                    EdgeUpdate::new(
                        EdgeKind::Mounts,
                        id.clone(),
                        vid.clone(),
                        Evidence::KernelConfirmed,
                    )
                    .attr("destination", dest.to_owned())
                    .attr(
                        "read_write",
                        m.get("RW").and_then(Value::as_bool).unwrap_or(true),
                    ),
                );
                if let Some(mp) = host_mounts.iter().find(|mp| {
                    src.starts_with(mp.as_str())
                        && (mp.as_str() == "/"
                            || src[mp.len()..].starts_with('/')
                            || src.len() == mp.len())
                }) {
                    patch.edge(EdgeUpdate::new(
                        EdgeKind::BackedBy,
                        vid,
                        crate::filesystems::mount_id(mp),
                        Evidence::Inferred,
                    ));
                }
            }
        }
        self.rates.sweep();
        self.inspected.retain(|k, _| live.contains(k));
        let degraded = (!degraded.is_empty()).then(|| degraded.join("; "));
        Ok(Sample { patch, degraded })
    }
}

fn s_owned(v: &Value, k: &str) -> Option<String> {
    s(v, k).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_responses_are_validated() {
        assert_eq!(
            parse_http_json(b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n[1]")
                .unwrap(),
            serde_json::json!([1])
        );
        assert_eq!(
            parse_http_json(b"HTTP/1.0 403 Forbidden\r\n\r\n{}")
                .unwrap_err()
                .reason,
            Unavailable::PermissionDenied
        );
        assert!(parse_http_json(b"garbage").is_err());
        assert!(parse_http_json(b"HTTP/1.0 200 OK\r\n\r\n{bad").is_err());
    }
}
