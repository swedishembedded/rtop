// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Disk health (NVMe SMART / Health log, ATA SMART) attached to block
//! devices.
//!
//! Polled once a minute: the read-only commands involved can wake a drive
//! from a low-power state. Each device gets a verdict (`ok`, `warning`,
//! `failing`) derived only from the drive's own reported state, plus the
//! underlying counters. Without access to the device nodes (usually root)
//! health is reported as unavailable, never as healthy.

use std::time::Duration;

use rtop_core::{CollectorId, EntityKind, Metric, NodeUpdate, Unavailable};
use rtop_linux::KernelError;
use rtop_linux::disk_health::{AtaSmart, NvmeSmart};
use rtop_linux::fs::KernelFsExt;

use crate::storage::block_device_id;
use crate::{Collector, CollectorError, Descriptor, Env, Isolation, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("disk_health");

#[derive(Debug)]
pub struct HealthCollector {
    env: Env,
}

impl HealthCollector {
    pub fn new(env: Env) -> Self {
        HealthCollector { env }
    }
}

/// Verdict and reason from an NVMe health log.
pub fn nvme_verdict(s: &NvmeSmart) -> (&'static str, String) {
    let w = s.critical_warning;
    if w & (0b1 | 0b100 | 0b1000 | 0b10000) != 0
        || s.available_spare_percent < s.available_spare_threshold_percent
    {
        let mut why = Vec::new();
        if w & 1 != 0 || s.available_spare_percent < s.available_spare_threshold_percent {
            why.push("spare capacity below threshold");
        }
        if w & 0b100 != 0 {
            why.push("reliability degraded");
        }
        if w & 0b1000 != 0 {
            why.push("media is read-only");
        }
        if w & 0b10000 != 0 {
            why.push("volatile backup failed");
        }
        return ("failing", why.join(", "));
    }
    if w & 0b10 != 0 {
        return ("warning", "temperature outside its limits".into());
    }
    if s.percentage_used >= 100 {
        return (
            "warning",
            format!("rated endurance used ({}%)", s.percentage_used),
        );
    }
    if s.media_errors > 0 {
        return (
            "warning",
            format!("{} media/data integrity errors", s.media_errors),
        );
    }
    ("ok", String::new())
}

/// Verdict and reason from ATA SMART data.
pub fn ata_verdict(s: &AtaSmart) -> (&'static str, String) {
    if s.healthy == Some(false) {
        return ("failing", "drive reports a SMART threshold exceeded".into());
    }
    let mut why = Vec::new();
    for (id, what) in [
        (5u8, "reallocated sectors"),
        (197, "pending sectors"),
        (198, "uncorrectable sectors"),
    ] {
        if let Some(n) = s.raw(id).filter(|n| *n > 0) {
            why.push(format!("{n} {what}"));
        }
    }
    if why.is_empty() {
        ("ok", String::new())
    } else {
        ("warning", why.join(", "))
    }
}

fn opt(v: Option<u64>) -> Metric {
    v.map_or(Metric::unavailable(Unavailable::Unsupported), Metric::u64)
}

impl Collector for HealthCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(60),
            timeout: Duration::from_secs(15),
            isolation: Isolation::MayBlock,
            essential: false,
            nice: 5,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let fs = self.env.fs.clone();
        let mut patch = ctx
            .patch(ID)
            .complete(rtop_core::Scope::Nodes(EntityKind::BlockDevice));
        let (mut ok, mut first_err) = (0usize, None);
        // NVMe controllers and the namespaces (block devices) they serve.
        let mut ctrls = fs.read_dir("/sys/class/nvme").unwrap_or_default();
        ctrls.sort();
        for ctrl in ctrls {
            let namespaces: Vec<String> = fs
                .read_dir(&format!("/sys/class/nvme/{ctrl}"))
                .unwrap_or_default()
                .into_iter()
                .filter(|n| n.starts_with(&ctrl) && n.contains('n'))
                .collect();
            let result = self.env.disk_health.nvme(&ctrl);
            let firmware = fs
                .read_attr(&format!("/sys/class/nvme/{ctrl}/firmware_rev"))
                .ok();
            for ns in namespaces {
                let mut n = NodeUpdate::augment(EntityKind::BlockDevice, block_device_id(&ns))
                    .attr("health_source", "nvme");
                if let Some(f) = &firmware {
                    n = n.text("firmware", f.trim());
                }
                match &result {
                    Ok(s) => {
                        let (verdict, why) = nvme_verdict(s);
                        n = n
                            .attr("health", verdict)
                            .text("health_detail", &why)
                            .attr("critical_warning", u64::from(s.critical_warning));
                        let big = |v: u128| u64::try_from(v).unwrap_or(u64::MAX);
                        n.push_metric("wear_percent", Metric::u64(u64::from(s.percentage_used)));
                        n.push_metric(
                            "available_spare_percent",
                            Metric::u64(u64::from(s.available_spare_percent)),
                        );
                        n.push_metric(
                            "temperature_celsius",
                            if s.temperature_kelvin > 0 {
                                Metric::f64(f64::from(s.temperature_kelvin) - 273.15)
                            } else {
                                Metric::unavailable(Unavailable::Unsupported)
                            },
                        );
                        n.push_metric("media_errors_total", Metric::u64(big(s.media_errors)));
                        n.push_metric(
                            "unsafe_shutdowns_total",
                            Metric::u64(big(s.unsafe_shutdowns)),
                        );
                        n.push_metric("power_on_hours", Metric::u64(big(s.power_on_hours)));
                        n.push_metric("power_cycles_total", Metric::u64(big(s.power_cycles)));
                        n.push_metric(
                            "data_read_bytes_total",
                            Metric::u64(big(s.data_units_read.saturating_mul(512_000))),
                        );
                        n.push_metric(
                            "data_written_bytes_total",
                            Metric::u64(big(s.data_units_written.saturating_mul(512_000))),
                        );
                    }
                    Err(e) => {
                        n = n.attr("health", "unknown");
                        n.push_metric(
                            "wear_percent",
                            Metric::unavailable(crate::unavailable_from(e)),
                        );
                    }
                }
                patch.node(n);
            }
            match result {
                Ok(_) => ok += 1,
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        // SATA/SAS disks behind libata.
        let mut disks = fs.read_dir("/sys/block").unwrap_or_default();
        disks.sort();
        for d in disks.into_iter().filter(|d| d.starts_with("sd")) {
            if fs
                .read_attr(&format!("/sys/block/{d}/device/vendor"))
                .map(|v| v.trim() == "ATA")
                .unwrap_or(false)
            {
                let mut n = NodeUpdate::augment(EntityKind::BlockDevice, block_device_id(&d))
                    .attr("health_source", "ata");
                match self.env.disk_health.ata(&d) {
                    Ok(s) => {
                        ok += 1;
                        let (verdict, why) = ata_verdict(&s);
                        n = n.attr("health", verdict).text("health_detail", &why);
                        n.push_metric("reallocated_sectors_total", opt(s.raw(5)));
                        n.push_metric("pending_sectors", opt(s.raw(197)));
                        n.push_metric("uncorrectable_sectors_total", opt(s.raw(198)));
                        n.push_metric("power_on_hours", opt(s.raw(9).map(|v| v & 0xffff_ffff)));
                        n.push_metric("power_cycles_total", opt(s.raw(12)));
                        n.push_metric(
                            "temperature_celsius",
                            s.raw(194)
                                .map_or(Metric::unavailable(Unavailable::Unsupported), |t| {
                                    Metric::f64((t & 0xff) as f64)
                                }),
                        );
                    }
                    Err(e) => {
                        n = n.attr("health", "unknown");
                        n.push_metric(
                            "reallocated_sectors_total",
                            Metric::unavailable(crate::unavailable_from(&e)),
                        );
                        first_err.get_or_insert(e);
                    }
                }
                patch.node(n);
            }
        }
        if ok == 0 {
            return Err(match first_err {
                Some(KernelError::PermissionDenied) => CollectorError::new(
                    Unavailable::PermissionDenied,
                    "reading drive health needs access to /dev/nvme* and /dev/sd* (usually root)",
                ),
                Some(e) => CollectorError::from_kernel("disk health", &e),
                None => CollectorError::new(Unavailable::Unsupported, "no NVMe or ATA disks found"),
            });
        }
        let degraded = first_err.map(|e| format!("some drives unreadable: {e}"));
        Ok(Sample { patch, degraded })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtop_linux::disk_health::AtaAttribute;

    #[test]
    fn verdicts_follow_the_drives_own_state() {
        let ok = NvmeSmart {
            available_spare_percent: 100,
            available_spare_threshold_percent: 10,
            ..Default::default()
        };
        assert_eq!(nvme_verdict(&ok).0, "ok");
        assert_eq!(
            nvme_verdict(&NvmeSmart {
                critical_warning: 0b100,
                ..ok
            })
            .0,
            "failing"
        );
        assert_eq!(
            nvme_verdict(&NvmeSmart {
                available_spare_percent: 5,
                ..ok
            })
            .0,
            "failing"
        );
        assert_eq!(
            nvme_verdict(&NvmeSmart {
                percentage_used: 104,
                ..ok
            })
            .0,
            "warning"
        );
        assert_eq!(
            nvme_verdict(&NvmeSmart {
                media_errors: 1,
                ..ok
            })
            .0,
            "warning"
        );
        let attr = |id, raw| AtaAttribute {
            id,
            value: 100,
            worst: 100,
            raw,
        };
        assert_eq!(
            ata_verdict(&AtaSmart {
                attributes: vec![attr(5, 0), attr(197, 0)],
                healthy: Some(true)
            })
            .0,
            "ok"
        );
        assert_eq!(
            ata_verdict(&AtaSmart {
                attributes: vec![attr(5, 8)],
                healthy: Some(true)
            }),
            ("warning", "8 reallocated sectors".into())
        );
        assert_eq!(
            ata_verdict(&AtaSmart {
                attributes: vec![],
                healthy: Some(false)
            })
            .0,
            "failing"
        );
    }
}
