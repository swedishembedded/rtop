// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Metric values with explicit availability.
//!
//! A measured zero is `Metric::Valid(Value::U64(0))`. Anything rtop could not
//! measure is `Metric::Unavailable(reason)`; there is no implicit default.

use std::fmt;

/// A numeric measurement. Integers stay integers so large counters keep full
/// precision in exports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    U64(u64),
    I64(i64),
    F64(f64),
}

impl Value {
    pub fn as_f64(self) -> f64 {
        match self {
            Value::U64(v) => v as f64,
            Value::I64(v) => v as f64,
            Value::F64(v) => v,
        }
    }
}

impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Value::U64(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::I64(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::F64(v)
    }
}

/// Why a metric has no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Unavailable {
    /// First observation taken; a rate needs a second one.
    Pending,
    PermissionDenied,
    /// The kernel, driver or hardware does not expose it.
    Unsupported,
    /// The collector providing it is switched off (e.g. emergency mode).
    Disabled,
    /// Not subscribed to in the current view.
    NotCollected,
    /// It cannot be measured cheaply or reliably (e.g. exclusive volume usage).
    NotMeasurable,
    /// The source returned data rtop could not interpret.
    InvalidData,
    /// The entity disappeared while it was being sampled.
    Removed,
    /// The provider did not answer within its deadline.
    TimedOut,
    /// The counter reset or wrapped ambiguously; a new baseline was taken.
    CounterReset,
}

impl Unavailable {
    /// Stable machine-readable name used in YAML/JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Unavailable::Pending => "pending",
            Unavailable::PermissionDenied => "permission_denied",
            Unavailable::Unsupported => "unsupported",
            Unavailable::Disabled => "disabled",
            Unavailable::NotCollected => "not_collected",
            Unavailable::NotMeasurable => "not_measurable",
            Unavailable::InvalidData => "invalid_data",
            Unavailable::Removed => "removed",
            Unavailable::TimedOut => "timed_out",
            Unavailable::CounterReset => "counter_reset",
        }
    }

    /// Short label for table cells (`N/A (perm)` style).
    pub fn short(self) -> &'static str {
        match self {
            Unavailable::Pending | Unavailable::CounterReset => "…",
            Unavailable::PermissionDenied => "N/A (perm)",
            Unavailable::Unsupported => "N/A (unsup)",
            Unavailable::Disabled => "N/A (off)",
            Unavailable::NotCollected => "N/A (idle)",
            Unavailable::NotMeasurable => "N/A",
            Unavailable::InvalidData => "N/A (bad)",
            Unavailable::Removed => "N/A (gone)",
            Unavailable::TimedOut => "N/A (timeout)",
        }
    }
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A metric reading as published by a collector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Metric {
    Valid(Value),
    /// A value derived from heuristics (e.g. Docker-style working set).
    Estimated(Value),
    Unavailable(Unavailable),
}

impl Metric {
    pub fn value(&self) -> Option<Value> {
        match self {
            Metric::Valid(v) | Metric::Estimated(v) => Some(*v),
            Metric::Unavailable(_) => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        self.value().map(Value::as_f64)
    }

    pub fn u64(v: u64) -> Self {
        Metric::Valid(Value::U64(v))
    }

    pub fn f64(v: f64) -> Self {
        if v.is_finite() {
            Metric::Valid(Value::F64(v))
        } else {
            Metric::Unavailable(Unavailable::InvalidData)
        }
    }

    pub fn i64(v: i64) -> Self {
        Metric::Valid(Value::I64(v))
    }

    pub fn unavailable(reason: Unavailable) -> Self {
        Metric::Unavailable(reason)
    }

    /// Turns a fallible reading into a metric.
    pub fn from_result<T: Into<Value>>(r: Result<T, Unavailable>) -> Self {
        match r {
            Ok(v) => match v.into() {
                Value::F64(f) => Metric::f64(f),
                other => Metric::Valid(other),
            },
            Err(e) => Metric::Unavailable(e),
        }
    }
}

/// Descriptive (non-metric) attribute of an entity: names, states, paths.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Attr {
    Text(String),
    Int(i64),
    UInt(u64),
    Bool(bool),
}

impl Attr {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Attr::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Attr::UInt(v) => Some(*v),
            Attr::Int(v) => u64::try_from(*v).ok(),
            _ => None,
        }
    }
}

impl From<&str> for Attr {
    fn from(s: &str) -> Self {
        Attr::Text(s.to_owned())
    }
}
impl From<String> for Attr {
    fn from(s: String) -> Self {
        Attr::Text(s)
    }
}
impl From<u64> for Attr {
    fn from(v: u64) -> Self {
        Attr::UInt(v)
    }
}
impl From<u32> for Attr {
    fn from(v: u32) -> Self {
        Attr::UInt(u64::from(v))
    }
}
impl From<i64> for Attr {
    fn from(v: i64) -> Self {
        Attr::Int(v)
    }
}
impl From<bool> for Attr {
    fn from(v: bool) -> Self {
        Attr::Bool(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_finite_floats_are_invalid_not_zero() {
        assert_eq!(
            Metric::f64(f64::NAN),
            Metric::Unavailable(Unavailable::InvalidData)
        );
        assert_eq!(
            Metric::f64(f64::INFINITY),
            Metric::Unavailable(Unavailable::InvalidData)
        );
        assert_eq!(Metric::f64(0.0).as_f64(), Some(0.0));
    }
}
