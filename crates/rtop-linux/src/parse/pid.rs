// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Per-process files under `/proc/PID/`.

use super::{find_key, key_values};

/// Fields of `/proc/PID/stat` that rtop uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidStat {
    pub pid: u32,
    /// Raw task name bytes; may contain spaces, parentheses and newlines.
    pub comm: Vec<u8>,
    pub state: u8,
    pub ppid: u32,
    pub pgrp: i32,
    pub session: i32,
    pub tty_nr: i32,
    pub minflt: u64,
    pub majflt: u64,
    pub utime: u64,
    pub stime: u64,
    pub priority: i64,
    pub nice: i64,
    pub num_threads: u64,
    /// Clock ticks since boot; with `pid` it identifies the process.
    pub starttime: u64,
    pub vsize: u64,
    /// Resident pages.
    pub rss_pages: u64,
    pub processor: Option<u32>,
    /// Aggregated block I/O delay in clock ticks (field 42).
    pub delayacct_blkio_ticks: Option<u64>,
}

/// Parses `/proc/PID/stat`. The command name is delimited by the *last*
/// `)` because the name itself may contain `)`.
pub fn stat(buf: &[u8]) -> Option<PidStat> {
    let open = buf.iter().position(|&b| b == b'(')?;
    let close = buf.iter().rposition(|&b| b == b')')?;
    if close < open {
        return None;
    }
    let pid = std::str::from_utf8(&buf[..open])
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let comm = buf[open + 1..close].to_vec();
    let rest = std::str::from_utf8(buf.get(close + 1..)?).ok()?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    // f[0] is field 3 (state).
    let n = |i: usize| -> Option<u64> { f.get(i - 3)?.parse().ok() };
    let s = |i: usize| -> Option<i64> { f.get(i - 3)?.parse().ok() };
    Some(PidStat {
        pid,
        comm,
        state: *f.first()?.as_bytes().first()?,
        ppid: u32::try_from(n(4)?).ok()?,
        pgrp: i32::try_from(s(5)?).ok()?,
        session: i32::try_from(s(6)?).ok()?,
        tty_nr: i32::try_from(s(7)?).ok()?,
        minflt: n(10)?,
        majflt: n(12)?,
        utime: n(14)?,
        stime: n(15)?,
        priority: s(18)?,
        nice: s(19)?,
        num_threads: n(20)?,
        starttime: n(22)?,
        vsize: n(23)?,
        rss_pages: n(24).or_else(|| s(24).map(|v| v.max(0) as u64))?,
        processor: n(39).and_then(|v| u32::try_from(v).ok()),
        delayacct_blkio_ticks: n(42),
    })
}

/// Human description of a process state letter.
pub fn state_name(state: u8) -> &'static str {
    match state {
        b'R' => "running",
        b'S' => "sleeping",
        b'D' => "disk_sleep",
        b'Z' => "zombie",
        b'T' => "stopped",
        b't' => "tracing_stop",
        b'X' | b'x' => "dead",
        b'I' => "idle",
        b'P' => "parked",
        b'W' => "waking",
        b'K' => "wakekill",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PidStatus {
    pub real_uid: Option<u32>,
    pub effective_uid: Option<u32>,
    pub threads: Option<u64>,
    pub vm_rss: Option<u64>,
    pub rss_anon: Option<u64>,
    pub rss_file: Option<u64>,
    pub rss_shmem: Option<u64>,
    pub vm_swap: Option<u64>,
    pub voluntary_ctxt_switches: Option<u64>,
    pub nonvoluntary_ctxt_switches: Option<u64>,
    /// Innermost namespace PID (`NSpid` last element).
    pub ns_pid: Option<u32>,
}

pub fn status(text: &str) -> PidStatus {
    let mut st = PidStatus::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k {
            "Uid" => {
                let mut ids = v.split_whitespace().map(|x| x.parse().ok());
                st.real_uid = ids.next().flatten();
                st.effective_uid = ids.next().flatten();
            }
            "NSpid" => st.ns_pid = v.split_whitespace().last().and_then(|x| x.parse().ok()),
            _ => {}
        }
    }
    for (k, v) in key_values(text) {
        match k {
            "Threads" => st.threads = Some(v),
            "VmRSS" => st.vm_rss = Some(v),
            "RssAnon" => st.rss_anon = Some(v),
            "RssFile" => st.rss_file = Some(v),
            "RssShmem" => st.rss_shmem = Some(v),
            "VmSwap" => st.vm_swap = Some(v),
            "voluntary_ctxt_switches" => st.voluntary_ctxt_switches = Some(v),
            "nonvoluntary_ctxt_switches" => st.nonvoluntary_ctxt_switches = Some(v),
            _ => {}
        }
    }
    st
}

