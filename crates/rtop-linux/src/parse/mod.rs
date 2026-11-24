// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Pure parsers for kernel text interfaces.
//!
//! Every parser accepts arbitrary bytes without panicking, tolerates
//! additional fields added by newer kernels and reports missing mandatory
//! fields as `None`/errors rather than zeros.

pub mod cgroup;
pub mod disk;
pub mod kmsg;
pub mod net;
pub mod pid;
pub mod system;

/// Parses `key value [unit]` lines (meminfo, vmstat, cpu.stat, memory.stat,
/// status-like files) into `(key, value)` pairs. `kB` units are converted to
/// bytes. Lines whose value is not an integer are skipped.
pub fn key_values(text: &str) -> impl Iterator<Item = (&str, u64)> {
    text.lines().filter_map(|line| {
        let mut it = line.split_whitespace();
        let key = it.next()?.trim_end_matches(':');
        let value: u64 = it.next()?.parse().ok()?;
        let value = match it.next() {
            Some("kB") => value.checked_mul(1024)?,
            _ => value,
        };
        Some((key, value))
    })
}

/// Finds one key in [`key_values`] output.
pub fn find_key(text: &str, key: &str) -> Option<u64> {
    key_values(text).find(|(k, _)| *k == key).map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_values_converts_kb_and_skips_garbage() {
        let t = "MemTotal:  100 kB\nHugePages_Total: 3\nbogus line here\nNoValue:\n";
        let v: Vec<_> = key_values(t).collect();
        assert_eq!(v, [("MemTotal", 102_400), ("HugePages_Total", 3)]);
    }
}
