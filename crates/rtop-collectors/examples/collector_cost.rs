// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Measures the CPU time each collector spends per sample on this machine,
//! with the subscriptions of the default (overview) screen, using the
//! thread CPU clock so time spent waiting or descheduled is excluded.
//!
//! cargo run --release -p rtop-collectors --example collector_cost [samples]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rtop_collectors::{Env, SampleContext, Subscriptions};
use rtop_core::Timestamp;

fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20);
    let subs = Arc::new(Subscriptions::default());
    subs.process_io.store(true, Ordering::Relaxed);
    let env = Env::live(subs);
    let mut collectors = rtop_collectors::all(&env);
    let mut total = Duration::ZERO;
    println!(
        "{:<12} {:>10} {:>9} {:>12}",
        "collector", "cpu/sample", "interval", "cpu % of 1"
    );
    for c in &mut collectors {
        let d = c.descriptor();
        let mut spent = Duration::ZERO;
        for i in 1..=n {
            let t = thread_cpu();
            let _ = c.sample(&SampleContext {
                now: Timestamp::from_millis(i * 1000),
                sequence: i,
            });
            spent += thread_cpu() - t;
        }
        let per = spent / n as u32;
        let pct = 100.0 * per.as_secs_f64() / d.interval.as_secs_f64();
        total += per.mul_f64(1.0 / d.interval.as_secs_f64());
        println!(
            "{:<12} {:>8.3}ms {:>8.1}s {:>11.3}%",
            d.id.as_str(),
            per.as_secs_f64() * 1e3,
            d.interval.as_secs_f64(),
            pct
        );
    }
    println!("{:<12} {:>32.3}%", "total", 100.0 * total.as_secs_f64());
}
