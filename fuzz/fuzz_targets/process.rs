// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! /proc/PID parsers (process names may contain any byte) and sanitising.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rtop_linux::parse::pid;

fuzz_target!(|data: &[u8]| {
    let _ = pid::stat(data);
    let _ = pid::cmdline(data);
    let s = String::from_utf8_lossy(data);
    let _ = pid::status(&s);
    let _ = pid::io(&s);
    let _ = pid::smaps_rollup(&s);
    let _ = pid::fd_target(&s);
    let _ = pid::cgroup_v2_path(&s);
    let clean = rtop_core::text::sanitize_bytes(data);
    assert!(!clean.chars().any(char::is_control), "sanitised text contains a control character");
});
