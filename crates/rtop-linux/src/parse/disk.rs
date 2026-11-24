// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! `/proc/diskstats` and `/proc/self/mountinfo`.

/// One `/proc/diskstats` line. Sector counts are in 512-byte units
/// regardless of the device's physical sector size. Discard and flush
/// fields exist only on newer kernels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskStat {
    pub major: u32,
    pub minor: u32,
    pub name: String,
    pub reads: u64,
    pub reads_merged: u64,
    pub sectors_read: u64,
    pub read_ms: u64,
    pub writes: u64,
    pub writes_merged: u64,
    pub sectors_written: u64,
    pub write_ms: u64,
    pub in_flight: u64,
    pub io_ms: u64,
    pub weighted_io_ms: u64,
    pub discards: Option<u64>,
    pub sectors_discarded: Option<u64>,
    pub discard_ms: Option<u64>,
    pub flushes: Option<u64>,
    pub flush_ms: Option<u64>,
}

pub fn diskstats(text: &str) -> Vec<DiskStat> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 14 {
                return None;
            }
            let n = |i: usize| -> Option<u64> { f.get(i)?.parse().ok() };
            Some(DiskStat {
                major: f[0].parse().ok()?,
                minor: f[1].parse().ok()?,
                name: f[2].to_owned(),
                reads: n(3)?,
                reads_merged: n(4)?,
                sectors_read: n(5)?,
                read_ms: n(6)?,
                writes: n(7)?,
                writes_merged: n(8)?,
                sectors_written: n(9)?,
                write_ms: n(10)?,
                in_flight: n(11)?,
                io_ms: n(12)?,
                weighted_io_ms: n(13)?,
                discards: n(14),
                sectors_discarded: n(16),
                discard_ms: n(17),
                flushes: n(18),
                flush_ms: n(19),
            })
        })
        .collect()
}

/// Whether a block device name is a partition or virtual device that
/// would double-count I/O if summed with its parent.
pub fn is_aggregate_device(name: &str) -> bool {
    !(name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub mount_id: u32,
    pub parent_id: u32,
    pub major: u32,
    pub minor: u32,
    /// Root of the mount within its filesystem (differs for bind mounts).
    pub root: String,
    pub mount_point: String,
    pub options: String,
    pub fs_type: String,
    pub source: String,
}

/// Undoes the octal escaping mountinfo applies to space, tab, newline and
/// backslash.
pub fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') as u32 * 64
                + (b[i + 2] - b'0') as u32 * 8
                + (b[i + 3] - b'0') as u32;
            if let Ok(byte) = u8::try_from(v) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn mountinfo(text: &str) -> Vec<MountInfo> {
    text.lines()
        .filter_map(|line| {
            let (pre, post) = line.split_once(" - ")?;
            let f: Vec<&str> = pre.split(' ').collect();
            let p: Vec<&str> = post.split(' ').collect();
            if f.len() < 6 || p.len() < 2 {
                return None;
            }
            let (maj, min) = f[2].split_once(':')?;
            Some(MountInfo {
                mount_id: f[0].parse().ok()?,
                parent_id: f[1].parse().ok()?,
                major: maj.parse().ok()?,
                minor: min.parse().ok()?,
                root: unescape_octal(f[3]),
                mount_point: unescape_octal(f[4]),
                options: f[5].to_owned(),
                fs_type: p[0].to_owned(),
                source: unescape_octal(p[1]),
            })
        })
        .collect()
}

/// Pseudo filesystems whose capacity is meaningless for storage monitoring.
pub fn is_pseudo_fs(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "proc"
            | "sysfs"
            | "devtmpfs"
            | "devpts"
            | "cgroup"
            | "cgroup2"
            | "securityfs"
            | "debugfs"
            | "tracefs"
            | "pstore"
            | "bpf"
            | "mqueue"
            | "hugetlbfs"
            | "configfs"
            | "fusectl"
            | "binfmt_misc"
            | "autofs"
            | "rpc_pipefs"
            | "nsfs"
            | "efivarfs"
            | "ramfs"
            | "selinuxfs"
    )
}

/// Filesystems whose `statvfs` can block indefinitely on a dead server.
pub fn is_remote_fs(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "nfs"
            | "nfs4"
            | "cifs"
            | "smb3"
            | "sshfs"
            | "fuse.sshfs"
            | "9p"
            | "ceph"
            | "glusterfs"
            | "fuse.glusterfs"
            | "afs"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn diskstats_old_and_new_formats() {
        let t = "   8       0 sda 100 2 800 50 200 3 1600 70 1 90 120\n 259 0 nvme0n1 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17\n";
        let d = diskstats(t);
        assert_eq!(d.len(), 2);
        assert_eq!(
            (d[0].name.as_str(), d[0].sectors_read, d[0].discards),
            ("sda", 800, None)
        );
        assert_eq!(
            (d[1].discards, d[1].flushes, d[1].flush_ms),
            (Some(12), Some(16), Some(17))
        );
    }

    #[test]
    fn mountinfo_with_optional_fields_and_escapes() {
        let t = "36 35 98:0 /mnt1 /mnt\\040with\\040space rw,noatime master:1 shared:2 - ext3 /dev/root rw,errors=continue\n25 1 0:22 / /proc rw - proc proc rw\n";
        let m = mountinfo(t);
        assert_eq!(m[0].mount_point, "/mnt with space");
        assert_eq!(
            (m[0].major, m[0].minor, m[0].fs_type.as_str()),
            (98, 0, "ext3")
        );
        assert_eq!(m[0].root, "/mnt1");
        assert!(is_pseudo_fs(&m[1].fs_type));
    }

    proptest! {
        #[test]
        fn never_panic(s in "\\PC*") {
            let _ = diskstats(&s);
            let _ = mountinfo(&s);
            let _ = unescape_octal(&s);
        }
    }
}
