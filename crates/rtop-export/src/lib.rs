// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0
//
// Swedish Embedded AB implements solutions for exposing system telemetry as
// stable, scriptable machine-readable interfaces for its clients. If your
// team needs expertise in observability data contracts then you can procure
// our services by sending an email to info@swedishembedded.com.

//! Serialises a [`SystemGraph`] as YAML or JSON (`rtop --stats`).
//!
//! The document is a public interface (schema version [`SCHEMA_VERSION`]):
//!
//! * nodes and edges are flat lists; edges reference nodes by id, so cycles
//!   in the graph can never cause recursive output;
//! * ordering is deterministic (ids, then keys, sorted);
//! * every metric appears under `metrics`, with `null` and a reason under
//!   `unavailable` when it has no value — never a substitute zero; values
//!   from stale collectors are listed under `stale`, heuristic ones under
//!   `estimated`;
//! * units are encoded in metric names (`_bytes`, `_per_second`, ...);
//! * all strings are sanitised, so output never contains terminal escapes.
//!
//! Both formats are emitted by hand from one intermediate [`Doc`] tree to
//! guarantee identical content and avoid an unmaintained YAML dependency.

use std::io::{self, Write};

use rtop_core::graph::MetricEntry;
use rtop_core::text::sanitize;
use rtop_core::{Attr, Metric, SystemGraph, Value};

pub const SCHEMA_VERSION: u64 = 1;

/// Intermediate document tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Doc {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    List(Vec<Doc>),
    Map(Vec<(String, Doc)>),
}

impl Doc {
    fn str(s: &str) -> Doc {
        Doc::Str(sanitize(s).into_owned())
    }

