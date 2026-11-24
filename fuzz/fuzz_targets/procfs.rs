// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! System-wide procfs/sysfs text parsers on arbitrary input.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rtop_linux::parse::{cgroup, disk, kmsg, key_values, net, system};

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let _ = system::proc_stat(&s);
    let _ = system::loadavg(&s);
    let _ = system::uptime(&s);
    let _ = system::pressure(&s);
    let _ = system::cpu_list(&s);
    let _ = key_values(&s).count();
    let _ = disk::diskstats(&s);
    let _ = disk::mountinfo(&s);
    let _ = net::net_dev(&s);
    let _ = net::inet_table(&s);
    let _ = net::unix_table(&s);
    let _ = net::snmp(&s);
    let _ = cgroup::limit(&s);
    let _ = cgroup::cpu_max(&s);
    let _ = cgroup::io_stat(&s);
    let _ = cgroup::container_from_path(&s);
    if let Some(r) = kmsg::record(&s) {
        let _ = kmsg::classify(&r.message);
        let _ = kmsg::subject_pid(&r.message);
    }
});
