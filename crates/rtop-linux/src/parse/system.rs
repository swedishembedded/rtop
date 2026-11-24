// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! System-wide procfs files: `/proc/stat`, `/proc/loadavg`, `/proc/uptime`,
//! `/proc/pressure/*`, and CPU list syntax.

/// Jiffies spent in each CPU state (`/proc/stat` cpu lines).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
    /// Already included in `user`; never add it again.
    pub guest: u64,
    /// Already included in `nice`.
    pub guest_nice: u64,
}

impl CpuTimes {
    /// Time doing work. iowait is idle time (the CPU was free to run
    /// something else) and guest time is already inside user/nice.
    pub fn busy(&self) -> u64 {
        self.user + self.nice + self.system + self.irq + self.softirq + self.steal
    }

    pub fn total(&self) -> u64 {
        self.busy() + self.idle + self.iowait
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcStat {
    pub total: Option<CpuTimes>,
    /// `(cpu index, times)`; indices may be sparse (offline CPUs omitted).
    pub cpus: Vec<(u32, CpuTimes)>,
    pub context_switches: Option<u64>,
    pub interrupts: Option<u64>,
    pub forks: Option<u64>,
    pub procs_running: Option<u64>,
    pub procs_blocked: Option<u64>,
    pub boot_time: Option<u64>,
}

fn cpu_times(fields: &mut std::str::SplitWhitespace<'_>) -> Option<CpuTimes> {
    let v: Vec<u64> = fields.map_while(|f| f.parse().ok()).collect();
    if v.len() < 4 {
        return None;
    }
    let g = |i: usize| v.get(i).copied().unwrap_or(0);
    Some(CpuTimes {
        user: g(0),
        nice: g(1),
        system: g(2),
        idle: g(3),
        iowait: g(4),
        irq: g(5),
        softirq: g(6),
        steal: g(7),
        guest: g(8),
        guest_nice: g(9),
    })
}

pub fn proc_stat(text: &str) -> ProcStat {
    let mut out = ProcStat::default();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let Some(key) = f.next() else { continue };
        let first_num =
            |f: &mut std::str::SplitWhitespace<'_>| f.next().and_then(|v| v.parse().ok());
        match key {
            "cpu" => out.total = cpu_times(&mut f),
            k if k.starts_with("cpu") => {
                if let (Ok(idx), Some(t)) = (k[3..].parse::<u32>(), cpu_times(&mut f)) {
                    out.cpus.push((idx, t));
                }
            }
            "ctxt" => out.context_switches = first_num(&mut f),
            "intr" => out.interrupts = first_num(&mut f),
            "processes" => out.forks = first_num(&mut f),
            "procs_running" => out.procs_running = first_num(&mut f),
            "procs_blocked" => out.procs_blocked = first_num(&mut f),
            "btime" => out.boot_time = first_num(&mut f),
            _ => {}
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadAvg {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
    pub runnable: u64,
    pub tasks: u64,
}

pub fn loadavg(text: &str) -> Option<LoadAvg> {
    let mut f = text.split_whitespace();
    let one = f.next()?.parse().ok()?;
    let five = f.next()?.parse().ok()?;
    let fifteen = f.next()?.parse().ok()?;
    let (r, t) = f.next()?.split_once('/')?;
    Some(LoadAvg {
        one,
        five,
        fifteen,
        runnable: r.parse().ok()?,
        tasks: t.parse().ok()?,
    })
}

/// `(uptime seconds, aggregate idle seconds)`.
pub fn uptime(text: &str) -> Option<(f64, f64)> {
    let mut f = text.split_whitespace();
    Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PressureLine {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    /// Cumulative stall time in microseconds.
    pub total_us: u64,
}

/// PSI `some`/`full` lines. `full` is absent for CPU on older kernels and
/// for the system-wide CPU file it may be present but always zero.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pressure {
    pub some: Option<PressureLine>,
    pub full: Option<PressureLine>,
}

pub fn pressure(text: &str) -> Pressure {
    let mut p = Pressure::default();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let kind = f.next();
        let mut l = PressureLine {
            avg10: f64::NAN,
            avg60: f64::NAN,
            avg300: f64::NAN,
            total_us: 0,
        };
        let mut seen = 0;
        for kv in f {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            let parsed = match k {
                "avg10" => v.parse().map(|x| l.avg10 = x).is_ok(),
                "avg60" => v.parse().map(|x| l.avg60 = x).is_ok(),
                "avg300" => v.parse().map(|x| l.avg300 = x).is_ok(),
                "total" => v.parse().map(|x| l.total_us = x).is_ok(),
                _ => false,
            };
            if parsed {
                seen += 1;
            }
        }
        let valid = seen == 4
            && [l.avg10, l.avg60, l.avg300]
                .iter()
                .all(|v| v.is_finite() && (0.0..=100.0).contains(v));
        match kind {
            Some("some") if valid => p.some = Some(l),
            Some("full") if valid => p.full = Some(l),
            _ => {}
        }
    }
    p
}

/// Parses kernel CPU list syntax (`0-3,8,10-11`). Bounded so a corrupt
/// range cannot allocate unboundedly.
pub fn cpu_list(text: &str) -> Option<Vec<u32>> {
    const MAX_CPUS: u32 = 1 << 16;
    let mut out = Vec::new();
    for part in text.trim().split(',').filter(|s| !s.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u32, u32) = (a.parse().ok()?, b.parse().ok()?);
                if a > b || b >= MAX_CPUS {
                    return None;
                }
                out.extend(a..=b);
            }
            None => {
                let v: u32 = part.parse().ok()?;
                if v >= MAX_CPUS {
                    return None;
                }
                out.push(v);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const STAT: &str = "cpu  100 5 50 1000 20 3 2 1 7 0\ncpu0 50 2 25 500 10 1 1 0 3 0\ncpu3 50 3 25 500 10 2 1 1 4 0\nintr 12345 1 2 3\nctxt 999\nbtime 1700000000\nprocesses 4242\nprocs_running 3\nprocs_blocked 1\nsoftirq 1 2 3\n";

    #[test]
    fn proc_stat_handles_sparse_cpus_and_excludes_guest() {
        let s = proc_stat(STAT);
        let t = s.total.unwrap();
        assert_eq!(t.busy(), 100 + 5 + 50 + 3 + 2 + 1);
        assert_eq!(t.total(), t.busy() + 1000 + 20);
        assert_eq!(s.cpus.iter().map(|c| c.0).collect::<Vec<_>>(), [0, 3]);
        assert_eq!(
            (s.context_switches, s.interrupts, s.forks),
            (Some(999), Some(12345), Some(4242))
        );
        assert_eq!((s.procs_running, s.procs_blocked), (Some(3), Some(1)));
    }

    #[test]
    fn old_kernels_with_four_cpu_fields_parse() {
        let s = proc_stat("cpu 1 2 3 4\n");
        assert_eq!(s.total.unwrap().iowait, 0);
    }

    #[test]
    fn pressure_tolerates_missing_full_and_rejects_garbage() {
        let p = pressure("some avg10=1.50 avg60=0.20 avg300=0.00 total=12345\n");
        assert_eq!(p.some.unwrap().total_us, 12345);
        assert!(p.full.is_none());
        let bad = pressure(
            "some avg10=nan avg60=0 avg300=0 total=1\nfull avg10=200 avg60=0 avg300=0 total=1\n",
        );
        assert!(bad.some.is_none() && bad.full.is_none());
    }

    #[test]
    fn loadavg_and_cpu_lists() {
        assert_eq!(loadavg("4.02 3.10 2.00 3/1234 5678\n").unwrap().tasks, 1234);
        assert_eq!(cpu_list("0-2,5\n"), Some(vec![0, 1, 2, 5]));
        assert_eq!(cpu_list(""), Some(vec![]));
        assert_eq!(cpu_list("3-1"), None);
        assert_eq!(cpu_list("0-4294967295"), None);
    }

    proptest! {
        #[test]
        fn parsers_never_panic(s in "\\PC*") {
            let _ = proc_stat(&s);
            let _ = loadavg(&s);
            let _ = pressure(&s);
            let _ = cpu_list(&s);
            let _ = uptime(&s);
        }
    }
}