    fn is_empty_container(&self) -> bool {
        matches!(self, Doc::List(v) if v.is_empty()) || matches!(self, Doc::Map(v) if v.is_empty())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ExportOptions {
    pub sample_window_ms: u64,
}

fn attr_doc(a: &Attr) -> Doc {
    match a {
        Attr::Text(s) => Doc::str(s),
        Attr::Int(v) => Doc::Int(*v),
        Attr::UInt(v) => Doc::UInt(*v),
        Attr::Bool(v) => Doc::Bool(*v),
    }
}

fn value_doc(v: Value) -> Doc {
    match v {
        Value::U64(v) => Doc::UInt(v),
        Value::I64(v) => Doc::Int(v),
        Value::F64(v) => Doc::Float((v * 1000.0).round() / 1000.0),
    }
}

const RESERVED: [&str; 11] = [
    "id",
    "kind",
    "source",
    "target",
    "evidence",
    "resolved",
    "metrics",
    "unavailable",
    "stale",
    "estimated",
    "first_seen_ms",
];

fn metric_block<'a>(
    g: &SystemGraph,
    metrics: impl Iterator<Item = (&'a &'static str, &'a MetricEntry)>,
    out: &mut Vec<(String, Doc)>,
) {
    let (mut vals, mut unav, mut stale, mut est) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (k, e) in metrics {
        match e.metric {
            Metric::Valid(v) => vals.push((k.to_string(), value_doc(v))),
            Metric::Estimated(v) => {
                vals.push((k.to_string(), value_doc(v)));
                est.push(Doc::str(k));
            }
            Metric::Unavailable(r) => {
                vals.push((k.to_string(), Doc::Null));
                unav.push((k.to_string(), Doc::str(r.as_str())));
            }
        }
        if g.is_stale(e.source) && e.metric.value().is_some() {
            stale.push(Doc::str(k));
        }
    }
    for (key, d) in [
        ("metrics", Doc::Map(vals)),
        ("unavailable", Doc::Map(unav)),
        ("stale", Doc::List(stale)),
        ("estimated", Doc::List(est)),
    ] {
        if !d.is_empty_container() {
            out.push((key.into(), d));
        }
    }
}

/// Builds the export document.
pub fn document(g: &SystemGraph, opts: ExportOptions) -> Doc {
    let complete = g.collectors().all(|(_, s)| {
        matches!(
            s.state,
            rtop_core::CollectorState::Available | rtop_core::CollectorState::Degraded
        )
    });
    let mut nodes = Vec::with_capacity(g.node_count());
    for n in g.nodes() {
        let mut m = vec![
            ("id".to_owned(), Doc::str(n.id.as_str())),
            ("kind".to_owned(), Doc::str(n.kind.as_str())),
        ];
        for (k, a) in &n.attrs {
            if !RESERVED.contains(k) {
                m.push((k.to_string(), attr_doc(&a.value)));
            }
        }
        metric_block(g, n.metrics.iter(), &mut m);
        nodes.push(Doc::Map(m));
    }
    let mut edges = Vec::with_capacity(g.edge_count());
    for e in g.edges() {
        let mut m = vec![
            ("kind".to_owned(), Doc::str(e.key.kind.as_str())),
            ("source".to_owned(), Doc::str(e.key.source.as_str())),
            ("target".to_owned(), Doc::str(e.key.target.as_str())),
            ("evidence".to_owned(), Doc::str(e.evidence.as_str())),
            ("resolved".to_owned(), Doc::Bool(g.is_resolved(e))),
        ];
        for (k, a) in &e.attrs {
            if !RESERVED.contains(k) {
                m.push((k.to_string(), attr_doc(&a.value)));
            }
        }
        metric_block(g, e.metrics.iter(), &mut m);
        edges.push(Doc::Map(m));
    }
    let events = g
        .events()
        .map(|ev| {
            let mut m = vec![
                ("severity".to_owned(), Doc::str(ev.severity.as_str())),
                ("category".to_owned(), Doc::str(ev.category.as_str())),
                ("source".to_owned(), Doc::str(ev.source.as_str())),
                ("message".to_owned(), Doc::str(&ev.message)),
            ];
            if let Some(w) = ev.wall_time {
                m.insert(0, ("unix_time".to_owned(), Doc::Float(w.floor())));
            }
            if !ev.subjects.is_empty() {
                m.push((
                    "subjects".to_owned(),
                    Doc::List(ev.subjects.iter().map(|s| Doc::str(s.as_str())).collect()),
                ));
            }
            Doc::Map(m)
        })
        .collect();
    let collectors = g
        .collectors()
        .map(|(id, s)| {
            let mut m = vec![("status".to_owned(), Doc::str(s.state.as_str()))];
            if let Some(r) = s.reason {
                m.push(("reason".to_owned(), Doc::str(r.as_str())));
            }
            if let Some(d) = &s.detail {
                m.push(("detail".to_owned(), Doc::str(d)));
            }
            if let Some(d) = s.last_duration {
                m.push((
                    "duration_ms".to_owned(),
                    Doc::Float((d.as_secs_f64() * 1e4).round() / 10.0),
                ));
            }
            m.push(("samples".to_owned(), Doc::UInt(s.samples)));
            if s.failures > 0 {
                m.push(("failures".to_owned(), Doc::UInt(s.failures)));
            }
            (id.as_str().to_owned(), Doc::Map(m))
        })
        .collect();
    Doc::Map(vec![
        ("schema_version".into(), Doc::UInt(SCHEMA_VERSION)),
        (
            "generator".into(),
            Doc::Str(format!("rtop {}", env!("CARGO_PKG_VERSION"))),
        ),
        ("sample_window_ms".into(), Doc::UInt(opts.sample_window_ms)),
        ("complete".into(), Doc::Bool(complete)),
        ("nodes".into(), Doc::List(nodes)),
        ("edges".into(), Doc::List(edges)),
        ("events".into(), Doc::List(events)),
        ("collectors".into(), Doc::Map(collectors)),
    ])
}

// ----- JSON ---------------------------------------------------------------

fn json_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    w.write_all(b"\"")?;
    for c in s.chars() {
        match c {
            '"' => w.write_all(b"\\\"")?,
            '\\' => w.write_all(b"\\\\")?,
            '\n' => w.write_all(b"\\n")?,
            '\t' => w.write_all(b"\\t")?,
            '\r' => w.write_all(b"\\r")?,
            c if (c as u32) < 0x20 || c == '\u{7f}' => write!(w, "\\u{:04x}", c as u32)?,
            c => write!(w, "{c}")?,
        }
    }
    w.write_all(b"\"")
}

fn float(f: f64) -> String {
    if f.is_finite() {
        let s = format!("{f}");
        if s.contains(['.', 'e', 'E']) {
            s
        } else {
            format!("{s}.0")
        }
    } else {
        "null".into()
    }
}

pub fn write_json(w: &mut impl Write, d: &Doc, indent: usize) -> io::Result<()> {
    let pad = |w: &mut dyn Write, n: usize| write!(w, "{:n$}", "");
    match d {
        Doc::Null => w.write_all(b"null"),
        Doc::Bool(b) => write!(w, "{b}"),
        Doc::Int(v) => write!(w, "{v}"),
        Doc::UInt(v) => write!(w, "{v}"),
        Doc::Float(f) => w.write_all(float(*f).as_bytes()),
        Doc::Str(s) => json_str(w, s),
        Doc::List(v) if v.is_empty() => w.write_all(b"[]"),
        Doc::Map(v) if v.is_empty() => w.write_all(b"{}"),
        Doc::List(v) => {
            w.write_all(b"[\n")?;
            for (i, x) in v.iter().enumerate() {
                pad(w, indent + 2)?;
                write_json(w, x, indent + 2)?;
                w.write_all(if i + 1 < v.len() { b",\n" } else { b"\n" })?;
            }
            pad(w, indent)?;
            w.write_all(b"]")
        }
        Doc::Map(v) => {
            w.write_all(b"{\n")?;
            for (i, (k, x)) in v.iter().enumerate() {
                pad(w, indent + 2)?;
                json_str(w, k)?;
                w.write_all(b": ")?;
                write_json(w, x, indent + 2)?;
                w.write_all(if i + 1 < v.len() { b",\n" } else { b"\n" })?;
            }
            pad(w, indent)?;
            w.write_all(b"}")
        }
    }
}

// ----- YAML ---------------------------------------------------------------

/// Whether `s` can be emitted as a plain YAML scalar that every YAML 1.1/1.2
/// parser reads back as the same string.
fn yaml_plain_ok(s: &str) -> bool {
    let Some(first) = s.chars().next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '/') || s.ends_with(':') {
        return false;
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_./:-@+".contains(c))
    {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "y" | "n" | "nan" | "inf"
    )
}

fn yaml_scalar(w: &mut impl Write, d: &Doc) -> io::Result<()> {
    match d {
        Doc::Null => w.write_all(b"null"),
        Doc::Bool(b) => write!(w, "{b}"),
        Doc::Int(v) => write!(w, "{v}"),
        Doc::UInt(v) => write!(w, "{v}"),
        Doc::Float(f) => w.write_all(float(*f).as_bytes()),
        // A JSON string is a valid YAML double-quoted scalar.
        Doc::Str(s) if yaml_plain_ok(s) => w.write_all(s.as_bytes()),
        Doc::Str(s) => json_str(w, s),
        Doc::List(_) => w.write_all(b"[]"),
        Doc::Map(_) => w.write_all(b"{}"),
    }
}

fn yaml_key(w: &mut impl Write, k: &str) -> io::Result<()> {
    if yaml_plain_ok(k) {
        w.write_all(k.as_bytes())
    } else {
        json_str(w, k)
    }
}

fn is_block(d: &Doc) -> bool {
    matches!(d, Doc::List(v) if !v.is_empty()) || matches!(d, Doc::Map(v) if !v.is_empty())
}

fn write_yaml_map(
    w: &mut impl Write,
    entries: &[(String, Doc)],
    indent: usize,
    first_inline: bool,
) -> io::Result<()> {
    for (i, (k, v)) in entries.iter().enumerate() {
        if !(first_inline && i == 0) {
            write!(w, "{:indent$}", "")?;
        }
        yaml_key(w, k)?;
        w.write_all(b":")?;
        if is_block(v) {
            w.write_all(b"\n")?;
            write_yaml_block(w, v, indent + 2)?;
        } else {
            w.write_all(b" ")?;
            yaml_scalar(w, v)?;
            w.write_all(b"\n")?;
        }
    }
    Ok(())
}

fn write_yaml_block(w: &mut impl Write, d: &Doc, indent: usize) -> io::Result<()> {
    match d {
        Doc::Map(entries) => write_yaml_map(w, entries, indent, false),
        Doc::List(items) => {
            for item in items {
                write!(w, "{:indent$}- ", "")?;
                match item {
                    Doc::Map(e) if !e.is_empty() => write_yaml_map(w, e, indent + 2, true)?,
                    Doc::List(l) if !l.is_empty() => {
                        w.write_all(b"\n")?;
                        write_yaml_block(w, item, indent + 2)?;
                    }
                    other => {
                        yaml_scalar(w, other)?;
                        w.write_all(b"\n")?;
                    }
                }
            }
            Ok(())
        }
        other => {
            write!(w, "{:indent$}", "")?;
            yaml_scalar(w, other)?;
            w.write_all(b"\n")
        }
    }
}

pub fn write_yaml(w: &mut impl Write, d: &Doc) -> io::Result<()> {
    write_yaml_block(w, d, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtop_collectors::Subscriptions;
    use rtop_collectors::demo::DemoMachine;
    use rtop_core::{
        CollectorId, EdgeKind, EdgeUpdate, EntityId, EntityKind, Evidence, GraphPatch,
        MonotonicClock, NodeUpdate, Timestamp, Unavailable,
    };
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;

    fn demo_graph() -> SystemGraph {
        let m = DemoMachine::new();
        let env = m.env(Subscriptions::all());
        rtop_runtime::snapshot_with(
            m.collectors(&env),
            Arc::new(MonotonicClock::new()),
            Duration::from_millis(50),
            Duration::from_secs(5),
            || m.advance(1.0),
        )
    }

    fn render(g: &SystemGraph) -> (String, String) {
        let d = document(
            g,
            ExportOptions {
                sample_window_ms: 1000,
            },
        );
        let (mut y, mut j) = (Vec::new(), Vec::new());
        write_yaml(&mut y, &d).unwrap();
        write_json(&mut j, &d, 0).unwrap();
        (String::from_utf8(y).unwrap(), String::from_utf8(j).unwrap())
    }

    #[test]
    fn yaml_and_json_parse_to_the_same_document() {
        let g = demo_graph();
        let (yaml, json) = render(&g);
        let j: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let y = &yaml_rust2::YamlLoader::load_from_str(&yaml).expect("valid YAML")[0];
        let nodes = j["nodes"].as_array().unwrap();
        assert_eq!(y["nodes"].as_vec().unwrap().len(), nodes.len());
        // Spot-check scalar equality across formats.
        for (yn, jn) in y["nodes"].as_vec().unwrap().iter().zip(nodes) {
            assert_eq!(yn["id"].as_str(), jn["id"].as_str());
        }
        assert_eq!(j["schema_version"], 1);
    }

    #[test]
    fn contract_ids_unique_edges_reference_nodes_no_escapes() {
        let g = demo_graph();
        let (yaml, json) = render(&g);
        assert!(!yaml.contains('\x1b') && !json.contains('\x1b'));
        let j: serde_json::Value = serde_json::from_str(&json).unwrap();
        let mut ids = HashSet::new();
        for n in j["nodes"].as_array().unwrap() {
            assert!(
                ids.insert(n["id"].as_str().unwrap().to_owned()),
                "duplicate id"
            );
            // Every null metric has a reason.
            if let Some(m) = n["metrics"].as_object() {
                for (k, v) in m {
                    if v.is_null() {
                        assert!(n["unavailable"][k].is_string(), "{k} null without reason");
                    }
                }
            }
        }
        for e in j["edges"].as_array().unwrap() {
            let ok = ids.contains(e["source"].as_str().unwrap())
                && ids.contains(e["target"].as_str().unwrap());
            assert_eq!(e["resolved"].as_bool().unwrap(), ok);
        }
        assert!(j["collectors"]["processes"]["status"].as_str() == Some("available"));
    }

    #[test]
    fn output_is_deterministic_and_cycles_terminate() {
        let mut g = SystemGraph::default();
        let a = EntityId::new(EntityKind::Process, "1:1");
        let b = EntityId::new(EntityKind::Process, "2:1");
        let mut p = GraphPatch::new(CollectorId("t"), 1, Timestamp(1));
        p.node(
            NodeUpdate::new(EntityKind::Process, a.clone())
                .text("name", "evil\x1b[31m\"name\"")
                .metric("x", Metric::Unavailable(Unavailable::PermissionDenied)),
        );
        p.node(NodeUpdate::new(EntityKind::Process, b.clone()).attr("name", "yes"));
        p.edge(EdgeUpdate::new(
            EdgeKind::ParentOf,
            a.clone(),
            b.clone(),
            Evidence::KernelConfirmed,
        ));
        p.edge(EdgeUpdate::new(
            EdgeKind::ParentOf,
            b,
            a,
            Evidence::KernelConfirmed,
        ));
        g.apply(p);
        let (y1, j1) = render(&g);
        let (y2, j2) = render(&g);
        assert_eq!((y1.clone(), j1.clone()), (y2, j2));
        let y = &yaml_rust2::YamlLoader::load_from_str(&y1).unwrap()[0];
        assert_eq!(
            y["nodes"][0]["name"].as_str(),
            Some("evil\\x1b[31m\"name\"")
        );
        // "yes" must stay a string, not become a boolean.
        assert_eq!(y["nodes"][1]["name"].as_str(), Some("yes"));
        assert!(y["nodes"][0]["metrics"]["x"].is_null());
        assert_eq!(
            y["nodes"][0]["unavailable"]["x"].as_str(),
            Some("permission_denied")
        );
    }
}
