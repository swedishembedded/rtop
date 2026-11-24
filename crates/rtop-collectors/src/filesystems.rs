// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Mounted filesystems and their capacity.
//!
//! `statvfs()` on a remote filesystem whose server is gone blocks in the
//! kernel indefinitely, and no Rust timeout can interrupt it. Remote mounts
//! are therefore queried on helper threads with a deadline: a mount whose
//! previous query is still stuck is reported as timed out and never gets a
//! second thread, so at most one thread per dead mount is ever parked.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rtop_core::{
    CollectorId, EdgeKind, EdgeUpdate, EntityId, EntityKind, Evidence, Metric, NodeUpdate, Scope,
    Unavailable,
};
use rtop_linux::fs::{FsUsage, KernelError, KernelFsExt};
use rtop_linux::parse::disk;

use crate::{Collector, CollectorError, Descriptor, Env, Isolation, LARGE, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("filesystems");
const REMOTE_DEADLINE: Duration = Duration::from_millis(500);
const MAX_MOUNTS: usize = 4096;

#[derive(Debug)]
pub struct FilesystemCollector {
    env: Env,
    stuck: Arc<Mutex<HashSet<String>>>,
}

impl FilesystemCollector {
    pub fn new(env: Env) -> Self {
        FilesystemCollector {
            env,
            stuck: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn usage(&self, mount: &str, remote: bool) -> Result<FsUsage, Unavailable> {
        if !remote {
            return self
                .env
                .fs
                .statvfs(mount)
                .map_err(|e| crate::unavailable_from(&e));
        }
        if self
            .stuck
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(mount)
        {
            return Err(Unavailable::TimedOut);
        }
        let (tx, rx) = mpsc::channel();
        let (fs, m, stuck) = (self.env.fs.clone(), mount.to_owned(), self.stuck.clone());
        stuck
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(m.clone());
        let spawned = std::thread::Builder::new()
            .name("rtop-statvfs".into())
            .spawn(move || {
                let r = fs.statvfs(&m);
                stuck.lock().unwrap_or_else(|p| p.into_inner()).remove(&m);
                let _ = tx.send(r);
            });
        if spawned.is_err() {
            self.stuck
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(mount);
            return Err(Unavailable::TimedOut);
        }
        match rx.recv_timeout(REMOTE_DEADLINE) {
            Ok(r) => r.map_err(|e: KernelError| crate::unavailable_from(&e)),
            Err(_) => Err(Unavailable::TimedOut),
        }
    }
}

pub fn mount_id(mount_point: &str) -> EntityId {
    EntityId::new(EntityKind::Mount, mount_point)
}

impl Collector for FilesystemCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(5),
            timeout: Duration::from_secs(3),
            isolation: Isolation::MayBlock,
            essential: false,
            nice: 5,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let fs = self.env.fs.clone();
        let text = fs
            .read_string("/proc/self/mountinfo", LARGE)
            .map_err(|e| CollectorError::from_kernel("/proc/self/mountinfo", &e))?;
        let mut patch = ctx
            .patch(ID)
            .complete(Scope::Nodes(EntityKind::Mount))
            .complete(Scope::Edges(EdgeKind::BackedBy));
        let mut seen = HashSet::new();
        let mut timed_out = 0;
        let mut devnames: HashMap<(u32, u32), Option<String>> = HashMap::new();
        for m in disk::mountinfo(&text)
            .into_iter()
            .filter(|m| !disk::is_pseudo_fs(&m.fs_type))
            .take(MAX_MOUNTS)
        {
            // Overmounted duplicates: keep the first (outermost listing order).
            if !seen.insert(m.mount_point.clone()) {
                continue;
            }
            let remote = disk::is_remote_fs(&m.fs_type);
            let id = mount_id(&m.mount_point);
            let mut n = NodeUpdate::new(EntityKind::Mount, id.clone())
                .text("name", &m.mount_point)
                .text("mount_point", &m.mount_point)
                .text("fs_type", &m.fs_type)
                .text("source", &m.source)
                .text("root", &m.root)
                .attr("read_only", m.options.split(',').any(|o| o == "ro"))
                .attr("remote", remote);
            match self.usage(&m.mount_point, remote) {
                Ok(u) => {
                    let used = u.total_bytes.saturating_sub(u.free_bytes);
                    n.push_metric("capacity_bytes", Metric::u64(u.total_bytes));
                    n.push_metric("used_bytes", Metric::u64(used));
                    n.push_metric("available_bytes", Metric::u64(u.available_bytes));
                    // As df: used / (used + available), reserved blocks excluded.
                    let denom = used + u.available_bytes;
                    n.push_metric(
                        "used_percent",
                        if denom == 0 {
                            Metric::unavailable(Unavailable::NotMeasurable)
                        } else {
                            Metric::f64(100.0 * used as f64 / denom as f64)
                        },
                    );
                    n.push_metric("inodes_total", Metric::u64(u.total_inodes));
                    n.push_metric("inodes_free", Metric::u64(u.free_inodes));
                }
                Err(reason) => {
                    if reason == Unavailable::TimedOut {
                        timed_out += 1;
                    }
                    for k in [
                        "capacity_bytes",
                        "used_bytes",
                        "available_bytes",
                        "used_percent",
                    ] {
                        n.push_metric(k, Metric::unavailable(reason));
                    }
                }
            }
            patch.node(n);
            if m.major != 0 {
                let name = devnames
                    .entry((m.major, m.minor))
                    .or_insert_with(|| {
                        fs.read_link(&format!("/sys/dev/block/{}:{}", m.major, m.minor))
                            .ok()
                            .and_then(|l| l.rsplit('/').next().map(str::to_owned))
                    })
                    .clone();
                if let Some(dev) = name {
                    patch.edge(EdgeUpdate::new(
                        EdgeKind::BackedBy,
                        id,
                        crate::storage::block_device_id(&dev),
                        Evidence::KernelConfirmed,
                    ));
                }
            }
        }
        let degraded = (timed_out > 0)
            .then(|| format!("{timed_out} remote filesystem(s) did not answer statvfs"));
        Ok(Sample { patch, degraded })
    }
}
