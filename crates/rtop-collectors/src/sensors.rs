// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Hardware sensors (hwmon, thermal zones), RAPL power and batteries.
//!
//! hwmon numbering is not stable and says nothing about what a sensor
//! measures; entities are keyed by chip name plus attribute and labelled
//! with the driver's own label. Units follow the hwmon ABI: millidegrees,
//! millivolts, milliamps, microwatts, microjoules. Battery energy (µWh) and
//! charge (µAh) are different quantities and are never mixed.

use std::collections::HashMap;
use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker};
use rtop_core::{
    CollectorId, EdgeKind, EntityId, EntityKind, Metric, NodeUpdate, Scope, Unavailable,
};
use rtop_linux::fs::{KernelFs, KernelFsExt};

use crate::{Collector, CollectorError, Descriptor, Env, Isolation, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("sensors");
/// Passes between rediscoveries of hwmon chips and their static attributes.
const DISCOVERY_REFRESH: u32 = 30;

/// Static description of one hwmon input.
#[derive(Debug, Clone)]
struct SensorSpec {
    id: EntityId,
    input: String,
    alarm: Option<String>,
    chip_key: String,
    label: String,
    kind: &'static str,
    unit: &'static str,
    scale: f64,
    max: Option<f64>,
    crit: Option<f64>,
    min: Option<f64>,
    /// Last read took over SLOW_READ (an NVMe admin command, an embedded
    /// controller transaction): refresh only every SLOW_EVERY passes.
    slow: bool,
    last: Option<Result<f64, Unavailable>>,
}

const SLOW_READ: std::time::Duration = std::time::Duration::from_millis(1);
const SLOW_EVERY: u32 = 5;

fn discover_hwmon(fs: &dyn KernelFs) -> Vec<SensorSpec> {
    let mut out = Vec::new();
    let mut chip_names: HashMap<String, u32> = HashMap::new();
    for hw in sorted_dir(fs, "/sys/class/hwmon") {
        let base = format!("/sys/class/hwmon/{hw}");
        let chip = fs
            .read_attr(&format!("{base}/name"))
            .unwrap_or_else(|_| hw.clone());
        // Two chips with the same driver name get distinct keys.
        let seq = chip_names.entry(chip.clone()).or_insert(0);
        let chip_key = if *seq == 0 {
            chip.clone()
        } else {
            format!("{chip}#{seq}")
        };
        *seq += 1;
        let attrs = sorted_dir(fs, &base);
        for attr in &attrs {
            let Some(stem) = attr.strip_suffix("_input") else {
                continue;
            };
            if out.len() >= MAX_SENSORS {
                return out;
            }
            let class = stem.trim_end_matches(|c: char| c.is_ascii_digit());
            let (kind, unit, scale) = match class {
                "temp" => ("temperature", "celsius", 1e-3),
                "fan" => ("fan", "rpm", 1.0),
                "in" => ("voltage", "volts", 1e-3),
                "curr" => ("current", "amperes", 1e-3),
                "power" => ("power", "watts", 1e-6),
                "humidity" => ("humidity", "percent", 1e-3),
                "freq" => ("frequency", "hertz", 1.0),
                _ => continue,
            };
            let has = |suffix: &str| attrs.iter().any(|a| a == &format!("{stem}_{suffix}"));
            let threshold = |suffix: &str| {
                has(suffix)
                    .then(|| {
                        fs.read_i64(&format!("{base}/{stem}_{suffix}"))
                            .ok()
                            .map(|v| v as f64 * scale)
                    })
                    .flatten()
            };
            out.push(SensorSpec {
                id: EntityId::new(EntityKind::Sensor, format_args!("{chip_key}/{stem}")),
                input: format!("{base}/{attr}"),
                alarm: has("alarm").then(|| format!("{base}/{stem}_alarm")),
                label: if has("label") {
                    fs.read_attr(&format!("{base}/{stem}_label"))
                        .unwrap_or_else(|_| stem.to_owned())
                } else {
                    stem.to_owned()
                },
                chip_key: chip_key.clone(),
                kind,
                unit,
                scale,
                max: threshold("max"),
                crit: threshold("crit"),
                min: threshold("min"),
                slow: false,
                last: None,
            });
        }
    }
    out
}
const MAX_SENSORS: usize = 4096;

#[derive(Debug)]
pub struct SensorCollector {
    env: Env,
    rates: RateTracker<String>,
    specs: Vec<SensorSpec>,
    specs_age: u32,
}

impl SensorCollector {
    pub fn new(env: Env) -> Self {
        SensorCollector {
            env,
            rates: RateTracker::new(),
            specs: Vec::new(),
            specs_age: 0,
        }
    }
}

fn sorted_dir(fs: &dyn KernelFs, path: &str) -> Vec<String> {
    let mut v = fs.read_dir(path).unwrap_or_default();
    v.sort_by_key(|s| (s.len(), s.clone()));
    v
}

impl Collector for SensorCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(4),
            isolation: Isolation::MayBlock,
            essential: false,
            nice: 5,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let fs = self.env.fs.clone();
        let fs = fs.as_ref();
        let mut patch = ctx
            .patch(ID)
            .complete(Scope::Nodes(EntityKind::Sensor))
            .complete(Scope::Nodes(EntityKind::Battery))
            .complete(Scope::Nodes(EntityKind::PowerSupply))
            .complete(Scope::Edges(EdgeKind::Measures));
        let mut hottest: Option<(f64, String)> = None;
        self.rates.begin();

        // Sensor names, labels and thresholds are static: they are
        // discovered every DISCOVERY_REFRESH passes; each sample then reads
        // only the live `*_input` (and alarm) attributes.
        if self.specs_age == 0 || self.specs_age >= DISCOVERY_REFRESH {
            self.specs = discover_hwmon(fs);
            self.specs_age = 0;
        }
        self.specs_age += 1;
        let pass = self.specs_age;
        for sp in &mut self.specs {
            let mut n = NodeUpdate::new(EntityKind::Sensor, sp.id.clone())
                .text("name", &format!("{} {}", sp.chip_key, sp.label))
                .text("chip", &sp.chip_key)
                .text("label", &sp.label)
                .attr("sensor_kind", sp.kind)
                .attr("unit", sp.unit);
            let value = match (&sp.last, sp.slow && !pass.is_multiple_of(SLOW_EVERY)) {
                (Some(last), true) => *last,
                _ => {
                    let started = std::time::Instant::now();
                    let v = fs
                        .read_i64(&sp.input)
                        .map(|v| v as f64 * sp.scale)
                        .map_err(|e| crate::unavailable_from(&e));
                    sp.slow = started.elapsed() > SLOW_READ;
                    sp.last = Some(v);
                    v
                }
            };
            match &value {
                Ok(v) => n.push_metric("value", Metric::f64(*v)),
                Err(e) => n.push_metric("value", Metric::unavailable(*e)),
            }
            for (key, v) in [("max", sp.max), ("critical", sp.crit), ("min", sp.min)] {
                if let Some(v) = v {
                    n.push_metric(key, Metric::f64(v));
                }
            }
            if let Some(path) = &sp.alarm
                && let Ok(a) = fs.read_u64(path)
            {
                n = n.attr("alarm", a != 0);
            }
            if let (Ok(v), "temperature") = (value, sp.kind)
                && hottest.as_ref().is_none_or(|(h, _)| v > *h)
            {
                hottest = Some((v, format!("{} {}", sp.chip_key, sp.label)));
            }
            patch.node(n);
        }

        for z in sorted_dir(fs, "/sys/class/thermal")
            .into_iter()
            .filter(|z| z.starts_with("thermal_zone"))
        {
            let base = format!("/sys/class/thermal/{z}");
            // A zone with its own hwmon device is already a hwmon sensor;
            // reading it twice doubles a slow ACPI/EC query.
            if sorted_dir(fs, &base).iter().any(|e| e.starts_with("hwmon")) {
                continue;
            }
            let ty = fs
                .read_attr(&format!("{base}/type"))
                .unwrap_or_else(|_| z.clone());
            let mut n = NodeUpdate::new(
                EntityKind::Sensor,
                EntityId::new(EntityKind::Sensor, format_args!("thermal/{z}")),
            )
            .text("name", &ty)
            .text("chip", "thermal")
            .text("label", &ty)
            .attr("sensor_kind", "temperature")
            .attr("unit", "celsius");
            match fs.read_i64(&format!("{base}/temp")) {
                Ok(v) => n.push_metric("value", Metric::f64(v as f64 / 1000.0)),
                Err(e) => n.push_metric("value", Metric::unavailable(crate::unavailable_from(&e))),
            }
            patch.node(n);
        }

        // RAPL: energy counters -> average power over the sample interval.
        let mut package_power = None;
        for zone in sorted_dir(fs, "/sys/class/powercap")
            .into_iter()
            .filter(|z| z.starts_with("intel-rapl:") && z.matches(':').count() == 1)
        {
            let base = format!("/sys/class/powercap/{zone}");
            let name = fs
                .read_attr(&format!("{base}/name"))
                .unwrap_or_else(|_| zone.clone());
            let id = EntityId::new(EntityKind::Sensor, format_args!("rapl/{zone}"));
            let mut n = NodeUpdate::new(EntityKind::Sensor, id)
                .text("name", &format!("RAPL {name}"))
                .text("chip", "rapl")
                .text("label", &name)
                .attr("sensor_kind", "power")
                .attr("unit", "watts");
            match fs.read_u64(&format!("{base}/energy_uj")) {
                Ok(uj) => {
                    // energy_uj wraps at max_energy_range_uj, not at a power of two.
                    let range = fs.read_u64(&format!("{base}/max_energy_range_uj")).ok();
                    let m = match (
                        self.rates
                            .observe(zone.clone(), uj, ctx.now, CounterWidth::Bits64),
                        range,
                    ) {
                        (Ok(r), _) => Metric::f64(r * 1e-6),
                        (Err(rtop_core::counter::RateError::Reset), Some(_)) => {
                            Metric::unavailable(Unavailable::Pending)
                        }
                        (Err(e), _) => Metric::unavailable(e.into()),
                    };
                    if name.starts_with("package") {
                        package_power = m.as_f64().map(|p| package_power.unwrap_or(0.0) + p);
                    }
                    n.push_metric("value", m);
                }
                Err(e) => n.push_metric("value", Metric::unavailable(crate::unavailable_from(&e))),
            }
            patch.node(n);
        }
        self.rates.sweep();

        let mut on_battery = None;
        for ps in sorted_dir(fs, "/sys/class/power_supply") {
            let base = format!("/sys/class/power_supply/{ps}");
            let ty = fs.read_attr(&format!("{base}/type")).unwrap_or_default();
            let num = |f: &str| fs.read_i64(&format!("{base}/{f}")).ok();
            if ty == "Battery" {
                let id = EntityId::new(EntityKind::Battery, &ps);
                let mut n = NodeUpdate::new(EntityKind::Battery, id).text("name", &ps);
                for (attr, key) in [
                    ("status", "status"),
                    ("manufacturer", "manufacturer"),
                    ("model_name", "model"),
                    ("technology", "technology"),
                    ("capacity_level", "capacity_level"),
                ] {
                    if let Ok(v) = fs.read_attr(&format!("{base}/{attr}")) {
                        n = n.text(key, v.trim());
                    }
                }
                let status = fs.read_attr(&format!("{base}/status")).unwrap_or_default();
                let opt = |v: Option<i64>, scale: f64| {
                    v.map_or(Metric::unavailable(Unavailable::Unsupported), |v| {
                        Metric::f64(v as f64 * scale)
                    })
                };
                n.push_metric("capacity_percent", opt(num("capacity"), 1.0));
                n.push_metric("energy_now_wh", opt(num("energy_now"), 1e-6));
                n.push_metric("energy_full_wh", opt(num("energy_full"), 1e-6));
                n.push_metric(
                    "energy_full_design_wh",
                    opt(num("energy_full_design"), 1e-6),
                );
                n.push_metric("charge_now_ah", opt(num("charge_now"), 1e-6));
                n.push_metric("charge_full_ah", opt(num("charge_full"), 1e-6));
                n.push_metric("voltage_v", opt(num("voltage_now"), 1e-6));
                n.push_metric("current_a", opt(num("current_now"), 1e-6));
                n.push_metric("cycle_count", opt(num("cycle_count"), 1.0));
                // Power: reported directly, or V*I. Health: full / design.
                let power = num("power_now").map(|p| p as f64 * 1e-6).or_else(|| {
                    num("voltage_now")
                        .zip(num("current_now"))
                        .map(|(v, i)| (v as f64 * 1e-6) * (i as f64 * 1e-6).abs())
                });
                n.push_metric(
                    "power_w",
                    power.map_or(Metric::unavailable(Unavailable::Unsupported), Metric::f64),
                );
                let health = num("energy_full")
                    .zip(num("energy_full_design"))
                    .or_else(|| num("charge_full").zip(num("charge_full_design")))
                    .filter(|(_, d)| *d > 0)
                    .map(|(f, d)| 100.0 * f as f64 / d as f64);
                n.push_metric(
                    "health_percent",
                    health.map_or(Metric::unavailable(Unavailable::Unsupported), Metric::f64),
                );
                // Remaining time from like quantities only (Wh / W or Ah / A).
                let remaining = match status.as_str() {
                    "Discharging" => num("energy_now")
                        .zip(power.map(|p| p / 1e-6))
                        .map(|(e, p)| e as f64 / p)
                        .or_else(|| {
                            num("charge_now")
                                .zip(num("current_now"))
                                .map(|(c, i)| c as f64 / (i as f64).abs())
                        }),
                    _ => None,
                };
                n.push_metric(
                    "time_remaining_seconds",
                    remaining
                        .filter(|r| r.is_finite() && *r > 0.0)
                        .map_or(Metric::unavailable(Unavailable::NotMeasurable), |h| {
                            Metric::f64(h * 3600.0)
                        }),
                );
                patch.node(n);
                on_battery = Some(status == "Discharging");
            } else if ty == "Mains" || ty == "USB" {
                let online = num("online").map(|v| v == 1);
                let mut n = NodeUpdate::new(
                    EntityKind::PowerSupply,
                    EntityId::new(EntityKind::PowerSupply, &ps),
                )
                .text("name", &ps)
                .attr("supply_type", ty);
                if let Some(o) = online {
                    n = n.attr("online", o);
                }
                patch.node(n);
            }
        }
        let mut h = NodeUpdate::new(EntityKind::Host, EntityId::host());
        if let Some((t, label)) = hottest {
            h = h
                .metric("temperature_max_celsius", Metric::f64(t))
                .text("hottest_sensor", &label);
        }
        if let Some(p) = package_power {
            h = h.metric("cpu_package_power_w", Metric::f64(p));
        }
        if let Some(b) = on_battery {
            h = h.attr("on_battery", b);
        }
        patch.node(h);
        Ok(Sample {
            patch,
            degraded: None,
        })
    }
}
