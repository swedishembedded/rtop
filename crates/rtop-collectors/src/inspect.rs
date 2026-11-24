// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! On-demand deep inspection of one process (progressive reveal level 3).
//!
//! These reads are too expensive to repeat for every process every second
//! (smaps walks every mapping, fd tables can hold millions of entries), so
//! they happen only for the process the user opened, bounded in size, and
//! only after confirming the PID still belongs to the same process.
//! Environment variables are never read: they routinely hold credentials.

use rtop_core::ProcessIdentity;
use rtop_core::text::sanitize;
use rtop_linux::KernelError;
use rtop_linux::fs::{KernelFs, KernelFsExt};
use rtop_linux::parse::pid::{self, FdTarget, SmapsRollup};

use crate::SMALL;

const MAX_FDS: usize = 4096;
const MAX_THREADS: usize = 4096;

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadInfo {
    pub tid: u32,
    pub name: String,
    pub state: &'static str,
    pub cpu_time_seconds: f64,
    pub last_cpu: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FdInfo {
    pub fd: u32,
    pub kind: &'static str,
    pub target: String,
    pub socket_inode: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessDetail {
    pub identity: ProcessIdentity,
    pub exe: Result<String, KernelError>,
    pub cwd: Result<String, KernelError>,
    pub memory: Result<SmapsRollup, KernelError>,
    pub threads: Vec<ThreadInfo>,
    pub threads_truncated: bool,
    pub fds: Result<Vec<FdInfo>, KernelError>,
    pub fds_truncated: bool,
    pub limits: Result<Vec<(String, String, String)>, KernelError>,
    pub oom_score: Option<i64>,
    pub wchan: Option<String>,
    pub namespaces: Vec<(String, String)>,
}

/// Inspects `identity`; fails with `NotFound` if the PID now belongs to a
/// different process (or none).
pub fn inspect(
    fs: &dyn KernelFs,
    identity: ProcessIdentity,
    clock_ticks: u64,
) -> Result<ProcessDetail, KernelError> {
    let base = format!("/proc/{}", identity.pid);
    let verify = |fs: &dyn KernelFs| -> Result<(), KernelError> {
        let mut buf = Vec::new();
        fs.read(&format!("{base}/stat"), &mut buf, SMALL)?;
        match pid::stat(&buf) {
            Some(s) if s.starttime == identity.start_time_ticks => Ok(()),
            _ => Err(KernelError::NotFound),
        }
    };
    verify(fs)?;
    let ticks = clock_ticks.max(1) as f64;

    let mut threads = Vec::new();
    let mut threads_truncated = false;
    let mut buf = Vec::new();
    let mut tids: Vec<u32> = fs
        .read_dir(&format!("{base}/task"))
        .unwrap_or_default()
        .iter()
        .filter_map(|t| t.parse().ok())
        .collect();
    tids.sort_unstable();
    for tid in tids {
        if threads.len() >= MAX_THREADS {
            threads_truncated = true;
            break;
        }
        if fs
            .read(&format!("{base}/task/{tid}/stat"), &mut buf, SMALL)
            .is_err()
        {
            continue;
        }
        if let Some(st) = pid::stat(&buf) {
            threads.push(ThreadInfo {
                tid,
                name: sanitize(&String::from_utf8_lossy(&st.comm)).into_owned(),
                state: pid::state_name(st.state),
                cpu_time_seconds: (st.utime + st.stime) as f64 / ticks,
                last_cpu: st.processor,
            });
        }
    }

    let mut fds_truncated = false;
    let fds = fs.read_dir(&format!("{base}/fd")).map(|names| {
        let mut nums: Vec<u32> = names.iter().filter_map(|n| n.parse().ok()).collect();
        nums.sort_unstable();
        if nums.len() > MAX_FDS {
            fds_truncated = true;
            nums.truncate(MAX_FDS);
        }
        nums.into_iter()
            .filter_map(|fd| {
                let link = fs.read_link(&format!("{base}/fd/{fd}")).ok()?;
                let target = sanitize(&link).into_owned();
                Some(match pid::fd_target(&link) {
                    FdTarget::Socket(ino) => FdInfo {
                        fd,
                        kind: "socket",
                        target,
                        socket_inode: Some(ino),
                    },
                    FdTarget::Pipe(_) => FdInfo {
                        fd,
                        kind: "pipe",
                        target,
                        socket_inode: None,
                    },
                    FdTarget::AnonInode(_) => FdInfo {
                        fd,
                        kind: "anon_inode",
                        target,
                        socket_inode: None,
                    },
                    FdTarget::Path(p) if p.starts_with("/dev/") => FdInfo {
                        fd,
                        kind: "device",
                        target,
                        socket_inode: None,
                    },
                    FdTarget::Path(_) => FdInfo {
                        fd,
                        kind: "file",
                        target,
                        socket_inode: None,
                    },
                })
            })
            .collect()
    });

    let limits = fs.read_string(&format!("{base}/limits"), SMALL).map(|t| {
        t.lines()
            .skip(1)
            .filter_map(|l| {
                // Columns are fixed-width: name (26), soft (21), hard (21), units.
                let name = l.get(..26)?.trim().to_owned();
                let soft = l.get(26..47)?.trim().to_owned();
                let hard = l.get(47..68)?.trim().to_owned();
                Some((name, soft, hard))
            })
            .collect()
    });

    let mut namespaces = Vec::new();
    for ns in ["cgroup", "ipc", "mnt", "net", "pid", "time", "user", "uts"] {
        if let Ok(l) = fs.read_link(&format!("{base}/ns/{ns}")) {
            namespaces.push((ns.to_owned(), l));
        }
    }

    let detail = ProcessDetail {
        identity,
        exe: fs
            .read_link(&format!("{base}/exe"))
            .map(|s| sanitize(&s).into_owned()),
        cwd: fs
            .read_link(&format!("{base}/cwd"))
            .map(|s| sanitize(&s).into_owned()),
        memory: fs
            .read_string(&format!("{base}/smaps_rollup"), SMALL)
            .map(|t| pid::smaps_rollup(&t)),
        threads,
        threads_truncated,
        fds,
        fds_truncated,
        limits,
        oom_score: fs.read_i64(&format!("{base}/oom_score")).ok(),
        wchan: fs
            .read_attr(&format!("{base}/wchan"))
            .ok()
            .filter(|w| !w.is_empty() && w != "0")
            .map(|w| sanitize(&w).into_owned()),
        namespaces,
    };
    // The PID could have been recycled while we read: verify again so the
    // result never mixes two processes.
    verify(fs)?;
    Ok(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspects_own_process_and_rejects_wrong_identity() {
        let fs = rtop_linux::RealKernelFs::live();
        let me = std::process::id();
        let mut buf = Vec::new();
        fs.read(&format!("/proc/{me}/stat"), &mut buf, SMALL)
            .unwrap();
        let start = pid::stat(&buf).unwrap().starttime;
        let d = inspect(
            &fs,
            ProcessIdentity {
                pid: me,
                start_time_ticks: start,
            },
            100,
        )
        .unwrap();
        assert!(!d.threads.is_empty());
        assert!(
            d.fds
                .unwrap()
                .iter()
                .any(|f| f.fd == 0 || f.fd == 1 || f.fd == 2)
        );
        assert!(d.exe.is_ok());
        assert_eq!(
            inspect(
                &fs,
                ProcessIdentity {
                    pid: me,
                    start_time_ticks: start + 1
                },
                100
            ),
            Err(KernelError::NotFound)
        );
    }
}