/// NUL-separated arguments joined with spaces. Empty for kernel threads.
pub fn cmdline(buf: &[u8]) -> String {
    let trimmed = buf.strip_suffix(&[0]).unwrap_or(buf);
    let parts: Vec<String> = trimmed
        .split(|&b| b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    parts.join(" ")
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PidIo {
    pub rchar: u64,
    pub wchar: u64,
    pub syscr: u64,
    pub syscw: u64,
    /// Bytes fetched from the storage layer.
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cancelled_write_bytes: u64,
}

pub fn io(text: &str) -> Option<PidIo> {
    Some(PidIo {
        rchar: find_key(text, "rchar")?,
        wchar: find_key(text, "wchar")?,
        syscr: find_key(text, "syscr")?,
        syscw: find_key(text, "syscw")?,
        read_bytes: find_key(text, "read_bytes")?,
        write_bytes: find_key(text, "write_bytes")?,
        cancelled_write_bytes: find_key(text, "cancelled_write_bytes").unwrap_or(0),
    })
}

/// cgroup v2 path from `/proc/PID/cgroup` (`0::/path`).
pub fn cgroup_v2_path(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::trim)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SmapsRollup {
    pub rss: Option<u64>,
    pub pss: Option<u64>,
    pub pss_anon: Option<u64>,
    pub pss_file: Option<u64>,
    pub shared: Option<u64>,
    pub private: Option<u64>,
    pub swap: Option<u64>,
    pub swap_pss: Option<u64>,
}

pub fn smaps_rollup(text: &str) -> SmapsRollup {
    let mut r = SmapsRollup::default();
    let (mut sc, mut sd, mut pc, mut pd) = (None, None, None, None);
    for (k, v) in key_values(text) {
        match k {
            "Rss" => r.rss = Some(v),
            "Pss" => r.pss = Some(v),
            "Pss_Anon" => r.pss_anon = Some(v),
            "Pss_File" => r.pss_file = Some(v),
            "Shared_Clean" => sc = Some(v),
            "Shared_Dirty" => sd = Some(v),
            "Private_Clean" => pc = Some(v),
            "Private_Dirty" => pd = Some(v),
            "Swap" => r.swap = Some(v),
            "SwapPss" => r.swap_pss = Some(v),
            _ => {}
        }
    }
    r.shared = sc.zip(sd).map(|(a, b)| a + b);
    r.private = pc.zip(pd).map(|(a, b)| a + b);
    r
}

/// What a `/proc/PID/fd/N` link points to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FdTarget {
    Socket(u64),
    Pipe(u64),
    AnonInode(String),
    Path(String),
}

pub fn fd_target(link: &str) -> FdTarget {
    let bracketed = |prefix: &str| {
        link.strip_prefix(prefix)
            .and_then(|r| r.strip_suffix(']'))
            .and_then(|n| n.parse().ok())
    };
    if let Some(ino) = bracketed("socket:[") {
        FdTarget::Socket(ino)
    } else if let Some(ino) = bracketed("pipe:[") {
        FdTarget::Pipe(ino)
    } else if let Some(kind) = link.strip_prefix("anon_inode:") {
        FdTarget::AnonInode(kind.to_owned())
    } else {
        FdTarget::Path(link.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn line(comm: &str) -> String {
        format!(
            "1234 ({comm}) S 1 1234 1234 0 -1 4194560 500 0 7 0 120 30 0 0 20 0 4 0 98765 1048576 256 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 11 0 0\n"
        )
    }

    #[test]
    fn stat_handles_hostile_names() {
        for name in ["chrome", "a b", "evil) R 9 9", "(paren)", "new\nline", ""] {
            let s = stat(line(name).as_bytes()).unwrap_or_else(|| panic!("{name:?}"));
            assert_eq!(s.comm, name.as_bytes());
            assert_eq!(
                (s.pid, s.state, s.ppid, s.utime, s.stime),
                (1234, b'S', 1, 120, 30)
            );
            assert_eq!(
                (s.num_threads, s.starttime, s.rss_pages, s.processor),
                (4, 98765, 256, Some(3))
            );
            assert_eq!(s.delayacct_blkio_ticks, Some(11));
        }
    }

    #[test]
    fn truncated_stat_is_rejected() {
        assert!(stat(b"1234 (x) S 1 2").is_none());
        assert!(stat(b"garbage").is_none());
    }

    #[test]
    fn status_and_io() {
        let st = status(
            "Name:\tx\nUid:\t1000\t1001\t1000\t1000\nThreads:\t7\nVmRSS:\t  2048 kB\nVmSwap:\t0 kB\nNSpid:\t400\t5\n",
        );
        assert_eq!(
            (
                st.real_uid,
                st.effective_uid,
                st.threads,
                st.vm_rss,
                st.vm_swap
            ),
            (Some(1000), Some(1001), Some(7), Some(2 << 20), Some(0))
        );
        assert_eq!(st.ns_pid, Some(5));
        let io = io("rchar: 1\nwchar: 2\nsyscr: 3\nsyscw: 4\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n").unwrap();
        assert_eq!((io.read_bytes, io.write_bytes), (4096, 8192));
        assert!(super::io("rchar: 1\n").is_none());
    }

    #[test]
    fn cmdline_and_fd_targets() {
        assert_eq!(cmdline(b"/bin/sh\0-c\0\0echo hi\0"), "/bin/sh -c  echo hi");
        assert_eq!(cmdline(b""), "");
        assert_eq!(fd_target("socket:[4242]"), FdTarget::Socket(4242));
        assert_eq!(fd_target("pipe:[7]"), FdTarget::Pipe(7));
        assert_eq!(fd_target("/dev/null"), FdTarget::Path("/dev/null".into()));
        assert_eq!(
            cgroup_v2_path("0::/user.slice/app.scope\n"),
            Some("/user.slice/app.scope")
        );
    }

    proptest! {
        #[test]
        fn pid_parsers_never_panic(b in proptest::collection::vec(any::<u8>(), 0..400)) {
            let _ = stat(&b);
            let _ = cmdline(&b);
            let s = String::from_utf8_lossy(&b);
            let _ = status(&s);
            let _ = io(&s);
            let _ = smaps_rollup(&s);
            let _ = fd_target(&s);
        }
    }
}
