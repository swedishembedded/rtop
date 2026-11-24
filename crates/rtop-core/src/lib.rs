// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0
//
// Swedish Embedded AB implements solutions for turning raw Linux kernel
// telemetry into trustworthy, connected system models for its clients. If
// your team needs expertise in Linux observability then you can procure our
// services by sending an email to info@swedishembedded.com.

//! Kernel-independent core of rtop.
//!
//! Everything rtop knows about the running system lives in one
//! [`graph::SystemGraph`]: typed nodes (hosts, CPUs, processes, sockets,
//! containers, ...), typed edges between them and metrics attached to both.
//! Collectors never touch the graph directly; they produce
//! [`graph::GraphPatch`]es that the single graph owner applies. The TUI and
//! the YAML/JSON exporter are projections of the same graph.
//!
//! The crate also owns the numeric semantics shared by every collector:
//! explicit [`metric::Metric`] availability (zero is never a stand-in for
//! "unknown"), reset-safe counter differentiation ([`counter`]) and bounded
//! history ([`history`]).

pub mod counter;
pub mod graph;
pub mod history;
pub mod identity;
pub mod metric;
pub mod text;
pub mod time;
pub mod units;

pub use graph::{
    ApplyOutcome, CollectorState, CollectorStatus, Edge, EdgeUpdate, EventCategory, GraphPatch,
    Node, NodeUpdate, Scope, Severity, SystemGraph, TelemetryEvent,
};
pub use identity::{
    CollectorId, EdgeKey, EdgeKind, EntityId, EntityKind, Evidence, ProcessIdentity,
};
pub use metric::{Attr, Metric, Unavailable, Value};
pub use time::{Clock, MonotonicClock, Timestamp, VirtualClock};
