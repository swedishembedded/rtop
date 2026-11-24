// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! cgroup v2 interface files.

/// A limit file such as `memory.max`: `max` means unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Unlimited,
    Bytes(u64),
}

pub fn limit(text: &str) -> Option<Limit> {
    match text.trim() {
        "max" => Some(Limit::Unlimited),
        v => v.parse().ok().map(Limit::Bytes),
    }
}

/// `cpu.max`: `"<quota|max> <period>"` → CPUs worth of quota.
pub fn cpu_max(text: &str) -> Option<Option<f64>> {
    let mut f = text.split_whitespace();
    let quota = f.next()?;
    let period: f64 = f.next().unwrap_or("100000").parse().ok()?;
    if quota == "max" {
        return Some(None);
    }
    let q: f64 = quota.parse().ok()?;
    if period <= 0.0 {
        return None;
    }
    Some(Some(q / period))
}

/// One device line of `io.stat`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IoStat {
    pub major: u32,
    pub minor: u32,
    pub rbytes: u64,
    pub wbytes: u64,
    pub rios: u64,
    pub wios: u64,
}

pub fn io_stat(text: &str) -> Vec<IoStat> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let (maj, min) = f.next()?.split_once(':')?;
            let mut s = IoStat {
                major: maj.parse().ok()?,
                minor: min.parse().ok()?,
                ..Default::default()
            };
            for kv in f {
                let Some((k, v)) = kv.split_once('=') else {
                    continue;
                };
                let Ok(v) = v.parse() else { continue };
                match k {
                    "rbytes" => s.rbytes = v,
                    "wbytes" => s.wbytes = v,
                    "rios" => s.rios = v,
                    "wios" => s.wios = v,
                    _ => {}
                }
            }
            Some(s)
        })
        .collect()
}

/// Recognises container runtimes in a cgroup path and returns
/// `(runtime, container id)`.
pub fn container_from_path(path: &str) -> Option<(&'static str, String)> {
    let leaf = path.rsplit('/').next()?;
    let hex64 = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    for (prefix, runtime) in [
        ("docker-", "docker"),
        ("libpod-", "podman"),
        ("cri-containerd-", "containerd"),
        ("crio-", "cri-o"),
    ] {
        if let Some(rest) = leaf.strip_prefix(prefix) {
            let id = rest.strip_suffix(".scope").unwrap_or(rest);
            if hex64(id) {
                return Some((runtime, id.to_owned()));
            }
        }
    }
    // cgroupfs driver: /docker/<id>
    let mut parts = path.rsplit('/');
    let id = parts.next()?;
    if parts.next() == Some("docker") && hex64(id) {
        return Some(("docker", id.to_owned()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_quota() {
        assert_eq!(limit("max\n"), Some(Limit::Unlimited));
        assert_eq!(limit("1024"), Some(Limit::Bytes(1024)));
        assert_eq!(limit("junk"), None);
        assert_eq!(cpu_max("max 100000"), Some(None));
        assert_eq!(cpu_max("50000 100000"), Some(Some(0.5)));
        assert_eq!(cpu_max("1 0"), None);
    }

    #[test]
    fn io_stat_lines() {
        let s = io_stat("8:0 rbytes=4096 wbytes=8192 rios=1 wios=2 dbytes=0 dios=0\n");
        assert_eq!((s[0].major, s[0].rbytes, s[0].wios), (8, 4096, 2));
    }

    #[test]
    fn container_ids() {
        let id = "a".repeat(64);
        assert_eq!(
            container_from_path(&format!("/system.slice/docker-{id}.scope")),
            Some(("docker", id.clone()))
        );
        assert_eq!(
            container_from_path(&format!("/docker/{id}")),
            Some(("docker", id.clone()))
        );
        assert_eq!(
            container_from_path(&format!("/machine.slice/libpod-{id}.scope")),
            Some(("podman", id))
        );
        assert_eq!(container_from_path("/user.slice/docker-short.scope"), None);
    }
}
