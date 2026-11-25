// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests of the `rtop` command line.

use std::io::Read;
use std::process::{Command, Stdio};

fn rtop() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rtop"))
}

#[test]
fn stats_json_on_demo_machine_is_a_valid_graph() {
    let out = rtop()
        .args(["--stats", "--json", "--demo", "--interval", "200ms"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is pure JSON");
    let nodes = doc["nodes"].as_array().unwrap();
    let host = nodes.iter().find(|n| n["kind"] == "host").unwrap();
    assert_eq!(host["hostname"], "demo-host");
    assert!(
        host["metrics"]["cpu_usage_percent"].is_number(),
        "rates need two samples"
    );
    assert!(
        nodes
            .iter()
            .any(|n| n["kind"] == "container" && n["name"] == "postgres-db")
    );
    assert!(
        doc["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "communicates_with")
    );
}

#[test]
fn stats_yaml_on_live_system_has_no_terminal_escapes() {
    let out = rtop()
        .args(["--stats", "--interval", "200ms"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("schema_version: 1\n"));
    assert!(!text.contains('\x1b'));
    assert!(text.contains("kind: process"));
}

#[test]
fn broken_pipe_is_not_an_error() {
    let mut child = rtop()
        .args(["--stats", "--demo", "--interval", "100ms"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = [0u8; 16];
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut first)
        .unwrap();
    drop(child.stdout.take());
    let status = child.wait().unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(status.success(), "{err}");
    assert!(err.is_empty(), "stderr: {err}");
}

#[test]
fn invalid_arguments_fail_with_usage() {
    let out = rtop()
        .args(["--stats", "--interval", "forever"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid duration"));
    assert!(
        !rtop().arg("--json").output().unwrap().status.success(),
        "--json requires --stats"
    );
}

/// `--strict` turns an incomplete snapshot into exit status 2 while still
/// printing the document; the demo machine is always incomplete because it
/// has no rtop process for the self-monitoring collector to measure.
#[test]
fn strict_fails_an_incomplete_snapshot_but_still_prints_it() {
    let lenient = rtop()
        .args(["--stats", "--json", "--demo", "--interval", "200ms"])
        .output()
        .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&lenient.stdout).unwrap();
    assert_eq!(doc["complete"], false);
    assert!(
        lenient.status.success(),
        "incomplete is not an error by default"
    );

    let strict = rtop()
        .args([
            "--stats",
            "--json",
            "--demo",
            "--interval",
            "200ms",
            "--strict",
        ])
        .output()
        .unwrap();
    assert_eq!(strict.status.code(), Some(2));
    let doc: serde_json::Value =
        serde_json::from_slice(&strict.stdout).expect("document still printed");
    assert!(doc["nodes"].as_array().is_some_and(|n| !n.is_empty()));
    assert!(String::from_utf8_lossy(&strict.stderr).contains("monitor"));
}
