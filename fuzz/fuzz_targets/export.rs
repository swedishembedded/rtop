// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The YAML/JSON emitters on graphs with arbitrary strings and numbers:
//! output must never contain raw control characters.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rtop_core::{CollectorId, EntityId, EntityKind, GraphPatch, Metric, NodeUpdate, SystemGraph, Timestamp};

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let mut g = SystemGraph::default();
    let mut p = GraphPatch::new(CollectorId("fuzz"), 1, Timestamp(1));
    let n = data.first().copied().unwrap_or(0);
    p.node(
        NodeUpdate::new(EntityKind::Process, EntityId::new(EntityKind::Process, format_args!("{n}:1")))
            .text("name", &s)
            .attr("cmdline", s.to_string())
            .metric("cpu_usage_percent", Metric::f64(f64::from(n) / 3.0)),
    );
    g.apply(p);
    let doc = rtop_export::document(&g, rtop_export::ExportOptions { sample_window_ms: 1000 });
    let (mut y, mut j) = (Vec::new(), Vec::new());
    rtop_export::write_yaml(&mut y, &doc).unwrap();
    rtop_export::write_json(&mut j, &doc, 0).unwrap();
    for out in [y, j] {
        assert!(!out.iter().any(|b| *b < 0x20 && *b != b'\n'), "raw control byte in export");
    }
});
