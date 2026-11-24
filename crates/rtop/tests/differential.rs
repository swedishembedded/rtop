// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Differential tests against established tools (procps, coreutils,
//! iproute2) on the live machine. Exact agreement where both read the same
//! kernel value; metric-specific tolerances where the system moves between
//! the two observations. A missing reference tool skips its comparison.

use std::process::Command;

use serde_json::Value;

fn stats() -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_rtop"))
        .args(["--stats", "--json", "--interval", "200ms"])
        .output()
        .expect("run rtop");
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).expect("JSON")
}

fn tool(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn host(doc: &Value) -> &Value {
    doc["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["kind"] == "host")
        .expect("host node")
}

#[test]
fn memory_total_matches_free_exactly() {
    let Some(free) = tool("free", &["-b"]) else {
        return;
    };
    let total: u64 = free
        .lines()
        .find(|l| l.starts_with("Mem:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap();
    assert_eq!(
        host(&stats())["metrics"]["memory_total_bytes"].as_u64(),
        Some(total)
    );
}

#[test]
fn logical_cpus_match_nproc_all() {
    let Some(n) = tool("nproc", &["--all"]) else {
        return;
    };
    let online = std::fs::read_to_string("/sys/devices/system/cpu/online").ok();
    let doc = stats();
    let cpus = host(&doc)["metrics"]["logical_cpus"].as_u64().unwrap();
    // rtop counts online CPUs; with none offline that equals nproc --all.
    if online.as_deref().map(str::trim)
        == Some(&format!("0-{}", n.trim().parse::<u64>().unwrap() - 1))
    {
        assert_eq!(cpus, n.trim().parse::<u64>().unwrap());
    }
    assert!(cpus >= 1);
}

#[test]
fn root_filesystem_capacity_matches_df() {
    let Some(df) = tool("df", &["-B1", "--output=size,avail", "/"]) else {
        return;
    };
    let vals: Vec<u64> = df
        .lines()
        .nth(1)
        .unwrap()
        .split_whitespace()
        .map(|v| v.parse().unwrap())
        .collect();
    let doc = stats();
    let root = doc["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["kind"] == "mount" && n["mount_point"] == "/");
    let Some(root) = root else { return }; // e.g. rootfs not listed in some containers
    assert_eq!(root["metrics"]["capacity_bytes"].as_u64(), Some(vals[0]));
    let avail = root["metrics"]["available_bytes"].as_u64().unwrap() as i128;
    // Available space moves with concurrent writes: allow 256 MiB.
    assert!(
        (avail - vals[1] as i128).abs() < 256 << 20,
        "rtop {avail} df {}",
        vals[1]
    );
}

#[test]
fn listening_tcp_sockets_match_ss() {
    let Some(ss) = tool("ss", &["-H", "-t", "-l", "-n"]) else {
        return;
    };
    let reference = ss.lines().filter(|l| !l.trim().is_empty()).count() as i64;
    let doc = stats();
    let ours = host(&doc)["metrics"]["sockets_tcp_listening"]
        .as_i64()
        .unwrap();
    // Services may open or close listeners between the two observations.
    assert!((ours - reference).abs() <= 3, "rtop {ours} ss {reference}");
}

#[test]
fn process_count_matches_ps() {
    let Some(ps) = tool("ps", &["-e", "-o", "pid="]) else {
        return;
    };
    let reference = ps.lines().filter(|l| !l.trim().is_empty()).count() as i64;
    let doc = stats();
    let ours = host(&doc)["metrics"]["processes_total"].as_i64().unwrap();
    let tolerance = (reference / 20).max(10);
    assert!(
        (ours - reference).abs() <= tolerance,
        "rtop {ours} ps {reference}"
    );
}
